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
    ai_air::{self, AiAirState},
    csar::is_fowl_csar_unit_unit,
    ephemeral::SlotInfo,
    objective::ObjGroupClass,
    player::SlotAuth,
    Db,
    SetS,
};
use crate::{
    group, group_by_name, group_health, group_mut, objective, objective_mut,
    spawnctx::{Despawn, SpawnCtx, SpawnLoc},
    unit, unit_by_name, unit_mut, Connected,
};
use anyhow::{anyhow, bail, Context, Result};
use bfprotocols::{
    cfg::{Action, ActionKind, Crate, Deployable, Troop, UnitTag, UnitTags, Vehicle},
    db::objective::{ObjectiveId, ObjectiveKind},
    stats::{self, EnId},
};
use bfprotocols::{
    db::group::{GroupId, UnitId},
    stats::Stat,
};
use chrono::prelude::*;
use compact_str::{format_compact, CompactString};
use dcso3::{
    azumith3d, centroid2d, centroid3d, change_heading,
    coalition::{Side, Static},
    coord::Coord,
    env::miz,
    env::miz::{Group as MizGroup, GroupKind, MizIndex},
    group::{Group as DcsGroup, GroupCategory},
    land::{Land, SurfaceType},
    net::{SlotId, Ucid},
    object::{DcsObject, DcsOid, Object},
    rotate2d_gen,
    static_object::{ClassStatic, StaticObject},
    trigger::MarkId,
    unit::{ClassUnit, Unit},
    LuaVec2, LuaVec3, MizLua, Position3, String, Vector2, Vector3,
};
use enumflags2::BitFlags;
use fxhash::{FxHashMap, FxHashSet};
use log::{debug, error, info, warn};
use serde_derive::{Deserialize, Serialize};
use smallvec::{smallvec, SmallVec};
use std::{cmp::max, collections::VecDeque};

#[derive(Debug, Clone)]
pub enum BirthRes {
    None,
    OccupiedSlot(SlotId),
    DynamicSlotDenied(Ucid, SlotAuth),
}

fn default_cost_fraction() -> f32 {
    1.
}

/// MOOSE/Hoggit static-on-ship offsets (`offsets = { x, y, angle }` in radians).
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct ShipCrateOffsets {
    pub x: f64,
    pub y: f64,
    pub angle: f64,
}

