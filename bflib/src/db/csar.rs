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

use super::{Db, player::InstancedPlayer};
use crate::msgq::MsgQ;
use anyhow::{Context, Result};
use bfprotocols::cfg::{LifeType, Vehicle};
use bfprotocols::stats::EnId;
use chrono::prelude::*;
use dcso3::{
    coalition::Side,
    controller::Command,
    coord::Coord,
    net::Ucid,
    object::{DcsObject, DcsOid, ObjectCategory},
    trigger::{CircleSpec, LineType, MarkId},
    unit::{ClassUnit, Unit},
    world::{SearchVolume, World},
    Color, LuaVec3, MizLua, String,
};
use fxhash::FxHashMap;
use log::{info, warn};
use serde_derive::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::cell::RefCell;
use std::rc::Rc;

pub const LIFE_TYPE_DISPLAY_ORDER: [LifeType; 5] = [
    LifeType::Standard,
    LifeType::Attack,
    LifeType::Intercept,
    LifeType::Recon,
    LifeType::Logistics,
];

pub fn life_type_display_label(lt: LifeType) -> &'static str {
    match lt {
        LifeType::Standard => "Standard",
        LifeType::Attack => "Attack",
        LifeType::Intercept => "Intercept",
        LifeType::Recon => "Recon",
        LifeType::Logistics => "Logistics",
    }
}

const CSAR_LANDED_CIRCLE_RADIUS_M: f64 = 500.;
const CSAR_PILOT_REBIND_RADIUS_M: f64 = 150.;

/// Downed pilot awaiting CSAR (persisted; future pickup/rescue hooks).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CsarDowned {
    pub pilot_unit: DcsOid<ClassUnit>,
    #[serde(default = "default_csar_life_type")]
    pub life_type: LifeType,
    pub aircraft_type: Vehicle,
    pub ejected_at: DateTime<Utc>,
    #[serde(default)]
    pub landed: bool,
    pub inst: InstancedPlayer,
    #[serde(default)]
    pub circle_mark_id: Option<MarkId>,
    #[serde(default)]
    pub point_mark_id: Option<MarkId>,
    /// Stored pilot unit id is stale (DCS replaces parachute with ground pilot).
    #[serde(default)]
    pub pilot_unit_stale: bool,
}

fn default_csar_life_type() -> LifeType {
    LifeType::Standard
}

fn csar_side_color(side: Side) -> Color {
    match side {
        Side::Red => Color::red(1.),
        Side::Blue => Color::blue(1.),
        Side::Neutral => Color::white(1.),
    }
}

fn csar_side_fill_color(side: Side) -> Color {
    match side {
        Side::Red => Color::red(0.35),
        Side::Blue => Color::blue(0.35),
        Side::Neutral => Color::white(0.35),
    }
}

fn csar_pilot_mark_label() -> String {
    String::from("downed pilot")
}

fn csar_pilot_mark_message(player_name: &str, life_type_label: &str) -> String {
    format!("{player_name} ({life_type_label})").into()
}

fn csar_pilot_circle_popup(player_name: &str, life_type_label: &str) -> String {
    format!(
        "downed pilot\n{}",
        csar_pilot_mark_message(player_name, life_type_label)
    )
    .into()
}

fn delete_csar_marks(msgs: &mut MsgQ, csar: &mut CsarDowned) {
    if let Some(id) = csar.circle_mark_id.take() {
        msgs.delete_mark(id);
    }
    if let Some(id) = csar.point_mark_id.take() {
        msgs.delete_mark(id);
    }
}

fn hide_csar_pilot_unit(pilot: &Unit) {
    let _ = pilot.set_name(String::from(""));
    if let Ok(ctrl) = pilot.get_controller() {
        if let Err(e) = ctrl.set_command(Command::SetInvisible(true)) {
            warn!("csar: SetInvisible failed: {e:?}");
        }
    }
}

fn csar_pilot_type_name(unit: &Unit) -> Option<String> {
    unit.get_type_name().ok()
}

fn is_csar_pilot_type(type_name: &str) -> bool {
    type_name.contains("Pilot") || type_name.contains("Parachute")
}

