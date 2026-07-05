/*
Copyright 2024 Eric Stokes.

This file is part of bflib.

bflib is free software: you can redistribute it and/or modify it under
the terms of the GNU Affero Public License as published by the Free
Software Foundation, either version 3 of the License, or (at your
option) any later version.

bflib is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE. See the GNU Affero Public License
for more details.
*/

use super::{
    aliases::resolve_objective_f10_map_label,
    logistics::{
        effective_hub_production, nearest_normal_logistics_hub, opr_feed_hub,
        objective_warehouse_fuel_infobar_amounts, production_feed_line_active,
        visible_occupied_supply_anchor, visible_production_feed_hub,
        virtual_resupply_delivery_efficiency_cached, virtual_resupply_link_active,
        virtual_resupply_threatened_blocks, virtual_resupply_threatened_without_deliveries,
    },
    objective::{Objective, Zone},
    persisted::Persisted,
};
use fxhash::FxHashMap;
use crate::msgq::MsgQ;
use bfprotocols::{
    cfg::Cfg,
    db::objective::{ObjectiveId, ObjectiveKind},
    fowl_miz_export::FowlMizExport,
};
use compact_str::{CompactString, format_compact};
use dcso3::{
    Color, LuaVec3, Vector2, Vector3,
    coalition::Side,
    trigger::{
        CircleSpec, LineType, MarkId, QuadSpec, SideFilter, TextSpec,
    },
};

/// Custom hub supply arrow: head length matches prior DCS default; width at 50%.
const SUPPLY_ARROW_HEAD_LENGTH_M: f64 = 2500.;
const SUPPLY_ARROW_HEAD_HALF_WIDTH_M: f64 = 500.;
/// Shaft width at ~60% of prior effective line weight.
const SUPPLY_ARROW_SHAFT_HALF_WIDTH_M: f64 = 150.;
const SUPPLY_CONNECTION_ALPHA: f32 = 0.6;

/// OPR→OLO feed shaft: half the supply-line shaft width; alpha at 100% Production.
const PRODUCTION_FEED_SHAFT_HALF_WIDTH_M: f64 = SUPPLY_ARROW_SHAFT_HALF_WIDTH_M / 2.;
const PRODUCTION_FEED_LINE_ALPHA: f32 = 0.5;
/// Normal OLO → occupied OLO resupply link (virtual, 100% delivery).
const OCCUPIED_HUB_SUPPLY_LINE_ALPHA: f32 = 0.5;
const OCCUPIED_HUB_SUPPLY_SHAFT_HALF_WIDTH_M: f64 = PRODUCTION_FEED_SHAFT_HALF_WIDTH_M * 2.;

#[derive(Debug, Clone, Copy)]
struct SupplyConnectionMark {
    shaft: MarkId,
    head: MarkId,
}

#[derive(Debug, Clone, Copy)]
struct SupplyConnectionGeometry {
    shaft: QuadSpec,
    head: [LuaVec3; 3],
}

static BAR_LOOKUP: [&'static str; 13] = [
    "░ ░ ░ ░ ░", // 0%
    "▒ ░ ░ ░ ░", // ~8%
    "▓ ░ ░ ░ ░", // ~17%
    "█ ▒ ░ ░ ░", // ~25%
    "█ ▓ ░ ░ ░", // ~33%
    "█ █ ▒ ░ ░", // ~42%
    "█ █ ▓ ░ ░", // ~50%
    "█ █ █ ▒ ░", // ~58%
    "█ █ █ ▓ ░", // ~67%
    "█ █ █ █ ░", // ~75%
    "█ █ █ █ ▒", // ~83%
    "█ █ █ █ ▓", // ~92%
    "█ █ █ █ █", // 100%
];

#[derive(Debug, Clone, Default)]
pub(super) struct ObjectiveMarkup {
    side: Side,
    threatened: bool,
    health: u8,
    logi: u8,
    supply: u8,
    fuel: u8,
    fuel_amounts: CompactString,
    fuel_kind_count: u8,
    production: u8,
    /// Last Production % shown on F10 stats (may differ from raw when threatened OPR feed is cut).
    display_production: u8,
    production_hp_sum: u32,
    production_repair_need: u16,
    production_repair: u16,
    static_repair_need: u16,
    static_repair: u16,
    spawnable_logi_repaired: bool,
    points: i32,
    name: String,
    owner_ring: MarkId,
    capturable_ring: MarkId,
    threatened_ring: MarkId,
    label: MarkId,
    /// Jedna statistika (FARP — vidí jen vlastník) nebo žádná při split.
    stats_label: Option<MarkId>,
    /// Statistika jen pro červenou stranu (když je markup `SideFilter::All`).
    stats_label_red: Option<MarkId>,
    /// Statistika jen pro modrou stranu (když je markup `SideFilter::All`).
    stats_label_blue: Option<MarkId>,
    pos: Vector2,
    supply_connections: FxHashMap<ObjectiveId, SupplyConnectionMark>,
    /// OPR → nearest OLO (quad shaft, no arrowhead).
    production_feed_hub: Option<ObjectiveId>,
    production_feed_line: Option<MarkId>,
    /// Nearest normal OLO → occupied OLO (solid coalition-colored line).
    occupied_supply_anchor: Option<ObjectiveId>,
    occupied_supply_line: Option<MarkId>,
}

fn text_color(side: Side, a: f32) -> Color {
    match side {
        Side::Red => Color::red(a),
        Side::Blue => Color::blue(a),
        Side::Neutral => Color::white(a),
    }
}

