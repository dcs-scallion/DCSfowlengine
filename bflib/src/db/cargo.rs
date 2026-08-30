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

use super::{Db, ephemeral::DeployableIndex, group::SpawnedGroup, objective::Objective};
use super::campaign_stats::{deployable_invest_bucket, InvestBucket};
use crate::{
    db::group::DeployKind,
    group, maybe, objective, objective_mut,
    spawnctx::{SpawnCtx, SpawnLoc},
    unit, unit_mut,
};
use anyhow::{Result, anyhow, bail};
use bfprotocols::{
    cfg::{CargoConfig, Crate, Deployable, DeployableKind, LifeType, LimitEnforceTyp, Troop, Vehicle},
    db::{
        group::GroupId,
        objective::{ObjectiveId, ObjectiveKind},
    },
    stats::Stat,
};
use chrono::prelude::*;
use compact_str::{CompactString, format_compact};
use dcso3::{
    LuaVec2, MizLua, Position3, String, Vector2, azumith2d, azumith2d_to, azumith3d, centroid2d,
    coalition::{Side, Static},
    env::miz::MizIndex,
    land::Land,
    net::{SlotId, Ucid},
    object::{DcsObject, DcsOid},
    radians_to_degrees,
    static_object::StaticObject,
    trigger::Trigger,
    unit::{ClassUnit, Unit},
};
use enumflags2::BitFlags;
use fxhash::FxHashMap;
use log::{debug, info, warn};
use serde_derive::{Deserialize, Serialize};

/// DCS CargoBayGates ramp/door draw arg (CH-47 / Mi-8 / Mi-24 / C-130).
const CARGO_BAY_DOOR_ARG: i32 = 86;
/// Hang/Open are ~0.55–1.0; closed is 0.
const CARGO_BAY_READY_MIN: f64 = 0.5;

fn type_requires_cargo_bay_door(type_name: &str) -> bool {
    matches!(
        type_name,
        "CH-47Fbl1" | "Mi-8MTV2" | "Mi-8MT" | "Mi-24P" | "C-130J-30"
    )
}

/// Match DCS dynamic-cargo gate: refuse load/unload until ramp/door is open.
fn ensure_cargo_bay_ready(unit: &Unit) -> Result<()> {
    let typ = unit.get_type_name()?;
    if !type_requires_cargo_bay_door(typ.as_str()) {
        return Ok(());
    }
    let v = unit.get_draw_argument_value(CARGO_BAY_DOOR_ARG)?;
    if v < CARGO_BAY_READY_MIN {
        bail!("CARGO BAY IS NOT READY, CHECK THE DOOR");
    }
    Ok(())
}
use smallvec::{SmallVec, smallvec};
use std::{cmp::max, fmt, sync::Arc};

#[derive(Debug, Clone, Copy)]
pub struct NearbyCrate<'a> {
    pub group: &'a SpawnedGroup,
    pub origin: ObjectiveId,
    pub crate_def: &'a Crate,
    pub pos: Vector2,
    pub heading: f64,
    pub distance: f64,
    /// True when this Fowl crate is in an ED bay / sling (not on the ground).
    pub loaded: bool,
}

#[derive(Debug, Clone)]
pub enum Unpakistan {
    Unpacked(String, u32),
    UnpackedFarp(String, u32),
    Repaired(String),
    RepairedBase(String, u8),
    RepairedProduction(String, u8),
    RepairedStaticQueue(String, u16, u16),
    TransferedSupplies(String, String),
}

#[derive(Debug, Clone, Copy)]
pub enum Oldest {
    Group(GroupId),
    Objective(ObjectiveId),
}