fn vehicle_type_for_dynamic_slot(unit: &Unit) -> Result<Vehicle> {
    let desc = unit.get_desc().context("dynamic slot unit getDesc")?;
    match desc.raw_get::<_, String>("typeName") {
        Ok(tn) if !tn.trim().is_empty() => Ok(Vehicle::from(tn)),
        _ => Ok(Vehicle::from(unit.get_type_name()?)),
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum DeployKind {
    #[serde(rename = "Objective")]
    ObjectiveDeprecated,
    #[serde(rename = "ObjectiveV2")]
    Objective {
        origin: ObjectiveId,
    },
    Deployed {
        player: Ucid,
        #[serde(default)]
        moved_by: Option<(Ucid, u32)>,
        spec: Deployable,
        #[serde(default = "default_cost_fraction")]
        cost_fraction: f32,
        #[serde(default)]
        origin: Option<ObjectiveId>,
    },
    Troop {
        player: Ucid,
        origin: Option<ObjectiveId>,
        #[serde(default)]
        moved_by: Option<(Ucid, u32)>,
        spec: Troop,
        #[serde(default = "default_cost_fraction")]
        cost_fraction: f32,
        /// Naval hub for deck link (carrier unload).
        #[serde(default)]
        ship_hub: Option<ObjectiveId>,
        /// Ship-relative offsets for carrier-deck troops.
        #[serde(default)]
        ship_offsets: Option<ShipCrateOffsets>,
    },
    Crate {
        origin: ObjectiveId,
        player: Ucid,
        spec: Crate,
        /// Naval hub used for deck link (may differ from `origin` on unload).
        #[serde(default)]
        ship_hub: Option<ObjectiveId>,
        /// Ship-relative offsets for carrier-deck crates (Hoggit `addStaticObject` link).
        #[serde(default)]
        ship_offsets: Option<ShipCrateOffsets>,
        /// Player who F8/sling-loaded this crate (List Cargo / revive).
        #[serde(default)]
        ed_carrier: Option<Ucid>,
    },
    Action {
        #[serde(skip)]
        marks: FxHashSet<MarkId>,
        loc: SpawnLoc,
        player: Option<Ucid>,
        name: String,
        spec: Action,
        time: DateTime<Utc>,
        destination: Option<Vector2>,
        rtb: Option<Vector2>,
        #[serde(default)]
        origin: Option<ObjectiveId>,
        #[serde(skip)]
        ammo: i32,
        #[serde(default)]
        ai_air: AiAirState,
        /// Coalition may command after lock expiry; set at round start, never re-locked.
        #[serde(default)]
        owner_lock_released: bool,
    },
}

fn default_factory_hp() -> u8 {
    100
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnedUnit {
    pub name: String,
    pub id: UnitId,
    pub group: GroupId,
    pub side: Side,
    pub typ: Vehicle,
    pub tags: UnitTags,
    pub template_name: String,
    pub spawn_pos: Vector2,
    pub spawn_heading: f64,
    pub spawn_position: Position3,
    pub pos: Vector2,
    pub heading: f64,
    pub position: Position3,
    pub dead: bool,
    /// OPR factory static HP; event-driven updates, persisted across saves.
    #[serde(default = "default_factory_hp")]
    pub hp_percent: u8,
    /// DCS `getLife0` for ME objective statics (repair queue: lowest first).
    #[serde(default)]
    pub static_max_life: i64,
    #[serde(skip)]
    pub moved: Option<DateTime<Utc>>,
    #[serde(skip)]
    pub airborne_velocity: Option<Vector3>,
    /// Last DCS `getFuel()` fraction (0..1) for ai air persist resume.
    #[serde(default)]
    pub fuel_fraction: Option<f32>,
}

impl Default for SpawnedUnit {
    fn default() -> Self {
        Self {
            name: String::from(""),
            id: UnitId::default(),
            group: GroupId::default(),
            side: Side::Neutral,
            typ: Vehicle(String::from("")),
            tags: UnitTags::default(),
            template_name: String::from(""),
            spawn_pos: Vector2::default(),
            spawn_heading: 0.,
            spawn_position: Position3::default(),
            pos: Vector2::default(),
            heading: 0.,
            position: Position3::default(),
            dead: false,
            hp_percent: default_factory_hp(),
            static_max_life: 0,
            moved: None,
            airborne_velocity: None,
            fuel_fraction: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnedGroup {
    pub id: GroupId,
    pub name: String,
    pub template_name: String,
    pub side: Side,
    /// ME coalition of the `.miz` template (`get_template` lookup). Not ownership.
    #[serde(default)]
    pub template_side: Option<Side>,
    pub kind: Option<GroupCategory>,
    pub class: ObjGroupClass,
    pub origin: DeployKind,
    pub units: SetS<UnitId>,
    pub tags: UnitTags,
}

impl Db {
    #[allow(dead_code)]
    pub fn groups(&self) -> impl Iterator<Item = (&GroupId, &SpawnedGroup)> {
        self.persisted.groups.into_iter()
    }

    pub fn group(&self, id: &GroupId) -> Result<&SpawnedGroup> {
        group!(self, id)
    }

    pub fn group_center(&self, id: &GroupId) -> Result<Vector2> {
        let group = group!(self, id)?;
        Ok(centroid2d(
            group
                .units
                .into_iter()
                .filter_map(|uid| self.persisted.units.get(uid))
                .filter_map(|unit| if unit.dead { None } else { Some(unit.pos) }),
        ))
    }

    #[allow(dead_code)]
    pub fn group_center3(&self, id: &GroupId) -> Result<Vector3> {
        let group = group!(self, id)?;
        Ok(centroid3d(
            group
                .units
                .into_iter()
                .filter_map(|uid| self.persisted.units.get(uid))
                .filter_map(
                    |unit| {
                        if unit.dead {
                            None
                        } else {
                            Some(unit.position.p.0)
                        }
                    },
                ),
        ))
    }

    #[allow(dead_code)]
    pub fn group_by_name(&self, name: &str) -> Result<&SpawnedGroup> {
        group_by_name!(self, name)
    }

    pub fn unit(&self, id: &UnitId) -> Result<&SpawnedUnit> {
        unit!(self, id)
    }

    #[allow(dead_code)]
    pub fn unit_by_name(&self, name: &str) -> Result<&SpawnedUnit> {
        unit_by_name!(self, name)
    }

    pub fn first_living_unit(&self, gid: &GroupId) -> Result<&DcsOid<ClassUnit>> {
        group!(self, gid)?
            .units
            .into_iter()
            .find_map(|uid| self.ephemeral.get_object_id_by_uid(uid))
            .ok_or_else(|| anyhow!("all units are dead"))
    }

    pub fn instanced_units(
        &self,
    ) -> impl Iterator<Item = (&SpawnedUnit, &DcsOid<ClassUnit>)> {
        self.persisted.units.into_iter().filter_map(|(uid, sp)| {
            self.ephemeral.object_id_by_uid.get(uid).map(|id| (sp, id))
        })
    }

    /// Living ME OPR factories / objective statics (StaticObject, not DCS Unit).
    pub fn living_objective_statics(&self) -> impl Iterator<Item = &SpawnedUnit> {
        self.persisted.units.into_iter().filter_map(|(_, sp)| {
            if sp.dead {
                return None;
            }
            let group = self.persisted.groups.get(&sp.group)?;
            group.class.is_me_objective_static().then_some(sp)
        })
    }

    fn me_objective_static_role_tag(class: ObjGroupClass) -> BitFlags<UnitTag> {
        match class {
            ObjGroupClass::Production => BitFlags::from(UnitTag::Factory),
            ObjGroupClass::ObjectiveStatic => BitFlags::from(UnitTag::Structure),
            _ => BitFlags::empty(),
        }
    }

    fn ensure_me_objective_static_role_tags(db: &mut Db, uid: UnitId) {
        let Some((gid, role)) = db.persisted.units.get(&uid).and_then(|u| {
            db.persisted
                .groups
                .get(&u.group)
                .map(|g| (u.group, Self::me_objective_static_role_tag(g.class)))
        }) else {
            return;
        };
        if role.is_empty() {
            return;
        }
        let mut dirty = false;
        if let Some(unit) = db.persisted.units.get_mut_cow(&uid) {
            if !unit.tags.0.contains(role) {
                unit.tags.0 |= role;
                dirty = true;
            }
        }
        if let Some(g) = db.persisted.groups.get_mut_cow(&gid) {
            if !g.tags.0.contains(role) {
                g.tags.0 |= role;
                dirty = true;
            }
        }
        if dirty {
            db.ephemeral.dirty();
        }
    }

    pub fn deployed(&self) -> impl Iterator<Item = &SpawnedGroup> {
        self.persisted
            .deployed
            .into_iter()
            .chain(self.persisted.troops.into_iter())
            .filter_map(|gid| self.persisted.groups.get(gid))
    }

    pub fn actions(&self) -> impl Iterator<Item = &SpawnedGroup> {
        self.persisted
            .actions
            .into_iter()
            .chain(self.persisted.troops.into_iter())
            .filter_map(|gid| self.persisted.groups.get(gid))
    }

    fn live_group_center_ground2<'lua>(
        lua: MizLua<'lua>,
        group: &SpawnedGroup,
    ) -> Option<Vector2> {
        let g = DcsGroup::get_by_name(lua, group.name.as_str()).ok()?;
        if !g.is_exist().ok()? {
            return None;
        }
        let n = g.get_size().ok()?;
        if n < 1 {
            return None;
        }
        let mut pts = SmallVec::<[Vector2; 16]>::new();
        for i in 1_i64..=n {
            let u = g.get_unit(i as usize).ok()?;
            let p = u.get_point().ok()?;
            pts.push(Vector2::new(p.0.x, p.0.z));
        }
        (!pts.is_empty()).then(|| centroid2d(pts))
    }

    pub(super) fn mark_group(&mut self, lua: MizLua, gid: &GroupId) -> Result<()> {
        if let Some(id) = self.ephemeral.group_marks.remove(gid) {
            self.ephemeral.msgs.delete_mark(id)
        }
        let group = group_mut!(self, gid)?;
        let group_center = Self::live_group_center_ground2(lua, group).unwrap_or_else(|| {
            centroid2d(
                group
                    .units
                    .into_iter()
                    .filter_map(|uid| self.persisted.units.get(uid))
                    .filter_map(|u| if u.dead { None } else { Some(u.pos) }),
            )
        });
        let id = match &mut group.origin {
            DeployKind::ObjectiveDeprecated => None,
            DeployKind::Objective { origin: oid } => match objective!(self, oid) {
                Err(_) => None,
                Ok(obj) => {
                    let show = self.ephemeral.cfg.objective_group_marks
                        || matches!(obj.kind, ObjectiveKind::Farp { mobile: true, .. });
                    if !show || group.side != obj.owner {
                        None
                    } else {
                        let msg = format_compact!(
                            "objective group id {} name {} of class {:?}",
                            group.id,
                            group.name,
                            group.class
                        );
                        Some(self.ephemeral.msgs.mark_to_side(
                            group.side,
                            group_center,
                            true,
                            msg,
                        ))
                    }
                }
            },
            DeployKind::Action { name, spec: _, destination, player, marks, .. } => {
                let pname = player
                    .as_ref()
                    .and_then(|p| self.persisted.players.get(p).map(|pl| pl.name.clone()))
                    .unwrap_or_else(|| String::from("Server"));
                let pos_msg = format_compact!("{name} {gid} deployed by {pname}");
                let pos_mark = self.ephemeral.msgs.mark_to_side(
                    group.side,
                    group_center,
                    true,
                    pos_msg,
                );
                match destination {
                    None => Some(pos_mark),
                    Some(dst) => {
                        if !marks.is_empty() {
                            Some(pos_mark)
                        } else {
                            let dst_msg = format_compact!("{name} {gid} destination");
                            marks.insert(
                                self.ephemeral
                                    .msgs
                                    .mark_to_side(group.side, *dst, true, dst_msg),
                            );
                            Some(pos_mark)
                        }
                    }
                }
            }
            DeployKind::Crate { player, spec, .. } => {
                let name = self
                    .persisted
                    .players
                    .get(player)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| String::from("Server"));
                let msg = format_compact!("{} {gid} deployed by {name}", spec.name);
                Some(self.ephemeral.msgs.mark_to_side(
                    group.side,
                    group_center,
                    true,
                    msg,
                ))
            }
            DeployKind::Deployed {
                spec,
                player,
                moved_by,
                cost_fraction: _,
                origin: _,
            } => {
                let name = self
                    .persisted
                    .players
                    .get(player)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| String::from("Server"));
                let resp = moved_by
                    .as_ref()
                    .and_then(|(u, _)| {
                        self.persisted.players.get(u).map(|pl| {
                            format_compact!("\nresponsible party: {}", pl.name.clone())
                        })
                    })
                    .unwrap_or(CompactString::from(""));
                let tail = spec.path.last().map(|p| p.as_str()).unwrap_or("deployed");
                let msg = format_compact!("{tail} {gid} deployed by {name}{resp}");
                Some(self.ephemeral.msgs.mark_to_side(
                    group.side,
                    group_center,
                    true,
                    msg,
                ))
            }
            DeployKind::Troop { player, spec, moved_by, .. } => {
                let name = self
                    .persisted
                    .players
                    .get(player)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| String::from("Server"));
                let resp = moved_by
                    .as_ref()
                    .and_then(|(u, _)| {
                        self.persisted.players.get(u).map(|pl| {
                            format_compact!("\nresponsible party: {}", pl.name.clone())
                        })
                    })
                    .unwrap_or(CompactString::from(""));
                let msg = format_compact!("{} {gid} deployed by {name}{resp}", spec.name);
                Some(self.ephemeral.msgs.mark_to_side(
                    group.side,
                    group_center,
                    true,
                    msg,
                ))
            }
        };
        if let Some(id) = id {
            self.ephemeral.group_marks.insert(*gid, id);
        }
        Ok(())
    }

    pub fn delete_group(&mut self, gid: &GroupId) -> Result<()> {
        let group = self
            .persisted
            .groups
            .remove_cow(gid)
            .ok_or_else(|| anyhow!("no such group {:?}", gid))?;
        self.persisted.groups_by_name.remove_cow(&group.name);
        self.persisted.groups_by_side.get_mut_cow(&group.side).map(|m| m.remove_cow(gid));
        match &group.origin {
            DeployKind::ObjectiveDeprecated | DeployKind::Objective { .. } => (),
            DeployKind::Action { marks, .. } => {
                for id in marks {
                    self.ephemeral.msgs().delete_mark(*id);
                }
                self.persisted.actions.remove_cow(gid);
                self.persisted.jtacs.remove_cow(gid);
                self.persisted.ewrs.remove_cow(gid);
            }
            DeployKind::Crate { player, .. } => {
                self.persisted.crates.remove_cow(gid);
                self.persisted.players[player].crates.remove_cow(gid);
            }
            DeployKind::Deployed { spec, .. } => {
                self.persisted.deployed.remove_cow(gid);
                if spec.jtac.is_some() {
                    self.persisted.jtacs.remove_cow(gid);
                }
                if spec.ewr.is_some() {
                    self.persisted.ewrs.remove_cow(gid);
                }
            }
            DeployKind::Troop { spec, .. } => {
                self.persisted.troops.remove_cow(gid);
                if spec.jtac.is_some() {
                    self.persisted.jtacs.remove_cow(gid);
                }
            }
        }
        if let Some(id) = self.ephemeral.group_marks.remove(gid) {
            self.ephemeral.msgs.delete_mark(id);
        }
        let mut units: SmallVec<[String; 16]> = smallvec![];
        for uid in &group.units {
            self.ephemeral.units_potentially_close_to_enemies.remove(uid);
            self.ephemeral.units_able_to_move.swap_remove(uid);
            if let Some(id) = self.ephemeral.object_id_by_uid.remove(uid) {
                self.ephemeral.uid_by_object_id.remove(&id);
            }
            if let Some(unit) = self.persisted.units.remove_cow(uid) {
                self.persisted.units_by_name.remove_cow(&unit.name);
                units.push(unit.name);
            }
        }
        self.ephemeral.dirty();
        match group.kind {
            None => {
                // it's a static, we have to get it's units
                for unit in &units {
                    self.ephemeral.push_despawn(*gid, Despawn::Static(unit.clone()))
                }
            }
            Some(_) => {
                if let Some(oids) = self.ephemeral.ai_air_dcs_oids.remove(gid) {
                    for oid in &oids {
                        self.ephemeral.gid_by_object_id.remove(oid);
                        self.ephemeral.push_despawn(*gid, Despawn::Group(oid.clone()));
                    }
                    self.ephemeral.object_id_by_gid.remove(gid);
                } else if let Some(oid) = self.ephemeral.object_id_by_gid.get(gid) {
                    self.ephemeral.push_despawn(*gid, Despawn::Group(oid.clone()));
                }
            }
        }
        self.ephemeral.stat(Stat::GroupDeleted { id: *gid });
        Ok(())
    }

    /// add the units to the db, but don't actually spawn them
    pub(super) fn add_group<'lua>(
        &mut self,
        spctx: &'lua SpawnCtx<'lua>,
        idx: &MizIndex,
        side: Side,
        location: SpawnLoc,
        template_name: &str,
        origin: DeployKind,
        extra_tags: BitFlags<UnitTag>,
        spawn_group_label: Option<&str>,
        naval_spawn_land_detail: Option<&str>,
    ) -> Result<GroupId> {
        fn distance<'a, F: Fn(f64, f64) -> f64>(
            pos: Vector2,
            cmp: F,
            positions: impl IntoIterator<Item = &'a Vector2>,
        ) -> f64 {
            positions
                .into_iter()
                .fold(None, |acc, p| {
                    let d = na::distance_squared(&(*p).into(), &pos.into());
                    let acc = match acc {
                        None => d,
                        Some(d) => d,
                    };
                    Some(cmp(acc, d))
                })
                .map(|d| d.sqrt())
                .unwrap_or(0.)
        }
        #[derive(Debug)]
        struct UnitPosition {
            heading: f64,
            position: Vector2,
            altitude: Option<f64>,
        }
        #[derive(Debug)]
        struct GroupPosition {
            positions: VecDeque<UnitPosition>,
            by_type: FxHashMap<String, VecDeque<UnitPosition>>,
        }
        fn compute_unit_positions(
            spctx: &SpawnCtx,
            idx: &MizIndex,
            location: SpawnLoc,
            template: &MizGroup,
        ) -> Result<GroupPosition> {
            let mut positions = template
                .units()?
                .into_iter()
                .map(|u| {
                    let u = u?;
                    Ok(UnitPosition {
                        heading: u.heading()?,
                        position: u.pos()?,
                        altitude: u.alt().unwrap_or(None),
                    })
                })
                .collect::<Result<VecDeque<_>>>()?;
            match location {
                SpawnLoc::InAir { pos, heading, altitude, speed: _ } => {
                    let group_center = centroid2d(positions.iter().map(|p| p.position));
                    let group_altitude = {
                        let (sum, i) = positions
                            .iter()
                            .filter_map(|p| p.altitude)
                            .fold((0., 0.), |(sum, i), a| (sum + a, i + 1.));
                        sum / i
                    };
                    for p in positions.iter_mut() {
                        p.position = p.position - group_center + pos;
                        p.heading = change_heading(p.heading, heading);
                        if let Some(a) = p.altitude {
                            p.altitude = Some(a - group_altitude + altitude);
                        }
                    }
                    rotate2d_gen(heading, positions.make_contiguous(), |p| {
                        &mut p.position
                    });
                    Ok(GroupPosition { positions, by_type: FxHashMap::default() })
                }
                SpawnLoc::AtPosWithCenter { pos, center, heading_add } => {
                    for p in positions.iter_mut() {
                        p.position = p.position - center + pos;
                        p.heading = change_heading(p.heading, heading_add);
                        p.altitude = None;
                    }
                    Ok(GroupPosition { positions, by_type: FxHashMap::default() })
                }
                SpawnLoc::AtTrigger { name, group_heading } => {
                    let group_center = centroid2d(positions.iter().map(|p| p.position));
                    let pos = spctx.get_trigger_zone(idx, name.as_str())?.pos()?;
                    for p in positions.iter_mut() {
                        p.position = p.position - group_center + pos;
                        p.heading = change_heading(p.heading, group_heading);
                        p.altitude = None;
                    }
                    rotate2d_gen(group_heading, positions.make_contiguous(), |p| {
                        &mut p.position
                    });
                    Ok(GroupPosition { positions, by_type: FxHashMap::default() })
                }
                SpawnLoc::AtPos { pos, offset_direction, group_heading } => {
                    let group_center = centroid2d(positions.iter().map(|p| p.position));
                    let radius = distance(
                        group_center,
                        f64::max,
                        positions.iter().map(|p| &p.position),
                    );
                    for p in positions.iter_mut() {
                        p.position =
                            p.position - group_center + pos + radius * offset_direction;
                    }
                    rotate2d_gen(group_heading, positions.make_contiguous(), |p| {
                        &mut p.position
                    });
                    let offset_magnitude = 20.
                        - distance(pos, f64::min, positions.iter().map(|p| &p.position));
                    for p in positions.iter_mut() {
                        p.position = p.position + offset_magnitude * offset_direction;
                        p.heading = change_heading(p.heading, group_heading);
                        p.altitude = None;
                    }
                    Ok(GroupPosition { positions, by_type: FxHashMap::default() })
                }
                SpawnLoc::AtPosOnShip {
                    pos,
                    group_heading,
                    altitude,
                } => {
                    let group_center = centroid2d(positions.iter().map(|p| p.position));
                    for p in positions.iter_mut() {
                        p.position = p.position - group_center + pos;
                        p.heading = change_heading(p.heading, group_heading);
                        p.altitude = Some(altitude);
                    }
                    rotate2d_gen(group_heading, positions.make_contiguous(), |p| {
                        &mut p.position
                    });
                    Ok(GroupPosition {
                        positions,
                        by_type: FxHashMap::default(),
                    })
                }
                SpawnLoc::AtPosWithComponents { pos, group_heading, component_pos } => {
                    let group_center = centroid2d(positions.iter().map(|p| p.position));
                    let center_by_typ: FxHashMap<String, Vector2> = {
                        let mut tbl = FxHashMap::default();
                        for unit in template.units()? {
                            let unit = unit?;
                            let pos = unit.pos()?;
                            let typ = unit.typ()?;
                            if component_pos.contains_key(&**typ) {
                                let (n, v) = tbl
                                    .entry(typ.clone())
                                    .or_insert_with(|| (0, Vector2::new(0., 0.)));
                                *v += pos;
                                *n += 1;
                            }
                        }
                        tbl.into_iter().map(|(k, (n, v))| (k, v / (n as f64))).collect()
                    };
                    let mut by_type: FxHashMap<String, VecDeque<UnitPosition>> =
                        FxHashMap::default();
                    positions.clear();
                    for unit in template.units()? {
                        let unit = unit?;
                        let typ = unit.typ()?;
                        let heading = unit.heading()?;
                        let position = unit.pos()?;
                        let group_center = match center_by_typ.get(&typ) {
                            None => group_center,
                            Some(pos) => *pos,
                        };
                        match component_pos.get(&typ) {
                            None => positions.push_back(UnitPosition {
                                position: position - group_center + pos,
                                heading: change_heading(heading, group_heading),
                                altitude: None,
                            }),
                            Some(pos) => by_type
                                .entry(typ.clone())
                                .or_default()
                                .push_back(UnitPosition {
                                    position: position - group_center + *pos,
                                    heading: change_heading(heading, group_heading),
                                    altitude: None,
                                }),
                        }
                    }
                    rotate2d_gen(group_heading, positions.make_contiguous(), |p| {
                        &mut p.position
                    });
                    for positions in by_type.values_mut() {
                        rotate2d_gen(group_heading, positions.make_contiguous(), |p| {
                            &mut p.position
                        });
                    }
                    Ok(GroupPosition { positions, by_type })
                }
            }
        }
        fn check_water(
            land: &Land,
            positions: &VecDeque<UnitPosition>,
            positions_by_typ: &FxHashMap<String, VecDeque<UnitPosition>>,
        ) -> Result<()> {
            for pos in
                positions.iter().chain(positions_by_typ.values().flat_map(|v| v.iter()))
            {
                match land.get_surface_type(LuaVec2(pos.position))? {
                    SurfaceType::Land | SurfaceType::Road | SurfaceType::Runway => (),
                    SurfaceType::ShallowWater | SurfaceType::Water => {
                        bail!("you can't spawn this unit in water")
                    }
                }
            }
            Ok(())
        }
        fn check_land(
            land: &Land,
            positions: &VecDeque<UnitPosition>,
            positions_by_typ: &FxHashMap<String, VecDeque<UnitPosition>>,
            naval_spawn_land_detail: Option<&str>,
        ) -> Result<()> {
            for pos in
                positions.iter().chain(positions_by_typ.values().flat_map(|v| v.iter()))
            {
                match land.get_surface_type(LuaVec2(pos.position))? {
                    SurfaceType::ShallowWater | SurfaceType::Water => (),
                    SurfaceType::Land | SurfaceType::Road | SurfaceType::Runway => {
                        if let Some(z) = naval_spawn_land_detail {
                            bail!(
                                "TISP trigger zone {:?}: naval spawn position is on land (expected water)",
                                z
                            );
                        }
                        bail!("you can't spawn this unit on land")
                    }
                }
            }
            Ok(())
        }
        /// Deck aircraft sit on land; only hull units tagged Boat must be on water at deploy.
        fn check_land_boat_hulls_at_deploy(
            land: &Land,
            unit_classification: &FxHashMap<Vehicle, UnitTags>,
            template: &MizGroup<'_>,
            gpos: &GroupPosition,
            naval_spawn_land_detail: Option<&str>,
        ) -> Result<()> {
            if !gpos.by_type.is_empty() {
                return check_land(
                    land,
                    &gpos.positions,
                    &gpos.by_type,
                    naval_spawn_land_detail,
                );
            }
            for (u, p) in template.units()?.into_iter().zip(gpos.positions.iter()) {
                let u = u?;
                let typ = u.typ()?;
                let tags = *unit_classification
                    .get(typ.as_str())
                    .ok_or_else(|| anyhow!("unit type not classified {typ}"))?;
                if !tags.contains(UnitTag::Boat) {
                    continue;
                }
                match land.get_surface_type(LuaVec2(p.position))? {
                    SurfaceType::ShallowWater | SurfaceType::Water => (),
                    SurfaceType::Land | SurfaceType::Road | SurfaceType::Runway => {
                        if let Some(z) = naval_spawn_land_detail {
                            bail!(
                                "TISP trigger zone {:?}: naval spawn position is on land (expected water)",
                                z
                            );
                        }
                        bail!("you can't spawn this unit on land")
                    }
                }
            }
            Ok(())
        }
        let land = Land::singleton(spctx.lua())?;
        let template_name = String::from(template_name);
        let template =
            spctx.get_template_ref(idx, GroupKind::Any, side, template_name.as_str())?;
        let mut template_unit_count = 0usize;
        for u in template.group.units()? {
            u?;
            template_unit_count += 1;
        }
        let mut gpos =
            compute_unit_positions(&spctx, idx, location.clone(), &template.group)?;
        let kind = GroupCategory::from_kind(template.category);
        let gid = GroupId::new();
        // naval spawn points need to be pre created in the miz, so they must be
        // spawned with the same name as the pre created group so that they move
        // to their destination.
        let group_name = if extra_tags.contains(UnitTag::NavalSpawnPoint) {
            template_name.clone()
        } else if let DeployKind::Crate { spec, .. } = &origin {
            // ED LOAD CARGOS shows unit name — prefer Fowl crate label over RCRATE/BCRATE.
            String::from(format_compact!("{}-{}", spec.name, gid))
        } else if let Some(lbl) = spawn_group_label {
            String::from(lbl)
        } else {
            String::from(format_compact!("{}-{}", template_name, gid))
        };
        let mut spawned = SpawnedGroup {
            id: gid,
            name: group_name.clone(),
            template_name: template_name.clone(),
            side,
            template_side: None,
            kind,
            origin,
            class: if extra_tags.contains(UnitTag::NavalSpawnPoint) {
                ObjGroupClass::Logi
            } else {
                ObjGroupClass::from(template_name.as_str())
            },
            units: SetS::new(),
            tags: UnitTags(BitFlags::empty()),
        };
        for unit in template.group.units()?.into_iter() {
            let unit = unit?;
            let typ = unit.typ()?;
            let tags = *self
                .ephemeral
                .cfg
                .unit_classification
                .get(typ.as_str())
                .ok_or_else(|| anyhow!("unit type not classified {typ}"))?;
            let tags = UnitTags(tags.0 | extra_tags);
            spawned.tags.0.insert(tags.0);
        }
        match &location {
            SpawnLoc::AtPosOnShip { .. } => (),
            SpawnLoc::AtPos { .. }
            | SpawnLoc::AtPosWithCenter { .. }
            | SpawnLoc::AtPosWithComponents { .. }
            | SpawnLoc::AtTrigger { .. } => {
                if let Some(tmpl) = self.ephemeral.cfg.crate_template.get(&side)
                    && &template_name == tmpl
                {
                    () // it's ok to spawn crates on ships
                } else if ai_air::ai_air_spawn_on_carrier_deck(&spawned.origin) {
                    () // ME TakeOffParking + linkUnit on carrier deck
                } else if matches!(
                    &spawned.origin,
                    DeployKind::Troop {
                        ship_offsets: Some(_),
                        ..
                    }
                ) {
                    () // DEP troops on carrier deck
                } else if spawned.tags.contains(UnitTag::Boat) {
                    check_land_boat_hulls_at_deploy(
                        &land,
                        &self.ephemeral.cfg.unit_classification,
                        &template.group,
                        &gpos,
                        naval_spawn_land_detail,
                    )
                        .with_context(|| format_compact!("placing group {group_name}"))?
                } else {
                    check_water(&land, &gpos.positions, &gpos.by_type)
                        .with_context(|| format_compact!("placing group {group_name}"))?
                }
            }
            SpawnLoc::InAir { .. } => (),
        }
        for unit in template.group.units()?.into_iter() {
            let uid = UnitId::new();
            let unit = unit?;
            let typ = unit.typ()?;
            let tags = *self
                .ephemeral
                .cfg
                .unit_classification
                .get(typ.as_str())
                .ok_or_else(|| anyhow!("unit type not classified {typ}"))?;
            let tags = UnitTags(tags.0 | extra_tags);
            let unit_tpl_name = unit.name()?;
            let unit_name = if extra_tags.contains(UnitTag::NavalSpawnPoint) {
                unit_tpl_name.clone()
            } else if matches!(spawned.origin, DeployKind::Crate { .. })
                || (spawn_group_label.is_some() && template_unit_count == 1)
            {
                group_name.clone()
            } else {
                String::from(format_compact!("{}-{}", group_name, uid))
            };
            let pos = match gpos.by_type.get_mut(&typ) {
                None => gpos.positions.pop_front().ok_or_else(|| {
                    anyhow!(
                        "internal: no queued position for unit type {:?} in template {:?} (main deque empty)",
                        typ.as_str(),
                        template_name
                    )
                })?,
                Some(positions) => positions.pop_front().ok_or_else(|| {
                    anyhow!(
                        "internal: no queued position for unit type {:?} in template {:?} (by_type deque empty)",
                        typ.as_str(),
                        template_name
                    )
                })?,
            };
            let position = {
                let mut p = Position3::default();
                p.p.x = pos.position.x;
                p.p.y = match pos.altitude {
                    None => land.get_height(LuaVec2(pos.position))?,
                    Some(alt) => alt,
                };
                p.p.z = pos.position.y;
                p
            };
            let spawned_unit = SpawnedUnit {
                id: uid,
                group: gid,
                side,
                typ: Vehicle(typ),
                tags,
                name: unit_name.clone(),
                template_name: unit_tpl_name,
                spawn_position: position,
                spawn_pos: pos.position,
                spawn_heading: pos.heading,
                position,
                pos: pos.position,
                heading: pos.heading,
                dead: false,
                hp_percent: 100,
                static_max_life: 0,
                moved: None,
                airborne_velocity: None,
                fuel_fraction: None,
            };
            spawned.units.insert_cow(uid);
            self.persisted.units.insert_cow(uid, spawned_unit);
            self.persisted.units_by_name.insert_cow(unit_name, uid);
        }
        match &mut spawned.origin {
            DeployKind::ObjectiveDeprecated | DeployKind::Objective { .. } => (),
            DeployKind::Action { spec, .. } => {
                self.persisted.actions.insert_cow(gid);
                match &spec.kind {
                    ActionKind::Drone(_) => {
                        self.persisted.jtacs.insert_cow(gid);
                    }
                    ActionKind::Awacs(_) => {
                        self.persisted.ewrs.insert_cow(gid);
                    }
                    _ => (),
                }
            }
            DeployKind::Crate { player, .. } => {
                self.persisted.crates.insert_cow(gid);
                self.persisted.players[player].crates.insert_cow(gid);
            }
            DeployKind::Deployed { spec, .. } => {
                self.persisted.deployed.insert_cow(gid);
                if spec.jtac.is_some() {
                    self.persisted.jtacs.insert_cow(gid);
                }
                if spec.ewr.is_some() {
                    self.persisted.ewrs.insert_cow(gid);
                }
            }
            DeployKind::Troop { spec, .. } => {
                self.persisted.troops.insert_cow(gid);
                if spec.jtac.is_some() {
                    self.persisted.jtacs.insert_cow(gid);
                }
            }
        }
        self.persisted.groups.insert_cow(gid, spawned);
        self.persisted.groups_by_name.insert_cow(group_name, gid);
        self.persisted.groups_by_side.get_or_default_cow(side).insert_cow(gid);
        self.ephemeral.dirty();
        self.mark_group(spctx.lua(), &gid)?;
        Ok(gid)
    }

    pub fn add_and_queue_group<'lua>(
        &mut self,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        location: SpawnLoc,
        template_name: &str,
        origin: DeployKind,
        extra_tags: BitFlags<UnitTag>,
        delay: Option<DateTime<Utc>>,
        spawn_group_label: Option<&str>,
        naval_spawn_land_detail: Option<&str>,
    ) -> Result<GroupId> {
        let gid = self.add_group(
            &spctx,
            idx,
            side,
            location,
            template_name,
            origin,
            extra_tags,
            spawn_group_label,
            naval_spawn_land_detail,
        )?;
        match delay {
            None => self.ephemeral.push_spawn(gid),
            Some(at) => self.ephemeral.delayspawnq.entry(at).or_default().push(gid),
        }
        Ok(gid)
    }

    pub(crate) fn unit_born(
        &mut self,
        lua: MizLua,
        unit: &Unit,
        connected: &Connected,
        birth_place: Option<&Object<'_>>,
        parking_subplace: Option<i64>,
    ) -> Result<BirthRes> {
        let id = unit.object_id()?;
        let name = unit.get_name()?;
        let player_in_unit = unit
            .get_player_name()
            .ok()
            .flatten()
            .and_then(|n| connected.get_by_name(&n));
        if player_in_unit.is_none() {
            if is_fowl_csar_unit_unit(unit) {
                return Ok(BirthRes::None);
            }
            if let Some(uid) = self.persisted.units_by_name.get(name.as_str()) {
                let unit = unit!(self, uid)?;
                self.ephemeral.uid_by_object_id.insert(id.clone(), *uid);
                self.ephemeral.object_id_by_uid.insert(*uid, id.clone());
                self.ephemeral.units_potentially_close_to_enemies.insert(*uid);
                if unit.tags.contains(UnitTag::Driveable) {
                    self.ephemeral.units_able_to_move.insert(*uid);
                }
                self.ephemeral.stat(Stat::Unit {
                    id: EnId::Unit(*uid),
                    gid: Some(unit.group),
                    owner: unit.side,
                    typ: stats::Unit { typ: unit.typ.clone(), tags: unit.tags },
                    pos: stats::Pos {
                        pos: Coord::singleton(lua)?
                            .lo_to_ll(LuaVec3(Vector3::new(unit.pos.x, 0., unit.pos.y)))?,
                        velocity: unit.airborne_velocity.unwrap_or_default(),
                    },
                });
                let gid = unit.group;
                if group_health!(self, gid)?.0 == 1 {
                    self.mark_group(lua, &gid)?
                }
                return Ok(BirthRes::None);
            }
        }
        let slot = unit.slot()?;
        let (si, deferred_validate) = match self.ephemeral.slot_info.get(&slot) {
            Some(si) => (si, false),
            None => {
                // it's a dynamic slot
                let typ = vehicle_type_for_dynamic_slot(unit)?;
                let pos = unit.get_ground_position()?;
                let obj_id = self
                    .objective_for_slot_birth(lua, birth_place, pos.0)
                    .or_else(|| {
                        Db::objective_for_dynamic_slot_pos(&self.persisted.objectives, pos.0)
                            .map(|o| o.id)
                    })
                    .ok_or_else(|| anyhow!("dynamic slot not near any objective"))?;
                let obj = objective!(self, obj_id)?;
                let gid = unit.get_group()?.id()?;
                let gid = miz::GroupId::from(gid.inner());
                self.ephemeral.slot_info.insert(
                    slot,
                    SlotInfo {
                        typ,
                        unit_name: unit.get_name()?,
                        objective: obj.id,
                        ground_start: false,
                        miz_gid: gid,
                        side: obj.owner,
                    },
                );
                self.ephemeral.slot_by_miz_gid.insert(gid, slot);
                (&self.ephemeral.slot_info[&slot], true)
            }
        };
        let name = unit.get_player_name()?;
        let ifo = name.and_then(|name| connected.get_by_name(&name));
        let ucid = match ifo {
            Some(ifo) => ifo.ucid,
            None => {
                error!("slot {slot} born with no player in it");
                unit.clone().destroy()?;
                return Ok(BirthRes::None);
            }
        };
        let side = si.side;
        let typ = si.typ.clone();
        let objective = si.objective;
        let tags = *self
            .ephemeral
            .cfg
            .unit_classification
            .get(&typ)
            .unwrap_or(&UnitTags::default());
        if deferred_validate {
            match self.try_occupy_slot_deferred(lua, Utc::now(), &ucid, slot) {
                SlotAuth::Yes(typ) => {
                    self.ephemeral.stat(Stat::Slot { id: ucid, slot, typ });
                }
                a => {
                    unit.clone().destroy()?;
                    return Ok(BirthRes::DynamicSlotDenied(ucid, a));
                }
            }
        }
        self.ephemeral.stat(Stat::Unit {
            id: EnId::Player(ucid),
            gid: None,
            owner: side,
            typ: stats::Unit { typ, tags },
            pos: stats::Pos {
                pos: Coord::singleton(lua)?.lo_to_ll(unit.get_point()?)?,
                velocity: Vector3::default(),
            },
        });
        self.player_entered_slot(lua, id, unit, slot, objective, ucid, parking_subplace)
            .context("entering player into slot")?;
        Ok(BirthRes::OccupiedSlot(slot))
    }

    /// ME static factory already in the mission; no `spawn()`.
    pub(super) fn register_production_static_group(
        &mut self,
        side: Side,
        oid: ObjectiveId,
        group_name: String,
        unit_name: String,
        unit_type: String,
        pos: Vector2,
        heading: f64,
    ) -> Result<()> {
        self.register_me_objective_static_group(
            side,
            oid,
            group_name,
            unit_name,
            unit_type,
            pos,
            heading,
            super::objective::ObjGroupClass::Production,
            "production factory",
        )
    }

    pub(super) fn register_objective_static_group(
        &mut self,
        side: Side,
        oid: ObjectiveId,
        group_name: String,
        unit_name: String,
        unit_type: String,
        pos: Vector2,
        heading: f64,
    ) -> Result<()> {
        self.register_me_objective_static_group(
            side,
            oid,
            group_name,
            unit_name,
            unit_type,
            pos,
            heading,
            super::objective::ObjGroupClass::ObjectiveStatic,
            "objective static",
        )
    }

    fn register_me_objective_static_group(
        &mut self,
        me_side: Side,
        oid: ObjectiveId,
        group_name: String,
        unit_name: String,
        unit_type: String,
        pos: Vector2,
        heading: f64,
        class: super::objective::ObjGroupClass,
        class_label: &str,
    ) -> Result<()> {
        if self.persisted.units_by_name.get(unit_name.as_str()).is_some() {
            return Ok(());
        }
        let owner = objective!(self, oid)?.owner;
        let mut tags = *self
            .ephemeral
            .cfg
            .unit_classification
            .get(unit_type.as_str())
            .ok_or_else(|| anyhow!("{class_label} unit type not classified: {unit_type}"))?;
        tags.0 |= Self::me_objective_static_role_tag(class);
        let gid = GroupId::new();
        let uid = UnitId::new();
        let position = {
            let mut p = Position3::default();
            p.p.x = pos.x;
            p.p.y = 0.;
            p.p.z = pos.y;
            p
        };
        let spawned_unit = SpawnedUnit {
            id: uid,
            group: gid,
            side: owner,
            typ: Vehicle(unit_type.clone()),
            tags,
            name: unit_name.clone(),
            template_name: unit_name.clone(),
            spawn_position: position,
            spawn_pos: pos,
            spawn_heading: heading,
            position,
            pos,
            heading,
            dead: false,
            hp_percent: 100,
            static_max_life: 0,
            moved: None,
            airborne_velocity: None,
            fuel_fraction: None,
        };
        let spawned = SpawnedGroup {
            id: gid,
            name: group_name.clone(),
            template_name: group_name.clone(),
            side: owner,
            template_side: Some(me_side),
            kind: None,
            origin: DeployKind::Objective { origin: oid },
            class,
            units: SetS::from_iter([uid]),
            tags,
        };
        self.persisted.units.insert_cow(uid, spawned_unit);
        self.persisted.units_by_name.insert_cow(unit_name, uid);
        self.persisted.groups.insert_cow(gid, spawned);
        self.persisted.groups_by_name.insert_cow(group_name, gid);
        self.persisted
            .groups_by_side
            .get_or_default_cow(owner)
            .insert_cow(gid);
        let obj = objective_mut!(self, oid)?;
        obj.groups.get_or_default_cow(owner).insert_cow(gid);
        self.persisted.objectives_by_group.insert_cow(gid, oid);
        self.ephemeral.dirty();
        Ok(())
    }

    pub fn static_born(&mut self, lua: MizLua, st: &StaticObject) -> Result<()> {
        let id = st.object_id()?;
        let name = st.get_name()?;
        if let Some(uid) = self.persisted.units_by_name.get(name.as_str()) {
            let uid = *uid;
            let remake_mark = if let Some(unit) = self.persisted.units.get(&uid) {
                let gid = unit.group;
                let was_place = self
                    .ephemeral
                    .shared_ed_place_ignore_dead
                    .remove(&gid);
                was_place
                    && matches!(
                        self.persisted.groups.get(&gid).map(|g| &g.origin),
                        Some(DeployKind::Crate { .. })
                    )
            } else {
                false
            };
            self.ephemeral.uid_by_static.insert(id, uid);
            Self::cache_unit_static_max_life(self, uid, st);
            if remake_mark {
                if let Some(unit) = self.persisted.units.get_mut_cow(&uid) {
                    unit.dead = false;
                    unit.hp_percent = 100;
                    if let Ok(pt) = st.get_point() {
                        unit.position.p = pt;
                        unit.pos = Vector2::new(pt.0.x, pt.0.z);
                    }
                }
                let gid = unit!(self, uid)?.group;
                let _ = self.mark_group(lua, &gid);
            }
        }
        Ok(())
    }

    fn cache_unit_static_max_life(db: &mut Db, uid: UnitId, st: &StaticObject) {
        let Some(l0) = st.try_get_life0() else {
            return;
        };
        if let Some(unit) = db.persisted.units.get_mut_cow(&uid) {
            unit.static_max_life = l0;
        }
    }

    /// ME objective statics may Birth before linking; bind DCS ids by name.
    pub(super) fn sync_production_static_uid_map(&mut self, lua: MizLua) -> Result<()> {
        let mut synced = 0usize;
        let statics: SmallVec<[(UnitId, CompactString); 32]> = self
            .persisted
            .units
            .into_iter()
            .filter_map(|(uid, unit)| {
                let group = self.persisted.groups.get(&unit.group)?;
                if !group.class.is_me_objective_static() {
                    return None;
                }
                Some((*uid, CompactString::from(unit.name.as_str())))
            })
            .collect();
        for (uid, name) in statics {
            Self::ensure_me_objective_static_role_tags(self, uid);
            match StaticObject::get_by_name(lua, name.as_str()) {
                Ok(Static::Static(st)) => {
                    let id = st.object_id()?;
                    self.ephemeral.uid_by_static.insert(id, uid);
                    Self::cache_unit_static_max_life(self, uid, &st);
                    if let Ok(pt) = st.get_point() {
                        if let Some(unit) = self.persisted.units.get_mut_cow(&uid) {
                            unit.position.p = pt;
                            unit.pos = Vector2::new(pt.0.x, pt.0.z);
                        }
                    }
                    synced += 1;
                }
                Ok(Static::Airbase(_)) => {}
                Err(e) => warn!("ME objective static {name} not in world: {e:?}"),
            }
        }
        if synced > 0 {
            debug!("synced {synced} ME objective static object id(s)");
        }
        Ok(())
    }

    /// After mission load DCS respawns ME factory statics at full health; apply persisted `dead`.
    pub(super) fn apply_persisted_production_factory_statics(&mut self, lua: MizLua) -> Result<()> {
        let mut destroyed = 0usize;
        for (_, unit) in self.persisted.units.into_iter() {
            if !matches!(
                self.persisted.groups.get(&unit.group),
                Some(g) if g.class.is_me_objective_static()
            ) {
                continue;
            }
            if !unit.dead {
                continue;
            }
            match StaticObject::get_by_name(lua, unit.name.as_str()) {
                Ok(Static::Static(st)) => {
                    let id = st.object_id()?;
                    st.destroy()?;
                    self.ephemeral.uid_by_static.remove(&id);
                    destroyed += 1;
                }
                Ok(Static::Airbase(_)) => {}
                Err(_) => (),
            }
        }
        if destroyed > 0 {
            info!(
                "destroyed {destroyed} ME objective static(s) to match persisted state"
            );
        }
        Ok(())
    }

    /// DCS respawns ME statics at full HP; apply persisted partial damage after load.
    pub(super) fn apply_persisted_production_factory_hp(&mut self, lua: MizLua) -> Result<()> {
        use super::objective::ObjGroupClass;

        let mut adjusted = 0usize;
        for (_, unit) in self.persisted.units.into_iter() {
            if !matches!(
                self.persisted.groups.get(&unit.group),
                Some(g) if g.class == ObjGroupClass::Production
            ) {
                continue;
            }
            if unit.dead || unit.hp_percent >= 100 {
                continue;
            }
            match StaticObject::get_by_name(lua, unit.name.as_str()) {
                Ok(Static::Static(st)) => {
                    if Self::apply_factory_hp_to_dcs(&st, unit.hp_percent).is_ok() {
                        adjusted += 1;
                    }
                }
                Ok(Static::Airbase(_)) => {}
                Err(_) => (),
            }
        }
        if adjusted > 0 {
            info!(
                "applied persisted HP to {adjusted} production factory static(s) after load"
            );
        }
        Ok(())
    }

    pub fn production_static_damaged(
        &mut self,
        lua: MizLua,
        id: &DcsOid<ClassStatic>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        use super::objective::ObjGroupClass;

        let uid = match self.ephemeral.uid_by_static.get(id).copied() {
            Some(uid) => uid,
            None => match StaticObject::get_instance(lua, id) {
                Ok(st) => {
                    let name = st.get_name()?;
                    match self.persisted.units_by_name.get(name.as_str()).copied() {
                        Some(uid) => {
                            self.ephemeral.uid_by_static.insert(id.clone(), uid);
                            uid
                        }
                        None => return Ok(()),
                    }
                }
                Err(_) => return Ok(()),
            },
        };
        let gid = match self.persisted.units.get(&uid) {
            Some(u) => u.group,
            None => return Ok(()),
        };
        if !matches!(
            self.persisted.groups.get(&gid),
            Some(g) if g.class == ObjGroupClass::Production
        ) {
            return Ok(());
        }
        let st = StaticObject::get_instance(lua, id)?;
        let hp = Self::static_hp_percent(&st)?;
        let oid = self.persisted.objectives_by_group.get(&gid).copied();
        match self.persisted.units.get_mut_cow(&uid) {
            None => return Ok(()),
            Some(unit) => {
                if unit.dead && hp == 0 {
                    return Ok(());
                }
                unit.hp_percent = hp;
                unit.dead = hp == 0;
                self.ephemeral.dirty();
            }
        }
        if let Some(oid) = oid {
            self.refresh_production_objective_after_factory_change(oid, now)?;
        }
        Ok(())
    }

    pub(crate) fn note_static_hit(
        &mut self,
        id: DcsOid<ClassStatic>,
        who: bfprotocols::shots::Who,
    ) {
        self.ephemeral.note_static_hit(id, who);
    }

    pub(crate) fn take_static_hit(
        &mut self,
        id: &DcsOid<ClassStatic>,
    ) -> Option<bfprotocols::shots::Who> {
        self.ephemeral.take_static_hit(id)
    }

    pub fn unit_dead(
        &mut self,
        id: &DcsOid<ClassUnit>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let (uid, player_ucid) = match self.ephemeral.unit_dead(&self.persisted, id) {
            None => return Ok(()),
            Some((uid, ucid)) => {
                if let Some(ucid) = ucid {
                    let slot = self
                        .persisted
                        .players
                        .get(&ucid)
                        .and_then(|p| p.current_slot.as_ref().map(|(s, _)| *s));
                    self.sync_player_deslot_state(&ucid, slot);
                    (uid, Some(ucid))
                } else {
                    (uid, None)
                }
            }
        };
        match self.persisted.units.get_mut_cow(&uid) {
            None => error!("unit_dead: missing unit {:?}", uid),
            Some(unit) => {
                unit.dead = true;
                unit.pos = unit.spawn_pos;
                unit.heading = unit.spawn_heading;
                unit.position = unit.spawn_position;
                let gid = unit.group;
                let typ = unit.typ.clone();
                // Player airframes are counted via campaign_record_player_airframe_loss.
                if player_ucid.is_none() {
                    self.campaign_record_unit_loss(gid, &typ);
                }
                self.ephemeral.dirty();
                if self.persisted.actions.contains(&gid) {
                    crate::db::ai_air::mark_ai_air_attrition(self, gid);
                }
                let health = group_health!(self, gid)?.0;
                if let Some(oid) = self.persisted.objectives_by_group.get(&gid).copied() {
                    self.update_objective_status(None, &oid, now)?;
                    if let Some(obj) = self.persisted.objectives.get(&oid) {
                        self.ephemeral
                            .update_objective_markup(&self.persisted, obj, &[]);
                    }
                    self.ephemeral.units_potentially_close_to_enemies.remove(&uid);
                    if health == 0 {
                        if let Some(id) = self.ephemeral.group_marks.remove(&gid) {
                            self.ephemeral.msgs.delete_mark(id);
                        }
                    }
                }
                if self.persisted.deployed.contains(&gid)
                    || self.persisted.troops.contains(&gid)
                    || self.persisted.crates.contains(&gid)
                {
                    if health == 0 {
                        match &group!(self, gid)?.origin {
                            DeployKind::Troop {
                                player,
                                moved_by: Some((ucid, p)),
                                ..
                            }
                            | DeployKind::Deployed {
                                player,
                                moved_by: Some((ucid, p)),
                                ..
                            } => {
                                let owner = self.persisted.players[player].name.clone();
                                let ucid = ucid.clone();
                                let p = -(*p as i32);
                                let msg = format_compact!(
                                    "for the death of {gid} which was deployed by {owner} and moved by you"
                                );
                                self.adjust_points(&ucid, p, &msg)
                            }
                            DeployKind::Troop { .. }
                            | DeployKind::Deployed { .. }
                            | DeployKind::Action { .. }
                            | DeployKind::Crate { .. }
                            | DeployKind::Objective { .. }
                            | DeployKind::ObjectiveDeprecated => (),
                        }
                        self.delete_group(&gid)?
                    }
                }
                if self.persisted.actions.contains(&gid) {
                    if let DeployKind::Action { player, spec, .. } =
                        &group!(self, gid)?.origin
                    {
                        if self.group_health(&gid)?.0 == 0 {
                            if let Some((penalty, ucid)) = spec
                                .penalty
                                .and_then(|p| player.as_ref().map(|pl| (p, pl.clone())))
                            {
                                self.adjust_points(
                                    &ucid,
                                    -(penalty as i32),
                                    &format_compact!(
                                        "for the loss of action group {gid}"
                                    ),
                                )
                            }
                            self.delete_group(&gid)?
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn static_dead(
        &mut self,
        lua: MizLua,
        id: &DcsOid<ClassStatic>,
        now: DateTime<Utc>,
        killer: Option<&bfprotocols::shots::Who>,
    ) -> Result<()> {
        let uid = match self.ephemeral.uid_by_static.remove(id) {
            Some(uid) => Some(uid),
            None => match StaticObject::get_instance(lua, id) {
                Ok(st) => {
                    let name = st.get_name()?;
                    self.persisted.units_by_name.get(name.as_str()).copied()
                }
                Err(e) => {
                    warn!("static_dead: unknown static object {:?}: {e:?}", id);
                    None
                }
            },
        };
        let Some(uid) = uid else {
            return Ok(());
        };
        if let Some(unit) = self.persisted.units.get(&uid) {
            // Keep flag until static_born so late Dead after respawn cannot mark the crate dead.
            if self
                .ephemeral
                .shared_ed_place_ignore_dead
                .contains(&unit.group)
            {
                info!(
                    "static_dead: ignore intentional ED unload place for crate {:?}",
                    unit.group
                );
                return Ok(());
            }
        }
        // MP native sling: DCS often fires Dead on unhook (no killer). Soft-respawn in place.
        if killer.is_none()
            && self.ephemeral.cfg.dynamic_cargo_delivery.enabled
            && self.persisted.units.get(&uid).is_some_and(|u| {
                let gid = u.group;
                self.persisted.crates.contains(&gid)
                    && (self.ephemeral.shared_ed_sling_landed.contains(&gid)
                        || self.ephemeral.shared_ed_prev_slung.contains_key(&gid))
            })
        {
            let gid = unit!(self, uid)?.group;
            info!("crate {gid}: sling unhook/impact Dead — soft-respawn in place");
            return self.soft_respawn_fowl_crate_after_sling_impact(lua, gid, uid);
        }
        if self.persisted.units.get(&uid).is_some_and(|u| u.dead) {
            return Ok(());
        }
        let (gid, unit_type) = {
            let unit = match self.persisted.units.get_mut_cow(&uid) {
                None => {
                    error!("static_dead: missing unit {:?}", uid);
                    return Ok(());
                }
                Some(unit) => unit,
            };
            unit.dead = true;
            unit.hp_percent = 0;
            (unit.group, unit.typ.clone())
        };
        self.ephemeral.dirty();
        self.campaign_record_static_loss(gid);
        if let Some(killer) = killer {
            if let Ok(group) = self.group(&gid) {
                self.campaign_top10_on_static_kill(killer, group.side);
            }
        }
        if let Some(oid) = self.persisted.objectives_by_group.get(&gid).copied() {
            let group = group!(self, gid)?;
            if group.class == super::objective::ObjGroupClass::Production {
                let obj = objective!(self, oid)?;
                if let Some(killer) = killer {
                    if *killer.side() != obj.owner {
                        if let Some(pts) = self.ephemeral.cfg.points.as_ref() {
                            let award = pts.production_kill as i32;
                            if award > 0 {
                                if let Some(ucid) = killer.ucid() {
                                    self.adjust_points(
                                        ucid,
                                        award,
                                        &format_compact!(
                                            "for destroying factory at {}",
                                            obj.name
                                        ),
                                    );
                                }
                            }
                        }
                    }
                }
                self.refresh_production_objective_after_factory_change(oid, now)?;
            } else if group.class == super::objective::ObjGroupClass::ObjectiveStatic {
                let obj = objective!(self, oid)?;
                if let Some(killer) = killer {
                    if *killer.side() != obj.owner {
                        let award = self
                            .ephemeral
                            .cfg
                            .objective_static_units
                            .get(unit_type.as_str())
                            .map(|c| c.kill_points as i32)
                            .unwrap_or(0);
                        if award > 0 {
                            if let Some(ucid) = killer.ucid() {
                                self.adjust_points(
                                    ucid,
                                    award,
                                    &format_compact!(
                                        "for destroying {} at {}",
                                        unit_type.as_str(),
                                        obj.name
                                    ),
                                );
                            }
                        }
                    }
                }
                self.refresh_objective_after_static_change(oid, now)?;
            }
        }
        if self.persisted.deployed.contains(&gid)
            || self.persisted.troops.contains(&gid)
            || self.persisted.crates.contains(&gid)
        {
            if self.group_health(&gid)?.0 == 0 {
                if self.persisted.crates.contains(&gid)
                    && self.ephemeral.cfg.dynamic_cargo_delivery.enabled
                {
                    if let Some(carrier) = self.crate_static_ed_carrier(lua, uid) {
                        if self.shared_ed_fowl_load_exceeds_cargo_cfg(lua, &carrier) {
                            if let Err(e) = self.reject_shared_ed_fowl_crate_over_limit(
                                lua, gid, uid, &carrier,
                            ) {
                                warn!(
                                    "crate {gid}: Fowl cargo limit reject failed: {e:?}; deleting"
                                );
                                self.delete_group(&gid)?
                            }
                        } else {
                            if let Some(group) = self.persisted.groups.get_mut_cow(&gid) {
                                if let DeployKind::Crate { ed_carrier, .. } = &mut group.origin {
                                    *ed_carrier = Some(carrier);
                                }
                            }
                            // F8 / sling despawn — keep DeployKind::Crate for unload revive.
                            if let Some(id) = self.ephemeral.group_marks.remove(&gid) {
                                self.ephemeral.msgs.delete_mark(id);
                            }
                            info!("crate {gid} despawned near transport; keeping for ED cargo load");
                        }
                    } else if killer.is_none()
                        && (self.ephemeral.shared_ed_sling_landed.contains(&gid)
                            || self.ephemeral.shared_ed_prev_slung.contains_key(&gid))
                    {
                        info!("crate {gid}: sling impact (late Dead) — soft-respawn in place");
                        self.soft_respawn_fowl_crate_after_sling_impact(lua, gid, uid)?;
                    } else {
                        self.delete_group(&gid)?
                    }
                } else {
                    self.delete_group(&gid)?
                }
            }
        }
        Ok(())
    }

    /// True if adding one more Fowl crate would exceed CFG `cargo` for this shared-ED carrier.
    fn shared_ed_fowl_load_exceeds_cargo_cfg(&self, lua: MizLua, carrier: &Ucid) -> bool {
        let Some(player) = self.persisted.players.get(carrier) else {
            return false;
        };
        let Some((slot, Some(inst))) = player.current_slot.as_ref() else {
            return false;
        };
        let Ok(capacity) = self.cargo_capacity(&inst.typ) else {
            return false;
        };
        let (crates, troops) = self.fowl_crate_and_troop_slot_usage_with_bay(lua, slot, carrier);
        capacity.crate_slots as usize <= crates
            || capacity.total_slots as usize <= crates.saturating_add(troops)
    }

    /// ED already took the crate into the bay; spit it back out for CFG Fowl slot limits.
    /// Force-open ramp, UnloadCargo (even if doors still closing), verify off board when possible,
    /// then relocate outside the bay footprint. After a few failed clears, force place anyway.
    pub(super) fn reject_shared_ed_fowl_crate_over_limit(
        &mut self,
        lua: MizLua,
        gid: GroupId,
        uid: UnitId,
        carrier: &Ucid,
    ) -> Result<()> {
        const BAY_DOOR_ARG: i32 = 86;
        const BAY_READY_MIN: f64 = 0.5;
        const MAX_UNLOAD_ATTEMPTS: u8 = 3;

        let (slot, name, typ) = {
            let player = self
                .persisted
                .players
                .get(carrier)
                .ok_or_else(|| anyhow!("reject Fowl crate: missing carrier player"))?;
            let Some((slot, Some(inst))) = player.current_slot.as_ref() else {
                bail!("reject Fowl crate: carrier has no slot instance");
            };
            let unit_name = self
                .persisted
                .units
                .get(&uid)
                .map(|u| u.name.clone())
                .ok_or_else(|| anyhow!("reject Fowl crate: missing unit {uid:?}"))?;
            (slot.clone(), unit_name, inst.typ.0.clone())
        };

        let ac = self
            .ephemeral
            .slot_instance_unit(lua, &slot)
            .context("reject Fowl crate: carrier unit")?;
        if crate::db::dynamic_cargo::fowl_crate_is_on_sling(lua, &ac, typ.as_str(), name.as_str()) {
            info!("crate {gid}: over-limit skip eject, still on sling ({name})");
            return Ok(());
        }

        let _ = ac.open_ramp(true);
        let bay_ready = ac.check_open_ramp().unwrap_or(false)
            || ac
                .get_draw_argument_value(BAY_DOOR_ARG)
                .map(|v| v >= BAY_READY_MIN)
                .unwrap_or(false);
        if !bay_ready {
            if self.ephemeral.shared_ed_eject_ramp_warned.insert(gid) {
                self.ephemeral.panel_to_player(
                    &self.persisted,
                    12,
                    carrier,
                    "Deployables cargo limit exceeded; cargo bay forced open to unload the over-limit crate.",
                );
            }
        }

        let on_board = |ac: &Unit, crate_name: &str| -> bool {
            let Ok(Some(cargos)) = ac.get_cargos_on_board() else {
                return false;
            };
            let mut found = false;
            let _ = cargos.for_each(|c| {
                let Ok(c) = c else {
                    return Ok(());
                };
                if c.get_name()
                    .map(|n| n.as_str() == crate_name)
                    .unwrap_or(false)
                {
                    found = true;
                }
                Ok(())
            });
            found
        };

        let attempts = self
            .ephemeral
            .shared_ed_eject_attempts
            .entry(gid)
            .or_insert(0);
        *attempts = attempts.saturating_add(1);
        let attempt = *attempts;

        if on_board(&ac, name.as_str()) {
            let mut matched: Option<StaticObject> = None;
            if let Ok(Some(cargos)) = ac.get_cargos_on_board() {
                let _ = cargos.for_each(|c| {
                    let Ok(c) = c else {
                        return Ok(());
                    };
                    if c.get_name()
                        .map(|n| n.as_str() == name.as_str())
                        .unwrap_or(false)
                    {
                        matched = Some(c);
                    }
                    Ok(())
                });
            }
            if let Some(cargo) = matched {
                if let Err(e) = ac.unload_cargo(&cargo) {
                    warn!("crate {gid}: UnloadCargo failed for {name}: {e:?}");
                }
            }
            if on_board(&ac, name.as_str()) {
                if attempt < MAX_UNLOAD_ATTEMPTS {
                    info!(
                        "crate {gid}: still on board after UnloadCargo ({name}); retry {attempt}/{MAX_UNLOAD_ATTEMPTS}"
                    );
                    return Ok(());
                }
                warn!(
                    "crate {gid}: UnloadCargo did not clear bay after {attempt} tries; defer place until bay clear ({name})"
                );
                // Force-clear ED bay object; do not respawn while still on board (ED ghost).
                if let Ok(Some(cargos)) = ac.get_cargos_on_board() {
                    let mut matched: Option<StaticObject> = None;
                    let _ = cargos.for_each(|c| {
                        let Ok(c) = c else {
                            return Ok(());
                        };
                        if c.get_name()
                            .map(|n| n.as_str() == name.as_str())
                            .unwrap_or(false)
                        {
                            matched = Some(c);
                        }
                        Ok(())
                    });
                    if let Some(cargo) = matched {
                        let _ = ac.unload_cargo(&cargo);
                        if on_board(&ac, name.as_str()) {
                            self.ephemeral.shared_ed_place_ignore_dead.insert(gid);
                            let _ = cargo.destroy();
                        }
                    }
                }
                if on_board(&ac, name.as_str()) {
                    self.ephemeral
                        .shared_ed_eject_pending_place
                        .insert(gid, carrier.clone());
                    self.ephemeral
                        .shared_ed_fowl_eject_grace_until
                        .insert(gid, Utc::now() + chrono::Duration::seconds(30));
                    if self.ephemeral.shared_ed_eject_ramp_warned.insert(gid) {
                        self.ephemeral.panel_to_player(
                            &self.persisted,
                            12,
                            carrier,
                            "Deployables cargo limit exceeded; clearing cargo bay ghost before returning the crate to the ground.",
                        );
                    }
                    return Ok(());
                }
            } else {
                info!("crate {gid}: UnloadCargo cleared bay slot for {name}");
            }
        }

        self.ephemeral.shared_ed_eject_pending_place.remove(&gid);
        self.place_fowl_crate_on_ed_unload_line(lua, gid, uid, carrier, &[])?;
        self.ephemeral.panel_to_player(
            &self.persisted,
            12,
            carrier,
            "Deployables cargo limit exceeded; crate returned to the ground. Warehouse supply and fuel containers are unaffected.",
        );
        info!(
            "crate {gid}: ejected over-limit Fowl crate {name} for {carrier:?} (respawn outside bay)"
        );
        Ok(())
    }

    /// Model horizontal extent + 1 m (nose-line lateral packing).
    pub(super) fn fowl_crate_spacing_m(lua: MizLua, name: Option<&str>) -> f64 {
        const FALLBACK_EXTENT_M: f64 = 3.0;
        const GAP_M: f64 = 1.0;
        let extent = name
            .and_then(|n| StaticObject::get_by_name(lua, n).ok())
            .and_then(|s| match s {
                Static::Static(st) if st.is_exist().unwrap_or(false) => Some(st),
                _ => None,
            })
            .and_then(|st| st.get_desc().ok())
            .and_then(|desc| desc.raw_get::<_, dcso3::Box3>("box").ok())
            .map(|b| {
                let dx = (b.max.x - b.min.x).abs();
                let dz = (b.max.z - b.min.z).abs();
                dx.max(dz).max(1.0)
            })
            .unwrap_or(FALLBACK_EXTENT_M);
        extent + GAP_M
    }

    /// Nose packing distance past bay OBB (airframe-specific; C-130 > 25 m).
    pub(super) fn fowl_crate_nose_m_for_carrier(&self, carrier: &Ucid) -> f64 {
        self.persisted
            .players
            .get(carrier)
            .and_then(|p| p.current_slot.as_ref())
            .and_then(|(_, inst)| inst.as_ref())
            .map(|inst| {
                crate::db::dynamic_cargo::fowl_crate_nose_distance_m(inst.typ.as_str())
            })
            .unwrap_or(25.)
    }

    /// Nose then alternating left/right from pilot view; spacing = crate size + 1 m.
    /// `extra_occupied`: same-tick unload reservations (spawn may still be deferred).
    pub(super) fn fowl_crate_nose_line_pos(
        &self,
        lua: MizLua,
        ac_pos: Vector2,
        forward: Vector2,
        exclude_gid: Option<GroupId>,
        spacing: f64,
        nose_m: f64,
        extra_occupied: &[Vector2],
    ) -> Vector2 {
        let flen = (forward.x * forward.x + forward.y * forward.y).sqrt();
        let forward = if flen > 1e-3 {
            forward / flen
        } else {
            Vector2::new(1., 0.)
        };
        let nose = ac_pos + forward * nose_m;
        // Pilot-right: looking along forward in XZ (Vector2 = x,z).
        let right = Vector2::new(forward.y, -forward.x);
        let search_r = nose_m + spacing * 12.;
        let mut occupied: SmallVec<[Vector2; 16]> = smallvec![];
        occupied.extend_from_slice(extra_occupied);
        for gid in &self.persisted.crates {
            if exclude_gid == Some(*gid) {
                continue;
            }
            let Some(group) = self.persisted.groups.get(gid) else {
                continue;
            };
            if !matches!(group.origin, DeployKind::Crate { .. }) {
                continue;
            }
            for uid in &group.units {
                let Some(unit) = self.persisted.units.get(uid) else {
                    continue;
                };
                if unit.dead {
                    continue;
                }
                let pos = match StaticObject::get_by_name(lua, unit.name.as_str()) {
                    Ok(Static::Static(st)) if st.is_exist().unwrap_or(false) => st
                        .get_point()
                        .map(|p| Vector2::new(p.0.x, p.0.z))
                        .unwrap_or(unit.pos),
                    _ => unit.pos,
                };
                if (pos - nose).magnitude() <= search_r {
                    occupied.push(pos);
                }
            }
        }
        for i in 0..24usize {
            let candidate = if i == 0 {
                nose
            } else {
                let rank = ((i + 1) / 2) as f64;
                let sign = if i % 2 == 1 { 1.0 } else { -1.0 };
                nose + right * (sign * rank * spacing)
            };
            let free = occupied
                .iter()
                .all(|p| (candidate - *p).magnitude() >= spacing * 0.95);
            if free {
                return candidate;
            }
        }
        nose
    }

    /// Destroy + respawn Fowl crate on F10-style unload line (nose + lateral packing).
    /// `extra_occupied`: same-tick nose-line reservations from sibling unloads.
    pub(super) fn place_fowl_crate_on_ed_unload_line(
        &mut self,
        lua: MizLua,
        gid: GroupId,
        uid: UnitId,
        carrier: &Ucid,
        extra_occupied: &[Vector2],
    ) -> Result<Vector2> {
        let (slot, side, name) = {
            let player = self
                .persisted
                .players
                .get(carrier)
                .ok_or_else(|| anyhow!("ED unload place: missing carrier player"))?;
            let Some((slot, Some(_))) = player.current_slot.as_ref() else {
                bail!("ED unload place: carrier has no slot instance");
            };
            let unit_name = self
                .persisted
                .units
                .get(&uid)
                .map(|u| u.name.clone())
                .ok_or_else(|| anyhow!("ED unload place: missing unit {uid:?}"))?;
            (slot.clone(), player.side, unit_name)
        };

        let (ac_pos, dir, heading, ac_alt) = {
            let player = self
                .persisted
                .players
                .get(carrier)
                .ok_or_else(|| anyhow!("ED unload place: missing carrier player"))?;
            let Some((_, Some(inst))) = player.current_slot.as_ref() else {
                bail!("ED unload place: carrier has no slot instance");
            };
            match self.ephemeral.slot_instance_unit(lua, &slot) {
                Ok(unit) => {
                    let pos = unit.get_position().unwrap_or(inst.position);
                    let p = unit.get_point().map(|v| v.0).unwrap_or(pos.p.0);
                    (
                        Vector2::new(p.x, p.z),
                        Vector2::new(pos.x.x, pos.x.z),
                        azumith3d(pos.x.0),
                        p.y,
                    )
                }
                Err(_) => (
                    Vector2::new(inst.position.p.x, inst.position.p.z),
                    Vector2::new(inst.position.x.x, inst.position.x.z),
                    azumith3d(inst.position.x.0),
                    inst.position.p.y,
                ),
            }
        };
        let mag = (dir.x * dir.x + dir.y * dir.y).sqrt();
        let dir = if mag > 1e-3 {
            dir / mag
        } else {
            Vector2::new(1., 0.)
        };
        let spacing = Self::fowl_crate_spacing_m(lua, Some(name.as_str()));
        let nose_m = self.fowl_crate_nose_m_for_carrier(carrier);
        let drop_pos = self.fowl_crate_nose_line_pos(
            lua,
            ac_pos,
            dir,
            Some(gid),
            spacing,
            nose_m,
            extra_occupied,
        );

        let (final_pos, alt, ship_hub, ship_offsets) =
            match self.point_near_logistics(side, ac_pos).ok() {
                Some((link_oid, _)) => {
                    match ai_air::try_ship_crate_at_world_pos(
                        lua,
                        self,
                        link_oid,
                        drop_pos,
                        heading,
                        ac_alt,
                    ) {
                        Ok(Some((deck_pos, altitude, offsets))) => {
                            (deck_pos, altitude, Some(link_oid), Some(offsets))
                        }
                        Ok(None) | Err(_) => {
                            match ai_air::resolve_ship_crate_deck_spawn(
                                lua,
                                self,
                                link_oid,
                                ac_pos,
                                dir,
                                heading,
                                ac_alt,
                            ) {
                                Ok(Some((deck_pos, altitude, offsets))) => {
                                    (deck_pos, altitude, Some(link_oid), Some(offsets))
                                }
                                Ok(None) | Err(_) => {
                                    let alt = Land::singleton(lua)?
                                        .get_height(LuaVec2(drop_pos))
                                        .unwrap_or(0.);
                                    (drop_pos, alt, None, None)
                                }
                            }
                        }
                    }
                }
                None => {
                    let alt = Land::singleton(lua)?
                        .get_height(LuaVec2(drop_pos))
                        .unwrap_or(0.);
                    (drop_pos, alt, None, None)
                }
            };
        // Never keep aircraft-bay altitude on land; re-sample ground under final pos.
        const GROUND_PLACE_AGL_M: f64 = 0.35;
        let (final_pos, alt, ship_hub, ship_offsets) = if ship_offsets.is_some() {
            (final_pos, alt, ship_hub, ship_offsets)
        } else {
            let ground = Land::singleton(lua)?
                .get_height(LuaVec2(final_pos))
                .unwrap_or(alt);
            (final_pos, ground + GROUND_PLACE_AGL_M, None, None)
        };

        // Prefer clearing ED bay before destroy+respawn (same name must leave getCargosOnBoard).
        if let Ok(ac) = self.ephemeral.slot_instance_unit(lua, &slot) {
            if crate::db::dynamic_cargo::unit_has_cargo_named(&ac, name.as_str()) {
                let _ = ac.open_ramp(true);
                let mut matched: Option<StaticObject> = None;
                if let Ok(Some(cargos)) = ac.get_cargos_on_board() {
                    let _ = cargos.for_each(|c| {
                        let Ok(c) = c else {
                            return Ok(());
                        };
                        if c.get_name()
                            .map(|n| n.as_str() == name.as_str())
                            .unwrap_or(false)
                        {
                            matched = Some(c);
                        }
                        Ok(())
                    });
                }
                if let Some(cargo) = matched {
                    let _ = ac.unload_cargo(&cargo);
                    if crate::db::dynamic_cargo::unit_has_cargo_named(&ac, name.as_str()) {
                        self.ephemeral.shared_ed_place_ignore_dead.insert(gid);
                        let _ = cargo.destroy();
                    }
                }
                if crate::db::dynamic_cargo::unit_has_cargo_named(&ac, name.as_str()) {
                    self.ephemeral
                        .shared_ed_eject_pending_place
                        .insert(gid, carrier.clone());
                    self.ephemeral
                        .shared_ed_fowl_eject_grace_until
                        .insert(gid, Utc::now() + chrono::Duration::seconds(30));
                    bail!(
                        "ED unload place deferred: {name} still on getCargosOnBoard"
                    );
                }
            }
        }
        if let Ok(Static::Static(st)) = StaticObject::get_by_name(lua, name.as_str()) {
            if st.is_exist().unwrap_or(false) {
                self.ephemeral.shared_ed_place_ignore_dead.insert(gid);
                let _ = st.destroy();
            }
        }

        let mut position = Position3::default();
        position.p.x = final_pos.x;
        position.p.y = alt;
        position.p.z = final_pos.y;

        // Keep / restore a player-readable unit name (ED F8 menu shows StaticObject name).
        // Opaque `fowl{uid}-{ms}` was only for ghost scrub; bay is already clear here.
        let spawn_name = self.fowl_crate_place_unit_name(gid, uid, name.as_str());
        if spawn_name.as_str() != name.as_str() {
            info!(
                "crate {gid}: restore unit name {name} -> {spawn_name} for ED unload place"
            );
        }
        if let Some(unit) = self.persisted.units.get_mut_cow(&uid) {
            if unit.name.as_str() != spawn_name.as_str() {
                self.persisted.units_by_name.remove_cow(&unit.name);
                unit.name = String::from(spawn_name.as_str());
                self.persisted
                    .units_by_name
                    .insert_cow(String::from(spawn_name.as_str()), uid);
            }
            unit.dead = false;
            unit.hp_percent = 100;
            unit.pos = final_pos;
            unit.spawn_pos = final_pos;
            unit.heading = heading;
            unit.spawn_heading = heading;
            unit.position = position;
            unit.spawn_position = position;
        }

        if let Some(group) = self.persisted.groups.get_mut_cow(&gid) {
            if let DeployKind::Crate {
                ed_carrier,
                ship_hub: sh,
                ship_offsets: so,
                ..
            } = &mut group.origin
            {
                *ed_carrier = None;
                *sh = ship_hub;
                *so = ship_offsets;
            }
        }
        if let Some(id) = self.ephemeral.object_id_by_uid.remove(&uid) {
            self.ephemeral.uid_by_object_id.remove(&id);
        }
        if let Some(id) = self.ephemeral.object_id_by_gid.remove(&gid) {
            self.ephemeral.gid_by_object_id.remove(&id);
        }
        if let Some(id) = self.ephemeral.group_marks.remove(&gid) {
            self.ephemeral.msgs.delete_mark(id);
        }

        self.ephemeral.shared_ed_eject_ramp_warned.remove(&gid);
        self.ephemeral.shared_ed_eject_attempts.remove(&gid);
        self.ephemeral.shared_ed_fowl_aboard.remove(&gid);
        self.ephemeral.shared_ed_sling_landed.remove(&gid);
        self.ephemeral.shared_ed_eject_pending_place.remove(&gid);
        self.ephemeral.shared_ed_bay_ghost_names.remove(&gid);
        self.ephemeral
            .shared_ed_fowl_eject_grace_until
            .insert(gid, Utc::now() + chrono::Duration::seconds(30));
        // Keep shared_ed_place_ignore_dead until static_born (may be deferred).
        self.ephemeral.push_spawn(gid);
        self.mark_group(lua, &gid)?;
        self.ephemeral.dirty();
        info!(
            "crate {gid}: placed on ED unload line for {carrier:?} as {spawn_name} at ({:.1},{:.1})",
            final_pos.x, final_pos.y
        );
        Ok(final_pos)
    }

    /// Readable StaticObject name for ED F8 / ground place (no opaque `fowl…` rename).
    fn fowl_crate_place_unit_name(&self, gid: GroupId, uid: UnitId, current: &str) -> String {
        if !current.starts_with("fowl") {
            return String::from(current);
        }
        let spec_name = self
            .persisted
            .groups
            .get(&gid)
            .and_then(|g| match &g.origin {
                DeployKind::Crate { spec, .. } => Some(spec.name.as_str()),
                _ => None,
            });
        match spec_name {
            Some(spec) if !spec.is_empty() => {
                String::from(format_compact!("{spec}-{uid}").as_str())
            }
            _ => String::from(current),
        }
    }

    /// Persist Fowl crate coords from the live DCS static (no destroy/respawn).
    pub(super) fn sync_fowl_crate_persisted_pos_from_live_static(
        &mut self,
        lua: MizLua,
        gid: GroupId,
        uid: UnitId,
    ) -> Result<bool> {
        const GROUND_PLACE_AGL_M: f64 = 0.35;
        let (name, _fallback, heading) = {
            let unit = unit!(self, uid)?;
            (unit.name.clone(), unit.pos, unit.heading)
        };
        let Ok(Static::Static(st)) = StaticObject::get_by_name(lua, name.as_str()) else {
            return Ok(false);
        };
        if !st.is_exist().unwrap_or(false) {
            return Ok(false);
        }
        let point = st.get_point()?;
        let pos = Vector2::new(point.0.x, point.0.z);
        let alt = if point.0.y > 1. {
            point.0.y
        } else {
            Land::singleton(lua)?
                .get_height(LuaVec2(pos))
                .unwrap_or(0.)
                + GROUND_PLACE_AGL_M
        };
        let mut position = Position3::default();
        position.p.x = pos.x;
        position.p.y = alt;
        position.p.z = pos.y;
        if let Some(unit) = self.persisted.units.get_mut_cow(&uid) {
            unit.dead = false;
            unit.hp_percent = 100;
            unit.pos = pos;
            unit.spawn_pos = pos;
            unit.heading = heading;
            unit.spawn_heading = heading;
            unit.position = position;
            unit.spawn_position = position;
        }
        if let Some(group) = self.persisted.groups.get_mut_cow(&gid) {
            if let DeployKind::Crate { ed_carrier, .. } = &mut group.origin {
                *ed_carrier = None;
            }
        }
        self.mark_group(lua, &gid)?;
        self.ephemeral.dirty();
        Ok(true)
    }

    /// Replace a Fowl crate DCS killed on sling unhook (MP physics) at its last ground pos.
    fn soft_respawn_fowl_crate_after_sling_impact(
        &mut self,
        lua: MizLua,
        gid: GroupId,
        uid: UnitId,
    ) -> Result<()> {
        const GROUND_PLACE_AGL_M: f64 = 0.35;
        let (name, fallback, heading) = {
            let unit = unit!(self, uid)?;
            (unit.name.clone(), unit.pos, unit.heading)
        };
        let pos = Self::crate_world_pos(lua, name.as_str(), fallback);
        let alt = Land::singleton(lua)?
            .get_height(LuaVec2(pos))
            .unwrap_or(0.)
            + GROUND_PLACE_AGL_M;
        let mut position = Position3::default();
        position.p.x = pos.x;
        position.p.y = alt;
        position.p.z = pos.y;

        self.ephemeral.shared_ed_place_ignore_dead.insert(gid);
        if let Ok(Static::Static(st)) = StaticObject::get_by_name(lua, name.as_str()) {
            if st.is_exist().unwrap_or(false) {
                let _ = st.destroy();
            }
        }
        self.ephemeral.uid_by_static.retain(|_, u| *u != uid);
        if let Some(unit) = self.persisted.units.get_mut_cow(&uid) {
            unit.dead = false;
            unit.hp_percent = 100;
            unit.pos = pos;
            unit.spawn_pos = pos;
            unit.heading = heading;
            unit.spawn_heading = heading;
            unit.position = position;
            unit.spawn_position = position;
        }
        if let Some(group) = self.persisted.groups.get_mut_cow(&gid) {
            if let DeployKind::Crate { ed_carrier, .. } = &mut group.origin {
                *ed_carrier = None;
            }
        }
        self.ephemeral.shared_ed_sling_landed.insert(gid);
        self.ephemeral.shared_ed_fowl_aboard.remove(&gid);
        self.ephemeral.shared_ed_prev_slung.remove(&gid);
        if let Some(id) = self.ephemeral.group_marks.remove(&gid) {
            self.ephemeral.msgs.delete_mark(id);
        }
        self.ephemeral.push_spawn(gid);
        self.mark_group(lua, &gid)?;
        self.ephemeral.dirty();
        info!(
            "crate {gid}: soft-respawn after sling impact as {name} at ({:.1},{:.1})",
            pos.x, pos.y
        );
        Ok(())
    }

    /// Nearest same-side ED cargo transport player when Dead is likely F8/sling.
    fn crate_static_ed_carrier(&self, lua: MizLua, uid: UnitId) -> Option<Ucid> {
        let unit = self.persisted.units.get(&uid)?;
        let group = self.persisted.groups.get(&unit.group)?;
        self.nearest_ed_cargo_carrier(lua, unit.pos, group.side)
    }

    fn nearest_ed_cargo_carrier(
        &self,
        lua: MizLua,
        crate_pos: Vector2,
        side: Side,
    ) -> Option<Ucid> {
        const MAX_DIST: f64 = 80.;
        let mut best: Option<(f64, Ucid)> = None;
        for (slot, ucid) in &self.ephemeral.players_by_slot {
            let Some(player) = self.persisted.players.get(ucid) else {
                continue;
            };
            if player.side != side {
                continue;
            }
            let Some((_, Some(inst))) = player.current_slot.as_ref() else {
                continue;
            };
            if !super::dynamic_cargo::uses_shared_ed_cargo_bay(
                &self.ephemeral.cfg.dynamic_cargo_delivery,
                inst.typ.as_str(),
            ) {
                continue;
            }
            let pos = match self.ephemeral.slot_instance_unit(lua, slot) {
                Ok(u) => u
                    .get_point()
                    .map(|p| Vector2::new(p.0.x, p.0.z))
                    .unwrap_or_else(|_| Vector2::new(inst.position.p.0.x, inst.position.p.0.z)),
                Err(_) => Vector2::new(inst.position.p.0.x, inst.position.p.0.z),
            };
            let dist = (pos - crate_pos).magnitude();
            if dist <= MAX_DIST {
                match best {
                    Some((d, _)) if dist >= d => {}
                    _ => best = Some((dist, ucid.clone())),
                }
            }
        }
        best.map(|(_, u)| u)
    }

    /// Re-link a Fowl crate after ED F8/sling unload (same static name).
    pub fn try_revive_fowl_crate_static(&mut self, lua: MizLua, st: &StaticObject) -> Result<()> {
        if !self.ephemeral.cfg.dynamic_cargo_delivery.enabled {
            return Ok(());
        }
        let name = st.get_name()?;
        let Some(uid) = self.persisted.units_by_name.get(name.as_str()).copied() else {
            return Ok(());
        };
        let (gid, was_dead, carrier) = {
            let Some(unit) = self.persisted.units.get(&uid) else {
                return Ok(());
            };
            if !self.persisted.crates.contains(&unit.group) {
                return Ok(());
            }
            let carrier = self
                .persisted
                .groups
                .get(&unit.group)
                .and_then(|g| match &g.origin {
                    DeployKind::Crate { ed_carrier, .. } => ed_carrier.clone(),
                    _ => None,
                });
            (unit.group, unit.dead, carrier)
        };
        if !was_dead {
            return Ok(());
        }
        if let Some(carrier) = &carrier {
            if !self.player_current_airframe_flyable(lua, carrier) {
                info!("crate {gid}: carrier airframe lost; deleting instead of revive ({name})");
                self.delete_group(&gid)?;
                if st.is_exist().unwrap_or(false) {
                    let _ = st.clone().destroy();
                }
                return Ok(());
            }
        }
        let point = st.get_point()?;
        if let Some(unit) = self.persisted.units.get_mut_cow(&uid) {
            unit.dead = false;
            unit.pos = Vector2::new(point.0.x, point.0.z);
            unit.position.p.0 = point.0;
            unit.hp_percent = 100;
        }
        if let Some(group) = self.persisted.groups.get_mut_cow(&gid) {
            if let DeployKind::Crate { ed_carrier, .. } = &mut group.origin {
                *ed_carrier = None;
            }
        }
        self.mark_group(lua, &gid)?;
        self.ephemeral.dirty();
        info!("revived Fowl crate {gid} after ED cargo unload ({name})");
        if let Some(carrier) = carrier {
            if self.ephemeral.shared_ed_sling_landed.contains(&gid) {
                info!("crate {gid}: revive in place (sling drop), skip unload line");
            } else if self.player_airframe_in_air(lua, &carrier)
                || self.fowl_crate_is_slung_by_player(lua, &carrier, name.as_str())
            {
                info!("crate {gid}: revive in place (sling / airborne), skip unload line");
            } else if let Err(e) =
                self.place_fowl_crate_on_ed_unload_line(lua, gid, uid, &carrier, &[])
            {
                warn!("crate {gid}: ED unload place after revive failed: {e:?}");
            }
        }
        Ok(())
    }

    pub fn group_health(&self, gid: &GroupId) -> Result<(usize, usize)> {
        group_health!(self, gid)
    }

    pub fn artillery_near_point(
        &self,
        side: Side,
        pos: Vector2,
    ) -> SmallVec<[GroupId; 8]> {
        let range2 = (self.ephemeral.cfg.artillery_mission_range as f64).powi(2);
        let artillery = self
            .deployed()
            .filter_map(|group| {
                if group.tags.contains(UnitTag::Artillery) && group.side == side {
                    let center = self.group_center(&group.id).ok()?;
                    if na::distance_squared(&center.into(), &pos.into()) <= range2 {
                        Some(group.id)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect::<SmallVec<[GroupId; 8]>>();
        artillery
    }

    pub fn calcm_near_point(
        &self,
        side: Side,
        lua: MizLua,
        pos: Vector2,
    ) -> SmallVec<[(GroupId, i32); 8]> {
        let range2 = (self.ephemeral.cfg.calcm_mission_range as f64).powi(2);
        let calcm = self
            .actions()
            .filter_map(|group| {
                if group.tags.contains(UnitTag::CALCM) && group.side == side {
                    let center = self.group_center(&group.id).ok()?;
                    if na::distance_squared(
                        &pos.into(),
                        &na::Point2::new(center.x, center.y),
                    ) <= range2
                    {
                        let mut unit: Option<Unit> = None;
                        let mut ammo = 0;
                        if let Some(uid) = group.units.into_iter().next() {
                            if let Some(id) = self.ephemeral.object_id_by_uid.get(&uid) {
                                let instance = match unit.take() {
                                    Some(unit) => unit.change_instance(id),
                                    None => Unit::get_instance(lua, id),
                                };
                                if let Ok(inst) = instance {
                                    ammo = ai_air::unit_calcm_missile_count(lua, &inst)
                                        .unwrap_or(0)
                                        .try_into()
                                        .unwrap_or(0);
                                }
                            }
                        }

                        Some((group.id, ammo))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect::<SmallVec<[(GroupId, i32); 8]>>();
        calcm
    }

    pub fn update_unit_positions_incremental(
        &mut self,
        lua: MizLua,
        now: DateTime<Utc>,
        mut last: usize,
    ) -> Result<(usize, Vec<DcsOid<ClassUnit>>)> {
        let total = self.ephemeral.units_able_to_move.len();
        if last < total {
            let mut uids: SmallVec<[UnitId; 64]> = smallvec![];
            let elts = self.ephemeral.units_able_to_move.as_slice();
            let stop = last + max(1, total >> 4);
            while last < total && uids.len() < stop {
                uids.push(elts[last]);
                last += 1;
            }
            Ok((last, self.update_unit_positions(lua, now, &uids)?))
        } else {
            Ok((0, vec![]))
        }
    }

    pub fn update_unit_positions(
        &mut self,
        lua: MizLua,
        now: DateTime<Utc>,
        units: &[UnitId],
    ) -> Result<Vec<DcsOid<ClassUnit>>> {
        let coord = Coord::singleton(lua)?;
        let mut unit: Option<Unit> = None;
        let mut moved: SmallVec<[GroupId; 16]> = smallvec![];
        let mut dead: Vec<DcsOid<ClassUnit>> = vec![];
        for uid in units {
            let id = match self.ephemeral.object_id_by_uid.get(&uid) {
                Some(id) => id,
                None => {
                    warn!("update_unit_positions skipping unknown unit {uid}");
                    continue;
                }
            };
            let instance = match unit.take() {
                Some(unit) => unit.change_instance(id),
                None => Unit::get_instance(lua, id),
            };
            let instance = match instance {
                Ok(i) => i,
                Err(e) => {
                    warn!(
                        "update_unit_positions skipping invalid instance {uid}, {:?}",
                        e
                    );
                    dead.push(id.clone());
                    continue;
                }
            };
            let pos = instance.get_position()?;
            let spunit = unit_mut!(self, uid)?;
            if (spunit.position.p.0 - pos.p.0).magnitude_squared() > 1.0 {
                moved.push(spunit.group);
                spunit.moved = Some(now);
                spunit.position = pos;
                spunit.pos = Vector2::new(pos.p.x, pos.p.z);
                spunit.heading = azumith3d(pos.x.0);
                self.ephemeral.units_potentially_close_to_enemies.insert(*uid);
                let v = if spunit.tags.contains(UnitTag::Aircraft) && instance.in_air()? {
                    let v = instance.get_velocity()?.0;
                    spunit.airborne_velocity = Some(v);
                    if let Ok(f) = instance.get_fuel() {
                        spunit.fuel_fraction = Some(f);
                    }
                    Some(v)
                } else {
                    spunit.airborne_velocity = None;
                    if spunit.tags.contains(UnitTag::Aircraft) {
                        if let Ok(f) = instance.get_fuel() {
                            spunit.fuel_fraction = Some(f);
                        }
                    }
                    None
                };
                self.ephemeral.stat(Stat::Position {
                    id: EnId::Unit(*uid),
                    pos: stats::Pos {
                        pos: coord.lo_to_ll(pos.p)?,
                        velocity: v.unwrap_or_default(),
                    },
                });
            }
            unit = Some(instance);
        }
        for gid in moved {
            self.ephemeral.dirty();
            self.mark_group(lua, &gid)?;
        }
        Ok(dead)
    }
}
