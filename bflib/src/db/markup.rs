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
    aliases::resolve_objective_display_name,
    objective::{Objective, Zone},
    persisted::Persisted,
};
use fxhash::FxHashMap;
use crate::msgq::MsgQ;
use bfprotocols::{
    cfg::Cfg,
    db::objective::{ObjectiveId, ObjectiveKind},
};
use compact_str::{CompactString, format_compact};
use dcso3::{
    Color, LuaVec3, Vector2, Vector3,
    coalition::Side,
    trigger::{
        ArrowSpec, CircleSpec, LineSpec, LineType, MarkId, QuadSpec, SideFilter, TextSpec,
    },
};

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
    production: u8,
    production_hp_sum: u32,
    production_repair_need: u16,
    production_repair: u16,
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
    supply_connections: FxHashMap<ObjectiveId, MarkId>,
    /// OPR → nearest OLO (`lineToAll`, no arrowhead).
    production_feed_hub: Option<ObjectiveId>,
    production_feed_line: Option<MarkId>,
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

fn objective_stats_text(obj: &Objective, limited: bool) -> CompactString {
    let get_idx = |val: u8| -> usize { (val as usize * 12 / 100).min(12) };
    match (&obj.kind, limited) {
        (ObjectiveKind::Production, true) => {
            format_compact!("\n\n{}", stat_infobar_row(BAR_LOOKUP[get_idx(obj.production)], obj.production, "Production"))
        }
        (ObjectiveKind::Production, false) => {
            format_compact!(
                "\n\n{}\n{}\n{}",
                stat_infobar_row(BAR_LOOKUP[get_idx(obj.production)], obj.production, "Production"),
                stat_plain_row("Points", &format_compact!("{}", obj.points)),
                stat_plain_repair_row(obj.production_repair, obj.production_repair_slots_needed()),
            )
        }
        (ObjectiveKind::Logistics, true) => format_compact!(
            "\n\n{}\n{}\n{}",
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.production)], obj.production, "Production"),
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.health)], obj.health, "Health"),
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.logi)], obj.logi, "Logi"),
        ),
        (ObjectiveKind::Logistics, false) => format_compact!(
            "\n\n{}\n{}\n{}\n{}\n{}\n{}",
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.production)], obj.production, "Production"),
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.health)], obj.health, "Health"),
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.logi)], obj.logi, "Logi"),
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.supply)], obj.supply, "Supply"),
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.fuel)], obj.fuel, "Fuel"),
            stat_plain_row("Points", &format_compact!("{}", obj.points)),
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
            stat_infobar_row(BAR_LOOKUP[get_idx(obj.fuel)], obj.fuel, "Fuel"),
            stat_plain_row("Points", &format_compact!("{}", obj.points)),
        ),
    }
}

fn objective_map_kind_label(kind: &ObjectiveKind) -> &'static str {
    match kind {
        ObjectiveKind::Logistics => "⌂ HUB",
        ObjectiveKind::Production => "⌂",
        _ => kind.name(),
    }
}

/// Lighter than hub `supply_connections` arrows; light green, same alpha as before.
fn production_feed_line_color() -> Color {
    Color::light_green(0.25)
}

