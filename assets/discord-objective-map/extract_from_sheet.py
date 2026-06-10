#!/usr/bin/env python3
"""Build discord map PNG icons from designer sources (PNG alpha or ICON-export sheet)."""
from __future__ import annotations

import json
import sys
from pathlib import Path

import numpy as np
from PIL import Image

ROOT = Path(__file__).resolve().parent


def hex_rgb(color: str) -> tuple[int, int, int]:
    color = color.lstrip("#")
    return int(color[0:2], 16), int(color[2:4], 16), int(color[4:6], 16)


def alpha_mask(img: Image.Image, threshold: int) -> np.ndarray:
    rgba = np.array(img.convert("RGBA"))
    alpha = rgba[:, :, 3]
    if alpha.max() > threshold:
        return alpha >= threshold
    lum = rgba[:, :, :3].max(axis=2)
    return lum >= threshold


def bbox_from_mask(mask: np.ndarray) -> tuple[int, int, int, int]:
    rows = np.where(mask.any(axis=1))[0]
    cols = np.where(mask.any(axis=0))[0]
    if rows.size == 0 or cols.size == 0:
        raise ValueError("empty icon mask")
    return int(cols[0]), int(rows[0]), int(cols[-1]), int(rows[-1])


def strip_label_block(block: np.ndarray) -> np.ndarray:
    """Drop OAB/OLO/… caption above the blank row gap; keep only the pictogram."""
    if not block.any():
        return block
    row_cov = block.sum(axis=1)
    h = block.shape[0]
    gap_blocks: list[tuple[int, int, int]] = []
    start: int | None = None
    for i, cov in enumerate(row_cov):
        if cov < 100:
            if start is None:
                start = i
        elif start is not None:
            if i - start >= 15:
                gap_blocks.append((start, i - 1, i - start))
            start = None
    if start is not None and h - start >= 15:
        gap_blocks.append((start, h - 1, h - start))
    upper = [g for g in gap_blocks if g[0] < h * 0.8]
    if not upper:
        return block
    cut = max(upper, key=lambda g: g[2])[1] + 1
    trimmed = block[cut:, :]
    return trimmed if trimmed.any() else block


def crop_mask(
    img: Image.Image,
    mask: np.ndarray,
    pad: int,
    *,
    strip_labels: bool = False,
) -> tuple[Image.Image, tuple[int, int]]:
    rows = np.where(mask.any(axis=1))[0]
    cols = np.where(mask.any(axis=0))[0]
    if rows.size == 0 or cols.size == 0:
        raise ValueError("empty icon mask")
    block = mask[rows[0] : rows[-1] + 1, cols[0] : cols[-1] + 1]
    if strip_labels:
        block = strip_label_block(block)
    x0, y0, x1, y1 = bbox_from_mask(block)
    x0 = max(0, x0 - pad)
    y0 = max(0, y0 - pad)
    x1 = min(block.shape[1] - 1, x1 + pad)
    y1 = min(block.shape[0] - 1, y1 + pad)
    sub_mask = block[y0 : y1 + 1, x0 : x1 + 1]
    gray = (sub_mask.astype(np.uint8) * 255)
    return Image.fromarray(gray, mode="L"), (gray.shape[1], gray.shape[0])


def segment_sheet(sheet: Image.Image, threshold: int, pad: int) -> list[tuple[Image.Image, tuple[int, int]]]:
    mask = alpha_mask(sheet, threshold)
    col_sum = mask.sum(axis=0)
    in_icon = col_sum > 5
    segments: list[tuple[int, int]] = []
    start: int | None = None
    for x, present in enumerate(in_icon):
        if present and start is None:
            start = x
        elif not present and start is not None:
            segments.append((start, x - 1))
            start = None
    if start is not None:
        segments.append((start, mask.shape[1] - 1))

    icons: list[tuple[Image.Image, tuple[int, int]]] = []
    for x0, x1 in segments:
        sub = mask[:, x0 : x1 + 1]
        rows = np.where(sub.any(axis=1))[0]
        y0, y1 = int(rows[0]), int(rows[-1])
        x0p = max(0, x0 - pad)
        y0p = max(0, y0 - pad)
        x1p = min(mask.shape[1] - 1, x1 + pad)
        y1p = min(mask.shape[0] - 1, y1 + pad)
        block = mask[y0p : y1p + 1, x0p : x1p + 1]
        block = strip_label_block(block)
        cx0, cy0, cx1, cy1 = bbox_from_mask(block)
        sub_mask = block[cy0 : cy1 + 1, cx0 : cx1 + 1]
        gray = Image.fromarray((sub_mask.astype(np.uint8) * 255), mode="L")
        icons.append((gray, gray.size))
    return icons