/// Cross-coalition view: Health/Logi only (no supply, fuel, or points).
fn enemy_objective_view(obj_owner: Side, viewer: Side) -> bool {
    matches!(
        (obj_owner, viewer),
        (Side::Red, Side::Blue) | (Side::Blue, Side::Red)
    )
}

/// Pad 0–100 column on infobar rows when DCS drops a digit (e.g. 100 → 99).
const STAT_VALUE_COL_WIDTH: usize = 5;
/// Gap between infobar block and value, and between value and label.
const STAT_BAR_GAP: &str = " ";
/// Plain rows (Repair, Points): `Label  value` from line start — no infobar alignment.
const STAT_PLAIN_LABEL_GAP: &str = "  ";

fn stat_value_column(value: &str) -> CompactString {
    let len = value.chars().count();
    if len > STAT_VALUE_COL_WIDTH {
        CompactString::from(value)
    } else {
        format_compact!("{:>w$}", value, w = STAT_VALUE_COL_WIDTH)
    }
}

fn stat_repair_field(queued: u16, need: u16) -> CompactString {
    format_compact!("{}/{}", queued, need)
}

fn stat_plain_row(label: &'static str, value: &str) -> CompactString {
    format_compact!("{label}{STAT_PLAIN_LABEL_GAP}{value}")
}

fn stat_plain_repair_row(queued: u16, need: u16) -> CompactString {
    stat_plain_row("Repair", stat_repair_field(queued, need).as_str())
}

fn base_stats_points_and_static_repair(obj: &Objective) -> CompactString {
    let points = stat_plain_row("Points", &format_compact!("{}", obj.points));
    if obj.show_static_repair_in_markup() {
        format_compact!(
            "{}\n{}",
            points,
            stat_plain_repair_row(obj.static_repair, obj.static_repair_need),
        )
    } else {
        points
    }
}

/// Infobar row: `{bar}{gap}{value}{gap}{label}`.
fn stat_row(bar: &str, value: &str, label: &'static str) -> CompactString {
    format_compact!(
        "{}{}{}{}{}",
        bar,
        STAT_BAR_GAP,
        stat_value_column(value),
        STAT_BAR_GAP,
        label
    )
}

fn stat_infobar_row(bar: &str, value: u8, label: &'static str) -> CompactString {
    stat_row(bar, &format_compact!("{}", value.min(100)), label)
}

fn stat_fuel_infobar_row(bar: &str, amounts: &str, kind_count: u8) -> CompactString {
    let label = if kind_count == 1 { "Fuel" } else { "Fuels" };
    format_compact!("{}{}{label} {amounts} t", bar, STAT_BAR_GAP)
}

fn production_pct_for_display(cfg: &Cfg, persisted: &Persisted, obj: &Objective) -> u8 {
    match obj.kind {
        ObjectiveKind::Logistics => effective_hub_production(cfg, persisted, obj),
        _ => obj.production,
    }
}

fn objective_stats_text(
    cfg: &Cfg,
    persisted: &Persisted,
    obj: &Objective,
    limited: bool,
    export: &FowlMizExport,
) -> CompactString {
    let get_idx = |val: u8| -> usize { (val as usize * 12 / 100).min(12) };
    let production = production_pct_for_display(cfg, persisted, obj);
    match (&obj.kind, limited) {
        (ObjectiveKind::Production, true) => {
            format_compact!("\n\n{}", stat_infobar_row(BAR_LOOKUP[get_idx(production)], production, "Production"))
        }
        (ObjectiveKind::Production, false) => {
            format_compact!(
                "\n\n{}\n{}\n{}",
                stat_infobar_row(BAR_LOOKUP[get_idx(production)], production, "Production"),
                stat_plain_row("Points", &format_compact!("{}", obj.points)),
                stat_plain_repair_row(obj.production_repair, obj.production_repair_slots_needed()),
            )
        }
        (ObjectiveKind::Logistics, true) => format_compact!(
            "\n\n{}\n{}\n{}",
            stat_infobar_row(BAR_LOOKUP[get_idx(production)], production, "Production"),
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.health)], obj.health, "Health"),
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.logi)], obj.logi, "Logi"),
        ),
        (ObjectiveKind::Logistics, false) => format_compact!(
            "\n\n{}\n{}\n{}\n{}\n{}\n{}",
            stat_infobar_row(BAR_LOOKUP[get_idx(production)], production, "Production"),
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.health)], obj.health, "Health"),
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.logi)], obj.logi, "Logi"),
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.supply)], obj.supply, "Supply"),
            {
                let (amounts, kinds) = objective_warehouse_fuel_infobar_amounts(export, obj);
                stat_fuel_infobar_row(
                    BAR_LOOKUP[get_idx(obj.fuel)],
                    amounts.as_str(),
                    kinds,
                )
            },
            base_stats_points_and_static_repair(obj),
        ),
        (_, true) => format_compact!(
            "\n\n{}\n{}",
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.health)], obj.health, "Health"),
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.logi)], obj.logi, "Logi"),
        ),
        (_, false) => format_compact!(
            "\n\n{}\n{}\n{}\n{}\n{}",
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.health)], obj.health, "Health"),
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.logi)], obj.logi, "Logi"),
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.supply)], obj.supply, "Supply"),
            {
                let (amounts, kinds) = objective_warehouse_fuel_infobar_amounts(export, obj);
                stat_fuel_infobar_row(
                    BAR_LOOKUP[get_idx(obj.fuel)],
                    amounts.as_str(),
                    kinds,
                )
            },
            base_stats_points_and_static_repair(obj),
        ),
    }
}

