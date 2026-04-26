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
    objective::{Objective, Zone},
    persisted::Persisted,
};
use crate::msgq::MsgQ;
use bfprotocols::{
    cfg::Cfg,
    db::objective::{ObjectiveId, ObjectiveKind},
};
use compact_str::{CompactString, format_compact};
use dcso3::{
    Color, LuaVec3, Vector2, Vector3,
    coalition::Side,
    trigger::{ArrowSpec, CircleSpec, LineType, MarkId, QuadSpec, SideFilter, TextSpec},
};
use fxhash::FxHashMap;

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
}

fn text_color(side: Side, a: f32) -> Color {
    match side {
        Side::Red => Color::red(a),
        Side::Blue => Color::blue(a),
        Side::Neutral => Color::white(a),
    }
}

/// Skrýt Supply a Fuel nepřátelské základně (červený tým nevidí u modrých, modrý u červených).
fn hide_supply_fuel_for_viewer(obj_owner: Side, viewer: Side) -> bool {
    matches!(
        (obj_owner, viewer),
        (Side::Red, Side::Blue) | (Side::Blue, Side::Red)
    )
}

fn objective_stats_text(obj: &Objective, hide_supply_fuel: bool) -> CompactString {
    let get_idx = |val: u8| -> usize { (val as usize * 12 / 100).min(12) };
    if hide_supply_fuel {
        format_compact!(
            "\n\n{} {:>3} Health\n{} {:>3} Logi\n\nPoints: {}",
            BAR_LOOKUP[get_idx(obj.health)],
            obj.health,
            BAR_LOOKUP[get_idx(obj.logi)],
            obj.logi,
            obj.points
        )
    } else {
        format_compact!(
            "\n\n{} {:>3} Health\n{} {:>3} Logi\n{} {:>3} Supply\n{} {:>3} Fuel\n\nPoints: {}",
            BAR_LOOKUP[get_idx(obj.health)],
            obj.health,
            BAR_LOOKUP[get_idx(obj.logi)],
            obj.logi,
            BAR_LOOKUP[get_idx(obj.supply)],
            obj.supply,
            BAR_LOOKUP[get_idx(obj.fuel)],
            obj.fuel,
            obj.points
        )
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
    }

    pub(super) fn update(
        &mut self,
        persisted: &Persisted,
        msgq: &mut MsgQ,
        obj: &Objective,
        moved: &[ObjectiveId],
    ) {
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
            self.points = obj.points;
            if let Some(id) = self.stats_label {
                msgq.set_markup_text(id, objective_stats_text(obj, false).into());
            } else if let (Some(id_r), Some(id_b)) = (self.stats_label_red, self.stats_label_blue) {
                msgq.set_markup_text(
                    id_r,
                    objective_stats_text(obj, hide_supply_fuel_for_viewer(obj.owner, Side::Red)).into(),
                );
                msgq.set_markup_text(
                    id_b,
                    objective_stats_text(obj, hide_supply_fuel_for_viewer(obj.owner, Side::Blue)).into(),
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
        for oid in moved {
            if obj.warehouse.destination.contains(oid) {
                if let Some(id) = self.supply_connections.get(oid) {
                    let dst = &persisted.objectives[oid];
                    let (spos, dpos) = arrow_coords(obj, dst);
                    msgq.set_markup_pos_start(*id, LuaVec3(Vector3::new(dpos.x, 0., dpos.y)));
                    msgq.set_markup_pos_end(*id, LuaVec3(Vector3::new(spos.x, 0., spos.y)));
                }
            }
        }
    }

    pub(super) fn new(cfg: &Cfg, msgq: &mut MsgQ, obj: &Objective, persisted: &Persisted) -> Self {
        let color_func = |a| text_color(obj.owner, a);
        let all_spec = match obj.kind {
            ObjectiveKind::Airbase | ObjectiveKind::Fob | ObjectiveKind::Logistics => SideFilter::All,
            ObjectiveKind::Farp { .. } => obj.owner.into(),
        };
        let mut t = ObjectiveMarkup::default();
        t.side = obj.owner;
        t.threatened = obj.threatened;
        t.health = obj.health;
        t.logi = obj.logi;
        t.supply = obj.supply;
        t.fuel = obj.fuel;
        t.name = format_compact!(" {} {} ", obj.name, obj.kind.name()).into();
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
                    hide_supply_fuel_for_viewer(obj.owner, Side::Red),
                )),
            );
            msgq.text_to_all(
                SideFilter::Blue,
                id_b,
                make_stats_spec(objective_stats_text(
                    obj,
                    hide_supply_fuel_for_viewer(obj.owner, Side::Blue),
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
        t
    }
}