fn sync_production_feed_line(
    t: &mut ObjectiveMarkup,
    msgq: &mut MsgQ,
    obj: &Objective,
    persisted: &Persisted,
) {
    if !matches!(obj.kind, ObjectiveKind::Production) {
        return;
    }
    let pos = obj.zone.pos();
    let collapsed = LuaVec3(Vector3::new(pos.x, 0., pos.y));
    if t.production_feed_line.is_none() {
        let id = MarkId::new();
        msgq.line_to(
            SideFilter::All,
            id,
            LineSpec {
                start: collapsed,
                end: collapsed,
                color: Color::light_green(0.),
                line_type: LineType::Solid,
                read_only: true,
            },
            None,
        );
        t.production_feed_line = Some(id);
    }
    let Some(id) = t.production_feed_line else {
        return;
    };
    let want_hub = if obj.production > 0 {
        obj.feed_hub
    } else {
        None
    };
    t.production_feed_hub = want_hub;
    match want_hub {
        None => {
            msgq.set_markup_pos_start(id, collapsed);
            msgq.set_markup_pos_end(id, collapsed);
            msgq.set_markup_color(id, Color::light_green(0.));
        }
        Some(hid) => {
            let hub = match persisted.objectives.get(&hid) {
                Some(h) => h,
                None => return,
            };
            let (spos, dpos) = arrow_coords(obj, hub);
            let start = LuaVec3(Vector3::new(spos.x, 0., spos.y));
            let end = LuaVec3(Vector3::new(dpos.x, 0., dpos.y));
            let color = production_feed_line_color();
            msgq.set_markup_pos_start(id, start);
            msgq.set_markup_pos_end(id, end);
            msgq.set_markup_color(id, color);
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

impl ObjectiveMarkup {
    pub(super) fn remove(self, msgq: &mut MsgQ) {
        let ObjectiveMarkup {
            owner_ring,
            capturable_ring,
            threatened_ring,
            supply_connections,
            production_feed_line,
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
        for (_, id) in supply_connections {
            msgq.delete_mark(id)
        }
        if let Some(id) = production_feed_line {
            msgq.delete_mark(id);
        }
    }

    pub(super) fn update(
        &mut self,
        persisted: &Persisted,
        msgq: &mut MsgQ,
        obj: &Objective,
        moved: &[ObjectiveId],
    ) {
        let old_production = self.production;
        if obj.owner != self.side {
            let color_func = |a| text_color(obj.owner, a);
            self.side = obj.owner;
            if let Some(id) = self.stats_label {
                msgq.set_markup_color(id, color_func(1.));
            }
            if let Some(id) = self.stats_label_red {
                msgq.set_markup_color(id, color_func(1.));
            }
            if let Some(id) = self.stats_label_blue {
                msgq.set_markup_color(id, color_func(1.));
            }
            msgq.set_markup_color(self.owner_ring, color_func(1.));
            
            for (_, id) in self.supply_connections.drain() {
                msgq.delete_mark(id);
            }
            if let Some(id) = self.production_feed_line.take() {
                msgq.delete_mark(id);
            }
            self.production_feed_hub = None;
        }
        if obj.threatened != self.threatened {
            self.threatened = obj.threatened;
            msgq.set_markup_color(
                self.threatened_ring,
                Color::yellow(if self.threatened { 0.75 } else { 0. }),
            );
        }
        if self.health != obj.health
            || self.logi != obj.logi
            || self.supply != obj.supply
            || self.fuel != obj.fuel
            || self.production != obj.production
            || self.production_hp_sum != obj.production_hp_sum
            || self.production_repair_need != obj.production_repair_need
            || self.production_repair != obj.production_repair
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
            self.production = obj.production;
            self.production_hp_sum = obj.production_hp_sum;
            self.production_repair_need = obj.production_repair_need;
            self.production_repair = obj.production_repair;
            self.points = obj.points;
            if let Some(id) = self.stats_label {
                msgq.set_markup_text(id, objective_stats_text(obj, false).into());
            } else if let (Some(id_r), Some(id_b)) = (self.stats_label_red, self.stats_label_blue) {
                msgq.set_markup_text(
                    id_r,
                    objective_stats_text(obj, enemy_objective_view(obj.owner, Side::Red)).into(),
                );
                msgq.set_markup_text(
                    id_b,
                    objective_stats_text(obj, enemy_objective_view(obj.owner, Side::Blue)).into(),
                );
            }
        }
        if let Zone::Circle { pos, .. } = obj.zone {
            if self.pos != pos {
                self.pos = pos;
                let v3 = LuaVec3(Vector3::new(pos.x, 0., pos.y));
                msgq.set_markup_pos_start(self.owner_ring, v3);
                msgq.set_markup_pos_start(self.capturable_ring, v3);
                msgq.set_markup_pos_start(self.threatened_ring, v3);
                msgq.set_markup_pos_start(self.label, LuaVec3(Vector3::new(pos.x + 1450., 1., pos.y + 1750.)));
                let stats_pos = LuaVec3(Vector3::new(pos.x + 1500., 1., pos.y + 1250.));
                if let Some(id) = self.stats_label {
                    msgq.set_markup_pos_start(id, stats_pos);
                }
                if let Some(id) = self.stats_label_red {
                    msgq.set_markup_pos_start(id, stats_pos);
                }
                if let Some(id) = self.stats_label_blue {
                    msgq.set_markup_pos_start(id, stats_pos);
                }
            }
        }
        if old_production != obj.production || self.production_feed_hub != obj.feed_hub {
            sync_production_feed_line(self, msgq, obj, persisted);
        }
        for oid in moved {
            if obj.warehouse.destination.contains(oid) {
                if let Some(id) = self.supply_connections.get(oid) {
                    let dst = &persisted.objectives[oid];
                    let (spos, dpos) = arrow_coords(obj, dst);
                    msgq.set_markup_pos_start(*id, LuaVec3(Vector3::new(dpos.x, 0., dpos.y)));
                    msgq.set_markup_pos_end(*id, LuaVec3(Vector3::new(spos.x, 0., spos.y)));
                }
            }
            if obj.feed_hub == Some(*oid) {
                sync_production_feed_line(self, msgq, obj, persisted);
            }
        }
        if let Zone::Circle { pos, .. } = obj.zone {
            if self.pos != pos {
                sync_production_feed_line(self, msgq, obj, persisted);
            }
        }
    }

    pub(super) fn new(
        cfg: &Cfg,
        msgq: &mut MsgQ,
        obj: &Objective,
        persisted: &Persisted,
        display_aliases: &FxHashMap<String, std::string::String>,
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
        t.production = obj.production;
        t.production_hp_sum = obj.production_hp_sum;
        t.production_repair_need = obj.production_repair_need;
        t.production_repair = obj.production_repair;
        let display = resolve_objective_display_name(display_aliases, obj);
        t.name = if matches!(obj.kind, ObjectiveKind::Farp { mobile: true, .. }) {
            format_compact!(" {} ", display).into()
        } else {
            format_compact!(" {} {} ", display, objective_map_kind_label(&obj.kind)).into()
        };
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
            sync_production_feed_line(&mut t, msgq, obj, persisted);
        }
        if let ObjectiveKind::Logistics = obj.kind {
            for oid in &obj.warehouse.destination {
                let id = MarkId::new();
                let dobj = &persisted.objectives[oid];
                let (spos, dpos) = arrow_coords(obj, dobj);
                msgq.arrow_to(if dobj.is_farp() { dobj.owner.into() } else { all_spec }, id, ArrowSpec {
                    start: LuaVec3(Vector3::new(dpos.x, 0., dpos.y)),
                    end: LuaVec3(Vector3::new(spos.x, 0., spos.y)),
                    color: Color::gray(0.5),
                    fill_color: Color::gray(0.5),
                    line_type: LineType::NoLine,
                    read_only: true,
                }, None);
                t.supply_connections.insert(*oid, id);
            }
        }

        let bg_color = match obj.owner {
            Side::Red => Color::red(0.8),
            Side::Blue => Color::blue(0.8),
            _ => Color::black(0.8),
        };

        msgq.text_to_all(all_spec, t.label, TextSpec {
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
            msgq.text_to_all(
                SideFilter::Red,
                id_r,
                make_stats_spec(objective_stats_text(
                    obj,
                    enemy_objective_view(obj.owner, Side::Red),
                )),
            );
            msgq.text_to_all(
                SideFilter::Blue,
                id_b,
                make_stats_spec(objective_stats_text(
                    obj,
                    enemy_objective_view(obj.owner, Side::Blue),
                )),
            );
            t.stats_label_red = Some(id_r);
            t.stats_label_blue = Some(id_b);
            t.stats_label = None;
        } else {
            let id = MarkId::new();
            msgq.text_to_all(all_spec, id, make_stats_spec(objective_stats_text(obj, false)));
            t.stats_label = Some(id);
            t.stats_label_red = None;
            t.stats_label_blue = None;
        }

        t
    }
}