def load_source_icon(path: Path, threshold: int, pad: int) -> tuple[Image.Image, tuple[int, int]] | None:
    if not path.is_file():
        return None
    if path.suffix.lower() == ".cpt":
        return None
    img = Image.open(path)
    return crop_mask(img, alpha_mask(img, threshold), pad, strip_labels=False)


def recolor(gray: Image.Image, rgb: tuple[int, int, int], threshold: int) -> Image.Image:
    arr = np.array(gray)
    mask = arr >= threshold
    out = np.zeros((arr.shape[0], arr.shape[1], 4), dtype=np.uint8)
    out[mask, 0] = rgb[0]
    out[mask, 1] = rgb[1]
    out[mask, 2] = rgb[2]
    out[mask, 3] = 255
    return Image.fromarray(out, mode="RGBA")


def place_uniform(
    icons: dict[str, tuple[Image.Image, tuple[int, int]]],
    canvas_px: int,
    pad_px: int,
) -> dict[str, Image.Image]:
    max_w = max(size[0] for _, size in icons.values())
    max_h = max(size[1] for _, size in icons.values())
    scale = min(
        (canvas_px - 2 * pad_px) / max_w,
        (canvas_px - 2 * pad_px) / max_h,
    )
    placed: dict[str, Image.Image] = {}
    for kind, (gray, (w, h)) in icons.items():
        nw = max(1, int(round(w * scale)))
        nh = max(1, int(round(h * scale)))
        placed[kind] = gray.resize((nw, nh), Image.Resampling.NEAREST)
        print(f"{kind}: {w}x{h} -> {nw}x{nh} (scale {scale:.4f}, tight)")
    return placed


def main() -> int:
    cfg = json.loads((ROOT / "icon-sheet.json").read_text(encoding="utf-8"))
    threshold = int(cfg.get("alpha_threshold", 32))
    pad = int(cfg.get("pad_px", 4))
    canvas_px = int(cfg["canvas_px"])

    icons: dict[str, tuple[Image.Image, tuple[int, int]]] = {}

    for kind, src in cfg.get("sources", {}).items():
        path = Path(src)
        if not path.is_absolute():
            path = ROOT / path
        for candidate in (path, path.with_suffix(".png")):
            loaded = load_source_icon(candidate, threshold, pad)
            if loaded is not None:
                icons[kind] = loaded
                w, h = loaded[1]
                print(f"{kind}: {candidate.name} ({w}x{h})")
                break
        else:
            print(f"Missing source for {kind}: {path}", file=sys.stderr)
            return 1

    preview_dir = ROOT / "_extract_preview"
    preview_dir.mkdir(exist_ok=True)
    for kind, (gray, _) in icons.items():
        gray.save(preview_dir / f"{kind}_mask.png")

    if cfg.get("uniform_scale", True):
        placed = place_uniform(icons, canvas_px, pad)
    else:
        placed = {
            kind: gray.resize((canvas_px, canvas_px), Image.Resampling.LANCZOS)
            for kind, (gray, _) in icons.items()
        }

    out_dir = ROOT / "png" / str(canvas_px)
    out_dir.mkdir(parents=True, exist_ok=True)

    for kind, gray in placed.items():
        stems = cfg["manifest_stems"][kind]
        for coalition, stem in stems.items():
            rgb = hex_rgb(cfg["palette"][coalition])
            dest = out_dir / f"{stem}.png"
            recolor(gray, rgb, threshold).save(dest)
            print(f"wrote {dest.relative_to(ROOT)}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
