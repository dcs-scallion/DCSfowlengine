#!/usr/bin/env py -3
"""Generate virtual resupply decay curve figure for the Fowl engine user guide.

Defaults match VirtualResupplyDecayConfig. Looks for *_CFG in the working directory
(cwd at launch), loads virtual_resupply_decay values, and writes virtual_resupply_decay.png
there (overwrites an existing file).
"""

from __future__ import annotations

import json
import math
import sys
from dataclasses import dataclass
from pathlib import Path

import matplotlib.pyplot as plt
import numpy as np
from matplotlib.collections import LineCollection
from matplotlib.patches import FancyBboxPatch
from matplotlib.transforms import blended_transform_factory

# Engine defaults (VirtualResupplyDecayConfig)
DEFAULT_REFERENCE_DISTANCE_KM = 250
DEFAULT_EFFICIENCY_AT_REFERENCE_PCT = 25
DEFAULT_EFFICIENCY_FLOOR_PCT = 3

WIDTH_PX = 1200
HEIGHT_PX = 600
DPI = 100
LINE_WIDTH = 5
AXIS_LABEL_PAD = 68
# Left margin band between y-axis title and tick numbers (axes fraction).
Y_LABEL_BAND_X = -0.075

LABEL_BBOX = dict(
    boxstyle="round,pad=0.35",
    facecolor="#161616",
    edgecolor="#555555",
    alpha=0.96,
)


@dataclass(frozen=True)
class DecayConfig:
    reference_distance_km: int
    efficiency_at_reference_pct: int
    efficiency_floor_pct: int

    @classmethod
    def defaults(cls) -> DecayConfig:
        return cls(
            reference_distance_km=DEFAULT_REFERENCE_DISTANCE_KM,
            efficiency_at_reference_pct=DEFAULT_EFFICIENCY_AT_REFERENCE_PCT,
            efficiency_floor_pct=DEFAULT_EFFICIENCY_FLOOR_PCT,
        )

    @classmethod
    def from_cfg_json(cls, data: dict) -> DecayConfig:
        decay = data.get("virtual_resupply_decay") or {}
        base = cls.defaults()
        return cls(
            reference_distance_km=int(decay.get("reference_distance_km", base.reference_distance_km)),
            efficiency_at_reference_pct=int(
                decay.get("efficiency_at_reference_pct", base.efficiency_at_reference_pct)
            ),
            efficiency_floor_pct=int(decay.get("efficiency_floor_pct", base.efficiency_floor_pct)),
        )


def decay_rate(cfg: DecayConfig) -> float:
    floor = float(cfg.efficiency_floor_pct)
    at_ref = float(cfg.efficiency_at_reference_pct)
    ref_km = float(max(cfg.reference_distance_km, 1))
    numer = max(at_ref - floor, float(np.finfo(float).eps))
    denom = max(100.0 - floor, float(np.finfo(float).eps))
    return -math.log(numer / denom) / ref_km


def efficiency_at_distance_km(cfg: DecayConfig, distance_km: float) -> float:
    floor = float(cfg.efficiency_floor_pct)
    if distance_km <= 0.0:
        return 100.0
    k = decay_rate(cfg)
    return floor + (100.0 - floor) * math.exp(-k * distance_km)


def supply_efficiency_color_rgb(
    efficiency_pct: float, floor_pct: int
) -> tuple[float, float, float]:
    """Matches bflib markup `supply_efficiency_color` (without alpha)."""
    if efficiency_pct >= 100.0:
        return (0.0, 1.0, 0.0)
    span = max(100 - floor_pct, 1)
    t = max(0.0, min(1.0, (100.0 - efficiency_pct) / span))
    if t <= 0.5:
        u = t / 0.5
        return (0.75 * u, 1.0, 0.0)
    u = (t - 0.5) / 0.5
    return (0.75 + 0.25 * u, 1.0 - 0.5 * u, 0.0)


def find_cfg_file(run_dir: Path) -> Path | None:
    matches = sorted(run_dir.glob("*_CFG"))
    if len(matches) == 1:
        return matches[0]
    if len(matches) > 1:
        print(
            f"Multiple *_CFG in {run_dir}, using {matches[0].name}",
            file=sys.stderr,
        )
        return matches[0]
    return None


def load_decay_config(cfg_path: Path | None) -> DecayConfig:
    if cfg_path is None:
        print("No *_CFG found, using engine defaults")
        return DecayConfig.defaults()
    with cfg_path.open(encoding="utf-8") as fh:
        data = json.load(fh)
    cfg = DecayConfig.from_cfg_json(data)
    print(
        f"Loaded virtual_resupply_decay from {cfg_path.name}: "
        f"reference_distance_km={cfg.reference_distance_km}, "
        f"efficiency_at_reference_pct={cfg.efficiency_at_reference_pct}, "
        f"efficiency_floor_pct={cfg.efficiency_floor_pct}",
    )
    return cfg