fn rebind_csar_pilot_unit(
    lua: MizLua,
    csar: &CsarDowned,
) -> Result<Option<DcsOid<ClassUnit>>> {
    let center = LuaVec3(csar.inst.position.p.0);
    let vol = SearchVolume::Sphere {
        point: center,
        radius: CSAR_PILOT_REBIND_RADIUS_M,
    };
    let found = Rc::new(RefCell::new(None::<DcsOid<ClassUnit>>));
    let found_cb = found.clone();
    World::singleton(lua)?.search_objects(ObjectCategory::Unit, vol, mlua::Value::Nil, move |_, obj, _| {
        let unit = match obj.as_unit() {
            Ok(u) => u,
            Err(_) => return Ok(true),
        };
        if !unit.is_exist().unwrap_or(false) {
            return Ok(true);
        }
        let Some(type_name) = csar_pilot_type_name(&unit) else {
            return Ok(true);
        };
        if !is_csar_pilot_type(&type_name) {
            return Ok(true);
        }
        if let Ok(id) = unit.object_id() {
            hide_csar_pilot_unit(&unit);
            *found_cb.borrow_mut() = Some(id);
        }
        Ok(true)
    })?;
    Ok(found.borrow().clone().filter(|id| *id != csar.pilot_unit))
}

fn set_csar_pilot_landed(csar: &mut CsarDowned, now: DateTime<Utc>, ucid: &Ucid) -> bool {
    if csar.landed {
        return false;
    }
    csar.landed = true;
    csar.inst.in_air = false;
    csar.inst.moved = Some(now);
    info!(
        "csar: downed pilot {ucid:?} ({}) on ground",
        csar.life_type
    );
    true
}

fn sync_csar_pilot_mark(
    msgs: &mut MsgQ,
    side: Side,
    player_name: &str,
    life_type_label: &str,
    csar: &mut CsarDowned,
    force_reposition: bool,
) {
    if !csar.landed {
        delete_csar_marks(msgs, csar);
        return;
    }
    let pos = LuaVec3(csar.inst.position.p.0);
    let recreate = csar.circle_mark_id.is_none()
        || csar.point_mark_id.is_none()
        || force_reposition;
    if !recreate {
        return;
    }
    delete_csar_marks(msgs, csar);
    let mark_body = csar_pilot_mark_message(player_name, life_type_label);
    let circle_id = MarkId::new();
    let spec = CircleSpec {
        center: pos,
        radius: CSAR_LANDED_CIRCLE_RADIUS_M,
        color: csar_side_color(side),
        fill_color: csar_side_fill_color(side),
        line_type: LineType::Solid,
        read_only: true,
    };
    msgs.circle_to_side(
        side,
        circle_id,
        spec,
        Some(csar_pilot_circle_popup(player_name, life_type_label)),
    );
    let point_id = MarkId::new();
    msgs.coalition_point_mark(
        side,
        point_id,
        pos,
        true,
        csar_pilot_mark_label(),
        Some(mark_body),
    );
    msgs.set_markup_color(point_id, csar_side_color(side));
    csar.circle_mark_id = Some(circle_id);
    csar.point_mark_id = Some(point_id);
}

impl Db {
    pub fn csar_enabled(&self) -> bool {
        self.ephemeral.cfg.csar.enabled
    }

    pub fn csar_downed_pilot(&self, ucid: &Ucid) -> bool {
        self.csar_enabled()
            && self
                .persisted
                .players
                .get(ucid)
                .is_some_and(|p| !p.csar_downed.is_empty())
    }

    pub fn csar_hidden_from_jtac(&self, ucid: &Ucid) -> bool {
        self.csar_downed_pilot(ucid)
    }

    pub fn csar_pilot_ucid(&self, pilot_unit: &DcsOid<ClassUnit>) -> Option<Ucid> {
        self.ephemeral
            .csar_pilot_unit
            .get(pilot_unit)
            .copied()
    }