/// OPR→OLO feed line; black, alpha scales linearly with OPR Production (0–100).
fn production_feed_line_color(production: u8) -> Color {
    let alpha = PRODUCTION_FEED_LINE_ALPHA * (f32::from(production) / 100.);
    Color::black(alpha)
}

fn production_feed_line_geometry(opr: &Objective, hub: &Objective) -> QuadSpec {
    let (spos, dpos) = arrow_coords(opr, hub);
    let dir = (dpos - spos).normalize();
    let perp = Vector2::new(-dir.y, dir.x);
    let hw = PRODUCTION_FEED_SHAFT_HALF_WIDTH_M;
    let v3 = |p: Vector2| LuaVec3(Vector3::new(p.x, 0., p.y));
    QuadSpec {
        p0: v3(spos + perp * hw),
        p1: v3(spos - perp * hw),
        p2: v3(dpos - perp * hw),
        p3: v3(dpos + perp * hw),
        color: Color::black(0.),
        fill_color: Color::black(0.),
        line_type: LineType::NoLine,
        read_only: true,
    }
}

fn occupied_hub_supply_line_geometry(anchor: &Objective, occ: &Objective) -> QuadSpec {
    let (spos, dpos) = arrow_coords(anchor, occ);
    let dir = (dpos - spos).normalize();
    let perp = Vector2::new(-dir.y, dir.x);
    let hw = OCCUPIED_HUB_SUPPLY_SHAFT_HALF_WIDTH_M;
    let v3 = |p: Vector2| LuaVec3(Vector3::new(p.x, 0., p.y));
    QuadSpec {
        p0: v3(spos + perp * hw),
        p1: v3(spos - perp * hw),
        p2: v3(dpos - perp * hw),
        p3: v3(dpos + perp * hw),
        color: Color::black(0.),
        fill_color: Color::black(0.),
        line_type: LineType::NoLine,
        read_only: true,
    }
}

fn sync_supply_connection_checked(
    cfg: &Cfg,
    efficiency_cache: &mut FxHashMap<(ObjectiveId, ObjectiveId), u8>,
    msgq: &mut MsgQ,
    mark: &mut SupplyConnectionMark,
    to: SideFilter,
    hub: &Objective,
    dest: &Objective,
) {
    if !virtual_resupply_link_active(cfg, hub, dest) {
        msgq.delete_underlay_mark(mark.shaft);
        msgq.delete_underlay_mark(mark.head);
        return;
    }
    let eff = virtual_resupply_delivery_efficiency_cached(efficiency_cache, cfg, hub, dest);
    let color = supply_efficiency_color(eff, cfg.virtual_resupply_decay.efficiency_floor_pct);
    sync_supply_connection(msgq, mark, to, hub, dest, color);
}

fn sync_production_feed_line(
    cfg: &Cfg,
    t: &mut ObjectiveMarkup,
    msgq: &mut MsgQ,
    obj: &Objective,
    persisted: &Persisted,
) {
    if !matches!(obj.kind, ObjectiveKind::Production) {
        return;
    }
    if let Some(id) = t.production_feed_line.take() {
        msgq.delete_underlay_mark(id);
    }
    let Some(hid) = opr_feed_hub(persisted, obj) else {
        t.production_feed_hub = None;
        return;
    };
    let hub = match persisted.objectives.get(&hid) {
        Some(h) => h,
        None => {
            t.production_feed_hub = None;
            return;
        }
    };
    if !production_feed_line_active(cfg, obj, hub) {
        t.production_feed_hub = None;
        return;
    }
    t.production_feed_hub = Some(hid);
    let mut spec = production_feed_line_geometry(obj, hub);
    let color = production_feed_line_color(obj.production);
    spec.color = color;
    spec.fill_color = color;
    let id = MarkId::new();
    msgq.quad_to_underlay(SideFilter::All, id, spec, None);
    t.production_feed_line = Some(id);
}

fn sync_occupied_hub_supply_line(
    cfg: &Cfg,
    t: &mut ObjectiveMarkup,
    msgq: &mut MsgQ,
    obj: &Objective,
    persisted: &Persisted,
) {
    if let Some(id) = t.occupied_supply_line.take() {
        msgq.delete_underlay_mark(id);
    }
    t.occupied_supply_anchor = None;
    if !obj.is_occupied_logistics_hub() {
        return;
    }
    if virtual_resupply_threatened_blocks(cfg, obj) {
        return;
    }
    let Some(aid) = nearest_normal_logistics_hub(persisted, obj.owner, obj.zone.pos()) else {
        return;
    };
    let anchor = match persisted.objectives.get(&aid) {
        Some(h) => h,
        None => return,
    };
    if virtual_resupply_threatened_blocks(cfg, anchor) {
        return;
    }
    t.occupied_supply_anchor = Some(aid);
    let mut spec = occupied_hub_supply_line_geometry(anchor, obj);
    let color = text_color(obj.owner, OCCUPIED_HUB_SUPPLY_LINE_ALPHA);
    spec.color = color;
    spec.fill_color = color;
    let id = MarkId::new();
    msgq.quad_to_underlay(SideFilter::All, id, spec, None);
    t.occupied_supply_line = Some(id);
}