def plot_max_distance_km(cfg: DecayConfig) -> float:
    return float(max(650, math.ceil(cfg.reference_distance_km * 2.6 / 50.0) * 50))


def x_axis_label_band_offset_pt(fig, ax) -> float:
    """Points below y=0 for the top edge of x-axis margin labels (below tick numbers)."""
    fig.canvas.draw()
    renderer = fig.canvas.get_renderer()
    tick_bottom = min(text.get_window_extent(renderer).y0 for text in ax.get_xticklabels())
    xlabel_top = ax.xaxis.label.get_window_extent(renderer).y1
    gap = tick_bottom - xlabel_top
    label_top_y = tick_bottom - max(8.0, gap * 0.15)
    anchor_display = ax.transData.transform((0.0, 0.0))
    return label_top_y - anchor_display[1]


def add_bottom_band_label(
    ax,
    *,
    x_data: float,
    text: str,
    arrow_to: tuple[float, float],
    ha: str,
    color: str,
    arrow_color: str,
    offset_pt: float,
) -> None:
    ax.annotate(
        text,
        xy=(x_data, 0),
        xycoords="data",
        xytext=(0, offset_pt),
        textcoords="offset points",
        fontsize=10,
        color=color,
        ha=ha,
        va="top",
        bbox=LABEL_BBOX,
        clip_on=False,
        arrowprops=(
            dict(arrowstyle="-|>", color=arrow_color, lw=1.0, shrinkA=2, shrinkB=3)
            if arrow_to == (x_data, 0.0)
            else None
        ),
        zorder=12,
    )
    if arrow_to != (x_data, 0.0):
        ax.annotate(
            "",
            xy=arrow_to,
            xytext=(x_data, 0),
            xycoords="data",
            textcoords="data",
            arrowprops=dict(arrowstyle="-|>", color=arrow_color, lw=1.0, shrinkA=2, shrinkB=3),
            zorder=11,
        )