    pub fn csar_active_ucids(&self) -> impl Iterator<Item = Ucid> + '_ {
        self.ephemeral.csar_pilot_unit.values().copied()
    }

    pub fn csar_downed_counts(&self, ucid: &Ucid) -> FxHashMap<LifeType, u32> {
        let mut counts = FxHashMap::default();
        let Some(player) = self.persisted.players.get(ucid) else {
            return counts;
        };
        for csar in &player.csar_downed {
            *counts.entry(csar.life_type).or_default() += 1;
        }
        counts
    }

    fn rebind_stale_csar_pilots(&mut self, lua: MizLua, ucid: &Ucid) -> Result<bool> {
        let stale: SmallVec<[(usize, DcsOid<ClassUnit>); 4]> =
            match self.persisted.players.get(ucid) {
                Some(p) => p
                    .csar_downed
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.pilot_unit_stale)
                    .map(|(i, c)| (i, c.pilot_unit.clone()))
                    .collect(),
                None => return Ok(false),
            };
        if stale.is_empty() {
            return Ok(false);
        }
        let mut rebound = false;
        for (idx, old_id) in stale {
            let csar = match self.persisted.players.get(ucid) {
                Some(p) => match p.csar_downed.get(idx) {
                    Some(c) => c.clone(),
                    None => continue,
                },
                None => break,
            };
            let Some(new_id) = rebind_csar_pilot_unit(lua, &csar)? else {
                continue;
            };
            self.ephemeral.csar_pilot_unit.remove(&old_id);
            self.ephemeral
                .csar_pilot_unit
                .insert(new_id.clone(), *ucid);
            if let Some(player) = self.persisted.players.get_mut_cow(ucid) {
                if let Some(entry) = player.csar_downed.get_mut(idx) {
                    entry.pilot_unit = new_id.clone();
                    entry.pilot_unit_stale = false;
                }
            }
            info!("csar: rebound downed pilot unit for {ucid:?} to {new_id:?}");
            rebound = true;
        }
        Ok(rebound)
    }

    fn destroy_csar_pilot_unit(lua: MizLua, pilot_unit: &DcsOid<ClassUnit>) {
        match Unit::get_instance(lua, pilot_unit) {
            Ok(unit) => {
                if unit.is_exist().unwrap_or(false) {
                    if let Err(e) = unit.destroy() {
                        warn!("csar: failed to destroy pilot unit {pilot_unit:?}: {e:?}");
                    }
                }
            }
            Err(e) => warn!("csar: pilot unit invalid for destroy {pilot_unit:?}: {e:?}"),
        }
    }

    fn queue_csar_pilot_destroy(&mut self, pilot_unit: DcsOid<ClassUnit>) {
        self.ephemeral
            .pending_csar_pilot_destroy
            .push(pilot_unit);
    }

    fn remove_csar_entry(&mut self, csar: &CsarDowned) {
        let mut csar = csar.clone();
        delete_csar_marks(self.ephemeral.msgs(), &mut csar);
        self.ephemeral.csar_pilot_unit.remove(&csar.pilot_unit);
        self.queue_csar_pilot_destroy(csar.pilot_unit.clone());
    }

    pub fn flush_pending_csar_destroys(&mut self, lua: MizLua) {
        if self.ephemeral.pending_csar_pilot_destroy.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.ephemeral.pending_csar_pilot_destroy);
        for pilot_unit in pending {
            Self::destroy_csar_pilot_unit(lua, &pilot_unit);
        }
    }

    fn csar_max_per_life_type(&self, life_type: LifeType) -> usize {
        self.ephemeral
            .cfg
            .default_lives
            .get(&life_type)
            .map(|(n, _)| *n as usize)
            .unwrap_or(1)
            .max(1)
    }

    fn trim_csar_downed_for_life_type(
        &mut self,
        ucid: &Ucid,
        life_type: LifeType,
    ) -> Result<()> {
        let max = self.csar_max_per_life_type(life_type);
        loop {
            let idx = {
                let player = self
                    .persisted
                    .players
                    .get(ucid)
                    .context("csar trim: no such player")?;
                if player
                    .csar_downed
                    .iter()
                    .filter(|c| c.life_type == life_type)
                    .count()
                    < max
                {
                    break;
                }
                player
                    .csar_downed
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| c.life_type == life_type)
                    .min_by_key(|(_, c)| c.ejected_at)
                    .map(|(i, _)| i)
            };
            let Some(idx) = idx else {
                break;
            };
            let csar = self
                .persisted
                .players
                .get_mut_cow(ucid)
                .context("csar trim: no such player")?
                .csar_downed
                .remove(idx);
            self.remove_csar_entry(&csar);
            info!("csar: trimmed oldest {life_type} downed pilot for {ucid:?}");
        }
        Ok(())
    }

    pub fn clear_csar_downed_for_life_type(&mut self, ucid: &Ucid, life_type: LifeType) {
        let Some(player) = self.persisted.players.get_mut_cow(ucid) else {
            return;
        };
        let removed: SmallVec<[CsarDowned; 4]> = player
            .csar_downed
            .extract_if(.., |c| c.life_type == life_type)
            .collect();
        if removed.is_empty() {
            return;
        }
        for csar in removed {
            self.remove_csar_entry(&csar);
        }
        self.ephemeral.dirty();
        info!("csar: cleared {life_type} downed pilots for {ucid:?}");
    }

    pub fn clear_csar_downed(&mut self, ucid: &Ucid) {
        let Some(player) = self.persisted.players.get_mut_cow(ucid) else {
            return;
        };
        let removed: SmallVec<[CsarDowned; 4]> = player.csar_downed.drain(..).collect();
        if removed.is_empty() {
            return;
        }
        for csar in removed {
            self.remove_csar_entry(&csar);
        }
        self.ephemeral.dirty();
        info!("csar: cleared all downed pilots for {ucid:?}");
    }

    fn sync_csar_marks_for_ucid(&mut self, ucid: &Ucid, force_reposition: bool) {
        let Some(player) = self.persisted.players.get(ucid) else {
            return;
        };
        if player.csar_downed.is_empty() {
            return;
        }
        let side = player.side;
        let name = player.name.clone();
        let len = player.csar_downed.len();
        let life_labels: SmallVec<[std::string::String; 4]> = player
            .csar_downed
            .iter()
            .map(|c| self.life_type_panel_label(c.life_type))
            .collect();
        let msgs = self.ephemeral.msgs();
        let Some(player) = self.persisted.players.get_mut_cow(ucid) else {
            return;
        };
        for (csar, life_type_label) in player.csar_downed[..len]
            .iter_mut()
            .zip(life_labels.iter())
        {
            sync_csar_pilot_mark(
                msgs,
                side,
                &name,
                life_type_label,
                csar,
                force_reposition,
            );
        }
    }

    pub fn on_csar_ejection(
        &mut self,
        lua: MizLua,
        aircraft: &Unit,
        pilot: &Unit,
        ucid: Ucid,
        now: DateTime<Utc>,
    ) -> Result<()> {
        if !self.csar_enabled() {
            return Ok(());
        }
        let _ = pilot.set_name(String::from(""));
        hide_csar_pilot_unit(pilot);
        let pilot_unit = pilot.object_id()?;
        let aircraft_id = aircraft.object_id()?;
        let slot = aircraft.slot()?;
        let typ = self
            .ephemeral
            .get_slot_info(&slot)
            .map(|s| s.typ.clone())
            .unwrap_or_else(|| Vehicle::from("unknown"));
        let life_type = self
            .ephemeral
            .cfg
            .life_types
            .get(&typ)
            .copied()
            .or_else(|| {
                self.persisted
                    .players
                    .get(&ucid)
                    .and_then(|p| p.airborne)
            })
            .unwrap_or(LifeType::Standard);
        let mut inst = self
            .persisted
            .players
            .get(&ucid)
            .and_then(|p| p.current_slot.as_ref())
            .and_then(|(_, i)| i.clone())
            .unwrap_or_default();
        inst.position = pilot.get_position()?;
        inst.velocity = pilot.get_velocity()?.0;
        inst.in_air = pilot.in_air()?;
        inst.moved = Some(now);
        if let Ok(name) = pilot.get_name() {
            inst.unit_name = name;
        }

        if let Some(old_slot) = self.ephemeral.get_slot_by_object_id(&aircraft_id).copied() {
            if let Some(id) = self.ephemeral.object_id_by_slot.remove(&old_slot) {
                self.ephemeral.slot_by_object_id.remove(&id);
            }
        }
        self.ephemeral
            .slot_by_object_id
            .remove(&aircraft_id);

        self.trim_csar_downed_for_life_type(&ucid, life_type)?;
        let player = self
            .persisted
            .players
            .get_mut_cow(&ucid)
            .context("csar ejection: no such player")?;
        player.csar_downed.push(CsarDowned {
            pilot_unit: pilot_unit.clone(),
            life_type,
            aircraft_type: typ,
            ejected_at: now,
            landed: !inst.in_air,
            inst,
            circle_mark_id: None,
            point_mark_id: None,
            pilot_unit_stale: false,
        });
        self.ephemeral
            .csar_pilot_unit
            .insert(pilot_unit.clone(), ucid);
        self.ephemeral.dirty();
        self.sync_csar_marks_for_ucid(&ucid, true);
        info!("csar: registered {life_type} downed pilot {ucid:?} at {pilot_unit:?}");
        let _ = lua;
        Ok(())
    }

    pub fn update_csar_downed_positions(
        &mut self,
        lua: MizLua,
        now: DateTime<Utc>,
        ucid: &Ucid,
    ) -> Result<()> {
        let pilot_units: SmallVec<[DcsOid<ClassUnit>; 4]> = match self.persisted.players.get(ucid) {
            Some(p) if !p.csar_downed.is_empty() => {
                p.csar_downed.iter().map(|c| c.pilot_unit.clone()).collect()
            }
            _ => return Ok(()),
        };
        let coord = Coord::singleton(lua)?;
        let mut mark_dirty = false;
        for pilot_unit in pilot_units {
            let instance = match Unit::get_instance(lua, &pilot_unit) {
                Ok(u) => u,
                Err(e) => {
                    if let Some(player) = self.persisted.players.get_mut_cow(ucid) {
                        if let Some(csar) = player
                            .csar_downed
                            .iter_mut()
                            .find(|c| c.pilot_unit == pilot_unit)
                        {
                            if !csar.pilot_unit_stale {
                                csar.pilot_unit_stale = true;
                                warn!(
                                    "csar: pilot unit stale for {ucid:?} {pilot_unit:?}: {e:?}"
                                );
                            }
                            if set_csar_pilot_landed(csar, now, ucid) {
                                mark_dirty = true;
                            }
                        }
                    }
                    continue;
                }
            };
            if !instance.is_exist().unwrap_or(false) {
                if let Some(player) = self.persisted.players.get_mut_cow(ucid) {
                    if let Some(csar) = player
                        .csar_downed
                        .iter_mut()
                        .find(|c| c.pilot_unit == pilot_unit)
                    {
                        if !csar.pilot_unit_stale {
                            csar.pilot_unit_stale = true;
                            warn!(
                                "csar: pilot unit no longer exists for {ucid:?} {pilot_unit:?}"
                            );
                        }
                        if set_csar_pilot_landed(csar, now, ucid) {
                            mark_dirty = true;
                        }
                    }
                }
                continue;
            }
            hide_csar_pilot_unit(&instance);
            let pos = instance.get_position()?;
            let velocity = instance.get_velocity()?.0;
            let in_air = instance.in_air()?;
            let Some(player) = self.persisted.players.get_mut_cow(ucid) else {
                return Ok(());
            };
            let Some(csar) = player
                .csar_downed
                .iter_mut()
                .find(|c| c.pilot_unit == pilot_unit)
            else {
                continue;
            };
            csar.pilot_unit_stale = false;
            let inst = &mut csar.inst;
            let moved = (inst.position.p.0 - pos.p.0).magnitude_squared() > 1.0;
            if moved {
                inst.position = pos;
                inst.velocity = velocity;
                inst.in_air = in_air;
                inst.moved = Some(now);
                mark_dirty = true;
                self.ephemeral.stat(bfprotocols::stats::Stat::Position {
                    id: EnId::Player(*ucid),
                    pos: bfprotocols::stats::Pos {
                        pos: coord.lo_to_ll(inst.position.p)?,
                        velocity: inst.velocity,
                    },
                });
            }
            if !in_air && !csar.landed {
                mark_dirty |= set_csar_pilot_landed(csar, now, ucid);
            }
        }
        if self.rebind_stale_csar_pilots(lua, ucid)? {
            mark_dirty = true;
        }
        if mark_dirty {
            self.sync_csar_marks_for_ucid(ucid, true);
        }
        self.ephemeral.dirty();
        Ok(())
    }

    pub fn on_csar_landing_after_ejection(
        &mut self,
        lua: MizLua,
        now: DateTime<Utc>,
        ucid: &Ucid,
    ) -> Result<()> {
        let Some(player) = self.persisted.players.get(ucid) else {
            return Ok(());
        };
        if player.csar_downed.is_empty() {
            return Ok(());
        }
        let mut mark_dirty = false;
        if let Some(player) = self.persisted.players.get_mut_cow(ucid) {
            for csar in &mut player.csar_downed {
                csar.pilot_unit_stale = true;
                if set_csar_pilot_landed(csar, now, ucid) {
                    mark_dirty = true;
                }
            }
        }
        if mark_dirty {
            self.sync_csar_marks_for_ucid(ucid, true);
        }
        self.update_csar_downed_positions(lua, now, ucid)
    }

    pub fn rebuild_csar_marks(&mut self) {
        if !self.csar_enabled() {
            return;
        }
        let ucids: SmallVec<[Ucid; 16]> = self
            .ephemeral
            .csar_pilot_unit
            .values()
            .copied()
            .collect();
        for ucid in ucids {
            self.sync_csar_marks_for_ucid(&ucid, true);
        }
    }
}
