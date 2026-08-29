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
    ai_air, Db, MapS, SetS, csar::CsarDowned, ephemeral::SlotInfo, group::DeployKind,
    objective::Objective,
};
use crate::{maybe, maybe_mut, objective, objective_mut};
use super::campaign_stats::InvestBucket;
use anyhow::{Context, Result, anyhow, bail};
use bfprotocols::{
    cfg::{LifeType, PointsCfg, UnitTag, UnitTags, Vehicle},
    db::{
        group::GroupId,
        objective::{ObjectiveId, ObjectiveKind},
    },
    shots::{Dead, Who},
    stats::{self, EnId, Stat},
};
use chrono::{Duration, prelude::*};
use compact_str::{CompactString, format_compact};
use dcso3::{
    MizLua, Position3, String, Vector2, Vector3,
    airbase::Airbase,
    coalition::Side,
    coord::Coord,
    net::{SlotId, Ucid},
    object::{DcsObject, DcsOid, Object},
    unit::{ClassUnit, Unit},
};
use fxhash::FxHashSet;
use log::{debug, error, info, warn};
use netidx::utils::Either;
use serde::Deserialize;
use serde_derive::Serialize;
use smallvec::{SmallVec, smallvec};
use std::cmp::{max, min};

struct VictimInfo {
    ucid: Ucid,
    name: String,
    ai_deployable: bool,
    life_type: Option<LifeType>,
}

#[derive(Debug, Clone)]
pub enum SlotAuth {
    Yes(Option<stats::Unit>),
    ObjectiveNotOwned(Side),
    ObjectiveHasNoLogistics,
    NoLives(LifeType),
    NoPoints {
        vehicle: Vehicle,
        cost: u32,
        balance: i32,
    },
    NotRegistered(Side),
    VehicleNotAvailable(Vehicle),
    Denied,
    AirborneDeslotBlocked {
        remaining_secs: u32,
    },
}

pub enum RegErr {
    AlreadyRegistered(Option<u8>, Side),
    AlreadyOn(Side),
}