fn resync_logistics_supply_connections(
    cfg: &Cfg,
    efficiency_cache: &mut FxHashMap<(ObjectiveId, ObjectiveId), u8>,
    t: &mut ObjectiveMarkup,
    msgq: &mut MsgQ,
    hub: &Objective,
    persisted: &Persisted,
) {
    if !matches!(hub.kind, ObjectiveKind::Logistics) {
        return;
    }
    for oid in hub.warehouse.destination.into_iter() {
        let Some(dst) = persisted.objectives.get(oid) else {
            continue;
        };
        let to = if dst.is_farp() {
            dst.owner.into()
        } else {
            SideFilter::All
        };
        if !t.supply_connections.contains_key(oid) {
            t.supply_connections.insert(
                *oid,
                SupplyConnectionMark {
                    shaft: MarkId::new(),
                    head: MarkId::new(),
                },
            );
        }
        let mark = t.supply_connections.get_mut(oid).expect("just inserted");
        sync_supply_connection_checked(cfg, efficiency_cache, msgq, mark, to, hub, dst);
    }
}

pub(super) fn sync_logistics_hub_production_displays(
    cfg: &Cfg,
    persisted: &Persisted,
    msgq: &mut MsgQ,
    markups: &mut FxHashMap<ObjectiveId, ObjectiveMarkup>,
    export: &FowlMizExport,
) {
    if !virtual_resupply_threatened_without_deliveries(cfg) {
        return;
    }
    for hid in persisted.logistics_hubs.into_iter() {
        let Some(hub) = persisted.objectives.get(hid) else {
            continue;
        };
        if !matches!(hub.kind, ObjectiveKind::Logistics) {
            continue;
        }
        let eff = effective_hub_production(cfg, persisted, hub);
        let Some(mk) = markups.get_mut(hid) else {
            continue;
        };
        if eff == mk.display_production {
            continue;
        }
        mk.display_production = eff;
        mk.production = hub.production;
        refresh_objective_stats_labels(cfg, msgq, mk, hub, persisted, export);
    }
}

fn refresh_objective_stats_labels(
    cfg: &Cfg,
    msgq: &mut MsgQ,
    t: &ObjectiveMarkup,
    obj: &Objective,
    persisted: &Persisted,
    export: &FowlMizExport,
) {
    if let Some(id) = t.stats_label {
        msgq.set_overlay_markup_text(
            id,
            objective_stats_text(cfg, persisted, obj, false, export).into(),
        );
    } else {
        if let Some(id) = t.stats_label_red {
            msgq.set_overlay_markup_text(
                id,
                objective_stats_text(
                    cfg,
                    persisted,
                    obj,
                    enemy_objective_view(obj.owner, Side::Red),
                    export,
                )
                .into(),
            );
        }
        if let Some(id) = t.stats_label_blue {
            msgq.set_overlay_markup_text(
                id,
                objective_stats_text(
                    cfg,
                    persisted,
                    obj,
                    enemy_objective_view(obj.owner, Side::Blue),
                    export,
                )
                .into(),
            );
        }
    }
}

fn arrow_coords(obj: &Objective, dst: &Objective) -> (Vector2, Vector2) {
    let pos = obj.zone.pos();
    let dpos = dst.zone.pos();
    let dir = (dpos - pos).normalize();
    let spos = pos + dir * obj.zone.radius() * 1.1;
    let rdir = (pos - dpos).normalize();
    let dpos = dpos + rdir * dst.zone.radius() * 1.1;
    (spos, dpos)
}

fn supply_connection_geometry(hub: &Objective, dest: &Objective) -> SupplyConnectionGeometry {
    let (spos, dpos) = arrow_coords(hub, dest);
    let to_hub = (spos - dpos).normalize();
    let perp = Vector2::new(-to_hub.y, to_hub.x);
    let tip = dpos;
    let base = tip + to_hub * SUPPLY_ARROW_HEAD_LENGTH_M;
    let hw = SUPPLY_ARROW_HEAD_HALF_WIDTH_M;
    let shaft_hw = SUPPLY_ARROW_SHAFT_HALF_WIDTH_M;
    let v3 = |p: Vector2| LuaVec3(Vector3::new(p.x, 0., p.y));
    SupplyConnectionGeometry {
        shaft: QuadSpec {
            p0: v3(spos + perp * shaft_hw),
            p1: v3(spos - perp * shaft_hw),
            p2: v3(base - perp * shaft_hw),
            p3: v3(base + perp * shaft_hw),
            color: Color::black(0.),
            fill_color: Color::black(0.),
            line_type: LineType::NoLine,
            read_only: true,
        },
        head: [
            v3(tip),
            v3(base + perp * hw),
            v3(base - perp * hw),
        ],
    }
}

fn supply_efficiency_color(efficiency_pct: u8, floor_pct: u8) -> Color {
    if efficiency_pct >= 100 {
        return Color::green(SUPPLY_CONNECTION_ALPHA);
    }
    let span = (100 - floor_pct).max(1) as f32;
    let t = ((100 - efficiency_pct) as f32 / span).clamp(0., 1.);
    let (r, g, b) = if t <= 0.5 {
        let u = t / 0.5;
        (0.75 * u, 1., 0.)
    } else {
        let u = (t - 0.5) / 0.5;
        (0.75 + 0.25 * u, 1. - 0.5 * u, 0.)
    };
    Color::from_rgba(r, g, b, SUPPLY_CONNECTION_ALPHA)
}

fn sync_supply_connection(
    msgq: &mut MsgQ,
    mark: &mut SupplyConnectionMark,
    to: SideFilter,
    hub: &Objective,
    dest: &Objective,
    color: Color,
) {
    let geom = supply_connection_geometry(hub, dest);
    let mut shaft = geom.shaft;
    shaft.color = color;
    shaft.fill_color = color;
    msgq.delete_underlay_mark(mark.shaft);
    msgq.delete_underlay_mark(mark.head);
    mark.shaft = MarkId::new();
    mark.head = MarkId::new();
    msgq.quad_to_underlay(to, mark.shaft, shaft, None);
    msgq.freeform_to_underlay(
        to,
        mark.head,
        geom.head,
        color,
        color,
        LineType::NoLine,
        true,
        None,
    );
}