def render(cfg: DecayConfig, out_path: Path) -> None:
    x_max = plot_max_distance_km(cfg)
    floor_line_start = max(x_max * 0.72, cfg.reference_distance_km * 1.5)

    distances = np.linspace(0.0, x_max, 800)
    efficiencies = np.array([efficiency_at_distance_km(cfg, d) for d in distances])
    ref_eff = efficiency_at_distance_km(cfg, float(cfg.reference_distance_km))

    fig_w = WIDTH_PX / DPI
    fig_h = HEIGHT_PX / DPI
    fig, ax = plt.subplots(figsize=(fig_w, fig_h), dpi=DPI)
    fig.patch.set_facecolor("#0a0a0a")
    ax.set_facecolor("#0a0a0a")

    ax.set_xlim(0, x_max)
    ax.set_ylim(0, 105)

    ax.tick_params(axis="both", colors="#c8c8c8", labelsize=11, pad=6)
    ax.grid(True, color="#333333", linewidth=0.6, alpha=0.9, zorder=0)
    for spine in ax.spines.values():
        spine.set_color("#555555")

    ax.set_xlabel(
        "Distance from logistics hub to objective (km)",
        fontsize=13,
        color="#e8e8e8",
        labelpad=AXIS_LABEL_PAD,
    )
    ax.set_ylabel(
        "Virtual resupply delivery efficiency (%)",
        fontsize=13,
        color="#e8e8e8",
        labelpad=AXIS_LABEL_PAD,
    )
    ax.set_title(
        "Virtual resupply efficiency vs. hub–objective distance",
        fontsize=16,
        fontweight="bold",
        color="#ffffff",
        pad=16,
    )

    points = np.array([distances, efficiencies]).T.reshape(-1, 1, 2)
    segments = np.concatenate([points[:-1], points[1:]], axis=1)
    mid_eff = (efficiencies[:-1] + efficiencies[1:]) / 2.0
    colors = [supply_efficiency_color_rgb(e, cfg.efficiency_floor_pct) for e in mid_eff]
    ax.add_collection(
        LineCollection(
            segments,
            colors=colors,
            linewidths=LINE_WIDTH,
            capstyle="round",
            joinstyle="round",
            zorder=2,
        )
    )

    ax.plot(
        [cfg.reference_distance_km, cfg.reference_distance_km],
        [0, ref_eff],
        color="#4caf50",
        linewidth=1.2,
        linestyle="--",
        alpha=0.85,
        zorder=1,
    )
    ax.plot(
        [0, cfg.reference_distance_km],
        [ref_eff, ref_eff],
        color="#4caf50",
        linewidth=1.2,
        linestyle="--",
        alpha=0.85,
        zorder=1,
    )
    ax.plot(
        [floor_line_start, x_max],
        [cfg.efficiency_floor_pct, cfg.efficiency_floor_pct],
        color="#ff9800",
        linewidth=1.2,
        linestyle=":",
        alpha=0.9,
        zorder=1,
    )

    ax.scatter([0], [100], s=48, color="#00e676", edgecolors="#ffffff", linewidths=0.8, zorder=4)
    ax.scatter(
        [cfg.reference_distance_km],
        [ref_eff],
        s=56,
        color="#cddc39",
        edgecolors="#ffffff",
        linewidths=0.8,
        zorder=4,
    )

    left_margin = blended_transform_factory(ax.transAxes, ax.transData)

    fig.subplots_adjust(left=0.16, right=0.93, top=0.90, bottom=0.31)
    bottom_label_offset_pt = x_axis_label_band_offset_pt(fig, ax)

    add_bottom_band_label(
        ax,
        x_data=float(cfg.reference_distance_km),
        text=f"reference_distance_km = {cfg.reference_distance_km}",
        arrow_to=(float(cfg.reference_distance_km), 0.0),
        ha="center",
        color="#a5d6a7",
        arrow_color="#81c784",
        offset_pt=bottom_label_offset_pt,
    )
    ax.annotate(
        f"efficiency_at_reference_pct = {cfg.efficiency_at_reference_pct}%",
        xy=(cfg.reference_distance_km, ref_eff),
        xycoords="data",
        xytext=(Y_LABEL_BAND_X, ref_eff),
        textcoords=left_margin,
        fontsize=10,
        color="#a5d6a7",
        ha="right",
        va="center",
        rotation=90,
        bbox=LABEL_BBOX,
        clip_on=False,
        arrowprops=dict(arrowstyle="-|>", color="#81c784", lw=1.0, shrinkA=4, shrinkB=3),
        zorder=12,
    )
    add_bottom_band_label(
        ax,
        x_data=x_max,
        text=f"efficiency_floor_pct = {cfg.efficiency_floor_pct}%",
        arrow_to=(x_max, float(cfg.efficiency_floor_pct)),
        ha="right",
        color="#ffcc80",
        arrow_color="#ffb74d",
        offset_pt=bottom_label_offset_pt,
    )

    k = decay_rate(cfg)
    panel_text = (
        "Engine formula (exponential decay with floor):\n"
        f"  E(d) = f + (100 - f) * exp(-k*d)\n"
        f"  k = -ln((r - f) / (100 - f)) / y\n"
        f"  k ~ {k:.5f}  (r={cfg.reference_distance_km}, "
        f"f={cfg.efficiency_floor_pct}, ref={cfg.efficiency_at_reference_pct})\n\n"
        "Curve color matches F10 supply-line gradient\n"
        "(green -> yellow -> orange by efficiency)."
    )
    panel_right = 0.97
    panel_width = 0.37 * 1.5
    panel_left = panel_right - panel_width
    panel_bottom = 0.56
    panel_height = 0.36
    ax.add_patch(
        FancyBboxPatch(
            (panel_left, panel_bottom),
            panel_width,
            panel_height,
            boxstyle="round,pad=0.02,rounding_size=0.02",
            transform=ax.transAxes,
            facecolor="#161616",
            edgecolor="#444444",
            linewidth=1.0,
            alpha=0.98,
            zorder=10,
        )
    )
    ax.text(
        panel_left + 0.02,
        panel_bottom + panel_height - 0.02,
        panel_text,
        transform=ax.transAxes,
        fontsize=9.0,
        color="#d0d0d0",
        va="top",
        ha="left",
        family="monospace",
        zorder=11,
        wrap=True,
    )

    out_path.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(
        out_path,
        facecolor=fig.get_facecolor(),
        dpi=DPI,
        bbox_inches=None,
        pad_inches=0,
    )
    plt.close(fig)


def main() -> None:
    run_dir = Path.cwd()
    cfg_path = find_cfg_file(run_dir)
    cfg = load_decay_config(cfg_path)
    out_path = run_dir / "virtual_resupply_decay.png"
    render(cfg, out_path)
    print(f"Wrote {out_path} ({WIDTH_PX}x{HEIGHT_PX} at {DPI} DPI)")


if __name__ == "__main__":
    main()