#[derive(Debug, Clone)]
pub enum TakeoffRes {
    TookLife(LifeType),
    NoLifeTaken,
    OutOfLives,
    OutOfPoints,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstancedPlayer {
    pub unit_name: String,
    pub position: Position3,
    pub velocity: Vector3,
    pub typ: Vehicle,
    pub in_air: bool,
    pub landed_at_objective: Option<ObjectiveId>,
    pub stopped_at_objective: bool,
    pub moved: Option<DateTime<Utc>>,
    pub cost_fraction: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Player {
    pub name: String,
    pub alts: SetS<String>,
    pub side: Side,
    pub side_switches: Option<u8>,
    pub lives: MapS<LifeType, (DateTime<Utc>, u8)>,
    pub crates: SetS<GroupId>,
    #[serde(default)]
    pub airborne: Option<LifeType>,
    /// Wall-clock end of observer/spectator lockout after airborne RELEASE / ejection.
    #[serde(default)]
    pub airborne_observer_penalty_until: Option<DateTime<Utc>>,
    /// Downed pilots awaiting CSAR (one entry per ejection; keyed by life type).
    #[serde(default)]
    pub csar_downed: Vec<CsarDowned>,
    #[serde(default)]
    pub points: i32,
    #[serde(default)]
    pub ai_team_kills: SetS<DateTime<Utc>>,
    #[serde(default)]
    pub player_team_kills: MapS<DateTime<Utc>, Ucid>,
    #[serde(skip)]
    pub current_slot: Option<(SlotId, Option<InstancedPlayer>)>,
    #[serde(skip)]
    pub changing_slots: bool,
    #[serde(skip)]
    pub jtac_or_spectators: bool,
    #[serde(skip)]
    pub provisional_points: i32,
}

impl Db {
    pub fn apply_airborne_observer_penalty(&mut self, ucid: &Ucid, now: DateTime<Utc>) -> bool {
        let secs = self.ephemeral.cfg.airborne_deslot_penalty_secs;
        if !self.ephemeral.cfg.airborne_deslot_block || secs == 0 {
            return false;
        }
        let Some(player) = self.persisted.players.get_mut_cow(ucid) else {
            return false;
        };
        if player
            .airborne_observer_penalty_until
            .is_some_and(|until| now < until)
        {
            return false;
        }
        player.airborne_observer_penalty_until =
            Some(now + Duration::seconds(secs as i64));
        player.current_slot.take();
        player.changing_slots = false;
        self.ephemeral.dirty();
        let penalty_points = self.ephemeral.cfg.airborne_deslot_penalty_points;
        if penalty_points > 0 {
            self.adjust_points(
                ucid,
                -(penalty_points as i32),
                "airborne deslot penalty",
            );
        }
        true
    }

    pub fn airborne_observer_penalty_remaining(
        &mut self,
        ucid: &Ucid,
        now: DateTime<Utc>,
    ) -> Option<u32> {
        let max_secs = self.ephemeral.cfg.airborne_deslot_penalty_secs.max(1) as i64;
        let player = self.persisted.players.get_mut_cow(ucid)?;
        let mut until = player.airborne_observer_penalty_until?;
        let cap = now + Duration::seconds(max_secs);
        if until > cap {
            warn!("airborne penalty capped for {ucid:?}");
            until = cap;
            player.airborne_observer_penalty_until = Some(until);
            self.ephemeral.dirty();
        }
        if now >= until {
            player.airborne_observer_penalty_until = None;
            self.ephemeral.dirty();
            return None;
        }
        let ms = (until - now).num_milliseconds().max(0);
        Some(((ms + 999) / 1000) as u32)
    }

    pub fn clear_airborne_session(&mut self, ucid: &Ucid) {
        if let Some(player) = self.persisted.players.get_mut_cow(ucid) {
            player.airborne = None;
            if let Some((_, Some(inst))) = &mut player.current_slot {
                inst.in_air = false;
            }
            self.ephemeral.dirty();
        }
    }

    /// Clear persisted slot state after ephemeral mappings are already gone.
    pub fn sync_player_deslot_state(&mut self, ucid: &Ucid, slot: Option<SlotId>) {
        if let Some(player) = self.persisted.players.get_mut_cow(ucid) {
            player.airborne = None;
            player.provisional_points = 0;
            player.changing_slots = false;
            if let Some(slot) = slot {
                if player.current_slot.as_ref().map(|(s, _)| *s) == Some(slot) {
                    player.current_slot.take();
                }
            } else {
                player.current_slot.take();
            }
            self.ephemeral.stat(Stat::Deslot { id: *ucid });
            self.ephemeral.dirty();
        }
    }

    pub fn player_deslot_slot(&mut self, ucid: &Ucid, slot: &SlotId) {
        self.delete_fowl_crates_on_carrier_deslot(None, ucid, Some(slot));
        let _ = self
            .ephemeral
            .player_deslot(&self.persisted, slot, Some(*ucid));
        self.sync_player_deslot_state(ucid, Some(*slot));
    }

    pub fn player_deslot(&mut self, ucid: &Ucid) {
        let slots: SmallVec<[SlotId; 4]> = self
            .ephemeral
            .players_by_slot
            .iter()
            .filter(|(_, u)| **u == *ucid)
            .map(|(s, _)| *s)
            .collect();
        self.delete_fowl_crates_on_carrier_deslot(None, ucid, None);
        for slot in slots {
            let _ = self
                .ephemeral
                .player_deslot(&self.persisted, &slot, Some(*ucid));
        }
        let current = self
            .persisted
            .players
            .get(ucid)
            .and_then(|p| p.current_slot.as_ref().map(|(s, _)| *s));
        self.sync_player_deslot_state(ucid, current);
    }

    pub fn player(&self, ucid: &Ucid) -> Option<&Player> {
        self.persisted.players.get(ucid)
    }

    pub fn player_mut(&mut self, ucid: &Ucid) -> Option<&mut Player> {
        self.persisted.players.get_mut_cow(ucid)
    }

    pub fn transfer_points(
        &mut self,
        source: &Ucid,
        target: Either<&Ucid, ObjectiveId>,
        amount: u32,
    ) -> Result<()> {
        let (source_side, sp_name) = {
            let sp = self
                .persisted
                .players
                .get(source)
                .ok_or_else(|| anyhow!("source player not found"))?;
            if sp.points < amount as i32 {
                bail!(
                    "insufficient balance, you have {}, you requested {}",
                    sp.points,
                    amount
                );
            }
            (sp.side, sp.name.clone())
        };
        match target {
            Either::Left(target_ucid) => {
                let tp = self
                    .persisted
                    .players
                    .get(target_ucid)
                    .ok_or_else(|| anyhow!("target player not found"))?;
                if tp.side != source_side {
                    bail!("can't transfer points to a player on the other team");
                }
            }
            Either::Right(oid) => {
                let obj = self
                    .persisted
                    .objectives
                    .get(&oid)
                    .ok_or_else(|| anyhow!("target objective not found"))?;
                if obj.owner != source_side {
                    bail!("can't transfer points to an enemy objective");
                }
            }
        }
        let sp = self
            .persisted
            .players
            .get_mut_cow(source)
            .ok_or_else(|| anyhow!("source player not found"))?;
        sp.points -= amount as i32;
        match target {
            Either::Left(target) => {
                let tp = self.persisted.players.get_mut_cow(target).unwrap();
                tp.points += amount as i32;
                let msg = format_compact!(
                    "{}(+{}) you received points from {}",
                    tp.points,
                    amount,
                    sp_name
                );
                self.ephemeral
                    .panel_to_player(&self.persisted, 10, target, msg);
                self.ephemeral.stat(Stat::PointsTransfer {
                    from: *source,
                    to: *target,
                    points: amount,
                });
                self.ephemeral.dirty();
                Ok(())
            }
            Either::Right(target) => {
                let obj = self.persisted.objectives.get_mut_cow(&target).unwrap();
                obj.points += amount as i32;
                self.ephemeral.stat(Stat::PointsTransferToObjective {
                    from: *source,
                    to: target,
                    points: amount,
                });
                self.ephemeral.dirty();
                Ok(())
            }
        }
    }

    pub fn player_reset_lives(&mut self, ucid: &Ucid) -> Result<()> {
        maybe_mut!(self.persisted.players, ucid, "player")?.lives = MapS::new();
        self.clear_csar_downed(ucid);
        self.ephemeral.stat(Stat::Life {
            id: *ucid,
            lives: MapS::new(),
        });
        self.ephemeral.dirty();
        Ok(())
    }

    pub fn instanced_players(&self) -> impl Iterator<Item = (&Ucid, &Player, &InstancedPlayer)> {
        self.ephemeral
            .players_by_slot
            .values()
            .filter_map(|ucid| {
                self.persisted.players.get(ucid).and_then(|player| {
                    player
                        .current_slot
                        .as_ref()
                        .and_then(|(_, inst)| inst.as_ref())
                        .map(|inst| (ucid, player, inst))
                })
            })
            .chain(self.ephemeral.csar_pilot_unit.iter().filter_map(|(pilot_oid, ucid)| {
                self.persisted.players.get(ucid).and_then(|player| {
                    player
                        .csar_downed
                        .iter()
                        .find(|c| c.pilot_unit == *pilot_oid)
                        .map(|csar| (ucid, player, &csar.inst))
                })
            }))
    }

    pub fn player_in_unit(&self, include_deployed: bool, id: &DcsOid<ClassUnit>) -> Option<Ucid> {
        match self
            .ephemeral
            .get_slot_by_object_id(id)
            .and_then(|s| self.ephemeral.players_by_slot.get(s))
        {
            Some(ucid) => Some(ucid.clone()),
            None => {
                if !include_deployed {
                    None
                } else {
                    self.ephemeral
                        .uid_by_object_id
                        .get(id)
                        .and_then(|uid| self.persisted.units.get(uid))
                        .and_then(|unit| self.persisted.groups.get(&unit.group))
                        .and_then(|group| match &group.origin {
                            DeployKind::Deployed {
                                player,
                                spec: _,
                                moved_by: _,
                                cost_fraction: _,
                                origin: _,
                            } => Some(player.clone()),
                            DeployKind::Troop {
                                player,
                                ..
                            } => Some(*player),
                            DeployKind::Action { player, .. } => player.clone(),
                            DeployKind::Crate { .. }
                            | DeployKind::Objective { .. }
                            | DeployKind::ObjectiveDeprecated
                            | DeployKind::CsarPilot { .. } => None,
                        })
                }
            }
        }
    }

    fn compute_flight_cost(&self, sifo: &SlotInfo, unit: &Unit) -> Result<(u32, bool, String)> {
        use std::fmt::Write;
        let mut m = String::from("");
        match self.ephemeral.cfg.points.as_ref() {
            None => Ok((0, false, m)),
            Some(points) => {
                let mut cost = *points.airframe_cost.get(&sifo.typ).unwrap_or(&0);
                write!(m, "{cost} for {}", sifo.typ).unwrap();
                if !points.weapon_cost.is_empty() {
                    for ammo in unit.get_ammo().context("getting ammo")? {
                        let ammo = ammo.context("unwrapping ammo")?;
                        let typ = ammo.type_name().context("getting ammo type name")?;
                        info!("ammo of type {typ} loaded");
                        if let Some(unit_cost) = points.weapon_cost.get(&typ) {
                            let n = ammo.count().context("getting ammo count")?;
                            let wcost = n * (*unit_cost);
                            write!(m, ", {wcost} for {n}x{typ}").unwrap();
                            cost += wcost;
                        }
                    }
                }
                Ok((cost, points.strict, m))
            }
        }
    }

    pub fn takeoff(
        &mut self,
        time: DateTime<Utc>,
        slot: SlotId,
        unit: &Unit,
        position: Vector2,
    ) -> Result<TakeoffRes> {
        self.cancel_csar_extract_for_slot(&slot);
        let sifo = self
            .ephemeral
            .slot_info
            .get(&slot)
            .ok_or_else(|| anyhow!("could not find slot {:?}", slot))?;
        let (cost, strict, cost_msg) = match self.compute_flight_cost(&sifo, unit) {
            Ok(cost) => cost,
            Err(e) => {
                error!("failed to compute flight cost {e:?}");
                (0, false, String::from(""))
            }
        };
        let (ucid, player) = self
            .ephemeral
            .players_by_slot
            .get(&slot)
            .and_then(|ucid| self.persisted.players.get_mut_cow(ucid).map(|p| (*ucid, p)))
            .ok_or_else(|| anyhow!("could not find player in slot {:?}", slot))?;
        let owned_objective = self
            .persisted
            .objectives
            .iter_mut_cow()
            .find_map(|(oid, obj)| {
                if obj.owner == player.side && obj.zone.contains(position) {
                    Some((oid, obj))
                } else {
                    None
                }
            });
        let life_type = match self.ephemeral.cfg.life_types.get(&sifo.typ) {
            None => bail!("no life type for vehicle {:?}", sifo.typ),
            Some(typ) => *typ,
        };
        let (_, player_lives) = player.lives.get_or_insert_cow(life_type, || {
            (time, self.ephemeral.cfg.default_lives[&life_type].0)
        });
        if let Some((_, Some(inst))) = &mut player.current_slot {
            inst.landed_at_objective = None;
        }
        let obj_balance = owned_objective.as_ref().map(|(_, o)| o.points).unwrap_or(0);
        let res = if strict && cost as i32 > max(0, player.points) + obj_balance {
            return Ok(TakeoffRes::OutOfPoints);
        } else if !self.ephemeral.cfg.limited_lives {
            player.airborne = Some(life_type);
            self.ephemeral.dirty();
            Ok(TakeoffRes::NoLifeTaken)
        } else if self.ephemeral.cfg.lives_birth {
            player.airborne = Some(life_type);
            self.ephemeral.dirty();
            Ok(TakeoffRes::NoLifeTaken)
        } else if owned_objective.is_some() {
            // paranoia
            if *player_lives == 0 {
                return Ok(TakeoffRes::OutOfLives);
            } else {
                player.airborne = Some(life_type);
                *player_lives -= 1;
                self.ephemeral.stat(Stat::Life {
                    id: ucid,
                    lives: player.lives.clone(),
                });
            }
            self.ephemeral.dirty();
            Ok(TakeoffRes::TookLife(life_type))
        } else {
            Ok(TakeoffRes::NoLifeTaken)
        };
        if cost > 0
            && let Some(oid) = owned_objective.map(|(id, _)| *id)
        {
            let frac = self.charge_for_item(
                &ucid,
                oid,
                cost,
                cost_msg.as_str(),
                InvestBucket::Air,
            );
            let player = &mut self.persisted.players[&ucid];
            match &mut player.current_slot {
                Some((_, Some(inst))) => inst.cost_fraction = frac,
                _ => (),
            }
        };
        self.ephemeral.stat(Stat::Takeoff { id: ucid });
        res
    }

    pub fn charge_for_item(
        &mut self,
        ucid: &Ucid,
        oid: ObjectiveId,
        cost: u32,
        msg: &str,
        invest: InvestBucket,
    ) -> f32 {
        match self.player(ucid) {
            None => 1.,
            Some(player) => {
                let side = player.side;
                let player_balance = player.points;
                let mut from_player = 0u32;
                let mut from_objective = 0u32;
                let (adj, frac) = match self.persisted.objectives.get_mut_cow(&oid) {
                    None => {
                        from_player = cost;
                        (-(cost as i32), 1.)
                    }
                    Some(obj) => {
                        if player_balance <= 0 {
                            obj.points -= cost as i32;
                            from_objective = cost;
                            (0, 0.)
                        } else if player_balance < cost as i32 {
                            let frac = player_balance as f32 / cost as f32;
                            let obj_cost = cost as i32 - player_balance;
                            obj.points -= obj_cost;
                            from_player = player_balance as u32;
                            from_objective = obj_cost as u32;
                            (-player_balance, frac)
                        } else {
                            from_player = cost;
                            (-(cost as i32), 1.)
                        }
                    }
                };
                if from_player > 0 || from_objective > 0 {
                    self.campaign_on_invested(side, invest, from_player, from_objective);
                }
                self.adjust_points(&ucid, adj, msg);
                self.ephemeral.dirty();
                frac
            }
        }
    }

    pub fn refund_points(
        &mut self,
        ucid: &Ucid,
        oid: ObjectiveId,
        cost: u32,
        frac: f32,
        msg: &str,
        invest: InvestBucket,
    ) {
        let mut from_objective = 0u32;
        if let Some(obj) = self.persisted.objectives.get_mut_cow(&oid) {
            let cost = (cost as f32 * (1. - frac)).round() as i32;
            obj.points += cost;
            from_objective = cost.max(0) as u32;
        }
        let cost = (cost as f32 * frac).round() as i32;
        if let Some(player) = self.persisted.players.get(ucid) {
            let side = player.side;
            self.campaign_on_refund(side, invest, cost.max(0) as u32, from_objective);
        }
        self.adjust_points(ucid, cost, msg);
    }

    /// ME zone, else nearest friendly airbase (helipads often sit outside the OFO circle).
    fn resolve_friendly_land_objective(
        &self,
        lua: MizLua,
        side: Side,
        position: Vector2,
    ) -> Option<ObjectiveId> {
        if let Some((oid, _)) = self.persisted.objectives.into_iter().find(|(_, o)| {
            o.owner == side && o.zone.contains(position)
        }) {
            return Some(*oid);
        }
        self.nearest_friendly_airbase_objective(lua, side, position)
    }

    fn nearest_friendly_airbase_objective(
        &self,
        lua: MizLua,
        side: Side,
        position: Vector2,
    ) -> Option<ObjectiveId> {
        let mut best: Option<(f64, ObjectiveId)> = None;
        for (oid, obj) in &self.persisted.objectives {
            if obj.owner != side {
                continue;
            }
            let Some(ab_id) = self.ephemeral.airbase_by_oid.get(oid) else {
                continue;
            };
            let Ok(ab) = Airbase::get_instance(lua, ab_id) else {
                continue;
            };
            let Ok(pt) = ab.get_point() else {
                continue;
            };
            let ab_pos = Vector2::new(pt.x, pt.z);
            let dist_sq = (ab_pos - position).magnitude_squared();
            let max_r = obj.zone.radius().max(3_000.0);
            if dist_sq <= max_r * max_r {
                if best.map(|(d, _)| dist_sq < d).unwrap_or(true) {
                    best = Some((dist_sq, *oid));
                }
            }
        }
        best.map(|(_, oid)| oid)
    }

    /// Land `place` (e.g. `Ochamchira-3`) → objective id.
    pub fn objective_id_for_land_place(
        &self,
        lua: MizLua,
        place: &Object,
    ) -> Option<ObjectiveId> {
        if let Ok(pid) = place.object_id() {
            for (oid, ab) in &self.ephemeral.airbase_by_oid {
                if ab.erased() == pid {
                    return Some(*oid);
                }
            }
            for (oid, abs) in &self.ephemeral.airbases_by_oid {
                if abs.iter().any(|a| a.erased() == pid) {
                    return Some(*oid);
                }
            }
            if let Ok(ab) = Airbase::get_instance_dyn(lua, &pid) {
                if let Ok(obj) = ab.as_object() {
                    if let Ok(name) = obj.get_name() {
                        if let Some(oid) = self.objective_id_for_airbase_name(name.as_str()) {
                            return Some(oid);
                        }
                    }
                }
            }
        }
        place
            .get_name()
            .ok()
            .and_then(|n| self.objective_id_for_airbase_name(n.as_str()))
    }

    fn objective_id_for_airbase_name(&self, name: &str) -> Option<ObjectiveId> {
        if let Some(oid) = self.persisted.objectives_by_name.get(name) {
            return Some(*oid);
        }
        if let Some((base, rest)) = name.rsplit_once('-') {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                if let Some(oid) = self.persisted.objectives_by_name.get(base) {
                    return Some(*oid);
                }
            }
        }
        None
    }

    pub fn land(&mut self, lua: MizLua, slot: SlotId, position: Vector2, unit: &Unit) -> bool {
        let sifo = match self.ephemeral.slot_info.get(&slot) {
            Some(sifo) => sifo,
            None => return false,
        };
        let (cost, _, cost_msg) = match self.compute_flight_cost(&sifo, unit) {
            Ok(cost) => cost,
            Err(e) => {
                error!("failed to compute flight cost {e:?}");
                (0, false, String::from(""))
            }
        };
        let Some(ucid) = self.ephemeral.players_by_slot.get(&slot).copied() else {
            return false;
        };
        let Some(side) = self.persisted.players.get(&ucid).map(|p| p.side) else {
            return false;
        };
        let owned_objective = self.resolve_friendly_land_objective(lua, side, position);
        self.ephemeral.stat(Stat::Land { id: ucid });
        let Some(oid) = owned_objective else {
            debug!(
                "land() not armed slot={slot:?} side={side:?} pos=({:.0},{:.0}) — outside friendly objective / airbase",
                position.x, position.y
            );
            return false;
        };
        let player = match self.persisted.players.get_mut_cow(&ucid) {
            Some(player) => player,
            None => return false,
        };
        player.airborne = None;
        let mut frac = 1.;
        if let Some((_, Some(inst))) = &mut player.current_slot {
            inst.position.p.x = position.x;
            inst.position.p.z = position.y;
            inst.landed_at_objective = Some(oid);
            frac = inst.cost_fraction;
        }
        if let Some(points) = self.ephemeral.cfg.points.as_ref() {
            let is_provisional = points.provisional;
            let provisional_points = player.provisional_points;
            player.provisional_points = 0;
            if cost > 0 {
                self.refund_points(
                    &ucid,
                    oid,
                    cost,
                    frac,
                    cost_msg.as_str(),
                    InvestBucket::Air,
                );
            }
            if is_provisional && provisional_points > 0 {
                self.adjust_points(
                    &ucid,
                    provisional_points as i32,
                    "provisional points committed",
                );
            }
        }
        self.ephemeral.dirty();
        true
    }

    pub fn maybe_reset_lives(&mut self, ucid: &Ucid, now: DateTime<Utc>) -> Result<()> {
        let mut lt_to_reset: SmallVec<[LifeType; 2]> = smallvec![];
        let mut reset = false;
        let lives_after = {
            let player = self
                .persisted
                .players
                .get_mut_cow(ucid)
                .ok_or_else(|| anyhow!("no such player {:?}", ucid))?;
            for (lt, (reset_ts, _n)) in player.lives.into_iter() {
                let reset_after = Duration::seconds(
                    maybe!(self.ephemeral.cfg.default_lives, lt, "default life")?.1 as i64,
                );
                if now - reset_ts >= reset_after {
                    lt_to_reset.push(*lt);
                }
            }
            for lt in &lt_to_reset {
                player.lives.remove_cow(lt);
                reset = true;
                self.ephemeral.dirty();
            }
            if reset {
                player.lives.clone()
            } else {
                MapS::new()
            }
        };
        if reset {
            self.ephemeral.stat(Stat::Life {
                id: *ucid,
                lives: lives_after,
            });
        }
        for lt in lt_to_reset {
            self.clear_csar_downed_for_life_type(ucid, lt);
        }
        Ok(())
    }

    fn slot_airframe_available(
        &self,
        lua: MizLua,
        objective: &Objective,
        typ: &str,
    ) -> Result<bool> {
        if let Some(inv) = objective.warehouse.equipment.get(typ) {
            if inv.stored > 0 {
                return Ok(true);
            }
        }
        if let ObjectiveKind::Farp { pad_template, .. } = &objective.kind {
            let count = if let Some(ab) = self.ephemeral.airbase_by_oid.get(&objective.id) {
                Airbase::get_instance(lua, ab)?
                    .get_warehouse()?
                    .get_item_count(String::from(typ))?
            } else {
                Airbase::get_by_name(lua, pad_template.clone())?
                    .get_warehouse()?
                    .get_item_count(String::from(typ))?
            };
            return Ok(count > 0);
        }
        Ok(false)
    }

    /// MP: FO/observer TryChangeSlot runs before PlayerLeaveUnit and clears `current_slot.inst`.
    fn stash_aircraft_deslot_snapshot(&mut self, ucid: &Ucid) {
        let Some(player) = self.persisted.players.get_mut_cow(ucid) else {
            return;
        };
        let Some((old_slot, Some(inst))) = player.current_slot.as_ref() else {
            return;
        };
        if matches!(old_slot, SlotId::Unit(_) | SlotId::MultiCrew(_, _)) {
            self.ephemeral
                .pending_aircraft_deslot
                .insert(*old_slot, inst.clone());
        }
    }

    pub fn try_occupy_slot(
        &mut self,
        lua: MizLua,
        time: DateTime<Utc>,
        slot_side: Side,
        slot: SlotId,
        ucid: &Ucid,
    ) -> SlotAuth {
        let exits_aircraft = slot.is_spectator()
            || matches!(
                slot,
                SlotId::Instructor(_, _)
                    | SlotId::ArtilleryCommander(_, _)
                    | SlotId::ForwardObserver(_, _)
                    | SlotId::Observer(_, _)
            );
        if exits_aircraft {
            self.stash_aircraft_deslot_snapshot(ucid);
        }
        let player = match self.persisted.players.get_mut_cow(ucid) {
            Some(player) => player,
            None => {
                if slot.is_spectator() {
                    return SlotAuth::Yes(None);
                }
                return SlotAuth::NotRegistered(slot_side);
            }
        };
        if slot.is_spectator() {
            player.jtac_or_spectators = true;
            player.current_slot = None;
            return SlotAuth::Yes(None);
        }
        if slot_side != player.side {
            if self.ephemeral.cfg.lock_sides {
                return SlotAuth::ObjectiveNotOwned(player.side);
            } else {
                player.side = slot_side;
            }
        }
        match slot {
            SlotId::Spectator => unreachable!(),
            SlotId::Instructor(_, _) => {
                if self.ephemeral.cfg.admins.contains_key(ucid) {
                    player.jtac_or_spectators = true;
                    player.current_slot = Some((slot, None));
                    SlotAuth::Yes(None)
                } else {
                    SlotAuth::Denied
                }
            }
            SlotId::ArtilleryCommander(_, _)
            | SlotId::ForwardObserver(_, _)
            | SlotId::Observer(_, _) => {
                if self.ephemeral.cfg.rules.ca.check(ucid) {
                    player.jtac_or_spectators = true;
                    player.current_slot = Some((slot, None));
                    SlotAuth::Yes(None)
                } else {
                    SlotAuth::Denied
                }
            }
            SlotId::Unit(_) | SlotId::MultiCrew(_, _) => {
                if self.ephemeral.slot_info.contains_key(&slot) {
                    self.try_occupy_slot_deferred(lua, time, ucid, slot)
                } else if self.ephemeral.dep_farp_pool_slot_ids.contains(&slot) {
                    SlotAuth::Denied
                } else {
                    player.changing_slots = true;
                    player.jtac_or_spectators = false;
                    return SlotAuth::Yes(None);
                }
            }
        }
    }

    pub fn try_occupy_slot_deferred(
        &mut self,
        lua: MizLua,
        time: DateTime<Utc>,
        ucid: &Ucid,
        slot: SlotId,
    ) -> SlotAuth {
        let sifo = match self.ephemeral.slot_info.get(&slot) {
            None => return SlotAuth::Denied,
            Some(sifo) => sifo,
        };
        if self.persisted.players.get(ucid).is_none() {
            if slot.is_spectator() {
                return SlotAuth::Yes(None);
            }
            return SlotAuth::NotRegistered(sifo.side);
        }
        let objective = match self.persisted.objectives.get(&sifo.objective) {
            Some(o) if o.owner != Side::Neutral => o,
            Some(_) | None => {
                let side = self
                    .persisted
                    .players
                    .get(ucid)
                    .map(|p| p.side)
                    .unwrap_or(sifo.side);
                return SlotAuth::ObjectiveNotOwned(side);
            }
        };
        let player_side = self.persisted.players.get(ucid).unwrap().side;
        if objective.owner != player_side {
            return SlotAuth::ObjectiveNotOwned(player_side);
        }
        if objective.captureable() {
            return SlotAuth::ObjectiveHasNoLogistics;
        }
        if matches!(&objective.kind, ObjectiveKind::Farp { .. })
            && !self.ephemeral.airbase_by_oid.contains_key(&objective.id)
        {
            return SlotAuth::VehicleNotAvailable(sifo.typ.clone());
        }
        if let Some(whcfg) = self.ephemeral.cfg.warehouse.as_ref() {
            let typ = sifo.typ.as_str();
            if !whcfg.exempt_airframes.contains(typ) {
                match self.slot_airframe_available(lua, objective, typ) {
                    Ok(true) => (),
                    Ok(false) => {
                        return SlotAuth::VehicleNotAvailable(sifo.typ.clone());
                    }
                    Err(e) => {
                        error!("carrier/airframe stock check failed: {e:?}");
                        return SlotAuth::VehicleNotAvailable(sifo.typ.clone());
                    }
                }
            }
        }
        let objective_points = objective.points;
        // Canonical reset path: also clears downed pilots of the reset life type.
        if let Err(e) = self.maybe_reset_lives(ucid, time) {
            warn!("lives reset check failed for {ucid:?}: {e:?}");
        }
        let sifo = match self.ephemeral.slot_info.get(&slot) {
            None => return SlotAuth::Denied,
            Some(sifo) => sifo,
        };
        let player = match self.persisted.players.get_mut_cow(ucid) {
            Some(player) => player,
            None => return SlotAuth::NotRegistered(sifo.side),
        };
        let life_type = self.ephemeral.cfg.life_types[&sifo.typ];
        macro_rules! yes {
            () => {
                player.changing_slots = false;
                player.jtac_or_spectators = false;
                break SlotAuth::Yes(Some(stats::Unit {
                    typ: sifo.typ.clone(),
                    tags: self
                        .ephemeral
                        .cfg
                        .unit_classification
                        .get(&sifo.typ)
                        .map(|t| *t)
                        .unwrap_or_default(),
                }));
            };
        }
        if let Some(points) = self.ephemeral.cfg.points.as_ref() {
            let cost = *points.airframe_cost.get(&sifo.typ).unwrap_or(&0) as i32;
            let balance = player.points + objective_points;
            if cost > 0 && balance < cost {
                return SlotAuth::NoPoints {
                    cost: cost as u32,
                    vehicle: sifo.typ.clone(),
                    balance,
                };
            }
        }
        loop {
            match player.lives.get(&life_type).map(|t| *t) {
                None => {
                    yes!();
                }
                Some((reset, n)) => {
                    let reset_after =
                        Duration::seconds(self.ephemeral.cfg.default_lives[&life_type].1 as i64);
                    if time - reset >= reset_after {
                        player.lives.remove_cow(&life_type);
                        self.ephemeral.stat(Stat::Life {
                            id: *ucid,
                            lives: player.lives.clone(),
                        });
                        self.ephemeral.dirty = true;
                    } else if n == 0 {
                        break SlotAuth::NoLives(life_type);
                    }
                    yes!();
                }
            }
        }
    }

    pub fn player_connected(&mut self, ucid: Ucid, name: String) {
        if let Some(player) = self.persisted.players.get(&ucid) {
            if player.current_slot.is_some() {
                self.player_deslot(&ucid)
            }
        }
        if let Some(player) = self.persisted.players.get_mut_cow(&ucid) {
            if player.name != name {
                player.alts.insert(name.clone());
                player.name = name;
                self.ephemeral.dirty()
            }
        }
        super::ai_air::extend_active_owner_locks_for_player(self, &ucid);
        self.campaign_on_connect(ucid, Utc::now());
    }

    pub fn register_player(&mut self, ucid: Ucid, name: String, side: Side) -> Result<(), RegErr> {
        match self.persisted.players.get(&ucid) {
            Some(p) if p.side != side => Err(RegErr::AlreadyRegistered(p.side_switches, p.side)),
            Some(_) => Err(RegErr::AlreadyOn(side)),
            None => {
                let points = self
                    .ephemeral
                    .cfg
                    .points
                    .as_ref()
                    .map(|p| p.new_player_join as i32)
                    .unwrap_or(0);
                self.persisted.players.insert_cow(
                    ucid,
                    Player {
                        name: name.clone(),
                        alts: SetS::from_iter([name.clone()]),
                        side,
                        side_switches: self.ephemeral.cfg.side_switches,
                        lives: MapS::new(),
                        crates: SetS::new(),
                        airborne: None,
                        airborne_observer_penalty_until: None,
                        csar_downed: vec![],
                        points,
                        provisional_points: 0,
                        current_slot: None,
                        changing_slots: false,
                        jtac_or_spectators: true,
                        ai_team_kills: SetS::new(),
                        player_team_kills: MapS::new(),
                    },
                );
                self.ephemeral.stat(Stat::Register {
                    initial_points: points,
                    name,
                    side,
                    id: ucid,
                });
                if points > 0 {
                    self.campaign_on_active_gain(side, points as i64);
                }
                self.campaign_on_register(ucid, side);
                self.ephemeral.dirty();
                Ok(())
            }
        }
    }

    pub fn force_sideswitch_player(&mut self, ucid: &Ucid, side: Side) -> Result<()> {
        let player = maybe_mut!(self.persisted.players, ucid, "no such player")?;
        player.side = side;
        self.ephemeral.stat(Stat::Sideswitch { id: *ucid, side });
        self.campaign_on_sideswitch(*ucid, side);
        self.ephemeral.dirty();
        Ok(())
    }

    pub fn sideswitch_player(&mut self, ucid: &Ucid, side: Side) -> Result<(), &'static str> {
        match self.persisted.players.get_mut_cow(ucid) {
            None => Err(
                "You are not registered. Select Blue or Red in the DCS lobby, then occupy a slot.",
            ),
            Some(player) => {
                if side == player.side {
                    Err("you are already on the requested side")
                } else if let Some(0) = player.side_switches {
                    Err("you can't switch sides again this round")
                } else if side == Side::Neutral {
                    Err("you can't switch to neutral")
                } else {
                    match &mut player.side_switches {
                        Some(n) => {
                            *n -= 1;
                        }
                        None => (),
                    }
                    player.side = side;
                    self.ephemeral.stat(Stat::Sideswitch { id: *ucid, side });
                    self.campaign_on_sideswitch(*ucid, side);
                    self.ephemeral.dirty();
                    Ok(())
                }
            }
        }
    }