fn draw_supply_connection(
    msgq: &mut MsgQ,
    to: SideFilter,
    hub: &Objective,
    dest: &Objective,
    color: Color,
) -> SupplyConnectionMark {
    let mut mark = SupplyConnectionMark {
        shaft: MarkId::new(),
        head: MarkId::new(),
    };
    sync_supply_connection(msgq, &mut mark, to, hub, dest, color);
    mark
}

fn overlay_side_filter(obj: &Objective) -> SideFilter {
    match obj.kind {
        ObjectiveKind::Airbase
        | ObjectiveKind::Fob
        | ObjectiveKind::Logistics
        | ObjectiveKind::Production => SideFilter::All,
        ObjectiveKind::Farp { .. } => obj.owner.into(),
    }
}

fn remove_objective_overlay_marks(msgq: &mut MsgQ, t: &mut ObjectiveMarkup) {
    msgq.delete_mark(t.label);
    if let Some(id) = t.stats_label.take() {
        msgq.delete_mark(id);
    }
    if let Some(id) = t.stats_label_red.take() {
        msgq.delete_mark(id);
    }
    if let Some(id) = t.stats_label_blue.take() {
        msgq.delete_mark(id);
    }
}

/// Name + infobar stats; recreate after supply/front-line shapes so DCS draws text on top.
fn install_objective_overlay_marks(
    msgq: &mut MsgQ,
    t: &mut ObjectiveMarkup,
    obj: &Objective,
    cfg: &Cfg,
    persisted: &Persisted,
    export: &FowlMizExport,
) {
    let color_func = |a| text_color(obj.owner, a);
    let all_spec = overlay_side_filter(obj);
    let pos3 = Vector3::new(t.pos.x, 0., t.pos.y);
    let bg_color = match obj.owner {
        Side::Red => Color::red(0.8),
        Side::Blue => Color::blue(0.8),
        _ => Color::black(0.8),
    };
    t.label = MarkId::new();
    msgq.text_to_overlay(all_spec, t.label, TextSpec {
        pos: LuaVec3(Vector3::new(pos3.x + 1500., 1., pos3.z + 2500.)),
        color: Color::white(1.0),
        fill_color: bg_color,
        font_size: 11,
        read_only: true,
        text: t.name.clone().into(),
    });
    let stats_pos = LuaVec3(Vector3::new(pos3.x + 1500., 1., pos3.z + 2500.));
    let make_stats_spec = |text: CompactString| TextSpec {
        pos: stats_pos,
        color: color_func(1.0),
        fill_color: Color::black(0.0),
        font_size: 10,
        read_only: true,
        text: text.into(),
    };
    if all_spec == SideFilter::All {
        let id_r = MarkId::new();
        let id_b = MarkId::new();
        msgq.text_to_overlay(
            SideFilter::Red,
            id_r,
            make_stats_spec(objective_stats_text(
                cfg,
                persisted,
                obj,
                enemy_objective_view(obj.owner, Side::Red),
                export,
            )),
        );
        msgq.text_to_overlay(
            SideFilter::Blue,
            id_b,
            make_stats_spec(objective_stats_text(
                cfg,
                persisted,
                obj,
                enemy_objective_view(obj.owner, Side::Blue),
                export,
            )),
        );
        t.stats_label_red = Some(id_r);
        t.stats_label_blue = Some(id_b);
        t.stats_label = None;
    } else {
        let id = MarkId::new();
        msgq.text_to_overlay(
            all_spec,
            id,
            make_stats_spec(objective_stats_text(cfg, persisted, obj, false, export)),
        );
        t.stats_label = Some(id);
        t.stats_label_red = None;
        t.stats_label_blue = None;
    }
}

fn sync_overlay_cache_from_objective(
    t: &mut ObjectiveMarkup,
    obj: &Objective,
    cfg: &Cfg,
    persisted: &Persisted,
    export: &FowlMizExport,
) {
    t.health = obj.health;
    t.logi = obj.logi;
    t.supply = obj.supply;
    t.fuel = obj.fuel;
    let (amounts, kinds) = objective_warehouse_fuel_infobar_amounts(export, obj);
    t.fuel_amounts = amounts;
    t.fuel_kind_count = kinds;
    t.production = obj.production;
    t.display_production = production_pct_for_display(cfg, persisted, obj);
    t.production_hp_sum = obj.production_hp_sum;
    t.production_repair_need = obj.production_repair_need;
    t.production_repair = obj.production_repair;
    t.static_repair_need = obj.static_repair_need;
    t.static_repair = obj.static_repair;
    t.spawnable_logi_repaired = obj.spawnable_logi_repaired;
    t.points = obj.points;
}

fn refresh_objective_overlay_text(msgq: &mut MsgQ, t: &ObjectiveMarkup, obj: &Objective) {
    let pos = obj.zone.pos();
    let pos3 = Vector3::new(pos.x, 0., pos.y);
    msgq.set_overlay_markup_pos_start(
        t.label,
        LuaVec3(Vector3::new(pos3.x + 1500., 1., pos3.z + 2500.)),
    );
    let stats_pos = LuaVec3(Vector3::new(pos3.x + 1500., 1., pos3.z + 2500.));
    if let Some(id) = t.stats_label {
        msgq.set_overlay_markup_pos_start(id, stats_pos);
    }
    if let Some(id) = t.stats_label_red {
        msgq.set_overlay_markup_pos_start(id, stats_pos);
    }
    if let Some(id) = t.stats_label_blue {
        msgq.set_overlay_markup_pos_start(id, stats_pos);
    }
}