impl fmt::Display for Unpakistan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unpacked(unit, 0) => write!(f, "unpacked a {unit}"),
            Self::Unpacked(unit, secs) => write!(
                f,
                "unpacked a {unit}, units will spawn in {secs} seconds get clear"
            ),
            Self::UnpackedFarp(loc, 0) => write!(f, "unpacked {loc}"),
            Self::UnpackedFarp(loc, secs) => write!(
                f,
                "unpacked {loc}, units will spawn in {secs} seconds get clear"
            ),
            Self::Repaired(unit) => write!(f, "repaired a {unit}"),
            Self::RepairedBase(base, logi) => write!(f, "repaired logistics at {base} to %{logi}"),
            Self::RepairedProduction(opr, pct) => {
                write!(f, "queued production repair at {opr} (production {pct}%)")
            }
            Self::RepairedStaticQueue(base, queued, need) => {
                write!(f, "queued static repair at {base} ({queued}/{need})")
            }
            Self::TransferedSupplies(from, to) => {
                write!(f, "transfered supplies from {from} to {to}")
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InternalTroop {
    pub player: Ucid,
    pub origin: Option<ObjectiveId>,
    pub cost_fraction: f32,
    pub troop: Troop,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InternalPilot {
    pub ucid: Ucid,
    pub name: String,
    pub life_type: LifeType,
    pub side: Side,
    pub enemy: bool,
    pub weight_kg: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cargo {
    pub troops: SmallVec<[InternalTroop; 2]>,
    pub crates: SmallVec<[(ObjectiveId, Crate); 1]>,
    #[serde(default)]
    pub pilots: SmallVec<[InternalPilot; 2]>,
}

impl Cargo {
    pub fn num_troops(&self) -> usize {
        self.troops.len()
    }

    pub fn num_crates(&self) -> usize {
        self.crates.len()
    }

    pub fn num_pilots(&self) -> usize {
        self.pilots.len()
    }

    /// 1 troop squad = 2 half-slots; 1 downed pilot = 1 half-slot.
    pub fn troop_half_slots(&self) -> usize {
        self.num_troops() * 2 + self.num_pilots()
    }

    pub fn total_half_slots(&self) -> usize {
        self.num_crates() * 2 + self.troop_half_slots()
    }

    pub fn num_total(&self) -> usize {
        self.num_crates() + self.num_troops() + (self.num_pilots() + 1) / 2
    }

    pub fn weight(&self) -> i64 {
        let cr = self
            .crates
            .iter()
            .fold(0, |acc, (_, cr)| acc + cr.weight as i64);
        let tr = self
            .troops
            .iter()
            .fold(cr, |acc, it| acc + it.troop.weight as i64);
        self.pilots
            .iter()
            .fold(tr, |acc, p| acc + p.weight_kg as i64)
    }
}

#[derive(Debug, Clone)]
pub struct SlotStats {
    pub name: String,
    pub side: Side,
    pub agl: f64,
    pub speed: f64,
    pub in_air: bool,
    pub pos: Position3,
    pub point: Vector2,
    pub ucid: Ucid,
}

impl SlotStats {
    pub fn get(db: &Db, lua: MizLua, slot: &SlotId) -> Result<Self> {
        let ucid = maybe!(db.ephemeral.players_by_slot, *slot, "no such player")?.clone();
        let side = maybe!(db.persisted.players, ucid, "no player for ucid")?.side;
        let unit = db.ephemeral.slot_instance_unit(lua, slot)?;
        let in_air = unit.in_air()?;
        let name = unit.get_name()?;
        let pos = unit.get_position()?;
        let point = Vector2::new(pos.p.x, pos.p.z);
        let ground_alt = Land::singleton(lua)?.get_height(LuaVec2(point))?;
        let agl = pos.p.y - ground_alt;
        let speed = unit.get_velocity()?.0.magnitude() * 3600. / 1000.;
        Ok(Self {
            name,
            side,
            agl,
            speed,
            in_air,
            pos,
            point,
            ucid,
        })
    }
}

impl Db {
    pub(super) fn point_near_logistics(
        &self,
        side: Side,
        point: Vector2,
    ) -> Result<(ObjectiveId, &Objective)> {
        let obj = self
            .persisted
            .objectives
            .into_iter()
            .find_map(|(oid, obj)| {
                if obj.owner == side && obj.logi() > 0 && obj.zone.contains(point) {
                    if matches!(obj.kind, ObjectiveKind::Production) {
                        return None;
                    }
                    return Some((oid, obj));
                }
                None
            });
        match obj {
            Some((oid, obj)) => Ok((*oid, obj)),
            None => bail!("not near friendly logistics"),
        }
    }

    pub fn spawn_crate(
        &mut self,
        lua: MizLua,
        idx: &MizIndex,
        slot: &SlotId,
        name: &str,
    ) -> Result<SlotStats> {
        debug!("db spawning crate");
        let st = SlotStats::get(self, lua, slot)?;
        if st.in_air {
            bail!("you must land to spawn crates")
        }
        let dir = {
            let mut d = Vector2::new(st.pos.x.x, st.pos.x.z);
            let mag = (d.x * d.x + d.y * d.y).sqrt();
            if mag > 1e-3 {
                d / mag
            } else {
                Vector2::new(1., 0.)
            }
        };
        let group_heading = azumith2d(dir);
        let (oid, _) = self.point_near_logistics(st.side, st.point)?;
        let spacing = Self::fowl_crate_spacing_m(lua, None);
        let nose_m = self
            .ephemeral
            .slot_info
            .get(slot)
            .map(|s| crate::db::dynamic_cargo::fowl_crate_nose_distance_m(s.typ.as_str()))
            .unwrap_or(25.);
        let drop_pos =
            self.fowl_crate_nose_line_pos(lua, st.point, dir, None, spacing, nose_m, &[]);
        // Pack on nose line past bay OBB; on ships walk back onto deck box.
        let (spawnpos, ship_hub, ship_offsets, deck_alt) =
            match crate::db::ai_air::try_ship_crate_at_world_pos(
                lua,
                self,
                oid,
                drop_pos,
                group_heading,
                st.pos.p.y,
            )? {
                Some((deck_spawn, altitude, offsets)) => (
                    SpawnLoc::AtPosOnShip {
                        pos: deck_spawn,
                        group_heading,
                        altitude,
                    },
                    Some(oid),
                    Some(offsets),
                    Some(altitude),
                ),
                None => match crate::db::ai_air::resolve_ship_crate_deck_spawn(
                    lua,
                    self,
                    oid,
                    st.point,
                    dir,
                    group_heading,
                    st.pos.p.y,
                )? {
                    Some((deck_spawn, altitude, offsets)) => (
                        SpawnLoc::AtPosOnShip {
                            pos: deck_spawn,
                            group_heading,
                            altitude,
                        },
                        Some(oid),
                        Some(offsets),
                        Some(altitude),
                    ),
                    None => (
                        SpawnLoc::AtPos {
                            pos: drop_pos,
                            offset_direction: Vector2::new(0., 0.),
                            group_heading,
                        },
                        None,
                        None,
                        None,
                    ),
                },
            };
        let to_delete = self.ephemeral.cfg.max_crates.and_then(|max_crates| {
            let crates = &self.persisted.players[&st.ucid].crates;
            if crates.len() < max_crates as usize {
                None
            } else {
                crates.into_iter().next().map(|id| *id)
            }
        });
        let dep_idx = self
            .ephemeral
            .deployable_idx
            .get(&st.side)
            .ok_or_else(|| anyhow!("{} doesn't have any deployables", st.side))?;
        let crate_cfg = dep_idx
            .crates_by_name
            .get(name)
            .ok_or_else(|| anyhow!("no such crate {name}"))?
            .clone();
        if !self.ephemeral.cfg.supply_transfer_players {
            if let Some(whcfg) = self.ephemeral.cfg.warehouse.as_ref() {
                if whcfg
                    .supply_transfer_crate
                    .get(&st.side)
                    .is_some_and(|c| c.name.as_str() == name)
                {
                    bail!("supply transfer crates are disabled for players")
                }
            }
        }
        if let Some((dep, player)) = dep_idx
            .deployables_by_crates
            .get(&crate_cfg.name)
            .and_then(|n| dep_idx.deployables_by_name.get(n))
            .and_then(|d| self.persisted.players.get(&st.ucid).map(|p| (d, p)))
        {
            if player.points < dep.cost as i32 {
                if let Some(si) = self.ephemeral.slot_info.get(slot) {
                    let gid = si.miz_gid;
                    let msg = format_compact!(
                        "WARNING: you have {} points, and this deployable costs {} points",
                        player.points,
                        dep.cost
                    );
                    self.ephemeral.msgs().panel_to_group(10, false, gid, msg);
                }
            }
        }
        let template = self
            .ephemeral
            .cfg
            .crate_template
            .get(&st.side)
            .ok_or_else(|| anyhow!("missing crate template for {:?} side", st.side))?
            .clone();
        let dk = DeployKind::Crate {
            origin: oid,
            player: st.ucid.clone(),
            spec: crate_cfg.clone(),
            ship_hub,
            ship_offsets,
            ed_carrier: None,
        };
        if let Some(gid) = to_delete {
            self.delete_group(&gid)?;
        }
        let gid = self.add_and_queue_group(
            &SpawnCtx::new(lua)?,
            idx,
            st.side,
            spawnpos,
            &template,
            dk,
            BitFlags::empty(),
            None,
            None,
            None,
        )?;
        if let Some(deck_alt) = deck_alt {
            self.spawn_and_validate_ship_crate(lua, idx, gid, deck_alt)?;
        }
        Ok(st)
    }

    /// Force-spawn a ship-linked crate and delete it if DCS placed it off the deck.
    fn spawn_and_validate_ship_crate(
        &mut self,
        lua: MizLua,
        idx: &MizIndex,
        gid: GroupId,
        deck_alt: f64,
    ) -> Result<()> {
        self.ephemeral.cancel_queued_spawn(gid);
        let spctx = SpawnCtx::new(lua)?;
        let mut perf = bfprotocols::perf::PerfInner::default();
        let group = group!(self, gid)?;
        self.ephemeral
            .spawn_group(&mut perf, &self.persisted, idx, &spctx, group, vec![])?;
        let unit_names: SmallVec<[String; 2]> = {
            let group = group!(self, gid)?;
            group
                .units
                .into_iter()
                .filter_map(|uid| self.persisted.units.get(uid).map(|u| u.name.clone()))
                .collect()
        };
        let on_deck = unit_names.iter().all(|name| {
            crate::db::ai_air::crate_spawned_on_deck(lua, name.as_str(), deck_alt)
        });
        if !on_deck {
            let _ = self.delete_group(&gid);
            bail!("cannot spawn crate off the deck; move farther onto the flight deck");
        }
        Ok(())
    }

    /// Live DCS world pos for a crate static (ship-linked crates move with the carrier).
    /// Ramp-adjacent point for load-range checks (not airframe center).
    fn crate_load_probe_point(&self, st: &SlotStats, slot: &SlotId) -> Vector2 {
        let typ = self
            .ephemeral
            .slot_info
            .get(slot)
            .map(|s| s.typ.as_str())
            .unwrap_or("");
        let nose_m = crate::db::dynamic_cargo::fowl_crate_nose_distance_m(typ);
        let dir = Vector2::new(st.pos.x.x, st.pos.x.z);
        let mag = (dir.x * dir.x + dir.y * dir.y).sqrt();
        let forward = if mag > 1e-3 {
            dir / mag
        } else {
            Vector2::new(1., 0.)
        };
        st.point + forward * nose_m
    }

    pub(super) fn crate_world_pos(lua: MizLua, name: &str, fallback: Vector2) -> Vector2 {
        match StaticObject::get_by_name(lua, name) {
            Ok(Static::Static(st)) if st.is_exist().unwrap_or(false) => st
                .as_object()
                .and_then(|o| o.get_point())
                .map(|p| Vector2::new(p.x, p.z))
                .unwrap_or(fallback),
            _ => fallback,
        }
    }

    fn troop_unit_world_pos(lua: MizLua, name: &str, fallback: Vector2) -> Vector2 {
        match Unit::get_by_name(lua, name) {
            Ok(u) if u.is_exist().unwrap_or(false) => u
                .get_point()
                .map(|p| Vector2::new(p.x, p.z))
                .unwrap_or(fallback),
            _ => fallback,
        }
    }

    fn list_crates_near_point<'a>(
        &'a self,
        lua: MizLua,
        point: Vector2,
        max_dist: f64,
    ) -> Result<SmallVec<[NearbyCrate<'a>; 4]>> {
        let mut res: SmallVec<[NearbyCrate; 4]> = smallvec![];
        for gid in &self.persisted.crates {
            let group = group!(self, gid)?;
            let (oid, crate_def) = match &group.origin {
                DeployKind::Crate {
                    origin: oid,
                    spec: crt,
                    ..
                } => (oid, crt),
                DeployKind::Deployed { .. }
                | DeployKind::Troop { .. }
                | DeployKind::Objective { .. }
                | DeployKind::ObjectiveDeprecated
                | DeployKind::Action { .. }
                | DeployKind::CsarPilot { .. } => {
                    bail!("group {:?} is listed in crates but isn't a crate", gid)
                }
            };
            for uid in &group.units {
                let unit = &unit!(self, uid)?;
                if unit.dead {
                    continue;
                }
                let pos = Self::crate_world_pos(lua, unit.name.as_str(), unit.pos);
                let distance = na::distance(&point.into(), &pos.into());
                if distance <= max_dist {
                    let heading = radians_to_degrees(azumith2d_to(point, pos));
                    let loaded = self.fowl_crate_is_on_ed_bay(lua, *gid);
                    res.push(NearbyCrate {
                        group,
                        origin: *oid,
                        crate_def,
                        pos,
                        heading,
                        distance,
                        loaded,
                    })
                }
            }
        }
        res.sort_by_key(|nc| (nc.distance * 1000.) as u32);
        Ok(res)
    }

    pub fn list_nearby_crates<'a>(
        &'a self,
        lua: MizLua,
        st: &SlotStats,
        slot: &SlotId,
    ) -> Result<SmallVec<[NearbyCrate<'a>; 4]>> {
        let max_dist = self.ephemeral.cfg.crate_load_distance as f64;
        let probe = self.crate_load_probe_point(st, slot);
        self.list_crates_near_point(lua, probe, max_dist)
    }

    pub fn destroy_nearby_crate(&mut self, lua: MizLua, slot: &SlotId) -> Result<()> {
        let st = SlotStats::get(self, lua, slot)?;
        if st.in_air {
            bail!("you must land to destroy crates")
        }
        let nearby = self.list_nearby_crates(lua, &st, slot)?;
        let closest = nearby
            .into_iter()
            .find(|nc| !nc.loaded)
            .ok_or_else(|| anyhow!("no nearby crates on the ground"))?;
        let gid = closest.group.id;
        self.delete_group(&gid)
    }

    /// True if the player unit still exists with life (copilot takeover after PilotDead).
    pub(super) fn object_airframe_flyable(lua: MizLua, id: &DcsOid<ClassUnit>) -> bool {
        match Unit::get_instance(lua, id) {
            Ok(u) => u.is_exist().unwrap_or(false) && u.get_life().map(|l| l >= 1).unwrap_or(false),
            Err(_) => false,
        }
    }

    pub(super) fn player_current_airframe_flyable(&self, lua: MizLua, ucid: &Ucid) -> bool {
        let Some(player) = self.persisted.players.get(ucid) else {
            return false;
        };
        let Some((slot, _)) = player.current_slot.as_ref() else {
            return false;
        };
        let Ok(unit) = self.ephemeral.slot_instance_unit(lua, slot) else {
            return false;
        };
        unit.is_exist().unwrap_or(false) && unit.get_life().map(|l| l >= 1).unwrap_or(false)
    }

    /// Crash / UnitLost / Dead: destroy bay crates. Skip PilotDead while the airframe still flies.
    pub fn destroy_fowl_crates_if_airframe_lost(
        &mut self,
        lua: MizLua,
        id: &DcsOid<ClassUnit>,
    ) {
        if Self::object_airframe_flyable(lua, id) {
            return;
        }
        let Some(ucid) = self.player_in_unit(false, id) else {
            return;
        };
        let slot = self.ephemeral.get_slot_by_object_id(id).cloned();
        self.delete_fowl_crates_for_carrier(Some(lua), &ucid, slot.as_ref(), true);
        self.destroy_dynamic_cargo_if_airframe_lost(lua, id);
    }

    /// Delete Fowl crates still in this player's ED bay when they leave the airframe.
    pub fn delete_fowl_crates_on_carrier_deslot(
        &mut self,
        lua: Option<MizLua>,
        ucid: &Ucid,
        slot: Option<&SlotId>,
    ) {
        self.delete_fowl_crates_for_carrier(lua, ucid, slot, false);
    }

    fn delete_fowl_crates_for_carrier(
        &mut self,
        lua: Option<MizLua>,
        ucid: &Ucid,
        slot: Option<&SlotId>,
        include_geometry: bool,
    ) {
        let mut gids: SmallVec<[GroupId; 8]> = smallvec![];
        for gid in &self.persisted.crates {
            let Some(group) = self.persisted.groups.get(gid) else {
                continue;
            };
            if let DeployKind::Crate {
                ed_carrier: Some(carrier),
                ..
            } = &group.origin
            {
                if carrier == ucid {
                    gids.push(*gid);
                }
            }
        }
        for (gid, carrier) in &self.ephemeral.shared_ed_fowl_aboard {
            if carrier == ucid {
                gids.push(*gid);
            }
        }
        for (gid, carrier) in &self.ephemeral.shared_ed_eject_pending_place {
            if carrier == ucid {
                gids.push(*gid);
            }
        }
        if let (Some(lua), Some(slot)) = (lua, slot) {
            for gid in self.fowl_crate_gids_on_ed_bay(lua, slot) {
                gids.push(gid);
            }
            if include_geometry {
                for gid in self.fowl_crate_gids_geometry_on_ed_bay(lua, slot) {
                    gids.push(gid);
                }
            }
        }
        gids.sort_unstable();
        gids.dedup();
        let why = if include_geometry {
            "airframe lost"
        } else {
            "deslot"
        };
        for gid in gids {
            if self.persisted.groups.get(&gid).is_none() {
                continue;
            }
            info!("{why} {ucid:?}: deleting Fowl crate {gid} left in cargo bay");
            if let Err(e) = self.delete_group(&gid) {
                warn!("{why} {ucid:?}: delete Fowl crate {gid} failed: {e:?}");
            }
        }
    }

    pub fn list_cargo(&self, slot: &SlotId) -> Option<&Cargo> {
        self.ephemeral.cargo.get(slot)
    }

    #[allow(dead_code)]
    pub fn is_player_deployed(&self, gid: &GroupId) -> bool {
        self.persisted.deployed.contains(gid)
    }

    pub fn cargo_capacity(&self, vehicle: &Vehicle) -> Result<CargoConfig> {
        let cargo_capacity = self
            .ephemeral
            .cfg
            .cargo
            .get(vehicle)
            .ok_or_else(|| anyhow!("{:?} can't carry cargo", vehicle))
            .map(|c| *c)?;
        Ok(cargo_capacity)
    }

    /// Fowl crates attributed to this player via ED F8/bay (`ed_carrier`).
    pub fn fowl_ed_carrier_crate_count(&self, ucid: &Ucid) -> usize {
        let mut n = 0usize;
        for gid in &self.persisted.crates {
            let Some(group) = self.persisted.groups.get(gid) else {
                continue;
            };
            if let DeployKind::Crate {
                ed_carrier: Some(carrier),
                ..
            } = &group.origin
            {
                if carrier == ucid {
                    n += 1;
                }
            }
        }
        n
    }

    /// Fowl crates on this slot via ED bay for List Cargo.
    /// Living crates: only `getCargosOnBoard` (OBB is too large — nose packing sits inside C-130 box).
    /// Dead F8-loaded: `ed_carrier` and/or still listed on board.
    pub fn fowl_crates_on_ed_bay(
        &self,
        lua: MizLua,
        slot: &SlotId,
    ) -> SmallVec<[(String, u32); 4]> {
        let mut out: SmallVec<[(String, u32); 4]> = smallvec![];
        let Some(ucid) = self.ephemeral.players_by_slot.get(slot) else {
            return out;
        };
        let Some(player) = self.persisted.players.get(ucid) else {
            return out;
        };
        let Some((_, Some(inst))) = player.current_slot.as_ref() else {
            return out;
        };
        let side = player.side;
        let ac = self.ephemeral.slot_instance_unit(lua, slot).ok();
        for gid in &self.persisted.crates {
            let Some(group) = self.persisted.groups.get(gid) else {
                continue;
            };
            if group.side != side {
                continue;
            }
            let DeployKind::Crate {
                spec, ed_carrier, ..
            } = &group.origin
            else {
                continue;
            };
            let mut aboard = false;
            let mut dead_on_carrier = false;
            let mut w = spec.weight;
            for uid in &group.units {
                let Some(unit) = self.persisted.units.get(uid) else {
                    continue;
                };
                let on_board = ac
                    .as_ref()
                    .map(|u| {
                        crate::db::dynamic_cargo::unit_has_cargo_named(u, unit.name.as_str())
                    })
                    .unwrap_or(false);
                if unit.dead {
                    if ed_carrier.as_ref() == Some(ucid) {
                        dead_on_carrier = true;
                    }
                    if on_board {
                        aboard = true;
                    }
                    continue;
                }
                if on_board {
                    aboard = true;
                    if let Ok(Static::Static(st)) =
                        StaticObject::get_by_name(lua, unit.name.as_str())
                    {
                        if let Ok(cw) = st.get_cargo_weight() {
                            if cw > 0. {
                                w = cw.round() as u32;
                            }
                        }
                    }
                    break;
                }
            }
            if !aboard && !dead_on_carrier {
                continue;
            }
            out.push((spec.name.clone(), w));
        }
        out
    }

    /// True if this Fowl crate is in an ED bay / sling (List Nearby "loaded", unpack gate).
    pub fn fowl_crate_is_on_ed_bay(&self, lua: MizLua, gid: GroupId) -> bool {
        let Some(group) = self.persisted.groups.get(&gid) else {
            return false;
        };
        if !matches!(group.origin, DeployKind::Crate { .. }) {
            return false;
        }
        for uid in &group.units {
            let Some(unit) = self.persisted.units.get(uid) else {
                continue;
            };
            for (slot, _) in &self.ephemeral.players_by_slot {
                if let Ok(ac) = self.ephemeral.slot_instance_unit(lua, slot) {
                    if crate::db::dynamic_cargo::unit_has_cargo_named(&ac, unit.name.as_str()) {
                        return true;
                    }
                }
            }
        }
        matches!(
            &group.origin,
            DeployKind::Crate {
                ed_carrier: Some(_),
                ..
            } if group.units.into_iter().any(|uid| {
                self.persisted.units.get(uid).is_some_and(|u| u.dead)
            })
        )
    }

    /// Group ids of Fowl crates currently on this slot's ED bay footprint,
    /// or dead F8-loaded crates (`ed_carrier`) until revive.
    pub fn fowl_crate_gids_on_ed_bay(
        &self,
        lua: MizLua,
        slot: &SlotId,
    ) -> SmallVec<[GroupId; 4]> {
        let mut out: SmallVec<[GroupId; 4]> = smallvec![];
        let Some(ucid) = self.ephemeral.players_by_slot.get(slot) else {
            return out;
        };
        let Some(player) = self.persisted.players.get(ucid) else {
            return out;
        };
        let Some((_, Some(inst))) = player.current_slot.as_ref() else {
            return out;
        };
        let side = player.side;
        let (hpt, fwd, landed, typ, ac) = match self.ephemeral.slot_instance_unit(lua, slot) {
            Ok(unit) => {
                let Ok(pt) = unit.get_point() else {
                    return out;
                };
                let landed = unit.in_air().map(|a| !a).unwrap_or(!inst.in_air);
                let pos = unit.get_position().unwrap_or(inst.position);
                (
                    pt.0,
                    Vector2::new(pos.x.x, pos.x.z),
                    landed,
                    inst.typ.0.clone(),
                    Some(unit),
                )
            }
            Err(_) => (
                inst.position.p.0,
                Vector2::new(inst.position.x.x, inst.position.x.z),
                !inst.in_air,
                inst.typ.0.clone(),
                None,
            ),
        };
        for gid in &self.persisted.crates {
            let Some(group) = self.persisted.groups.get(gid) else {
                continue;
            };
            if group.side != side {
                continue;
            }
            let DeployKind::Crate { ed_carrier, .. } = &group.origin else {
                continue;
            };
            let mut aboard = false;
            let mut dead_on_carrier = false;
            for uid in &group.units {
                let Some(unit) = self.persisted.units.get(uid) else {
                    continue;
                };
                if unit.dead {
                    if ed_carrier.as_ref() == Some(ucid) {
                        dead_on_carrier = true;
                    }
                    if ac
                        .as_ref()
                        .map(|u| {
                            crate::db::dynamic_cargo::unit_has_cargo_named(u, unit.name.as_str())
                        })
                        .unwrap_or(false)
                    {
                        aboard = true;
                    }
                    continue;
                }
                if ac
                    .as_ref()
                    .map(|u| crate::db::dynamic_cargo::unit_has_cargo_named(u, unit.name.as_str()))
                    .unwrap_or(false)
                {
                    aboard = true;
                    break;
                }
                let Ok(Static::Static(st)) = StaticObject::get_by_name(lua, unit.name.as_str())
                else {
                    continue;
                };
                if crate::db::dynamic_cargo::dynamic_cargo_is_aboard_unit(
                    hpt,
                    fwd,
                    landed,
                    typ.as_str(),
                    &st,
                )
                .unwrap_or(false)
                {
                    aboard = true;
                    break;
                }
            }
            if !aboard && !dead_on_carrier {
                continue;
            }
            out.push(*gid);
        }
        out
    }

    /// F8 keeps Fowl crates alive in the bay (no Dead) — enforce CFG `cargo` on a tick.
    pub fn enforce_shared_ed_fowl_cargo_limits(&mut self, lua: MizLua) {
        if !self.ephemeral.cfg.dynamic_cargo_delivery.enabled {
            return;
        }
        let cfg = &self.ephemeral.cfg.dynamic_cargo_delivery;
        let carriers: SmallVec<[(SlotId, Ucid); 8]> = self
            .ephemeral
            .players_by_slot
            .iter()
            .filter_map(|(slot, ucid)| {
                let player = self.persisted.players.get(ucid)?;
                let (_, Some(inst)) = player.current_slot.as_ref()? else {
                    return None;
                };
                if !crate::db::dynamic_cargo::uses_shared_ed_cargo_bay(cfg, inst.typ.as_str()) {
                    return None;
                }
                Some((slot.clone(), ucid.clone()))
            })
            .collect();
        for (slot, ucid) in carriers {
            let Some(player) = self.persisted.players.get(&ucid) else {
                continue;
            };
            let Some((_, Some(inst))) = player.current_slot.as_ref() else {
                continue;
            };
            let Ok(capacity) = self.cargo_capacity(&inst.typ) else {
                continue;
            };
            let hybrid = self.ephemeral.cargo.get(&slot);
            let hybrid_crates = hybrid.map(|c| c.num_crates()).unwrap_or(0);
            let troops = hybrid.map(|c| c.num_troops()).unwrap_or(0);
            // Geometry for living F8 crates; ed_carrier ghosts must not drive eject.
            let mut bay_gids = self.fowl_crate_gids_geometry_on_ed_bay(lua, &slot);
            let ac = self.ephemeral.slot_instance_unit(lua, &slot).ok();
            bay_gids.retain(|gid| {
                if self.ephemeral.shared_ed_eject_pending_place.contains_key(gid) {
                    return false;
                }
                let on_board = self
                    .persisted
                    .groups
                    .get(gid)
                    .and_then(|g| g.units.into_iter().next())
                    .and_then(|uid| self.persisted.units.get(uid))
                    .zip(ac.as_ref())
                    .is_some_and(|(unit, ac)| {
                        crate::db::dynamic_cargo::unit_has_cargo_named(ac, unit.name.as_str())
                    });
                // Grace only ignores geometry false-positives near the nose — never
                // skip crates that ED still lists on board (CFG capacity must count them).
                if !on_board
                    && self
                        .ephemeral
                        .shared_ed_fowl_eject_grace_until
                        .get(gid)
                        .is_some_and(|until| Utc::now() < *until)
                {
                    return false;
                }
                true
            });
            let crate_room = (capacity.crate_slots as usize).saturating_sub(hybrid_crates);
            let total_room = (capacity.total_slots as usize)
                .saturating_sub(hybrid_crates.saturating_add(troops));
            let allowed = crate_room.min(total_room);
            if bay_gids.len() <= allowed {
                continue;
            }
            bay_gids.sort();
            let excess = bay_gids.len() - allowed;
            let to_eject: SmallVec<[GroupId; 4]> =
                bay_gids.into_iter().rev().take(excess).collect();
            for gid in to_eject {
                let Some(uid) = self
                    .persisted
                    .groups
                    .get(&gid)
                    .and_then(|g| g.units.into_iter().next().copied())
                else {
                    continue;
                };
                if let Err(e) =
                    self.reject_shared_ed_fowl_crate_over_limit(lua, gid, uid, &ucid)
                {
                    warn!("enforce shared ED Fowl cargo limit: eject {gid} failed: {e:?}");
                }
            }
        }
        self.sweep_shared_ed_eject_bay_ghosts(lua);
    }

    /// UnloadCargo leftovers / deferred ground place after over-limit eject.
    /// Ghost scrub uses only pre-rename names — never the living crate's current name
    /// (that would F8-unload a legitimately reloaded crate during grace).
    fn sweep_shared_ed_eject_bay_ghosts(&mut self, lua: MizLua) {
        let now = Utc::now();
        self.ephemeral
            .shared_ed_fowl_eject_grace_until
            .retain(|_, until| now < *until);

        let pending: SmallVec<[(GroupId, Ucid); 8]> = self
            .ephemeral
            .shared_ed_eject_pending_place
            .iter()
            .map(|(g, u)| (*g, u.clone()))
            .collect();
        let ghost_scrub: SmallVec<[(GroupId, String); 8]> = self
            .ephemeral
            .shared_ed_bay_ghost_names
            .iter()
            .map(|(g, n)| (*g, n.clone()))
            .collect();

        for (gid, old_name) in ghost_scrub {
            if self.ephemeral.shared_ed_eject_pending_place.contains_key(&gid) {
                continue;
            }
            // Never scrub the post-rename living name.
            if self
                .persisted
                .groups
                .get(&gid)
                .and_then(|g| g.units.into_iter().next())
                .and_then(|uid| self.persisted.units.get(uid))
                .is_some_and(|u| u.name.as_str() == old_name.as_str())
            {
                self.ephemeral.shared_ed_bay_ghost_names.remove(&gid);
                continue;
            }
            if !self.try_clear_ed_bay_cargo_named(lua, gid, old_name.as_str()) {
                self.ephemeral.shared_ed_bay_ghost_names.remove(&gid);
                // Old F8 ghost gone — no need to keep grace blocking relocate/count.
                self.ephemeral.shared_ed_fowl_eject_grace_until.remove(&gid);
            }
        }

        for (gid, ucid) in pending {
            let Some(uid) = self
                .persisted
                .groups
                .get(&gid)
                .and_then(|g| g.units.into_iter().next().copied())
            else {
                self.ephemeral.shared_ed_eject_pending_place.remove(&gid);
                continue;
            };
            let Some(name) = self.persisted.units.get(&uid).map(|u| u.name.clone()) else {
                self.ephemeral.shared_ed_eject_pending_place.remove(&gid);
                continue;
            };
            if self.try_clear_ed_bay_cargo_named(lua, gid, name.as_str()) {
                continue;
            }
            if self.fowl_crate_is_slung_by_player(lua, &ucid, name.as_str())
                || self.player_airframe_in_air(lua, &ucid)
            {
                continue;
            }
            if self.ephemeral.shared_ed_sling_landed.contains(&gid) {
                self.ephemeral.shared_ed_eject_pending_place.remove(&gid);
                continue;
            }
            self.ephemeral.shared_ed_eject_pending_place.remove(&gid);
            if let Err(e) = self.place_fowl_crate_on_ed_unload_line(lua, gid, uid, &ucid, &[]) {
                warn!("crate {gid}: deferred ED unload place failed: {e:?}");
                self.ephemeral
                    .shared_ed_eject_pending_place
                    .insert(gid, ucid);
            } else {
                self.ephemeral.panel_to_player(
                    &self.persisted,
                    12,
                    &ucid,
                    "Deployables cargo limit exceeded; crate returned to the ground. Warehouse supply and fuel containers are unaffected.",
                );
                info!("crate {gid}: deferred eject place after ED bay clear for {ucid:?}");
            }
        }
    }

    /// Returns true if the crate name is still listed on some player ED bay.
    pub(super) fn try_clear_ed_bay_cargo_named(
        &mut self,
        lua: MizLua,
        gid: GroupId,
        name: &str,
    ) -> bool {
        let mut still = false;
        for (slot, _) in self.ephemeral.players_by_slot.clone() {
            let Ok(ac) = self.ephemeral.slot_instance_unit(lua, &slot) else {
                continue;
            };
            if !crate::db::dynamic_cargo::unit_has_cargo_named(&ac, name) {
                continue;
            }
            still = true;
            let _ = ac.open_ramp(true);
            let mut matched: Option<StaticObject> = None;
            if let Ok(Some(cargos)) = ac.get_cargos_on_board() {
                let _ = cargos.for_each(|c| {
                    let Ok(c) = c else {
                        return Ok(());
                    };
                    if c.get_name()
                        .map(|n| n.as_str() == name)
                        .unwrap_or(false)
                    {
                        matched = Some(c);
                    }
                    Ok(())
                });
            }
            if let Some(cargo) = matched {
                let _ = ac.unload_cargo(&cargo);
                if crate::db::dynamic_cargo::unit_has_cargo_named(&ac, name) {
                    self.ephemeral.shared_ed_place_ignore_dead.insert(gid);
                    let _ = cargo.destroy();
                }
            }
            if crate::db::dynamic_cargo::unit_has_cargo_named(&ac, name) {
                still = true;
            } else {
                still = false;
                info!("crate {gid}: cleared ED bay ghost ({name})");
            }
        }
        still
    }

    /// Fowl crates on this slot's ED bay: OBB footprint and/or `getCargosOnBoard`.
    fn fowl_crate_gids_geometry_on_ed_bay(
        &self,
        lua: MizLua,
        slot: &SlotId,
    ) -> SmallVec<[GroupId; 4]> {
        let mut out: SmallVec<[GroupId; 4]> = smallvec![];
        let Some(ucid) = self.ephemeral.players_by_slot.get(slot) else {
            return out;
        };
        let Some(player) = self.persisted.players.get(ucid) else {
            return out;
        };
        let Some((_, Some(inst))) = player.current_slot.as_ref() else {
            return out;
        };
        let side = player.side;
        let (hpt, fwd, landed, typ, ac) = match self.ephemeral.slot_instance_unit(lua, slot) {
            Ok(unit) => {
                let Ok(pt) = unit.get_point() else {
                    return out;
                };
                let landed = unit.in_air().map(|a| !a).unwrap_or(!inst.in_air);
                let pos = unit.get_position().unwrap_or(inst.position);
                (
                    pt.0,
                    Vector2::new(pos.x.x, pos.x.z),
                    landed,
                    inst.typ.0.clone(),
                    Some(unit),
                )
            }
            Err(_) => (
                inst.position.p.0,
                Vector2::new(inst.position.x.x, inst.position.x.z),
                !inst.in_air,
                inst.typ.0.clone(),
                None,
            ),
        };
        for gid in &self.persisted.crates {
            let Some(group) = self.persisted.groups.get(gid) else {
                continue;
            };
            if group.side != side {
                continue;
            }
            if !matches!(group.origin, DeployKind::Crate { .. }) {
                continue;
            }
            for uid in &group.units {
                let Some(unit) = self.persisted.units.get(uid) else {
                    continue;
                };
                if ac
                    .as_ref()
                    .map(|u| crate::db::dynamic_cargo::unit_has_cargo_named(u, unit.name.as_str()))
                    .unwrap_or(false)
                {
                    out.push(*gid);
                    break;
                }
                if unit.dead {
                    continue;
                }
                let Ok(Static::Static(st)) = StaticObject::get_by_name(lua, unit.name.as_str())
                else {
                    continue;
                };
                if crate::db::dynamic_cargo::dynamic_cargo_is_aboard_unit(
                    hpt,
                    fwd,
                    landed,
                    typ.as_str(),
                    &st,
                )
                .unwrap_or(false)
                {
                    out.push(*gid);
                    break;
                }
            }
        }
        out
    }

    /// After F8 unload: move Fowl Deployable/Repair crates to nose line (not ED warehouse cargo).
    pub fn relocate_fowl_crates_after_shared_ed_unload(&mut self, lua: MizLua) {
        if !self.ephemeral.cfg.dynamic_cargo_delivery.enabled {
            return;
        }
        let cfg = &self.ephemeral.cfg.dynamic_cargo_delivery;
        let carriers: SmallVec<[(SlotId, Ucid); 8]> = self
            .ephemeral
            .players_by_slot
            .iter()
            .filter_map(|(slot, ucid)| {
                let player = self.persisted.players.get(ucid)?;
                let (_, Some(inst)) = player.current_slot.as_ref()? else {
                    return None;
                };
                if !crate::db::dynamic_cargo::uses_shared_ed_cargo_bay(cfg, inst.typ.as_str()) {
                    return None;
                }
                Some((slot.clone(), ucid.clone()))
            })
            .collect();

        let mut current: FxHashMap<GroupId, Ucid> = FxHashMap::default();
        let mut cur_slung: FxHashMap<GroupId, Ucid> = FxHashMap::default();
        for (slot, ucid) in &carriers {
            let ac = self.ephemeral.slot_instance_unit(lua, slot).ok();
            let typ = self
                .persisted
                .players
                .get(ucid)
                .and_then(|p| p.current_slot.as_ref())
                .and_then(|(_, inst)| inst.as_ref())
                .map(|i| i.typ.0.clone());
            for gid in self.fowl_crate_gids_geometry_on_ed_bay(lua, slot) {
                let Some(group) = self.persisted.groups.get(&gid) else {
                    continue;
                };
                let DeployKind::Crate { ed_carrier, .. } = &group.origin else {
                    continue;
                };
                let mut on_board = false;
                let mut dead_on_carrier = false;
                let mut slung = false;
                if self.ephemeral.shared_ed_sling_landed.contains(&gid) {
                    for uid in &group.units {
                        let Some(unit) = self.persisted.units.get(uid) else {
                            continue;
                        };
                        if ac
                            .as_ref()
                            .map(|u| {
                                crate::db::dynamic_cargo::unit_has_cargo_named(u, unit.name.as_str())
                            })
                            .unwrap_or(false)
                        {
                            on_board = true;
                        }
                        if let (Some(ac), Some(typ)) = (ac.as_ref(), typ.as_ref()) {
                            if crate::db::dynamic_cargo::fowl_crate_is_on_sling(
                                lua,
                                ac,
                                typ.as_str(),
                                unit.name.as_str(),
                            ) {
                                slung = true;
                            }
                        }
                    }
                    if on_board || slung {
                        self.ephemeral.shared_ed_sling_landed.remove(&gid);
                    } else {
                        continue;
                    }
                }
                for uid in &group.units {
                    let Some(unit) = self.persisted.units.get(uid) else {
                        continue;
                    };
                    if ac
                        .as_ref()
                        .map(|u| {
                            crate::db::dynamic_cargo::unit_has_cargo_named(u, unit.name.as_str())
                        })
                        .unwrap_or(false)
                    {
                        on_board = true;
                    }
                    if let (Some(ac), Some(typ)) = (ac.as_ref(), typ.as_ref()) {
                        if crate::db::dynamic_cargo::fowl_crate_is_on_sling(
                            lua,
                            ac,
                            typ.as_str(),
                            unit.name.as_str(),
                        ) {
                            slung = true;
                        }
                    }
                    if unit.dead && ed_carrier.as_ref() == Some(ucid) {
                        dead_on_carrier = true;
                    }
                }
                // OBB alone is not "aboard" — F8 rear dumps sit inside the C-130 box.
                // Sling stays aboard until the hook actually releases on the ground.
                if on_board || dead_on_carrier || slung {
                    current.insert(gid, ucid.clone());
                }
                if slung {
                    cur_slung.insert(gid, ucid.clone());
                }
            }
        }

        // Hook release: was slung last tick, no longer slung — keep DCS landing pos (skip nose line).
        for (gid, ucid) in &self.ephemeral.shared_ed_prev_slung.clone() {
            if !cur_slung.contains_key(gid) {
                let Some(uid) = self
                    .persisted
                    .groups
                    .get(gid)
                    .and_then(|g| g.units.into_iter().next().copied())
                else {
                    continue;
                };
                match self.sync_fowl_crate_persisted_pos_from_live_static(lua, *gid, uid) {
                    Ok(true) => info!(
                        "crate {gid}: hook-release detected, synced live static pos (skip unload line)"
                    ),
                    Ok(false) => info!(
                        "crate {gid}: hook-release detected, no live static (skip unload line)"
                    ),
                    Err(e) => warn!("crate {gid}: hook-release pos sync failed: {e:?}"),
                }
                self.ephemeral.shared_ed_sling_landed.insert(*gid);
                current.remove(gid);
                let _ = ucid;
            }
        }
        self.ephemeral.shared_ed_prev_slung = cur_slung;

        // F8 can keep crates alive in-bay without Dead — drop stale F10 marks.
        for gid in current.keys() {
            if let Some(id) = self.ephemeral.group_marks.remove(gid) {
                self.ephemeral.msgs.delete_mark(id);
            }
        }

        let unloaded: SmallVec<[(GroupId, Ucid); 8]> = self
            .ephemeral
            .shared_ed_fowl_aboard
            .iter()
            .filter(|(gid, _)| !current.contains_key(gid))
            .map(|(gid, ucid)| (*gid, ucid.clone()))
            .collect();

        let mut reserved: SmallVec<[Vector2; 8]> = smallvec![];
        for (gid, ucid) in unloaded {
            if self.ephemeral.shared_ed_eject_pending_place.contains_key(&gid) {
                continue;
            }
            if self.ephemeral.shared_ed_sling_landed.contains(&gid) {
                continue;
            }
            let Some(uid) = self
                .persisted
                .groups
                .get(&gid)
                .and_then(|g| g.units.into_iter().next().copied())
            else {
                continue;
            };
            if !self.persisted.crates.contains(&gid) {
                continue;
            }
            // Skip dead (revive path places); skip if already off the map.
            let Some(unit) = self.persisted.units.get(&uid) else {
                continue;
            };
            if unit.dead {
                continue;
            }
            if self.fowl_crate_is_slung_by_player(lua, &ucid, unit.name.as_str()) {
                current.insert(gid, ucid);
                continue;
            }
            if self.player_airframe_in_air(lua, &ucid) {
                continue;
            }
            if !self.player_current_airframe_flyable(lua, &ucid) {
                info!("crate {gid}: carrier airframe lost; deleting instead of F8 unload place");
                if let Err(e) = self.delete_group(&gid) {
                    warn!("crate {gid}: delete after airframe loss failed: {e:?}");
                }
                continue;
            }
            let in_grace = self
                .ephemeral
                .shared_ed_fowl_eject_grace_until
                .get(&gid)
                .is_some_and(|until| Utc::now() < *until);
            if in_grace {
                // Skip re-place only when already outside the bay OBB; if still inside
                // (F8 rear dump / failed scrub), move to the nose line anyway.
                let still_in_bay = carriers.iter().any(|(slot, cu)| {
                    cu == &ucid
                        && self
                            .fowl_crate_gids_geometry_on_ed_bay(lua, slot)
                            .contains(&gid)
                });
                if !still_in_bay {
                    continue;
                }
            }
            match self.place_fowl_crate_on_ed_unload_line(lua, gid, uid, &ucid, &reserved) {
                Ok(pos) => reserved.push(pos),
                Err(e) => warn!("crate {gid}: F8 unload place failed: {e:?}"),
            }
        }

        self.ephemeral.shared_ed_fowl_aboard = current;
    }

    /// Hybrid inventory + ED-bay Fowl crates for CFG `cargo` slot display / troop checks.
    pub fn fowl_crate_and_troop_slot_usage(
        &self,
        slot: &SlotId,
        ucid: &Ucid,
    ) -> (usize, usize) {
        let hybrid = self.ephemeral.cargo.get(slot);
        let crates = hybrid.map(|c| c.num_crates()).unwrap_or(0)
            + self.fowl_ed_carrier_crate_count(ucid);
        let troops = hybrid.map(|c| c.num_troops()).unwrap_or(0);
        (crates, troops)
    }

    /// Like `fowl_crate_and_troop_slot_usage`, but also counts Fowl statics currently in the ED bay.
    pub fn fowl_crate_and_troop_slot_usage_with_bay(
        &self,
        lua: MizLua,
        slot: &SlotId,
        ucid: &Ucid,
    ) -> (usize, usize) {
        let hybrid = self.ephemeral.cargo.get(slot);
        let troops = hybrid.map(|c| c.num_troops()).unwrap_or(0);
        let hybrid_crates = hybrid.map(|c| c.num_crates()).unwrap_or(0);
        let ed_bay = self.fowl_crates_on_ed_bay(lua, slot).len();
        (hybrid_crates.saturating_add(ed_bay), troops)
    }

    pub fn number_deployed(&self, side: Side, name: &str) -> Result<(usize, Option<Oldest>)> {
        let mut n = 0;
        let mut oldest = None;
        for gid in &self.persisted.deployed {
            let group = &group!(self, gid)?;
            if let DeployKind::Deployed { spec: d, .. } = &group.origin {
                if let Some(d_name) = d.path.last() {
                    if group.side == side && d_name.as_str() == name {
                        if oldest.is_none() {
                            oldest = Some(Oldest::Group(*gid));
                        }
                        n += 1;
                    }
                }
            }
        }
        for oid in &self.persisted.farps {
            let obj = objective!(self, oid)?;
            if let ObjectiveKind::Farp {
                spec,
                pad_template: _,
                mobile: _,
                ..
            } = &obj.kind
            {
                if let Some(d_name) = spec.path.last() {
                    if obj.owner == side && d_name.as_str() == name {
                        if oldest.is_none() {
                            oldest = Some(Oldest::Objective(*oid));
                        }
                        n += 1;
                    }
                }
            }
        }
        Ok((n, oldest))
    }

    pub fn deployable_by_crate<'a>(
        &'a self,
        side: &Side,
        name: &str,
    ) -> Option<(&'a String, &'a Deployable)> {
        self.ephemeral.deployable_idx.get(side).and_then(|idx| {
            idx.deployables_by_crates
                .get(name)
                .and_then(|name| idx.deployables_by_name.get(name).map(|dep| (name, dep)))
        })
    }

    pub fn number_troops_deployed(
        &self,
        side: Side,
        name: &str,
    ) -> Result<(usize, Option<GroupId>)> {
        let mut n = 0;
        let mut oldest = None;
        for gid in &self.persisted.troops {
            let group = group!(self, gid)?;
            if let DeployKind::Troop { spec: tr, .. } = &group.origin {
                if group.side == side && name == tr.name.as_str() {
                    if oldest.is_none() {
                        oldest = Some(*gid);
                    }
                    n += 1;
                }
            }
        }
        Ok((n, oldest))
    }

    pub fn number_crates_deployed(&self, st: &SlotStats) -> Result<(usize, Option<GroupId>)> {
        let player = maybe!(self.persisted.players, &st.ucid, "no such player")?;
        let n = player.crates.len();
        let oldest = player.crates.into_iter().next().map(|id| *id);
        Ok((n, oldest))
    }

    pub fn unpakistan(&mut self, lua: MizLua, idx: &MizIndex, slot: &SlotId) -> Result<Unpakistan> {
        #[derive(Clone)]
        struct Cifo {
            pos: Vector2,
            group: GroupId,
            origin: ObjectiveId,
            crate_def: Crate,
        }
        impl<'a> From<NearbyCrate<'a>> for Cifo {
            fn from(nc: NearbyCrate<'a>) -> Self {
                Self {
                    pos: nc.pos,
                    group: nc.group.id,
                    origin: nc.origin,
                    crate_def: nc.crate_def.clone(),
                }
            }
        }
        fn nearby(db: &Db, lua: MizLua, st: &SlotStats, slot: &SlotId) -> Result<SmallVec<[Cifo; 8]>> {
            let nearby_player = db
                .list_nearby_crates(lua, st, slot)?
                .into_iter()
                .filter(|nc| !db.fowl_crate_is_on_ed_bay(lua, nc.group.id))
                .map(Cifo::from)
                .collect::<SmallVec<[Cifo; 8]>>();
            if nearby_player.is_empty() {
                Ok(nearby_player)
            } else {
                let sp = db.ephemeral.cfg.crate_spread as f64;
                let mut crates = FxHashMap::default();
                for cr in &nearby_player {
                    for cr in db
                        .list_crates_near_point(lua, cr.pos, sp)?
                        .into_iter()
                        .filter(|nc| !db.fowl_crate_is_on_ed_bay(lua, nc.group.id))
                        .map(Cifo::from)
                    {
                        crates.entry(cr.group).or_insert(cr);
                    }
                }
                Ok(crates.into_iter().map(|(_, cr)| cr).collect())
            }
        }
        fn buildable(
            nearby: &SmallVec<[Cifo; 8]>,
            didx: &DeployableIndex,
            player_pos: Vector2,
        ) -> std::result::Result<
            FxHashMap<String, FxHashMap<String, Vec<Cifo>>>,
            SmallVec<[CompactString; 2]>,
        > {
            let mut candidates: FxHashMap<String, FxHashMap<String, Vec<Cifo>>> =
                FxHashMap::default();
            let mut reasons = smallvec![];
            for cr in nearby {
                if let Some(dep) = didx.deployables_by_crates.get(&cr.crate_def.name) {
                    candidates
                        .entry(dep.clone())
                        .or_default()
                        .entry(cr.crate_def.name.clone())
                        .or_default()
                        .push(cr.clone());
                }
            }
            candidates.retain(|dep, have| {
                let spec = &didx.deployables_by_name[dep];
                for req in &spec.crates {
                    match have.get_mut(&req.name) {
                        Some(ids) if ids.len() >= req.required as usize => {
                            ids.sort_by(|a, b| {
                                na::distance(&player_pos.into(), &a.pos.into())
                                    .partial_cmp(&na::distance(&player_pos.into(), &b.pos.into()))
                                    .unwrap_or(std::cmp::Ordering::Equal)
                            });
                            ids.truncate(req.required as usize);
                        }
                        Some(_) | None => {
                            reasons
                                .push(format_compact!("can't spawn {dep} missing {}\n", req.name));
                            return false;
                        }
                    }
                }
                true
            });
            if candidates.is_empty() {
                Err(reasons)
            } else {
                Ok(candidates)
            }
        }
        fn crates_cluster_within_spread(
            have: &FxHashMap<String, Vec<Cifo>>,
            max_spread_m: f64,
        ) -> bool {
            let max_dist_sq = max_spread_m.powi(2);
            let mut positions: SmallVec<[Vector2; 8]> = smallvec![];
            for crs in have.values() {
                for cr in crs {
                    positions.push(cr.pos);
                }
            }
            for (i, a) in positions.iter().enumerate() {
                for b in positions.iter().skip(i + 1) {
                    if na::distance_squared(&(*a).into(), &(*b).into()) > max_dist_sq {
                        return false;
                    }
                }
            }
            true
        }
        fn base_repairable(
            db: &Db,
            side: Side,
            nearby: &SmallVec<[Cifo; 8]>,
        ) -> FxHashMap<GroupId, Cifo> {
            let cr = &db.ephemeral.cfg.repair_crate[&side];
            nearby
                .iter()
                .filter(|ci| ci.crate_def.name == cr.name)
                .map(|ci| (ci.group, ci.clone()))
                .collect()
        }
        fn supply_transferrable(
            db: &Db,
            side: Side,
            nearby: &SmallVec<[Cifo; 8]>,
        ) -> SmallVec<[(GroupId, Cifo); 2]> {
            if let Some(whcfg) = db.ephemeral.cfg.warehouse.as_ref() {
                let cr = &whcfg.supply_transfer_crate[&side];
                nearby
                    .iter()
                    .filter(|ci| ci.crate_def.name == cr.name)
                    .map(|ci| (ci.group, ci.clone()))
                    .collect()
            } else {
                smallvec![]
            }
        }
        fn repairable(
            db: &Db,
            nearby: &SmallVec<[Cifo; 8]>,
            didx: &DeployableIndex,
            max_dist: f64,
        ) -> std::result::Result<
            FxHashMap<String, (GroupId, Vec<Cifo>)>,
            SmallVec<[CompactString; 2]>,
        > {
            let mut repairs: FxHashMap<String, (GroupId, Vec<Cifo>)> = FxHashMap::default();
            let mut reasons = smallvec![];
            let max_dist = max_dist.powi(2);
            for cr in nearby {
                if let Some(dep) = didx.deployables_by_repair.get(&cr.crate_def.name) {
                    let mut group_to_repair = None;
                    for gid in &db.persisted.deployed {
                        let group = &db.persisted.groups[gid];
                        match &group.origin {
                            DeployKind::Deployed { spec: d, .. } if d.path.last() == Some(&dep) => {
                                for uid in &group.units {
                                    let unit_pos = db.persisted.units[uid].pos;
                                    if na::distance_squared(&unit_pos.into(), &cr.pos.into())
                                        <= max_dist
                                    {
                                        group_to_repair = Some(*gid);
                                        break;
                                    }
                                }
                                reasons.push(format_compact!("not close enough to repair {dep}"));
                            }
                            DeployKind::Deployed { .. }
                            | DeployKind::Crate { .. }
                            | DeployKind::Objective { .. }
                            | DeployKind::ObjectiveDeprecated
                            | DeployKind::Troop { .. }
                            | DeployKind::CsarPilot { .. }
                            | DeployKind::Action { .. } => (),
                        }
                    }
                    if let Some(gid) = group_to_repair {
                        let (_, crates) =
                            repairs.entry(dep.clone()).or_insert_with(|| (gid, vec![]));
                        crates.push(cr.clone())
                    }
                }
            }
            repairs.retain(|dep, (_, have)| {
                let required = have[0].crate_def.required as usize;
                if have.len() < required {
                    reasons.push(format_compact!("not enough crates to repair {dep}\n"));
                    false
                } else {
                    while have.len() > required {
                        have.pop();
                    }
                    true
                }
            });
            if repairs.is_empty() {
                Err(reasons)
            } else {
                Ok(repairs)
            }
        }
        fn too_close<'a, I: Iterator<Item = &'a Cifo>, F: Fn() -> I>(
            db: &Db,
            side: Side,
            centroid: Vector2,
            logistics: bool,
            iter: F,
        ) -> bool {
            db.persisted.objectives.into_iter().any(|(oid, obj)| {
                let mut check = false;
                for cr in iter() {
                    check |= oid == &cr.origin;
                }
                check |= logistics || obj.owner == side;
                check && (logistics || obj.threatened) && {
                    let excl_dist_sq = (db
                        .ephemeral
                        .cfg
                        .logistics_exclusion_for(&obj.kind) as f64)
                        .powi(2);
                    let dist = na::distance_squared(&obj.zone.pos().into(), &centroid.into());
                    dist <= excl_dist_sq || obj.zone.scale(1.1).contains(centroid.into())
                }
            })
        }
        struct UnpackBaseDistanceViolation {
            name: CompactString,
            dist_m: f64,
            min_m: u32,
        }
        fn too_close_to_friendly_base_for_unpack(
            db: &Db,
            side: Side,
            centroid: Vector2,
        ) -> Option<UnpackBaseDistanceViolation> {
            let min_m = db.ephemeral.cfg.deployable_unpack_min_base_distance_m;
            if min_m == 0 {
                return None;
            }
            let min_sq = (min_m as f64).powi(2);
            let mut nearest: Option<UnpackBaseDistanceViolation> = None;
            for (_, obj) in db.persisted.objectives.into_iter() {
                // Only live friendly bases; capturable (logi 0 / white ring) and enemy/neutral skip.
                if obj.owner != side || obj.threatened || obj.captureable() {
                    continue;
                }
                let dist_sq = na::distance_squared(&obj.zone.pos().into(), &centroid.into());
                if dist_sq >= min_sq {
                    continue;
                }
                let dist_m = dist_sq.sqrt();
                let replace = match &nearest {
                    None => true,
                    Some(cur) => dist_m < cur.dist_m,
                };
                if replace {
                    nearest = Some(UnpackBaseDistanceViolation {
                        name: CompactString::from(obj.name.as_str()),
                        dist_m,
                        min_m,
                    });
                }
            }
            nearest
        }
        fn unpack_base_distance_reason(v: &UnpackBaseDistanceViolation) -> CompactString {
            format_compact!(
                "Deployables must be at least {:.1} km from friendly base {} (nearest center {:.1} km)",
                v.min_m as f64 / 1000.,
                v.name,
                v.dist_m / 1000.
            )
        }
        fn close_enough_to_repair<'a, I: Iterator<Item = &'a Cifo>, F: Fn() -> I>(
            db: &Db,
            side: Side,
            centroid: Vector2,
            iter: F,
        ) -> Option<ObjectiveId> {
            db.persisted.objectives.into_iter().find_map(|(oid, obj)| {
                let mut is_origin = false;
                for cr in iter() {
                    is_origin |= oid == &cr.origin;
                }
                if obj.owner == side
                    && !obj.captureable()
                    && !matches!(obj.kind, ObjectiveKind::Production)
                    && !is_origin
                    && obj.zone.contains(centroid)
                {
                    Some(*oid)
                } else {
                    None
                }
            })
        }
        fn close_enough_to_production_repair<'a, I: Iterator<Item = &'a Cifo>, F: Fn() -> I>(
            db: &Db,
            side: Side,
            centroid: Vector2,
            iter: F,
        ) -> Option<ObjectiveId> {
            db.persisted.objectives.into_iter().find_map(|(oid, obj)| {
                let mut is_origin = false;
                for cr in iter() {
                    is_origin |= oid == &cr.origin;
                }
                if matches!(obj.kind, ObjectiveKind::Production)
                    && obj.owner == side
                    && !is_origin
                    && obj.zone.contains(centroid)
                {
                    Some(*oid)
                } else {
                    None
                }
            })
        }
        fn compute_positions(
            have: &FxHashMap<String, Vec<Cifo>>,
            centroid: Vector2,
            group_heading: f64,
        ) -> Result<SpawnLoc> {
            let mut num_by_typ: FxHashMap<String, usize> = FxHashMap::default();
            let mut pos_by_typ: FxHashMap<String, Vector2> = FxHashMap::default();
            for cr in have.iter().flat_map(|(_, cr)| cr.iter()) {
                if let Some(typ) = cr.crate_def.pos_unit.as_ref() {
                    *pos_by_typ.entry(typ.clone()).or_default() += cr.pos;
                    *num_by_typ.entry(typ.clone()).or_default() += 1;
                }
            }
            for (typ, pos) in pos_by_typ.iter_mut() {
                if let Some(n) = num_by_typ.get(typ) {
                    *pos /= *n as f64
                }
            }
            let spawnloc = if pos_by_typ.is_empty() {
                SpawnLoc::AtPos {
                    pos: centroid,
                    offset_direction: Vector2::default(),
                    group_heading,
                }
            } else {
                SpawnLoc::AtPosWithComponents {
                    pos: centroid,
                    group_heading,
                    component_pos: pos_by_typ,
                }
            };
            Ok(spawnloc)
        }
        fn enforce_deploy_limits(
            db: &mut Db,
            side: Side,
            spec: &Deployable,
            dep: &String,
            origin: ObjectiveId,
            ucid: &Ucid,
        ) -> Result<ObjectiveId> {
            if let Some(player) = db.persisted.players.get(ucid)
                && let Some(obj) = db.persisted.objectives.get(&origin)
            {
                let player_points = max(0, player.points);
                if spec.cost as i32 > player_points + obj.points {
                    bail!(
                        "there are {} available points, this deployable costs {} points to unpack",
                        player_points,
                        spec.cost
                    )
                }
            }
            let (n, oldest) = db.number_deployed(side, &**dep)?;
            if n >= spec.limit as usize {
                match spec.limit_enforce {
                    LimitEnforceTyp::DenyCrate => {
                        bail!("the max number of {:?} are already deployed", dep)
                    }
                    LimitEnforceTyp::DeleteOldest => match oldest {
                        Some(Oldest::Group(gid)) => db.delete_group(&gid)?,
                        Some(Oldest::Objective(oid)) => db.delete_objective(&oid)?,
                        None => (),
                    },
                }
            }
            Ok(origin)
        }
        fn require_repair_cost_points(
            db: &Db,
            ucid: &Ucid,
            oid: ObjectiveId,
            cost_points: i32,
            action: &str,
        ) -> Result<()> {
            if cost_points >= 0 {
                return Ok(());
            }
            let cost = cost_points.unsigned_abs();
            let Some(player) = db.persisted.players.get(ucid) else {
                return Ok(());
            };
            let Some(obj) = db.persisted.objectives.get(&oid) else {
                return Ok(());
            };
            let available = max(0, player.points) + obj.points;
            if available < cost as i32 {
                bail!(
                    "there are {} points available, {} costs {} points",
                    available,
                    action,
                    cost
                );
            }
            Ok(())
        }
        fn charge_repair_cost(
            db: &mut Db,
            ucid: &Ucid,
            oid: ObjectiveId,
            cost_points: i32,
            msg: &str,
        ) {
            if cost_points < 0 {
                db.charge_for_item(
                    ucid,
                    oid,
                    cost_points.unsigned_abs(),
                    msg,
                    InvestBucket::Ground,
                );
            }
        }
        fn award_repair_points(db: &mut Db, ucid: &Ucid, cost_points: i32, msg: &str) {
            if cost_points > 0 {
                db.adjust_points(ucid, cost_points, msg);
            }
        }
        let st = SlotStats::get(self, lua, slot)?;
        if st.in_air {
            bail!("you must land to unpack crates")
        }
        let max_dist = self.ephemeral.cfg.crate_spread as f64;
        let nearby = nearby(self, lua, &st, slot)?;
        let didx = Arc::clone(
            self.ephemeral
                .deployable_idx
                .get(&st.side)
                .ok_or_else(|| anyhow!("{:?} can't deploy anything", st.side))?,
        );
        if nearby.is_empty() {
            let any_ground = self.list_nearby_crates(lua, &st, slot)?;
            if any_ground
                .iter()
                .any(|nc| self.fowl_crate_is_on_ed_bay(lua, nc.group.id))
            {
                bail!("unload crates from the aircraft before unpacking");
            }
            bail!("no nearby crates")
        }
        let mut reasons: SmallVec<[CompactString; 2]> = smallvec![];
        let base_repairs = base_repairable(self, st.side, &nearby);
        let supply_transfer = supply_transferrable(self, st.side, &nearby);
        if !base_repairs.is_empty() {
            let centroid = centroid2d(base_repairs.iter().map(|(_, c)| c.pos));
            let oid = close_enough_to_repair(self, st.side, centroid, || {
                base_repairs.iter().map(|(_, c)| c)
            });
            if let Some(oid) = oid {
                let obj = objective!(self, oid)?;
                if obj.spawnable_logi_repaired
                    && obj.static_repair_need == 0
                    && obj.logi == 100
                {
                    reasons.push("objective logistics are completely repaired".into());
                } else if !obj.spawnable_logi_repaired {
                    if let Some(amount) = self
                        .ephemeral
                        .cfg
                        .points
                        .as_ref()
                        .map(|p| p.logistics_repair)
                    {
                        require_repair_cost_points(
                            self,
                            &st.ucid,
                            oid,
                            amount,
                            "logistics repair",
                        )?;
                        charge_repair_cost(
                            self,
                            &st.ucid,
                            oid,
                            amount,
                            "for logistics repair",
                        );
                    }
                    self.repair_one_logi_step(st.side, Utc::now(), oid)?;
                    self.delete_group(base_repairs.keys().next().unwrap())?;
                    self.ephemeral.stat(Stat::Repair {
                        id: oid,
                        by: st.ucid,
                    });
                    if let Some(amount) = self
                        .ephemeral
                        .cfg
                        .points
                        .as_ref()
                        .map(|p| p.logistics_repair)
                    {
                        award_repair_points(self, &st.ucid, amount, "for logistics repair");
                    }
                    let obj = objective!(self, oid)?;
                    return Ok(Unpakistan::RepairedBase(obj.name.clone(), obj.logi()));
                } else if obj.can_queue_static_repair_crate() {
                    let cost = self.ephemeral.cfg.static_repair_crate_cost;
                    require_repair_cost_points(
                        self,
                        &st.ucid,
                        oid,
                        cost,
                        "static repair",
                    )?;
                    charge_repair_cost(
                        self,
                        &st.ucid,
                        oid,
                        cost,
                        "for static repair crate queue",
                    );
                    let rate = self.ephemeral.cfg.static_repair_rate_seconds;
                    let now = Utc::now();
                    let obj = objective_mut!(self, oid)?;
                    if obj.static_repair == 0 {
                        obj.static_repair_due =
                            now + chrono::Duration::seconds(rate.max(1) as i64);
                    }
                    obj.static_repair += 1;
                    self.delete_group(base_repairs.keys().next().unwrap())?;
                    self.ephemeral.stat(Stat::Repair {
                        id: oid,
                        by: st.ucid,
                    });
                    award_repair_points(
                        self,
                        &st.ucid,
                        cost,
                        "for static repair crate queue",
                    );
                    let obj = objective!(self, oid)?;
                    return Ok(Unpakistan::RepairedStaticQueue(
                        obj.name.clone(),
                        obj.static_repair,
                        obj.static_repair_need,
                    ));
                } else if obj.static_repair_need > 0 {
                    reasons.push(
                        "static repair queue is full (unload more repair crates)".into(),
                    );
                } else {
                    reasons.push("objective logistics are completely repaired".into());
                }
            } else {
                reasons.push("not close enough to a friendly objective".into());
            }
        }
        if !base_repairs.is_empty() {
            let centroid = centroid2d(base_repairs.iter().map(|(_, c)| c.pos));
            if let Some(oid) = close_enough_to_production_repair(self, st.side, centroid, || {
                base_repairs.iter().map(|(_, c)| c)
            }) {
                let obj = objective!(self, oid)?;
                if obj.production >= 100 {
                    reasons.push("OPR production is already at 100%".into());
                } else if !obj.can_queue_production_repair_crate() {
                    reasons.push(
                        "repair queue is full or would exceed OPR damage (100% - Production)"
                            .into(),
                    );
                } else {
                    let cost = self.ephemeral.cfg.production_repair_crate_cost;
                    require_repair_cost_points(
                        self,
                        &st.ucid,
                        oid,
                        cost,
                        "OPR production repair",
                    )?;
                    charge_repair_cost(
                        self,
                        &st.ucid,
                        oid,
                        cost,
                        "for OPR production repair crate",
                    );
                    let rate = self.ephemeral.cfg.production_repair_rate_seconds;
                    let now = Utc::now();
                    let obj = objective_mut!(self, oid)?;
                    if obj.production_repair == 0 {
                        obj.production_repair_due = now
                            + chrono::Duration::seconds(rate.max(1) as i64);
                    }
                    obj.production_repair += 1;
                    self.delete_group(base_repairs.keys().next().unwrap())?;
                    self.ephemeral.stat(Stat::Repair {
                        id: oid,
                        by: st.ucid,
                    });
                    award_repair_points(
                        self,
                        &st.ucid,
                        cost,
                        "for OPR production repair crate",
                    );
                    let obj = objective!(self, oid)?;
                    return Ok(Unpakistan::RepairedProduction(
                        obj.name.clone(),
                        obj.production,
                    ));
                }
            }
        }
        if !supply_transfer.is_empty() {
            let centroid = centroid2d(supply_transfer.iter().map(|(_, c)| c.pos));
            let oid = close_enough_to_repair(self, st.side, centroid, || {
                base_repairs.iter().map(|(_, c)| c)
            });
            if let Some(to) = oid {
                let (gid, _) = supply_transfer.into_iter().next().unwrap();
                if let DeployKind::Crate {
                    origin: from,
                    player: _,
                    spec: _,
                    ..
                } = self.persisted.groups[&gid].origin
                {
                    self.transfer_supplies(lua, from, to)?;
                    self.delete_group(&gid)?;
                    self.ephemeral.stat(Stat::SupplyTransfer {
                        from,
                        to,
                        by: st.ucid,
                    });
                    if let Some(amount) = self
                        .ephemeral
                        .cfg
                        .points
                        .as_ref()
                        .map(|p| p.logistics_transfer)
                    {
                        self.adjust_points(&st.ucid, amount as i32, "for supply transfer");
                    }
                    return Ok(Unpakistan::TransferedSupplies(
                        objective!(self, from)?.name.clone(),
                        objective!(self, to)?.name.clone(),
                    ));
                }
            } else {
                reasons.push("not close enough to a friendly objective".into());
            }
        }
        match buildable(&nearby, &didx, st.point) {
            Err(mut build_reasons) => reasons.append(&mut build_reasons),
            Ok(mut candidates) => {
                let (dep, have) = candidates.drain().next().unwrap();
                let spec = maybe!(didx.deployables_by_name, dep, "deployable")?.clone();
                let cluster_spread = self.ephemeral.cfg.crate_spread as f64;
                if !crates_cluster_within_spread(&have, cluster_spread) {
                    reasons.push(format_compact!(
                        "can't spawn {dep}: required crates must be within {} m of each other",
                        cluster_spread as u32
                    ));
                } else {
                let centroid = centroid2d(have.values().flat_map(|c| c.iter()).map(|c| c.pos));
                let too_close =
                    too_close(self, st.side, centroid, spec.kind.is_objective(), || {
                        have.values().flat_map(|c| c.iter())
                    });
                if too_close {
                    if spec.kind.is_group() {
                        reasons.push("can't unpack that here while enemies are close".into());
                    } else {
                        reasons.push("can't unpack that here".into())
                    }
                } else if spec.kind.is_group()
                    && let Some(v) =
                        too_close_to_friendly_base_for_unpack(self, st.side, centroid)
                {
                    reasons.push(unpack_base_distance_reason(&v));
                } else {
                    let spctx = SpawnCtx::new(lua)?;
                    let origins = {
                        let mut oids = have
                            .values()
                            .flat_map(|crs| crs.iter())
                            .map(|cr| cr.origin)
                            .collect::<SmallVec<[_; 8]>>();
                        oids.sort();
                        oids.dedup();
                        oids
                    };
                    let can_deploy = origins.iter().fold(Err(anyhow!("")), |res, oid| match res {
                        Ok(oid) => Ok(oid),
                        Err(_) => enforce_deploy_limits(self, st.side, &spec, &dep, *oid, &st.ucid),
                    });
                    match can_deploy {
                        Err(e) => reasons.push(format_compact!("{e}")),
                        Ok(from_obj) => match &spec.kind {
                            DeployableKind::Objective(parts) => {
                                match self.ground_farp_site_clear_of_water(
                                    &spctx, idx, st.side, centroid, &spec, parts,
                                ) {
                                    Err(e) => reasons.push(format_compact!("{e}")),
                                    Ok(()) => {
                                        match self.add_farp(
                                            lua, &spctx, idx, st.side, centroid, &spec, parts,
                                        ) {
                                            Err(e) => reasons.push(format_compact!("{e}")),
                                            Ok(oid) => {
                                                for cr in have.values().flat_map(|c| c.iter()) {
                                                    self.delete_group(&cr.group)?
                                                }
                                                self.ephemeral.stat(Stat::DeployFarp {
                                                    oid,
                                                    by: st.ucid,
                                                    deployable: dep,
                                                });
                                                self.charge_for_item(
                                                    &st.ucid,
                                                    from_obj,
                                                    spec.cost,
                                                    "for farp spawn",
                                                    InvestBucket::Ground,
                                                );
                                                let name = objective!(self, oid)?.name.clone();
                                                return Ok(Unpakistan::UnpackedFarp(
                                                    name,
                                                    spec.spawn_delay_secs,
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                            DeployableKind::Group { template } => {
                                let pos = self.ephemeral.slot_instance_pos(lua, slot)?;
                                let spawnloc =
                                    compute_positions(&have, centroid, azumith3d(pos.x.0))?;
                                let origin = DeployKind::Deployed {
                                    player: st.ucid.clone(),
                                    moved_by: None,
                                    spec: spec.clone(),
                                    cost_fraction: 1.,
                                    origin: Some(from_obj),
                                };
                                // Unique vs ME template (`{template}-{gid}`); never bare template name.
                                let delay = if spec.spawn_delay_secs == 0 {
                                    None
                                } else {
                                    Some(
                                        Utc::now()
                                            + chrono::Duration::seconds(
                                                spec.spawn_delay_secs as i64,
                                            ),
                                    )
                                };
                                let gid = self.add_and_queue_group(
                                    &spctx,
                                    idx,
                                    st.side,
                                    spawnloc,
                                    template.as_str(),
                                    origin,
                                    BitFlags::empty(),
                                    delay,
                                    None,
                                    None,
                                )?;
                                for cr in have.values().flat_map(|c| c.iter()) {
                                    self.delete_group(&cr.group)?
                                }
                                self.ephemeral.stat(Stat::DeployGroup {
                                    gid,
                                    by: st.ucid,
                                    deployable: dep.clone(),
                                });
                                let invest = deployable_invest_bucket(template, &self.ephemeral.cfg);
                                let frac = self.charge_for_item(
                                    &st.ucid,
                                    from_obj,
                                    spec.cost,
                                    &format_compact!("for {dep} unpack"),
                                    invest,
                                );
                                if let DeployKind::Deployed { cost_fraction, .. } =
                                    &mut self.persisted.groups[&gid].origin
                                {
                                    *cost_fraction = frac;
                                }
                                return Ok(Unpakistan::Unpacked(dep, spec.spawn_delay_secs));
                            }
                        },
                    }
                }
                }
            }
        }
        match repairable(self, &nearby, &didx, max_dist) {
            Err(mut rep_reasons) => reasons.append(&mut rep_reasons),
            Ok(mut candidates) => {
                let (dep, (gid, have)) = candidates.drain().next().unwrap();
                let spec = maybe!(didx.deployables_by_name, dep, "deployable")?.clone();
                let player = maybe!(self.persisted.players, &st.ucid, "player")?;
                let centroid = centroid2d(have.iter().map(|c| c.pos));
                if spec.repair_cost > 0 && spec.repair_cost as i32 > player.points {
                    reasons.push(format_compact!(
                        "Repairing {dep} costs {}, you have {}",
                        spec.repair_cost,
                        player.points
                    ));
                } else if too_close(self, st.side, centroid, false, || have.iter()) {
                    reasons.push("can't repair that here while enemies are close".into())
                } else if let Some(v) =
                    too_close_to_friendly_base_for_unpack(self, st.side, centroid)
                {
                    reasons.push(unpack_base_distance_reason(&v));
                } else {
                    let group = group!(self, gid)?;
                    for uid in &group.units {
                        let unit = unit_mut!(self, uid)?;
                        unit.dead = false;
                    }
                    for cr in &have {
                        self.delete_group(&cr.group)?
                    }
                    self.ephemeral.push_spawn(gid);
                    if spec.repair_cost > 0 {
                        self.adjust_points(
                            &st.ucid,
                            -(spec.repair_cost as i32),
                            &format_compact!("for {dep} repair"),
                        );
                    }
                    self.ephemeral.dirty();
                    return Ok(Unpakistan::Repaired(dep));
                }
            }
        }
        bail!(
            reasons
                .into_iter()
                .fold(CompactString::new(""), |mut acc, r| {
                    if acc.is_empty() {
                        acc.push_str(r.as_str());
                    } else {
                        acc.push('\n');
                        acc.push_str(r.as_str());
                    }
                    acc
                })
        )
    }

    pub fn unload_crate(&mut self, lua: MizLua, idx: &MizIndex, slot: &SlotId) -> Result<Crate> {
        let st = SlotStats::get(self, lua, slot)?;
        let cargo = self.ephemeral.cargo.get(slot);
        if cargo.map(|c| c.crates.is_empty()).unwrap_or(true) {
            bail!("no crates onboard")
        }
        let unit = self.ephemeral.slot_instance_unit(lua, slot)?;
        ensure_cargo_bay_ready(&unit)?;
        let cargo = self.ephemeral.cargo.get_mut(slot).unwrap();
        let (oid, crate_cfg) = cargo.crates.pop().unwrap();
        let weight = cargo.weight();
        if st.in_air && st.speed > crate_cfg.max_drop_speed as f64 {
            let max_sp = (crate_cfg.max_drop_speed * 3600) / 1000;
            let max_al = crate_cfg.max_drop_height_agl;
            cargo.crates.push((oid, crate_cfg));
            bail!(
                "you are going too fast to unload your cargo, speed must be at or below {} km/h, and altitude agl must be at or below {} m",
                max_sp,
                max_al
            )
        }
        if st.in_air && st.agl > crate_cfg.max_drop_height_agl as f64 {
            let max_sp = (crate_cfg.max_drop_speed * 3600) / 1000;
            let max_al = crate_cfg.max_drop_height_agl;
            cargo.crates.push((oid, crate_cfg));
            bail!(
                "you are too high to unload your cargo, altitude agl must be at or below {} m, and speed must be at or below {} km/h",
                max_al,
                max_sp
            )
        }
        Trigger::singleton(lua)?
            .action()?
            .set_unit_internal_cargo(st.name, weight)?;
        let template = self
            .ephemeral
            .cfg
            .crate_template
            .get(&st.side)
            .ok_or_else(|| anyhow!("missing crate template for {:?}", st.side))?
            .clone();
        let dir = {
            let mut d = Vector2::new(st.pos.x.x, st.pos.x.z);
            let mag = (d.x * d.x + d.y * d.y).sqrt();
            if mag > 1e-3 {
                d / mag
            } else {
                Vector2::new(1., 0.)
            }
        };
        let group_heading = azumith3d(st.pos.x.0);
        let spacing = Self::fowl_crate_spacing_m(lua, None);
        let nose_m = self
            .ephemeral
            .slot_info
            .get(slot)
            .map(|s| crate::db::dynamic_cargo::fowl_crate_nose_distance_m(s.typ.as_str()))
            .unwrap_or(25.);
        let drop_pos =
            self.fowl_crate_nose_line_pos(lua, st.point, dir, None, spacing, nose_m, &[]);
        // Deck path only when standing on a friendly logistics hub (never crate origin/carrier fallback).
        let near_logi = self.point_near_logistics(st.side, st.point).ok();
        let (spawnpos, ship_hub, ship_offsets, deck_alt) = match near_logi {
            Some((link_oid, _)) => {
                match crate::db::ai_air::try_ship_crate_at_world_pos(
                    lua,
                    self,
                    link_oid,
                    drop_pos,
                    group_heading,
                    st.pos.p.y,
                ) {
                    Ok(Some((deck_spawn, altitude, offsets))) => (
                        SpawnLoc::AtPosOnShip {
                            pos: deck_spawn,
                            group_heading,
                            altitude,
                        },
                        Some(link_oid),
                        Some(offsets),
                        Some(altitude),
                    ),
                    Ok(None) | Err(_) => {
                        match crate::db::ai_air::resolve_ship_crate_deck_spawn(
                            lua,
                            self,
                            link_oid,
                            st.point,
                            dir,
                            group_heading,
                            st.pos.p.y,
                        ) {
                            Ok(Some((deck_spawn, altitude, offsets))) => (
                                SpawnLoc::AtPosOnShip {
                                    pos: deck_spawn,
                                    group_heading,
                                    altitude,
                                },
                                Some(link_oid),
                                Some(offsets),
                                Some(altitude),
                            ),
                            Ok(None) | Err(_) => (
                                SpawnLoc::AtPos {
                                    pos: drop_pos,
                                    offset_direction: Vector2::new(0., 0.),
                                    group_heading,
                                },
                                None,
                                None,
                                None,
                            ),
                        }
                    }
                }
            }
            None => (
                SpawnLoc::AtPos {
                    pos: drop_pos,
                    offset_direction: Vector2::new(0., 0.),
                    group_heading,
                },
                None,
                None,
                None,
            ),
        };
        let dk = DeployKind::Crate {
            origin: oid,
            player: st.ucid,
            spec: crate_cfg.clone(),
            ship_hub,
            ship_offsets,
            ed_carrier: None,
        };
        let spctx = SpawnCtx::new(lua)?;
        let gid = match self.add_and_queue_group(
            &spctx,
            idx,
            st.side,
            spawnpos,
            &template,
            dk,
            BitFlags::empty(),
            None,
            None,
            None,
        ) {
            Ok(gid) => gid,
            Err(e) => {
                self.ephemeral
                    .cargo
                    .get_mut(slot)
                    .unwrap()
                    .crates
                    .push((oid, crate_cfg));
                return Err(e);
            }
        };
        if let Some(deck_alt) = deck_alt {
            if let Err(e) = self.spawn_and_validate_ship_crate(lua, idx, gid, deck_alt) {
                self.ephemeral
                    .cargo
                    .get_mut(slot)
                    .unwrap()
                    .crates
                    .push((oid, crate_cfg));
                return Err(e);
            }
        }
        Ok(crate_cfg)
    }

    pub fn unit_cargo_cfg(&self, slot: &SlotId) -> Result<(CargoConfig, Side, String)> {
        let si = self
            .ephemeral
            .get_slot_info(slot)
            .ok_or_else(|| anyhow!("no such slot"))?;
        let side = si.side;
        let unit_name = si.unit_name.clone();
        let cargo_capacity = self.cargo_capacity(&si.typ)?;
        Ok((cargo_capacity, side, unit_name))
    }

    pub fn load_nearby_crate(&mut self, lua: MizLua, slot: &SlotId) -> Result<Crate> {
        let st = SlotStats::get(self, lua, slot)?;
        let unit = self.ephemeral.slot_instance_unit(lua, slot)?;
        ensure_cargo_bay_ready(&unit)?;
        let (cargo_capacity, side, unit_name) = self.unit_cargo_cfg(slot)?;
        let ucid = self.ephemeral.player_in_slot(slot).cloned();
        let ed_crates = ucid
            .as_ref()
            .map(|_| self.fowl_crates_on_ed_bay(lua, slot).len())
            .unwrap_or(0);
        let cargo = self.ephemeral.cargo.entry(slot.clone()).or_default();
        let crates = cargo.num_crates().saturating_add(ed_crates);
        if cargo_capacity.crate_slots as usize <= crates
            || cargo_capacity.total_slots as usize * 2
                < cargo.total_half_slots() + ed_crates * 2 + 2
        {
            bail!("you already have a full load onboard")
        }
        let (gid, oid, crate_def) = {
            let mut nearby = self.list_nearby_crates(lua, &st, slot)?;
            nearby.retain(|nc| nc.group.side == side && !nc.loaded);
            if nearby.is_empty() {
                bail!(
                    "no friendly crates within {} meters of the cargo ramp",
                    self.ephemeral.cfg.crate_load_distance
                );
            }
            let the_crate = nearby.first().unwrap();
            let gid = the_crate.group.id;
            let crate_def = the_crate.crate_def.clone();
            let oid = the_crate.origin;
            (gid, oid, crate_def)
        };
        let cargo = self.ephemeral.cargo.get_mut(slot).unwrap();
        cargo.crates.push((oid, crate_def.clone()));
        let weight = cargo.weight();
        self.delete_group(&gid)?;
        Trigger::singleton(lua)?
            .action()?
            .set_unit_internal_cargo(unit_name, weight as i64)?;
        if let Some(ucid) = ucid {
            info!(
                "crate load: {ucid:?} slot {slot:?} loaded {} ({gid})",
                crate_def.name
            );
        }
        Ok(crate_def)
    }

    /// F10 unload for shared-ED bay airframes (Mi-8 / C-130 / etc.).
    pub fn unload_ed_bay_crate(&mut self, lua: MizLua, slot: &SlotId) -> Result<String> {
        let st = SlotStats::get(self, lua, slot)?;
        if st.in_air {
            bail!("you must land to unload crates")
        }
        let unit = self.ephemeral.slot_instance_unit(lua, slot)?;
        ensure_cargo_bay_ready(&unit)?;
        let ucid = self
            .ephemeral
            .player_in_slot(slot)
            .cloned()
            .ok_or_else(|| anyhow!("no player in slot"))?;
        let gids = self.fowl_crate_gids_on_ed_bay(lua, slot);
        if gids.is_empty() {
            bail!("no Fowl crates in the cargo bay")
        }
        let gid = gids[0];
        let uid = self
            .persisted
            .groups
            .get(&gid)
            .and_then(|g| g.units.clone().into_iter().next().copied())
            .ok_or_else(|| anyhow!("no unit for crate group {gid}"))?;
        let name = self
            .persisted
            .units
            .get(&uid)
            .map(|u| u.name.clone())
            .unwrap_or_else(|| String::from(format_compact!("crate {gid}")));
        if self
            .ephemeral
            .shared_ed_eject_pending_place
            .contains_key(&gid)
        {
            bail!("{name} is still clearing from the bay — wait a few seconds")
        }
        match self.place_fowl_crate_on_ed_unload_line(lua, gid, uid, &ucid, &[]) {
            Ok(_) => {
                info!("crate unload: {ucid:?} slot {slot:?} placed {name} ({gid}) on unload line");
                Ok(name)
            }
            Err(e) => {
                warn!("crate unload failed for {ucid:?} slot {slot:?} {name} ({gid}): {e:?}");
                Err(e)
            }
        }
    }

    pub fn load_troops(
        &mut self,
        lua: MizLua,
        slot: &SlotId,
        name: &str,
    ) -> Result<(Troop, ObjectiveId)> {
        let (cargo_capacity, side, unit_name) = self.unit_cargo_cfg(slot)?;
        let pos = self.ephemeral.slot_instance_pos(lua, slot)?;
        let point = Vector2::new(pos.p.x, pos.p.z);
        let (origin, _) = self.point_near_logistics(side, point)?;
        let troop_cfg = self
            .ephemeral
            .deployable_idx
            .get(&side)
            .and_then(|idx| idx.squads_by_name.get(name))
            .ok_or_else(|| anyhow!("no such squad {name}"))?
            .clone();
        let ucid = self
            .ephemeral
            .player_in_slot(slot)
            .cloned()
            .ok_or_else(|| anyhow!("can't find player in slot {slot:?}"))?;
        if self.ephemeral.cfg.points.is_some() {
            if let Some(player) = self.persisted.players.get(&ucid)
                && let Some(obj) = self.persisted.objectives.get(&origin)
            {
                let points = max(0, player.points) + obj.points;
                if troop_cfg.cost > 0 && points < troop_cfg.cost as i32 {
                    bail!(
                        "there are {} points available, this troop costs {} points",
                        points,
                        troop_cfg.cost
                    )
                }
            }
        }
        let (crates, _) = self.fowl_crate_and_troop_slot_usage_with_bay(lua, slot, &ucid);
        {
            let cargo = self.ephemeral.cargo.entry(slot.clone()).or_default();
            if cargo.troop_half_slots() + 2 > cargo_capacity.troop_slots as usize * 2
                || cargo.troop_half_slots() + crates * 2 + 2
                    > cargo_capacity.total_slots as usize * 2
            {
                bail!("you already have a full load onboard")
            }
        }
        let cost_fraction = self.charge_for_item(
            &ucid,
            origin,
            troop_cfg.cost,
            &format_compact!("for {name} troop"),
            InvestBucket::Ground,
        );
        let cargo = self.ephemeral.cargo.entry(slot.clone()).or_default();
        cargo.troops.push(InternalTroop {
            player: ucid,
            origin: Some(origin),
            cost_fraction,
            troop: troop_cfg.clone(),
        });
        Trigger::singleton(lua)?
            .action()?
            .set_unit_internal_cargo(unit_name, cargo.weight() as i64)?;
        Ok((troop_cfg, origin))
    }

    pub fn unload_troops(
        &mut self,
        lua: MizLua,
        idx: &MizIndex,
        slot: &SlotId,
    ) -> Result<(Troop, GroupId, Option<ObjectiveId>)> {
        let cargo = self.ephemeral.cargo.get(slot);
        if cargo.map(|c| c.troops.is_empty()).unwrap_or(true) {
            bail!("no troops onboard")
        }
        let unit = self.ephemeral.slot_instance_unit(lua, slot)?;
        if unit.in_air()? {
            bail!("you must land to unload troops")
        }
        let unit_name = unit.get_name()?;
        let side = self
            .ephemeral
            .get_slot_info(slot)
            .ok_or_else(|| anyhow!("no slot info for {slot:?}"))?
            .side;
        let pos = unit.get_position()?;
        let oid = Db::objective_near_point(
            &self.persisted.objectives,
            Vector2::new(pos.p.0.x, pos.p.0.z),
            |_| true,
        )
        .map(|(_, _, o)| o.id);
        let point = Vector2::new(pos.p.x, pos.p.z);
        match self.point_near_logistics(side, point) {
            Ok((_, obj)) if obj.threatened => {
                bail!("you can't deploy troops here while enemies are near")
            }
            Ok(_) | Err(_) => (),
        }
        let cargo = self.ephemeral.cargo.get(slot).unwrap();
        let it = cargo.troops.last().unwrap();
        let (n, oldest) = self.number_troops_deployed(side, it.troop.name.as_str())?;
        let to_delete = if n < it.troop.limit as usize {
            None
        } else {
            match it.troop.limit_enforce {
                LimitEnforceTyp::DeleteOldest => oldest,
                LimitEnforceTyp::DenyCrate => {
                    bail!(
                        "the maximum number of {} troops are already deployed",
                        it.troop.name
                    )
                }
            }
        };
        let dir = Vector2::new(pos.x.x, pos.x.z);
        let group_heading = azumith3d(pos.x.0);
        let (spawnpos, ship_hub, ship_offsets) =
            match self.point_near_logistics(side, point) {
                Ok((oid, _)) => match crate::db::ai_air::resolve_ship_crate_deck_spawn(
                    lua,
                    self,
                    oid,
                    point,
                    dir,
                    group_heading,
                    pos.p.y,
                )? {
                    Some((deck_spawn, altitude, offsets)) => (
                        SpawnLoc::AtPosOnShip {
                            pos: deck_spawn,
                            group_heading,
                            altitude,
                        },
                        Some(oid),
                        Some(offsets),
                    ),
                    None => (
                        SpawnLoc::AtPos {
                            pos: point,
                            offset_direction: dir,
                            group_heading,
                        },
                        None,
                        None,
                    ),
                },
                Err(_) => (
                    SpawnLoc::AtPos {
                        pos: point,
                        offset_direction: dir,
                        group_heading,
                    },
                    None,
                    None,
                ),
            };
        let cargo = self.ephemeral.cargo.get_mut(slot).unwrap();
        let it = cargo.troops.pop().unwrap();
        Trigger::singleton(lua)?
            .action()?
            .set_unit_internal_cargo(unit_name, cargo.weight())?;
        let dk = DeployKind::Troop {
            player: it.player,
            moved_by: None,
            spec: it.troop.clone(),
            origin: it.origin,
            cost_fraction: it.cost_fraction,
            ship_hub,
            ship_offsets,
        };
        let spctx = SpawnCtx::new(lua)?;
        if let Some(gid) = to_delete {
            self.delete_group(&gid)?
        }
        match self.add_and_queue_group(
            &spctx,
            idx,
            side,
            spawnpos,
            &*it.troop.template,
            dk,
            BitFlags::empty(),
            None,
            None,
            None,
        ) {
            Ok(gid) => {
                self.ephemeral.stat(Stat::DeployTroop {
                    gid,
                    troop: it.troop.name.clone(),
                    by: it.player,
                });
                self.try_start_csar_capture(lua, gid, side, point);
                Ok((it.troop, gid, oid))
            }
            Err(e) => {
                self.ephemeral.cargo.get_mut(slot).unwrap().troops.push(it);
                Err(e)
            }
        }
    }

    pub fn return_troops(&mut self, lua: MizLua, slot: &SlotId) -> Result<Troop> {
        let cargo = self.ephemeral.cargo.get(slot);
        if cargo.map(|c| c.troops.is_empty()).unwrap_or(true) {
            bail!("no troops onboard")
        }
        let unit = self.ephemeral.slot_instance_unit(lua, slot)?;
        if unit.in_air()? {
            bail!("you must land to return your troops")
        }
        let unit_name = unit.get_name()?;
        let side = self
            .ephemeral
            .get_slot_info(slot)
            .ok_or_else(|| anyhow!("no slot info for {slot:?}"))?
            .side;
        let pos = unit.get_position()?;
        let point = Vector2::new(pos.p.x, pos.p.z);
        if self.point_near_logistics(side, point).is_err() {
            bail!("you are not close enough to friendly logistics to return troops")
        }
        let cargo = self.ephemeral.cargo.get_mut(slot).unwrap();
        let it = cargo.troops.pop().unwrap();
        Trigger::singleton(lua)?
            .action()?
            .set_unit_internal_cargo(unit_name, cargo.weight())?;
        match it.origin {
            None => self.adjust_points(&it.player, it.troop.cost as i32, "for troop return"),
            Some(oid) => {
                self.refund_points(
                    &it.player,
                    oid,
                    it.troop.cost,
                    it.cost_fraction,
                    "for troop return",
                    InvestBucket::Ground,
                );
            }
        }
        Ok(it.troop)
    }

    pub fn extract_troops(&mut self, lua: MizLua, slot: &SlotId) -> Result<Troop> {
        let (cargo_capacity, side, unit_name) = self.unit_cargo_cfg(slot)?;
        let pos = self.ephemeral.slot_instance_pos(lua, slot)?;
        let point = Vector2::new(pos.p.x, pos.p.z);
        let (gid, it) = {
            let max_dist = (self.ephemeral.cfg.crate_load_distance as f64).powi(2);
            self.persisted
                .troops
                .into_iter()
                .filter_map(|gid| self.persisted.groups.get(gid).map(|g| (*gid, g)))
                .find_map(|(gid, g)| {
                    if let DeployKind::Troop {
                        spec,
                        player,
                        origin,
                        cost_fraction,
                        ..
                    } = &g.origin
                    {
                        if g.side == side {
                            let in_range = g
                                .units
                                .into_iter()
                                .filter_map(|uid| self.persisted.units.get(uid))
                                .any(|u| {
                                    let upos = Self::troop_unit_world_pos(
                                        lua,
                                        u.name.as_str(),
                                        u.pos,
                                    );
                                    na::distance_squared(&upos.into(), &point.into()) <= max_dist
                                });
                            if in_range {
                                return Some((
                                    gid,
                                    InternalTroop {
                                        player: *player,
                                        origin: *origin,
                                        cost_fraction: *cost_fraction,
                                        troop: spec.clone(),
                                    },
                                ));
                            }
                        }
                    }
                    None
                })
                .ok_or_else(|| anyhow!("no troops in range"))?
        };
        let ucid = self
            .ephemeral
            .player_in_slot(slot)
            .cloned()
            .ok_or_else(|| anyhow!("can't find player in slot {slot:?}"))?;
        let (crates, _) = self.fowl_crate_and_troop_slot_usage_with_bay(lua, slot, &ucid);
        if {
            let cargo = self.ephemeral.cargo.entry(slot.clone()).or_default();
            cargo.troop_half_slots() + 2 > cargo_capacity.troop_slots as usize * 2
                || cargo.troop_half_slots() + crates * 2 + 2
                    > cargo_capacity.total_slots as usize * 2
        } {
            bail!("you already have a full load onboard")
        }
        let cargo = self.ephemeral.cargo.entry(slot.clone()).or_default();
        let troop_cfg = it.troop.clone();
        cargo.troops.push(it);
        Trigger::singleton(lua)?
            .action()?
            .set_unit_internal_cargo(unit_name, cargo.weight() as i64)?;
        self.delete_group(&gid)?;
        Ok(troop_cfg)
    }
}