    pub fn update_player_positions<'a>(
        &mut self,
        lua: MizLua,
        now: DateTime<Utc>,
        ids: impl IntoIterator<Item = &'a Ucid>,
    ) -> Result<Vec<DcsOid<ClassUnit>>> {
        let mut dead: Vec<DcsOid<ClassUnit>> = vec![];
        let mut unit: Option<Unit> = None;
        let coord = Coord::singleton(lua)?;
        for ucid in ids {
            let mut inform_cost = None;
            if !self
                .ephemeral
                .players_by_slot
                .values()
                .any(|u| u == ucid)
            {
                continue;
            }
            if let Some(player) = self.persisted.players.get_mut_cow(ucid) {
                if let Some((slot, Some(inst))) = &mut player.current_slot {
                    if let Some(id) = self.ephemeral.object_id_by_slot.get(slot) {
                        let instance = match unit.take() {
                            Some(unit) => unit.change_instance(id),
                            None => Unit::get_instance(lua, id),
                        };
                        let instance = match instance {
                            Ok(i) => Ok(i),
                            Err(_) => {
                                warn!("failed to get unit by id, trying by name");
                                Unit::get_by_name(lua, &inst.unit_name)
                            }
                        };
                        match instance {
                            Ok(instance) => {
                                let pos = instance.get_position()?;
                                if (inst.position.p.0 - pos.p.0).magnitude_squared() > 1.0 {
                                    if inst.stopped_at_objective {
                                        inform_cost = Some((*slot, player.points));
                                    }
                                    inst.stopped_at_objective = false;
                                    let was_in_air = inst.in_air;
                                    inst.position = pos;
                                    inst.velocity = instance.get_velocity()?.0;
                                    inst.in_air = instance.in_air()?;
                                    if !was_in_air && inst.in_air {
                                        self.ephemeral.clear_player_hub_slot_claim(slot);
                                    }
                                    inst.moved = Some(now);
                                } else if inst.landed_at_objective.is_some() {
                                    inst.stopped_at_objective = true;
                                }
                                unit = Some(instance);
                                self.ephemeral.stat(Stat::Position {
                                    id: EnId::Player(*ucid),
                                    pos: stats::Pos {
                                        pos: coord.lo_to_ll(inst.position.p)?,
                                        velocity: inst.velocity,
                                    },
                                });
                            }
                            Err(e) => {
                                warn!(
                                    "updating player positions, skipping invalid unit {ucid:?}, {id:?}, player {e:?}",
                                );
                                dead.push(id.clone())
                            }
                        }
                    }
                }
            }
            if let (Some(unit), Some((slot, balance))) = (&unit, inform_cost) {
                let sifo = self
                    .ephemeral
                    .slot_info
                    .get(&slot)
                    .ok_or_else(|| anyhow!("could not find slot {:?}", slot))?;
                let (cost, strict, cost_msg) = self.compute_flight_cost(sifo, &unit)?;
                if cost > 0 {
                    let m = if strict && cost as i32 > balance {
                        format_compact!(
                            "Your flight will cost {cost}, and you have {balance}. {cost_msg}"
                        )
                    } else {
                        format_compact!("Your flight will cost {cost}. {cost_msg}")
                    };
                    self.ephemeral.panel_to_player(&self.persisted, 60, ucid, m)
                }
            }
        }
        Ok(dead)
    }

    pub fn update_player_positions_incremental(
        &mut self,
        lua: MizLua,
        now: DateTime<Utc>,
        i: usize,
    ) -> Result<(usize, Vec<DcsOid<ClassUnit>>)> {
        let total = self.ephemeral.players_by_slot.len();
        if i < total {
            let stop = min(total, i + max(1, total / 10));
            let players: SmallVec<[Ucid; 64]> = self.ephemeral.players_by_slot.as_slice()[i..stop]
                .into_iter()
                .map(|(_, ucid)| *ucid)
                .collect();
            let dead = self.update_player_positions(lua, now, &players)?;
            Ok((stop, dead))
        } else {
            Ok((0, vec![]))
        }
    }

    pub fn player_entered_slot(
        &mut self,
        lua: MizLua,
        id: DcsOid<ClassUnit>,
        unit: &Unit,
        slot: SlotId,
        oid: ObjectiveId,
        ucid: Ucid,
        parking_subplace: Option<i64>,
    ) -> Result<()> {
        let stale: SmallVec<[SlotId; 4]> = self
            .ephemeral
            .players_by_slot
            .iter()
            .filter(|(s, u)| **u == ucid && **s != slot)
            .map(|(s, _)| *s)
            .collect();
        for stale_slot in stale {
            let _ = self
                .ephemeral
                .player_deslot(&self.persisted, &stale_slot, Some(ucid));
            self.sync_player_deslot_state(&ucid, Some(stale_slot));
        }
        if let Some(old_ucid) = self.ephemeral.players_by_slot.get(&slot) {
            let old_ucid = *old_ucid;
            if old_ucid != ucid {
                self.player_deslot(&old_ucid)
            }
        }
        let sifo = maybe!(self.ephemeral.slot_info, slot, "slot")?.clone();
        let player = maybe!(self.persisted.players, ucid, "player")?;
        let life_typ = self.ephemeral.cfg.life_types[&sifo.typ];
        match player.lives.get(&life_typ) {
            Some((_, n)) if *n == 0 => {
                info!("player {ucid} has no lives for this unit type");
                self.player_deslot(&ucid);
                unit.clone().destroy()?;
                return Ok(());
            }
            None | Some((_, _)) => (),
        }
        if self.ephemeral.cfg.limited_lives && self.ephemeral.cfg.lives_birth {
            let player = maybe_mut!(self.persisted.players, ucid, "player")?;
            let (_, player_lives) = player.lives.get_or_insert_cow(life_typ, || {
                (Utc::now(), self.ephemeral.cfg.default_lives[&life_typ].0)
            });
            if *player_lives == 0 {
                info!("player {ucid} has no lives for this unit type");
                self.player_deslot(&ucid);
                unit.clone().destroy()?;
                return Ok(());
            }
            *player_lives -= 1;
            info!(
                "life taken on slot entry for {ucid} ({life_typ}), remaining {}",
                *player_lives
            );
            self.ephemeral.stat(Stat::Life {
                id: ucid,
                lives: player.lives.clone(),
            });
            self.ephemeral.dirty();
        }
        self.ephemeral.players_by_slot.insert(slot, ucid);
        self.ephemeral
            .slot_by_object_id
            .insert(id.clone(), slot.clone());
        self.ephemeral
            .object_id_by_slot
            .insert(slot.clone(), id.clone());
        let position = unit.get_position()?;
        let point = Vector2::new(position.p.x, position.p.z);
        let in_air = unit.in_air()?;
        {
            let obj = objective_mut!(self, oid)?;
            let mut adjust_warehouse = || -> Result<()> {
                let id = maybe!(self.ephemeral.airbase_by_oid, obj.id, "airbase")?;
                let wh = Airbase::get_instance(lua, id)
                    .context("getting airbase")?
                    .get_warehouse()
                    .context("getting warehouse")?;
                let typ_name = sifo.typ.0.clone();
                let prev = obj
                    .warehouse
                    .equipment
                    .get(&typ_name)
                    .map(|i| i.stored)
                    .unwrap_or(0);
                let target = prev.saturating_sub(1);
                wh.set_item(typ_name.clone(), target).with_context(|| {
                    format_compact!("setting {typ_name} in warehouse")
                })?;
                maybe_mut!(obj.warehouse.equipment, typ_name, "equip")?.stored = target;
                info!(
                    "slot airframe: set {typ_name} at {:?} to {target} (prev={prev})",
                    obj.id
                );
                // DCS also consumes the airframe on spawn, often after this call.
                let due = Utc::now() + Duration::seconds(2);
                self.ephemeral.pending_deslot_airframe_fix.push((
                    due,
                    obj.id,
                    typ_name,
                    target,
                ));
                if sifo.ground_start {
                    for wep in unit.get_ammo()? {
                        let wep = wep?;
                        let count = wep.count()?;
                        let typ = wep.type_name()?;
                        let whcnt = wh.get_item_count(typ.clone())?;
                        debug!("removing {count} {typ} from the warehouse which contains {whcnt}");
                        wh.remove_item(typ.clone(), count)?;
                        if let Some(inv) = obj.warehouse.equipment.get_mut_cow(&typ) {
                            inv.stored = whcnt.saturating_sub(count);
                        }
                    }
                }
                Ok(())
            };
            if let Err(e) = adjust_warehouse() {
                error!("failed to adjust warehouse {:?}", e)
            }
        }
        let hub_claims = if !in_air {
            let obj = objective!(self, oid)?;
            ai_air::resolve_player_parking_claims(lua, self, obj, parking_subplace, point)?
        } else {
            FxHashSet::default()
        };
        let player = maybe_mut!(self.persisted.players, ucid, "player")?;
        let landed_at_objective = {
            let obj = objective!(self, oid)?;
            if obj.zone.contains(point) {
                Some(oid)
            } else if !sifo.ground_start {
                Some(oid)
            } else {
                self.persisted
                    .objectives
                    .into_iter()
                    .find(|(_, o)| o.zone.contains(point))
                    .map(|(oid, _)| *oid)
            }
        };
        if !hub_claims.is_empty() {
            info!(
                "player hub slot claims {hub_claims:?} subplace {parking_subplace:?} for slot {slot} at objective {oid}"
            );
            let claims: smallvec::SmallVec<[(ObjectiveId, ai_air::HubSlotKind, i64); 4]> =
                hub_claims.iter().copied().collect();
            self.ephemeral
                .set_player_hub_slot_claims(slot, &claims, Some((oid, point)), parking_subplace);
        }
        player.current_slot = Some((
            slot,
            Some(InstancedPlayer {
                unit_name: unit.get_name()?,
                position,
                velocity: unit.get_velocity()?.0,
                in_air,
                typ: Vehicle::from(unit.get_type_name()?),
                landed_at_objective,
                stopped_at_objective: true,
                moved: None,
                cost_fraction: 1.,
            }),
        ));
        player.changing_slots = false;
        player.provisional_points = 0;
        self.ephemeral.dirty();
        Ok(())
    }

    /// Credit airframe (and FARP ammo) to the landing objective warehouse on deslot.
    /// Use absolute `set_item(prev+1)` — DCS often returns the airframe *after* leave-unit,
    /// so `add_item` races and double-counts (seen as 0→2 C-130 on ferry deslot).
    fn credit_airframe_on_deslot(
        &mut self,
        lua: MizLua,
        objid: &DcsOid<ClassUnit>,
        land_oid: ObjectiveId,
        slot: &SlotId,
        typ: &Vehicle,
    ) -> Result<()> {
        let typ_name = typ.0.clone();
        let home_oid = self
            .ephemeral
            .slot_info
            .get(slot)
            .map(|s| s.objective);
        let land_ab = {
            let id = maybe!(self.ephemeral.airbase_by_oid, land_oid, "airbase")?;
            Airbase::get_instance(lua, &id).context("get land airbase")?
        };
        let land_wh = land_ab.get_warehouse().context("get land warehouse")?;
        let land_dcs = land_wh
            .get_item_count(typ_name.clone())
            .context("land get_item_count")?;
        let land_prev = self
            .persisted
            .objectives
            .get(&land_oid)
            .and_then(|o| o.warehouse.equipment.get(&typ_name))
            .map(|i| i.stored)
            .unwrap_or(0);
        let target = land_prev.saturating_add(1);
        let is_airbase = {
            let obj = objective!(self, land_oid)?;
            obj.kind.is_airbase()
                || self
                    .ephemeral
                    .cfg
                    .extra_fixed_wing_objectives
                    .contains(obj.name())
        };

        // Undo DCS return to slot home when ferrying to another pad.
        if let Some(home) = home_oid.filter(|h| *h != land_oid) {
            let home_prev = self
                .persisted
                .objectives
                .get(&home)
                .and_then(|o| o.warehouse.equipment.get(&typ_name))
                .map(|i| i.stored)
                .unwrap_or(0);
            let home_ab = {
                let id = maybe!(self.ephemeral.airbase_by_oid, home, "home airbase")?;
                Airbase::get_instance(lua, &id).context("get home airbase")?
            };
            let home_wh = home_ab.get_warehouse().context("get home warehouse")?;
            let home_dcs = home_wh
                .get_item_count(typ_name.clone())
                .context("home get_item_count")?;
            if home_dcs > home_prev {
                home_wh
                    .set_item(typ_name.clone(), home_prev)
                    .context("clamp airframe at slot home")?;
                {
                    let home_obj = objective_mut!(self, home).context("home objective")?;
                    let inv = home_obj
                        .warehouse
                        .equipment
                        .get_or_default_cow(typ_name.clone());
                    inv.stored = home_prev;
                }
                info!(
                    "deslot airframe: clamped home {home:?} {typ_name} {home_dcs}->{home_prev} (ferry to {land_oid:?})"
                );
            }
        }

        land_wh
            .set_item(typ_name.clone(), target)
            .context("set airframe at landed warehouse")?;
        let land_after = land_wh
            .get_item_count(typ_name.clone())
            .unwrap_or(target);
        info!(
            "deslot airframe: set {typ_name} at {land_oid:?} to {target} (dcs_before={land_dcs} prev={land_prev} dcs_after={land_after})"
        );

        let mut sync: SmallVec<[String; 4]> = smallvec![typ_name.clone()];
        if !is_airbase && let Ok(unit) = Unit::get_instance(lua, objid) {
            for ammo in unit.get_ammo().context("get ammo")? {
                let ammo = ammo.context("ammo")?;
                let count = ammo.count().context("ammo count")?;
                let ammo_typ = ammo.type_name().context("ammo typ")?;
                sync.push(ammo_typ.clone());
                land_wh
                    .add_item(ammo_typ, count)
                    .context("add item to warehouse")?;
            }
        }

        {
            let obj = objective_mut!(self, land_oid).context("get objective")?;
            for name in &sync {
                let count = if name.as_str() == typ_name.as_str() {
                    target
                } else {
                    land_wh
                        .get_item_count(name.clone())
                        .context("getting item")?
                };
                let inv = obj.warehouse.equipment.get_or_default_cow(name.clone());
                inv.stored = count;
                if inv.capacity < count {
                    inv.capacity = count.max(1);
                }
            }
        }
        // DCS often applies warehouse return a beat after leave-unit — re-clamp once.
        let due = Utc::now() + Duration::seconds(2);
        self.ephemeral
            .pending_deslot_airframe_fix
            .push((due, land_oid, typ_name, target));
        self.ephemeral.dirty();
        Ok(())
    }

    pub fn process_pending_deslot_airframe_fixes(&mut self, lua: MizLua, now: DateTime<Utc>) {
        if self.ephemeral.pending_deslot_airframe_fix.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.ephemeral.pending_deslot_airframe_fix);
        let mut keep = Vec::new();
        for (due, oid, typ_name, target) in pending {
            if now < due {
                keep.push((due, oid, typ_name, target));
                continue;
            }
            if let Err(e) = self.apply_deslot_airframe_clamp(lua, oid, &typ_name, target) {
                warn!("deslot airframe deferred clamp {oid:?} {typ_name}: {e:?}");
            }
        }
        self.ephemeral.pending_deslot_airframe_fix = keep;
    }

    fn apply_deslot_airframe_clamp(
        &mut self,
        lua: MizLua,
        oid: ObjectiveId,
        typ_name: &str,
        target: u32,
    ) -> Result<()> {
        let id = maybe!(self.ephemeral.airbase_by_oid, oid, "airbase")?;
        let wh = Airbase::get_instance(lua, &id)
            .context("get airbase")?
            .get_warehouse()
            .context("get warehouse")?;
        let dcs = wh
            .get_item_count(String::from(typ_name))
            .context("get_item_count")?;
        if dcs == target {
            return Ok(());
        }
        wh.set_item(String::from(typ_name), target)
            .context("set_item clamp")?;
        {
            let obj = objective_mut!(self, oid).context("objective")?;
            let inv = obj
                .warehouse
                .equipment
                .get_or_default_cow(String::from(typ_name));
            inv.stored = target;
            if inv.capacity < target {
                inv.capacity = target.max(1);
            }
        }
        self.ephemeral.dirty();
        info!(
            "airframe warehouse clamp {typ_name} at {oid:?} {dcs}->{target}"
        );
        Ok(())
    }

    pub fn mark_landed_at_objective_from_place(&mut self, slot: &SlotId, oid: ObjectiveId) {
        let Some(ucid) = self.ephemeral.player_in_slot(slot).cloned() else {
            return;
        };
        let side_ok = self
            .persisted
            .players
            .get(&ucid)
            .zip(self.persisted.objectives.get(&oid))
            .is_some_and(|(p, o)| o.owner == p.side);
        if !side_ok {
            return;
        }
        let Some(player) = self.persisted.players.get_mut_cow(&ucid) else {
            return;
        };
        if let Some((_, Some(inst))) = player.current_slot.as_mut() {
            inst.landed_at_objective = Some(oid);
            inst.stopped_at_objective = true;
        }
    }

    /// DCS returned the airframe to a warehouse but life was not returned — align Fowl stock.
    fn align_fowl_airframe_to_dcs_after_deslot(
        &mut self,
        lua: MizLua,
        side: Side,
        slot: &SlotId,
        typ: &Vehicle,
        hint_pos: Option<Vector2>,
    ) -> Result<()> {
        let typ_name = typ.0.clone();
        let home = self.ephemeral.slot_info.get(slot).map(|s| s.objective);
        let mut candidates: SmallVec<[ObjectiveId; 4]> = smallvec![];
        if let Some(oid) = home {
            candidates.push(oid);
        }
        if let Some(pos) = hint_pos {
            if let Some(oid) = self.nearest_friendly_airbase_objective(lua, side, pos) {
                candidates.push(oid);
            }
            if let Some(oid) = self.resolve_friendly_land_objective(lua, side, pos) {
                candidates.push(oid);
            }
        }
        candidates.sort_unstable();
        candidates.dedup();

        let mut bumps: SmallVec<[(ObjectiveId, u32, u32, f64); 4]> = smallvec![];
        for oid in &candidates {
            let Some(ab_id) = self.ephemeral.airbase_by_oid.get(oid) else {
                continue;
            };
            let Ok(ab) = Airbase::get_instance(lua, ab_id) else {
                continue;
            };
            let Ok(wh) = ab.get_warehouse() else {
                continue;
            };
            let Ok(dcs) = wh.get_item_count(typ_name.clone()) else {
                continue;
            };
            let prev = self
                .persisted
                .objectives
                .get(oid)
                .and_then(|o| o.warehouse.equipment.get(&typ_name))
                .map(|i| i.stored)
                .unwrap_or(0);
            if dcs <= prev {
                continue;
            }
            let dist = hint_pos
                .and_then(|pos| {
                    ab.get_point().ok().map(|pt| {
                        (Vector2::new(pt.x, pt.z) - pos).magnitude_squared()
                    })
                })
                .unwrap_or(f64::MAX);
            bumps.push((*oid, prev, dcs, dist));
        }
        if bumps.is_empty() {
            return Ok(());
        }
        bumps.sort_by(|a, b| {
            a.3
                .partial_cmp(&b.3)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| (b.2 - b.1).cmp(&(a.2 - a.1)))
        });
        let (oid, prev, dcs, _) = bumps[0];
        {
            let obj = objective_mut!(self, oid)?;
            let inv = obj
                .warehouse
                .equipment
                .get_or_default_cow(typ_name.clone());
            inv.stored = dcs;
            if inv.capacity < dcs {
                inv.capacity = dcs.max(1);
            }
        }
        self.ephemeral.dirty();
        info!(
            "deslot airframe: aligned Fowl {typ_name} at {oid:?} {prev}->{dcs} (DCS warehouse, life not returned)"
        );
        Ok(())
    }

    pub fn player_left_unit(
        &mut self,
        lua: MizLua,
        now: DateTime<Utc>,
        objid: &DcsOid<ClassUnit>,
    ) -> Result<(
        Vec<DcsOid<ClassUnit>>,
        Option<(Ucid, SlotId, LifeType)>,
        Option<(Ucid, SlotId)>,
    )> {
        let mut dead = vec![];
        let mut returned_life = None;
        let mut deslot = None;
        if let Some(uid) = self.ephemeral.uid_by_object_id.get(objid) {
            let uid = *uid;
            match self.update_unit_positions(lua, now, &[uid]) {
                Ok(v) => dead = v,
                Err(e) => error!("could not sync final CA unit position {e}"),
            }
            self.ephemeral.units_able_to_move.swap_remove(&uid);
        }
        if let Some(slot) = self.ephemeral.slot_by_object_id.get(&objid) {
            if let Some(ucid) = self.ephemeral.player_in_slot(slot) {
                let ucid = ucid.clone();
                let slot = *slot;
                self.delete_fowl_crates_on_carrier_deslot(Some(lua), &ucid, Some(&slot));
                deslot = Some((ucid.clone(), slot));
                let (side, typ, flagged_oid, stopped_at_objective) = {
                    let player = maybe_mut!(self.persisted.players, ucid, "player")?;
                    let inst = if let Some((cur_slot, Some(inst))) = player.current_slot.as_ref() {
                        if *cur_slot == slot {
                            Some(inst.clone())
                        } else {
                            self.ephemeral.pending_aircraft_deslot.get(&slot).cloned()
                        }
                    } else {
                        self.ephemeral.pending_aircraft_deslot.get(&slot).cloned()
                    };
                    let Some(inst) = inst else {
                        return Ok((dead, returned_life, deslot));
                    };
                    (
                        player.side,
                        inst.typ.clone(),
                        inst.landed_at_objective,
                        inst.stopped_at_objective,
                    )
                };

                let unit = Unit::get_instance(lua, objid).ok();
                let on_ground = unit
                    .as_ref()
                    .map(|u| u.is_exist().unwrap_or(false) && !u.in_air().unwrap_or(true))
                    .unwrap_or(false);
                let pos = unit
                    .as_ref()
                    .and_then(|u| u.get_ground_position().ok())
                    .map(|p| p.0);

                let land_oid = if on_ground {
                    pos.and_then(|p| self.resolve_friendly_land_objective(lua, side, p))
                        .or(flagged_oid)
                } else if flagged_oid.is_some() && stopped_at_objective {
                    flagged_oid
                } else {
                    None
                };

                if let Some(oid) = land_oid {
                    if let Some(player) = self.persisted.players.get_mut_cow(&ucid) {
                        if let Some((cur_slot, Some(inst))) = player.current_slot.as_mut() {
                            if *cur_slot == slot {
                                inst.landed_at_objective = Some(oid);
                            }
                        }
                    }
                    if self.ephemeral.cfg.limited_lives {
                        if let Some(life_type) = self.ephemeral.cfg.life_types.get(&typ).copied() {
                            let player = maybe_mut!(self.persisted.players, ucid, "player")?;
                            if let Some((_, player_lives)) = player.lives.get_mut_cow(&life_type) {
                                *player_lives += 1;
                                if *player_lives
                                    >= self.ephemeral.cfg.default_lives[&life_type].0
                                {
                                    player.lives.remove_cow(&life_type);
                                }
                                self.ephemeral.stat(Stat::Life {
                                    id: ucid,
                                    lives: player.lives.clone(),
                                });
                                returned_life = Some((ucid, slot, life_type));
                                info!(
                                    "life returned on deslot for {ucid} ({life_type}) at {oid:?}"
                                );
                                self.ephemeral.dirty();
                            }
                        }
                    }
                    if let Err(e) =
                        self.credit_airframe_on_deslot(lua, objid, oid, &slot, &typ)
                    {
                        error!("unable to fix warehouse {:?}", e)
                    }
                } else if let Err(e) = self.align_fowl_airframe_to_dcs_after_deslot(
                    lua,
                    side,
                    &slot,
                    &typ,
                    pos,
                ) {
                    error!("unable to align warehouse after deslot {:?}", e)
                }
            }
        }
        Ok((dead, returned_life, deslot))
    }

    pub fn player_disconnected(&mut self, ucid: &Ucid) {
        self.campaign_on_disconnect(ucid, Utc::now());
        super::ai_air::extend_active_owner_locks_for_player(self, ucid);
        if let Some((_, Some(inst))) = self
            .persisted
            .players
            .get(&ucid)
            .and_then(|p| p.current_slot.as_ref())
        {
            if let Some(oid) = inst.landed_at_objective {
                self.ephemeral.push_sync_warehouse(oid, inst.typ.clone());
            }
        }
        self.delete_fowl_crates_on_carrier_deslot(None, ucid, None);
        self.ephemeral.stat(Stat::Disconnect { id: *ucid });
        self.player_deslot(ucid);
    }

    fn apply_teamkill_penalty(
        &mut self,
        shooter: Ucid,
        total_points: u32,
        victim_info: &Option<VictimInfo>,
    ) -> CompactString {
        let side = self.persisted.players.get(&shooter).map(|p| p.side);
        let window = self
            .ephemeral
            .cfg
            .points
            .as_ref()
            .map(|p| p.tk_window as i64)
            .unwrap_or(0);
        let now = Utc::now();
        match victim_info.as_ref() {
            None => {
                let (tp, deducted) = {
                    let player = &mut self.persisted.players[&shooter];
                    let penalty: u32 = player
                        .ai_team_kills
                        .into_iter()
                        .map(|ts| total_points >> ((now - *ts).num_hours() / window))
                        .sum();
                    let deducted = total_points + penalty;
                    player.points -= deducted as i32;
                    player.ai_team_kills.insert_cow(now);
                    (player.points, deducted)
                };
                if let Some(side) = side {
                    self.campaign_on_active_gain(side, -(deducted as i64));
                }
                format_compact!("{tp}(-{deducted}) points, you have killed a friendly unit")
            }
            Some(VictimInfo {
                name,
                life_type: None,
                ai_deployable: false,
                ucid: _,
            }) => {
                let tp = {
                    let player = &mut self.persisted.players[&shooter];
                    player.points -= total_points as i32;
                    player.points
                };
                if let Some(side) = side {
                    self.campaign_on_active_gain(side, -(total_points as i64));
                }
                format_compact!(
                    "{}(-{total_points})you have team killed {name} on the ground",
                    tp
                )
            }
            Some(VictimInfo {
                name,
                life_type: None,
                ai_deployable: true,
                ucid: _,
            }) => {
                let tp = {
                    let player = &mut self.persisted.players[&shooter];
                    player.points -= total_points as i32;
                    player.points
                };
                if let Some(side) = side {
                    self.campaign_on_active_gain(side, -(total_points as i64));
                }
                format_compact!(
                    "{}(-{total_points})you have team killed {name}'s ai unit",
                    tp
                )
            }
            Some(VictimInfo {
                ucid,
                name,
                life_type: Some(life_type),
                ..
            }) => {
                let (tp, penalty_points, lives_snapshot, deplane, lost) = {
                    let player = &mut self.persisted.players[&shooter];
                    let (penalty_points, penalty_lives): (u32, f32) = player
                        .player_team_kills
                        .into_iter()
                        .fold((total_points, 1.), |(pp, pl), (ts, _)| {
                            let windows = (now - *ts).num_hours() / window;
                            let pp = pp + (total_points >> windows);
                            let pl = pl + (1. / (max(1, windows * 2) as f32));
                            (pp, pl)
                        });
                    let deplane_possible = penalty_lives > 1.5;
                    let mut penalty_lives = penalty_lives.round() as u32;
                    let mut lost: SmallVec<[(LifeType, u8); 5]> = smallvec![];
                    let mut life_type = *life_type;
                    let deplane = loop {
                        let (_, player_lives) = player.lives.get_or_insert_cow(life_type, || {
                            (Utc::now(), self.ephemeral.cfg.default_lives[&life_type].0)
                        });
                        if *player_lives as u32 >= penalty_lives {
                            lost.push((life_type, penalty_lives as u8));
                            *player_lives -= penalty_lives as u8;
                            break false;
                        } else {
                            if *player_lives > 0 {
                                lost.push((life_type, *player_lives));
                            }
                            penalty_lives -= *player_lives as u32;
                            *player_lives = 0;
                            match life_type.up() {
                                None => break deplane_possible,
                                Some(lt) => {
                                    life_type = lt;
                                }
                            }
                        }
                    };
                    let lives_snapshot = player.lives.clone();
                    player.points -= penalty_points as i32;
                    player.player_team_kills.insert_cow(now, *ucid);
                    let tp = player.points;
                    self.ephemeral.dirty();
                    (tp, penalty_points, lives_snapshot, deplane, lost)
                };
                self.ephemeral.stat(Stat::Life {
                    id: shooter,
                    lives: lives_snapshot,
                });
                if let Some(side) = side {
                    self.campaign_on_active_gain(side, -(penalty_points as i64));
                }
                use std::fmt::Write;
                let mut msg = CompactString::from("");
                write!(
                    msg,
                    "{tp}(-{penalty_points}) points, you have team killed {name}.\n",
                )
                .unwrap();
                if lost.len() > 0 {
                    write!(msg, "\nYou have lost\n").unwrap();
                    for (ty, n) in lost {
                        write!(msg, "{n} {ty} life\n").unwrap()
                    }
                };
                if deplane {
                    write!(msg, "Shortly you will be deplaned\n").unwrap();
                    write!(
                        msg,
                        "your death may be monitored for quality assurance purposes\n"
                    )
                    .unwrap();
                    write!(msg, "have a nice day").unwrap();
                    self.ephemeral
                        .force_player_to_spectators_at(&shooter, now + Duration::seconds(30));
                }
                msg
            }
        }
    }

    fn victim_group_id(&self, victim: &Who) -> Option<GroupId> {
        match victim {
            Who::AI { gid, .. } => Some(*gid),
            Who::Player { unit, .. } => self
                .ephemeral
                .get_uid_by_object_id(unit)
                .and_then(|uid| self.persisted.units.get(uid).map(|u| u.group)),
        }
    }

    fn dead_target_unit_tags(&self, dead: &Dead) -> Option<UnitTags> {
        if let Some(gid) = self.victim_group_id(&dead.victim) {
            if let Some(group) = self.persisted.groups.get(&gid) {
                for uid in group.units.clone().into_iter() {
                    let unit = self.persisted.units.get(uid)?;
                    if let Some(tags) = self
                        .ephemeral
                        .cfg
                        .unit_classification
                        .get(unit.typ.as_str())
                    {
                        if tags.contains(UnitTag::ShipCarrier)
                            || tags.contains(UnitTag::ShipWithHeliport)
                            || tags.contains(UnitTag::ShipNoHeliport)
                        {
                            return Some(*tags);
                        }
                    }
                }
            }
        }
        dead.shots
            .iter()
            .find(|s| !s.target_typ.trim().is_empty())
            .and_then(|s| {
                self.ephemeral
                    .cfg
                    .unit_classification
                    .get(s.target_typ.as_str())
            })
            .copied()
            .filter(|tags| {
                tags.contains(UnitTag::ShipCarrier)
                    || tags.contains(UnitTag::ShipWithHeliport)
                    || tags.contains(UnitTag::ShipNoHeliport)
            })
    }

    fn ship_kill_points(&self, cfg: &PointsCfg, dead: &Dead) -> Option<u32> {
        let tags = self.dead_target_unit_tags(dead)?;
        if tags.contains(UnitTag::ShipCarrier) {
            Some(cfg.carrier_kill)
        } else if tags.contains(UnitTag::ShipWithHeliport) {
            Some(cfg.ships_with_heliport_kill)
        } else if tags.contains(UnitTag::ShipNoHeliport) {
            Some(cfg.ships_no_heliport_kill)
        } else {
            None
        }
    }

    pub fn award_kill_points(&mut self, cfg: &PointsCfg, dead: &Dead) {
        let mut hit_by: SmallVec<[(Ucid, bool); 16]> = smallvec![];
        let valid_shots = || {
            // why are you hitting yourself
            dead.shots
                .iter()
                .filter(|shot| match (&shot.shooter, &shot.target) {
                    (Who::AI { gid: g0, .. }, Who::AI { gid: g1, .. }) => g0 != g1,
                    (Who::Player { ucid: u0, .. }, Who::Player { ucid: u1, .. }) => u0 != u1,
                    (
                        Who::AI {
                            ucid: Some(u0),
                            side: s0,
                            ..
                        },
                        Who::Player {
                            side: s1, ucid: u1, ..
                        },
                    ) => u0 != u1 && s0 != s1,
                    (Who::Player { ucid: u1, .. }, Who::AI { ucid: Some(u0), .. }) => u0 != u1,
                    (Who::AI { .. }, Who::Player { .. }) | (Who::Player { .. }, Who::AI { .. }) => {
                        true
                    }
                })
        };
        for shot in valid_shots() {
            let k = match shot.shooter {
                Who::Player { ucid, .. } => (ucid, cfg.provisional),
                Who::AI { ucid, .. } => match ucid {
                    Some(ucid) => (ucid, false),
                    None => continue,
                },
            };
            if shot.hit && !hit_by.contains(&k) {
                hit_by.push(k)
            }
        }
        // Force-to-crash / no-hit credit only when nobody (incl. AI SAM) landed a hit.
        if hit_by.is_empty() && !dead.has_enemy_hit() {
            for shot in valid_shots() {
                let k = match shot.shooter {
                    Who::Player { ucid, .. } => (ucid, cfg.provisional),
                    Who::AI { ucid, .. } => match ucid {
                        Some(ucid) => (ucid, false),
                        None => continue,
                    },
                };
                if dead.time - shot.time <= Duration::minutes(3) && !hit_by.contains(&k) {
                    hit_by.push(k);
                }
            }
        }
        if !hit_by.is_empty() {
            let total_points = self.ship_kill_points(cfg, dead).unwrap_or_else(|| {
                    (&dead.shots)
                        .into_iter()
                        .find(|s| s.target_typ.trim() != "")
                        .map(|s| &s.target_typ)
                        .and_then(|typ| {
                            self.ephemeral.cfg.unit_classification.get(typ.as_str())
                        })
                        .map(|tags| {
                            if tags.contains(UnitTag::LR | UnitTag::TrackRadar | UnitTag::SAM) {
                                cfg.ground_kill + cfg.lr_sam_bonus
                            } else if tags.contains(UnitTag::EWR) {
                                cfg.ewr_kill
                            } else if tags.contains(UnitTag::AWACS) {
                                cfg.awacs_kill
                            } else if tags.contains(UnitTag::Aircraft)
                                || tags.contains(UnitTag::Helicopter)
                            {
                                cfg.air_kill
                            } else {
                                cfg.ground_kill
                            }
                        })
                        .unwrap_or(cfg.ground_kill)
                });
            let pps = (total_points as f32 / hit_by.len() as f32).ceil() as i32;
            let victim_info = match &dead.victim {
                Who::Player { ucid, unit, .. } => self.persisted.players.get(ucid).map(|p| {
                    let life_type = if self.is_combined_arms_life_unit(unit) {
                        Some(LifeType::CombinedArms)
                    } else {
                        p.airborne
                    };
                    VictimInfo {
                        ucid: *ucid,
                        name: p.name.clone(),
                        life_type,
                        ai_deployable: false,
                    }
                }),
                Who::AI { ucid: None, .. } => None,
                Who::AI { ucid: Some(i), .. } => {
                    self.persisted.players.get(i).map(|p| VictimInfo {
                        ucid: *i,
                        name: p.name.clone(),
                        life_type: None,
                        ai_deployable: true,
                    })
                }
            };
            for (ucid, provisional) in hit_by {
                let side = self.persisted.players.get(&ucid).map(|p| p.side);
                let msg = if side == Some(*dead.victim.side()) {
                    self.apply_teamkill_penalty(ucid, total_points, &victim_info)
                } else {
                    let (tp, award_pps) = {
                        let Some(player) = self.persisted.players.get_mut_cow(&ucid) else {
                            continue;
                        };
                        let tp = if provisional {
                            player.provisional_points += pps;
                            player.provisional_points
                        } else {
                            player.points += pps;
                            player.points
                        };
                        (tp, if provisional { 0 } else { pps })
                    };
                    if award_pps != 0 {
                        if let Some(side) = side {
                            self.campaign_on_active_gain(side, award_pps as i64);
                        }
                    }
                    let pm = if provisional { " provisional" } else { "" };
                    match &victim_info {
                        None => format_compact!("{tp}(+{pps}){pm} points"),
                        Some(vi) => {
                            if vi.ai_deployable {
                                format_compact!(
                                    "{tp}(+{pps}){pm} points, killed {}'s deployed ai unit",
                                    vi.name
                                )
                            } else {
                                format_compact!("{tp}(+{pps}){pm} points, killed {}", vi.name)
                            }
                        }
                    }
                };
                debug!("{ucid} kill message: {msg}");
                self.ephemeral
                    .panel_to_player(&self.persisted, 10, &ucid, msg)
            }
        }
    }

    pub fn adjust_points(&mut self, ucid: &Ucid, amount: i32, why: &str) {
        if let Some(player) = self.persisted.players.get_mut_cow(ucid) {
            player.points += amount;
            let pp = player.points;
            if amount != 0 {
                self.campaign_on_adjust_points(ucid, amount, why);
                let m = format_compact!("{}({}) points {}", pp, amount, why);
                self.ephemeral.stat(Stat::Points {
                    points: amount,
                    reason: m.clone().into(),
                    id: *ucid,
                });
                self.ephemeral.panel_to_player(&self.persisted, 10, ucid, m);
                self.ephemeral.dirty();
            }
        }
    }

    /// Ground unit eligible for Combined Arms lives (deployable, troop, or objective defense).
    pub fn is_combined_arms_life_unit(&self, id: &DcsOid<ClassUnit>) -> bool {
        self.ephemeral
            .uid_by_object_id
            .get(id)
            .and_then(|uid| self.persisted.units.get(uid))
            .and_then(|unit| self.persisted.groups.get(&unit.group))
            .is_some_and(|g| {
                matches!(
                    g.origin,
                    DeployKind::Deployed { .. }
                        | DeployKind::Troop { .. }
                        | DeployKind::Objective { .. }
                        | DeployKind::ObjectiveDeprecated
                )
            })
    }

    pub fn ca_controller(&self, id: &DcsOid<ClassUnit>) -> Option<Ucid> {
        self.ephemeral.ca_controller_by_oid.get(id).copied()
    }

    /// True when this player already has a Combined Arms life in escrow (from enter).
    pub fn ca_player_has_escrow(&self, ucid: &Ucid) -> bool {
        self.ephemeral.ca_oid_by_controller.contains_key(ucid)
    }

    pub fn clear_ca_control(&mut self, id: &DcsOid<ClassUnit>) {
        if let Some(ucid) = self.ephemeral.ca_controller_by_oid.remove(id) {
            if self
                .ephemeral
                .ca_oid_by_controller
                .get(&ucid)
                .is_some_and(|oid| oid == id)
            {
                self.ephemeral.ca_oid_by_controller.remove(&ucid);
            }
        }
    }

    pub fn register_ca_control(&mut self, id: DcsOid<ClassUnit>, ucid: Ucid) {
        if let Some(old_oid) = self.ephemeral.ca_oid_by_controller.insert(ucid, id.clone()) {
            self.ephemeral.ca_controller_by_oid.remove(&old_oid);
        }
        if let Some(old_ucid) = self.ephemeral.ca_controller_by_oid.insert(id, ucid) {
            if old_ucid != ucid {
                self.ephemeral.ca_oid_by_controller.remove(&old_ucid);
            }
        }
    }

    fn ca_lives_configured(&self) -> bool {
        self.ephemeral.cfg.limited_lives
            && self
                .ephemeral
                .cfg
                .default_lives
                .contains_key(&LifeType::CombinedArms)
    }

    /// Remaining Combined Arms lives after applying reset timer (None = unlimited / not configured).
    pub fn combined_arms_lives_remaining(&mut self, ucid: &Ucid, now: DateTime<Utc>) -> Option<u8> {
        if !self.ca_lives_configured() {
            return None;
        }
        let &(max_lives, reset_secs) = self
            .ephemeral
            .cfg
            .default_lives
            .get(&LifeType::CombinedArms)?;
        let player = self.persisted.players.get_mut_cow(ucid)?;
        let (since, n) = player
            .lives
            .get_or_insert_cow(LifeType::CombinedArms, || (now, max_lives));
        if reset_secs > 0 && (now - *since).num_seconds() >= reset_secs as i64 {
            *since = now;
            *n = max_lives;
        }
        Some(*n)
    }

    fn take_combined_arms_life(&mut self, ucid: &Ucid, now: DateTime<Utc>) -> Result<()> {
        let &(max_lives, reset_secs) = self
            .ephemeral
            .cfg
            .default_lives
            .get(&LifeType::CombinedArms)
            .ok_or_else(|| anyhow!("CombinedArms lives not configured"))?;
        let player = self
            .persisted
            .players
            .get_mut_cow(ucid)
            .ok_or_else(|| anyhow!("no player {ucid}"))?;
        let (since, n) = player
            .lives
            .get_or_insert_cow(LifeType::CombinedArms, || (now, max_lives));
        if reset_secs > 0 && (now - *since).num_seconds() >= reset_secs as i64 {
            *since = now;
            *n = max_lives;
        }
        if *n == 0 {
            bail!("no Combined Arms lives remaining");
        }
        *n -= 1;
        info!(
            "life taken on Combined Arms enter for {ucid}, remaining {}",
            *n
        );
        let lives_snap = player.lives.clone();
        self.ephemeral.stat(Stat::Life {
            id: *ucid,
            lives: lives_snap,
        });
        self.ephemeral.dirty();
        Ok(())
    }

    fn return_combined_arms_life(&mut self, ucid: &Ucid) -> bool {
        if !self.ca_lives_configured() {
            return false;
        }
        let max_lives = self.ephemeral.cfg.default_lives[&LifeType::CombinedArms].0;
        let Some(player) = self.persisted.players.get_mut_cow(ucid) else {
            return false;
        };
        let Some((_, n)) = player.lives.get_mut_cow(&LifeType::CombinedArms) else {
            return false;
        };
        *n += 1;
        if *n >= max_lives {
            player.lives.remove_cow(&LifeType::CombinedArms);
        }
        let lives_snap = player.lives.clone();
        self.ephemeral.stat(Stat::Life {
            id: *ucid,
            lives: lives_snap,
        });
        self.ephemeral.dirty();
        info!("life returned on Combined Arms leave for {ucid}");
        true
    }

    /// Direct control enter (`PlayerEnterUnit`): escrow one Combined Arms life.
    /// Do not call from Shot / map-command paths — AI fire must not take lives.
    pub fn on_combined_arms_enter(
        &mut self,
        id: DcsOid<ClassUnit>,
        ucid: Ucid,
        now: DateTime<Utc>,
    ) -> Result<CaEnterRes> {
        if !self.is_combined_arms_life_unit(&id) {
            return Ok(CaEnterRes::NotApplicable);
        }
        // Already escrowed for this player (unit switch): remount only.
        if self.ephemeral.ca_oid_by_controller.get(&ucid) == Some(&id)
            || self.ephemeral.ca_oid_by_controller.contains_key(&ucid)
        {
            self.register_ca_control(id, ucid);
            return Ok(CaEnterRes::Ok);
        }
        // Displaced previous controller keeps their escrow until leave/dead of their unit;
        // register_ca_control drops the old mapping for this unit — return that life.
        if let Some(old_ucid) = self.ephemeral.ca_controller_by_oid.get(&id).copied() {
            if old_ucid != ucid {
                self.clear_ca_control(&id);
                self.return_combined_arms_life(&old_ucid);
            }
        }
        if self.ca_lives_configured() {
            if let Some(0) = self.combined_arms_lives_remaining(&ucid, now) {
                self.ephemeral.panel_to_player(
                    &self.persisted,
                    15,
                    &ucid,
                    "no Combined Arms lives remaining; wait for life reset",
                );
                return Ok(CaEnterRes::Rejected);
            }
            self.take_combined_arms_life(&ucid, now)?;
            self.register_ca_control(id, ucid);
            return Ok(CaEnterRes::LifeTaken);
        }
        self.register_ca_control(id, ucid);
        Ok(CaEnterRes::Ok)
    }

    /// Player left a CA unit. Returns life when `return_life` (unit still alive).
    pub fn on_combined_arms_leave(
        &mut self,
        id: &DcsOid<ClassUnit>,
        return_life: bool,
    ) -> Option<(Ucid, bool)> {
        let ucid = self.ephemeral.ca_controller_by_oid.remove(id)?;
        if self
            .ephemeral
            .ca_oid_by_controller
            .get(&ucid)
            .is_some_and(|oid| oid == id)
        {
            self.ephemeral.ca_oid_by_controller.remove(&ucid);
        }
        let returned = return_life && self.return_combined_arms_life(&ucid);
        Some((ucid, returned))
    }

    /// Controlled CA unit destroyed: escrow stays spent (no second deduct, no return).
    pub fn on_combined_arms_unit_dead(&mut self, id: &DcsOid<ClassUnit>) {
        let Some(ucid) = self.ephemeral.ca_controller_by_oid.remove(id) else {
            return;
        };
        self.ephemeral.ca_oid_by_controller.remove(&ucid);
        info!("combined arms unit dead for {ucid}; life escrow kept");
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaEnterRes {
    NotApplicable,
    Rejected,
    Ok,
    LifeTaken,
}