impl ObjectiveMarkup {
    pub(super) fn remove(self, msgq: &mut MsgQ) {
        let ObjectiveMarkup {
            owner_ring,
            capturable_ring,
            threatened_ring,
            supply_connections,
            production_feed_line,
            occupied_supply_line,
            label,
            stats_label,
            stats_label_red,
            stats_label_blue,
            ..
        } = self;
        msgq.delete_mark(owner_ring);
        msgq.delete_mark(threatened_ring);
        msgq.delete_mark(capturable_ring);
        msgq.delete_mark(label);
        if let Some(id) = stats_label {
            msgq.delete_mark(id);
        }
        if let Some(id) = stats_label_red {
            msgq.delete_mark(id);
        }
        if let Some(id) = stats_label_blue {
            msgq.delete_mark(id);
        }
        for (_, mark) in supply_connections {
            msgq.delete_underlay_mark(mark.shaft);
            msgq.delete_underlay_mark(mark.head);
        }
        if let Some(id) = production_feed_line {
            msgq.delete_underlay_mark(id);
        }
        if let Some(id) = occupied_supply_line {
            msgq.delete_underlay_mark(id);
        }
    }

    pub(super) fn update(
        &mut self,
        cfg: &Cfg,
        efficiency_cache: &mut FxHashMap<(ObjectiveId, ObjectiveId), u8>,
        persisted: &Persisted,
        msgq: &mut MsgQ,
        obj: &Objective,
        moved: &[ObjectiveId],
        export: &FowlMizExport,
    ) -> bool {
        let mut underlay_dirty = false;
        let old_production = self.production;
        if obj.owner != self.side {
            let color_func = |a| text_color(obj.owner, a);
            self.side = obj.owner;
            if let Some(id) = self.stats_label {
                msgq.set_overlay_markup_color(id, color_func(1.));
            }
            if let Some(id) = self.stats_label_red {
                msgq.set_overlay_markup_color(id, color_func(1.));
            }
            if let Some(id) = self.stats_label_blue {
                msgq.set_overlay_markup_color(id, color_func(1.));
            }
            msgq.set_markup_color(self.owner_ring, color_func(1.));
            
            if !self.supply_connections.is_empty() {
                underlay_dirty = true;
            }
            for (_, mark) in self.supply_connections.drain() {
                msgq.delete_underlay_mark(mark.shaft);
                msgq.delete_underlay_mark(mark.head);
            }
            if let Some(id) = self.production_feed_line.take() {
                msgq.delete_underlay_mark(id);
                underlay_dirty = true;
            }
            self.production_feed_hub = None;
        }
        if obj.threatened != self.threatened {
            self.threatened = obj.threatened;
            msgq.set_markup_color(
                self.threatened_ring,
                Color::yellow(if self.threatened { 0.75 } else { 0. }),
            );
            underlay_dirty = true;
            if matches!(obj.kind, ObjectiveKind::Logistics) {
                resync_logistics_supply_connections(
                    cfg,
                    efficiency_cache,
                    self,
                    msgq,
                    obj,
                    persisted,
                );
                sync_occupied_hub_supply_line(cfg, self, msgq, obj, persisted);
            }
            if matches!(obj.kind, ObjectiveKind::Production) {
                sync_production_feed_line(cfg, self, msgq, obj, persisted);
            }
            if matches!(obj.kind, ObjectiveKind::Logistics | ObjectiveKind::Production) {
                self.display_production = production_pct_for_display(cfg, persisted, obj);
                refresh_objective_stats_labels(cfg, msgq, self, obj, persisted, export);
            }
        }
        let (fuel_amounts, fuel_kind_count) = objective_warehouse_fuel_infobar_amounts(export, obj);
        if self.health != obj.health
            || self.logi != obj.logi
            || self.supply != obj.supply
            || self.fuel != obj.fuel
            || self.fuel_amounts != fuel_amounts
            || self.fuel_kind_count != fuel_kind_count
            || self.production != obj.production
            || self.production_hp_sum != obj.production_hp_sum
            || self.production_repair_need != obj.production_repair_need
            || self.production_repair != obj.production_repair
            || self.static_repair_need != obj.static_repair_need
            || self.static_repair != obj.static_repair
            || self.spawnable_logi_repaired != obj.spawnable_logi_repaired
            || self.points != obj.points
        {
            if self.logi != obj.logi {
                msgq.set_markup_color(
                    self.capturable_ring,
                    Color::white(if obj.captureable() { 0.75 } else { 0. }),
                );
            }
            self.health = obj.health;
            self.logi = obj.logi;
            self.supply = obj.supply;
            self.fuel = obj.fuel;
            self.fuel_amounts = fuel_amounts;
            self.fuel_kind_count = fuel_kind_count;
            self.production = obj.production;
            self.production_hp_sum = obj.production_hp_sum;
            self.production_repair_need = obj.production_repair_need;
            self.production_repair = obj.production_repair;
            self.static_repair_need = obj.static_repair_need;
            self.static_repair = obj.static_repair;
            self.spawnable_logi_repaired = obj.spawnable_logi_repaired;
            self.points = obj.points;
            if let Some(id) = self.stats_label {
                msgq.set_overlay_markup_text(
                    id,
                    objective_stats_text(cfg, persisted, obj, false, export).into(),
                );
            } else if let (Some(id_r), Some(id_b)) = (self.stats_label_red, self.stats_label_blue) {
                msgq.set_overlay_markup_text(
                    id_r,
                    objective_stats_text(cfg, persisted, obj, enemy_objective_view(obj.owner, Side::Red), export)
                        .into(),
                );
                msgq.set_overlay_markup_text(
                    id_b,
                    objective_stats_text(cfg, persisted, obj, enemy_objective_view(obj.owner, Side::Blue), export)
                        .into(),
                );
            }
            if matches!(obj.kind, ObjectiveKind::Logistics | ObjectiveKind::Production) {
                self.display_production = production_pct_for_display(cfg, persisted, obj);
            }
        }
        if let Zone::Circle { pos, .. } = obj.zone {
            if self.pos != pos {
                self.pos = pos;
                let v3 = LuaVec3(Vector3::new(pos.x, 0., pos.y));
                msgq.set_markup_pos_start(self.owner_ring, v3);
                msgq.set_markup_pos_start(self.capturable_ring, v3);
                msgq.set_markup_pos_start(self.threatened_ring, v3);
                msgq.set_overlay_markup_pos_start(self.label, LuaVec3(Vector3::new(pos.x + 1500., 1., pos.y + 2500.)));
                let stats_pos = LuaVec3(Vector3::new(pos.x + 1500., 1., pos.y + 2500.));
                if let Some(id) = self.stats_label {
                    msgq.set_overlay_markup_pos_start(id, stats_pos);
                }
                if let Some(id) = self.stats_label_red {
                    msgq.set_overlay_markup_pos_start(id, stats_pos);
                }
                if let Some(id) = self.stats_label_blue {
                    msgq.set_overlay_markup_pos_start(id, stats_pos);
                }
            }
        }
        let visible_feed = visible_production_feed_hub(cfg, persisted, obj);
        if old_production != obj.production || visible_feed != self.production_feed_hub {
            if matches!(obj.kind, ObjectiveKind::Production) {
                underlay_dirty = true;
            }
            sync_production_feed_line(cfg, self, msgq, obj, persisted);
        }
        let mut supply_resynced = false;
        for oid in moved {
            if !supply_resynced
                && matches!(obj.kind, ObjectiveKind::Logistics)
                && obj.warehouse.destination.contains(oid)
            {
                underlay_dirty = true;
                resync_logistics_supply_connections(
                    cfg,
                    efficiency_cache,
                    self,
                    msgq,
                    obj,
                    persisted,
                );
                supply_resynced = true;
            }
            if matches!(obj.kind, ObjectiveKind::Production)
                && opr_feed_hub(persisted, obj) == Some(*oid)
            {
                underlay_dirty = true;
                sync_production_feed_line(cfg, self, msgq, obj, persisted);
            }
            if matches!(obj.kind, ObjectiveKind::Logistics) {
                if persisted.objectives.get(oid).is_some_and(|opr| {
                    opr_feed_hub(persisted, opr) == Some(obj.id)
                }) {
                    self.display_production = production_pct_for_display(cfg, persisted, obj);
                    refresh_objective_stats_labels(cfg, msgq, self, obj, persisted, export);
                }
            }
        }
        if let Zone::Circle { pos, .. } = obj.zone {
            if self.pos != pos {
                if matches!(obj.kind, ObjectiveKind::Production) {
                    underlay_dirty = true;
                }
                sync_production_feed_line(cfg, self, msgq, obj, persisted);
            }
        }
        if matches!(obj.kind, ObjectiveKind::Logistics) {
            let want_anchor = visible_occupied_supply_anchor(cfg, persisted, obj);
            if want_anchor != self.occupied_supply_anchor || self.pos != obj.zone.pos() {
                underlay_dirty = true;
                sync_occupied_hub_supply_line(cfg, self, msgq, obj, persisted);
            }
        }
        if virtual_resupply_threatened_without_deliveries(cfg)
            && matches!(obj.kind, ObjectiveKind::Logistics | ObjectiveKind::Production)
        {
            let disp = production_pct_for_display(cfg, persisted, obj);
            if disp != self.display_production {
                self.display_production = disp;
                refresh_objective_stats_labels(cfg, msgq, self, obj, persisted, export);
            }
        }
        refresh_objective_overlay_text(msgq, self, obj);
        underlay_dirty
    }

    pub(super) fn raise_overlay(
        &mut self,
        msgq: &mut MsgQ,
        obj: &Objective,
        cfg: &Cfg,
        persisted: &Persisted,
        export: &FowlMizExport,
    ) {
        remove_objective_overlay_marks(msgq, self);
        install_objective_overlay_marks(msgq, self, obj, cfg, persisted, export);
        sync_overlay_cache_from_objective(self, obj, cfg, persisted, export);
    }

    pub(super) fn new(
        cfg: &Cfg,
        efficiency_cache: &mut FxHashMap<(ObjectiveId, ObjectiveId), u8>,
        msgq: &mut MsgQ,
        obj: &Objective,
        persisted: &Persisted,
        display_aliases: &FxHashMap<String, std::string::String>,
        export: &FowlMizExport,
    ) -> Self {
        let color_func = |a| text_color(obj.owner, a);
        let all_spec = match obj.kind {
            ObjectiveKind::Airbase
            | ObjectiveKind::Fob
            | ObjectiveKind::Logistics
            | ObjectiveKind::Production => SideFilter::All,
            ObjectiveKind::Farp { .. } => obj.owner.into(),
        };
        let mut t = ObjectiveMarkup::default();
        t.side = obj.owner;
        t.threatened = obj.threatened;
        t.health = obj.health;
        t.logi = obj.logi;
        t.supply = obj.supply;
        t.fuel = obj.fuel;
        let (amounts, kinds) = objective_warehouse_fuel_infobar_amounts(export, obj);
        t.fuel_amounts = amounts;
        t.fuel_kind_count = kinds;
        t.production = obj.production;
        t.display_production = production_pct_for_display(cfg, persisted, obj);
        t.production_hp_sum = obj.production_hp_sum;
        t.production_repair_need = obj.production_repair_need;
        t.production_repair = obj.production_repair;
        t.static_repair_need = obj.static_repair_need;
        t.static_repair = obj.static_repair;
        t.spawnable_logi_repaired = obj.spawnable_logi_repaired;
        let label = resolve_objective_f10_map_label(display_aliases, obj);
        t.name = format_compact!(" {} ", label).into();
        t.pos = obj.zone.pos();
        let pos3 = Vector3::new(t.pos.x, 0., t.pos.y);

        macro_rules! threat_circle {
            ($radius:expr) => {
                msgq.circle_to_all(all_spec, t.threatened_ring, CircleSpec {
                    center: LuaVec3(pos3),
                    radius: (cfg.logistics_exclusion as f64).max($radius * 1.1),
                    color: Color::yellow(if obj.threatened { 0.75 } else { 0. }),
                    fill_color: Color::black(0.),
                    line_type: LineType::Solid,
                    read_only: true,
                }, None)
            };
        }

        match obj.zone {
            Zone::Circle { radius, .. } => {
                msgq.circle_to_all(all_spec, t.owner_ring, CircleSpec {
                    center: LuaVec3(pos3),
                    radius,
                    color: color_func(1.),
                    fill_color: Color::black(0.),
                    line_type: LineType::Dashed,
                    read_only: true,
                }, None);
                threat_circle!(radius);
            }
            Zone::Quad { points, pos } => {
                msgq.quad_to_all(all_spec, t.owner_ring, QuadSpec {
                    p0: LuaVec3(Vector3::new(points.p0.x, 0., points.p0.y)),
                    p1: LuaVec3(Vector3::new(points.p1.x, 0., points.p1.y)),
                    p2: LuaVec3(Vector3::new(points.p2.x, 0., points.p2.y)),
                    p3: LuaVec3(Vector3::new(points.p3.x, 0., points.p3.y)),
                    color: color_func(1.),
                    fill_color: Color::black(0.),
                    line_type: LineType::Dashed,
                    read_only: true,
                }, None);
                if !points.contains_circle(pos, cfg.logistics_exclusion as f64) {
                    threat_circle!(0.);
                } else {
                    let points = points.scale(1.1);
                    msgq.quad_to_all(all_spec, t.threatened_ring, QuadSpec {
                        p0: LuaVec3(Vector3::new(points.p0.x, 0., points.p0.y)),
                        p1: LuaVec3(Vector3::new(points.p1.x, 0., points.p1.y)),
                        p2: LuaVec3(Vector3::new(points.p2.x, 0., points.p2.y)),
                        p3: LuaVec3(Vector3::new(points.p3.x, 0., points.p3.y)),
                        color: Color::yellow(if obj.threatened { 0.75 } else { 0. }),
                        fill_color: Color::black(0.),
                        line_type: LineType::Solid,
                        read_only: true,
                    }, None);
                }
            }
        }

        match obj.zone {
            Zone::Circle { pos: _, radius } => {
                msgq.circle_to_all(all_spec, t.capturable_ring, CircleSpec {
                    center: LuaVec3(pos3),
                    radius: radius as f64 * 0.9,
                    color: Color::white(if obj.captureable() { 0.75 } else { 0. }),
                    fill_color: Color::black(0.),
                    line_type: LineType::Solid,
                    read_only: true,
                }, None);
            }
            Zone::Quad { pos: _, points } => {
                let points = points.scale(0.9);
                msgq.quad_to_all(all_spec, t.capturable_ring, QuadSpec {
                    p0: LuaVec3(Vector3::new(points.p0.x, 0., points.p0.y)),
                    p1: LuaVec3(Vector3::new(points.p1.x, 0., points.p1.y)),
                    p2: LuaVec3(Vector3::new(points.p2.x, 0., points.p2.y)),
                    p3: LuaVec3(Vector3::new(points.p3.x, 0., points.p3.y)),
                    color: Color::white(if obj.captureable() { 0.75 } else { 0. }),
                    fill_color: Color::black(0.),
                    line_type: LineType::Solid,
                    read_only: true,
                }, None);
            }
        }

        if matches!(obj.kind, ObjectiveKind::Production) {
            sync_production_feed_line(cfg, &mut t, msgq, obj, persisted);
        }
        if let ObjectiveKind::Logistics = obj.kind {
            for oid in &obj.warehouse.destination {
                let dobj = &persisted.objectives[oid];
                let to = if dobj.is_farp() {
                    dobj.owner.into()
                } else {
                    all_spec
                };
                let mut mark = SupplyConnectionMark {
                    shaft: MarkId::new(),
                    head: MarkId::new(),
                };
                sync_supply_connection_checked(
                    cfg,
                    efficiency_cache,
                    msgq,
                    &mut mark,
                    to,
                    obj,
                    dobj,
                );
                t.supply_connections.insert(*oid, mark);
            }
            sync_occupied_hub_supply_line(cfg, &mut t, msgq, obj, persisted);
        }

        install_objective_overlay_marks(msgq, &mut t, obj, cfg, persisted, export);

        t
    }
}