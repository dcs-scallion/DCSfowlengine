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
use crate::{
    msgq::MsgQ,
    spawnctx::{SpawnCtx, Spawned},
};
use anyhow::{Context, Result, bail};
use bfprotocols::cfg::{LifeType, Vehicle};
use bfprotocols::stats::EnId;
use chrono::prelude::*;
use dcso3::{
    coalition::{Coalition, Side},
    controller::Command,
    coord::Coord,
    country,
    env::miz::{self, GroupKind, Miz, MizIndex, Skill},
    group::Group,
    group::GroupCategory,
    net::Ucid,
    object::{DcsObject, DcsOid},
    trigger::{CircleSpec, LineType, MarkId},
    unit::{ClassUnit, Unit},
    Position3,
    Color, LuaVec3, LuaEnv, MizLua, String,
};
use na::Vector2;
use fxhash::{FxHashMap, FxHashSet};
use log::{info, warn};
use mlua::{FromLua, Value};
use serde_derive::{Deserialize, Serialize};
use smallvec::SmallVec;

pub const LIFE_TYPE_DISPLAY_ORDER: [LifeType; 6] = [
    LifeType::Standard,
    LifeType::Attack,
    LifeType::Intercept,
    LifeType::Recon,
    LifeType::Logistics,
    LifeType::CombinedArms,
];

pub fn life_type_display_label(lt: LifeType) -> &'static str {
    match lt {
        LifeType::Standard => "Standard",
        LifeType::Attack => "Attack",
        LifeType::Intercept => "Intercept",
        LifeType::Recon => "Recon",
        LifeType::Logistics => "Logistics",
        LifeType::CombinedArms => "Combined Arms",
    }
}

pub fn life_type_map_abbrev(lt: LifeType) -> &'static str {
    match lt {
        LifeType::Standard => "S",
        LifeType::Attack => "A",
        LifeType::Intercept => "I",
        LifeType::Recon => "R",
        LifeType::Logistics => "L",
        LifeType::CombinedArms => "CA",
    }
}

const CSAR_LANDED_CIRCLE_RADIUS_M: f64 = 500.;

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
    /// Ejection point for engine logic; map marks use `inst.position` only after `landed`.
    #[serde(default)]
    pub eject_position: Option<Position3>,
    #[serde(default)]
    pub circle_mark_id: Option<MarkId>,
    #[serde(default)]
    pub point_mark_id: Option<MarkId>,
    /// Stored pilot unit id is stale (DCS replaces parachute with ground pilot).
    #[serde(default)]
    pub pilot_unit_stale: bool,
    /// DCS type name of the ejection pilot unit (for respawn after mission reload).
    #[serde(default)]
    pub pilot_type_name: String,
    #[serde(default = "default_csar_pilot_category")]
    pub pilot_category: GroupCategory,
}

fn default_csar_pilot_category() -> GroupCategory {
    GroupCategory::Airplane
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

fn csar_circle_color() -> Color {
    Color::green(1.)
}

fn csar_circle_fill_color() -> Color {
    Color::green(0.35)
}

fn csar_life_type_mark_label(base: &str) -> String {
    if base.ends_with(" live") {
        base.into()
    } else {
        format!("{base} live").into()
    }
}

fn csar_pilot_mark_label(player_name: &str, life_type_label: &str) -> String {
    format!(
        "downed pilot {player_name} ({})",
        csar_life_type_mark_label(life_type_label)
    )
    .into()
}

fn csar_pilot_mark_message(player_name: &str, life_type_label: &str) -> String {
    format!("{player_name} ({life_type_label})").into()
}

fn csar_pilot_circle_popup(player_name: &str, life_type_label: &str) -> String {
    csar_pilot_mark_label(player_name, life_type_label)
}

fn delete_csar_marks(msgs: &mut MsgQ, csar: &mut CsarDowned) {
    if let Some(id) = csar.circle_mark_id.take() {
        msgs.delete_mark(id);
    }
    if let Some(id) = csar.point_mark_id.take() {
        msgs.delete_mark(id);
    }
}

fn prepare_csar_pilot_unit(pilot: &Unit, landed: bool) {
    let _ = pilot.set_name(String::from(""));
    if let Ok(ctrl) = pilot.get_controller() {
        let immortal = !landed;
        if let Err(e) = ctrl.set_command(Command::SetImmortal(immortal)) {
            warn!("csar: SetImmortal({immortal}) failed: {e:?}");
        }
        if let Err(e) = ctrl.set_command(Command::SetInvisible(false)) {
            warn!("csar: SetInvisible(false) failed: {e:?}");
        }
    }
}

fn csar_pilot_type_name(unit: &Unit) -> Option<String> {
    unit.get_type_name().ok()
}

fn csar_infantry_type_candidates(side: Side) -> &'static [&'static str] {
    match side {
        Side::Red => &["Soldier AK", "Infantry AK", "Infantry AK-74"],
        Side::Blue => &["Soldier M4", "Infantry M4", "Infantry M4 Georgia"],
        Side::Neutral => &["Soldier M4", "Infantry M4"],
    }
}

fn csar_spawn_names(ucid: &Ucid, ejected_at: DateTime<Utc>) -> (String, String) {
    let tag = format!("{ucid}_{}", ejected_at.timestamp());
    (
        format!("FowlCsar_{tag}").into(),
        format!("FowlCsarPilot_{tag}").into(),
    )
}

fn coalition_country_for_side(lua: MizLua, side: Side) -> Result<country::Country> {
    let miz = Miz::singleton(lua)?;
    let coa = miz.coalition(side)?;
    let countries = coa.countries()?;
    let country = countries
        .first()
        .with_context(|| format!("no countries for coalition {side:?}"))?;
    country.id()
}

fn try_spawn_csar_pilot_infantry(
    lua: MizLua,
    ucid: &Ucid,
    side: Side,
    csar: &CsarDowned,
    type_name: &str,
) -> Result<DcsOid<ClassUnit>> {
    let country = coalition_country_for_side(lua, side)?;
    let pos = csar.inst.position.p.0;
    let map_x = pos.x;
    let map_y = pos.z;
    let alt = pos.y;
    let (group_name, unit_name) = csar_spawn_names(ucid, csar.ejected_at);
    let lua_inner = lua.inner();
    let group_tbl = lua_inner.create_table()?;
    group_tbl.raw_set("name", group_name.as_str())?;
    group_tbl.raw_set("task", "Ground Nothing")?;
    group_tbl.raw_set("x", map_x)?;
    group_tbl.raw_set("y", map_y)?;
    group_tbl.raw_set("hidden", false)?;
    group_tbl.raw_set("uncontrolled", true)?;
    group_tbl.raw_set("start_time", 0i64)?;
    let route = lua_inner.create_table()?;
    route.raw_set("points", lua_inner.create_table()?)?;
    group_tbl.raw_set("route", route)?;
    let units = lua_inner.create_table()?;
    let unit_tbl = lua_inner.create_table()?;
    unit_tbl.raw_set("name", unit_name.as_str())?;
    unit_tbl.raw_set("type", type_name)?;
    unit_tbl.raw_set("x", map_x)?;
    unit_tbl.raw_set("y", map_y)?;
    unit_tbl.raw_set("alt", alt)?;
    unit_tbl.raw_set("heading", 0f64)?;
    unit_tbl.raw_set("skill", Skill::Average)?;
    unit_tbl.raw_set("speed", 0f64)?;
    unit_tbl.raw_set("payload", lua_inner.create_table()?)?;
    units.raw_set(1, unit_tbl)?;
    group_tbl.raw_set("units", units)?;
    let group_data = miz::Group::from_lua(Value::Table(group_tbl), lua_inner)
        .map_err(|e| anyhow::anyhow!("csar pilot group table: {e}"))?;
    let spawned = Coalition::singleton(lua)?
        .add_group(country, GroupCategory::Ground, group_data)
        .with_context(|| format!("spawning csar infantry pilot type {type_name}"))?;
    let pilot = spawned
        .get_unit(1)
        .with_context(|| format!("csar pilot group has no unit type {type_name}"))?;
    prepare_csar_pilot_unit(&pilot, csar.landed);
    let id = pilot.object_id()?;
    info!(
        "csar: spawned infantry pilot unit {id:?} type {type_name} at [{map_x}, {alt}, {map_y}] for {ucid:?}"
    );
    Ok(id)
}

fn try_spawn_csar_pilot_from_template(
    lua: MizLua,
    idx: &MizIndex,
    ucid: &Ucid,
    side: Side,
    csar: &CsarDowned,
    template_name: &str,
) -> Result<DcsOid<ClassUnit>> {
    let spctx = SpawnCtx::new(lua)?;
    let template = spctx
        .get_template(idx, GroupKind::Any, side, template_name)
        .with_context(|| format!("csar pilot template {template_name}"))?;
    let pos = csar.inst.position.p.0;
    let (group_name, unit_name) = csar_spawn_names(ucid, csar.ejected_at);
    template.group.set_name(group_name)?;
    template.group.set("lateActivation", false)?;
    template.group.set("hidden", false)?;
    template.group.set("uncontrolled", true)?;
    template.group.set("x", pos.x)?;
    template.group.set("y", pos.z)?;
    let units = template.group.units()?;
    let mut first = true;
    for unit in units {
        let unit = unit?;
        if first {
            unit.set_name(unit_name.clone())?;
            unit.set_pos(Vector2::new(pos.x, pos.z))?;
            unit.set_alt(pos.y)?;
            first = false;
        }
    }
    if first {
        bail!("csar pilot template {template_name} has no units");
    }
    let spawned = spctx
        .spawn(template)
        .with_context(|| format!("spawning csar pilot from template {template_name}"))?;
    let pilot = match spawned {
        Spawned::Group(group) => group
            .get_unit(1)
            .with_context(|| format!("csar pilot template {template_name} has no unit"))?,
        Spawned::Static => {
            bail!("csar pilot template {template_name} is a static object")
        }
    };
    prepare_csar_pilot_unit(&pilot, csar.landed);
    let id = pilot.object_id()?;
    info!(
        "csar: spawned template pilot unit {id:?} from {template_name} at [{}, {}, {}] for {ucid:?}",
        pos.x, pos.y, pos.z
    );
    Ok(id)
}

pub(crate) fn is_fowl_csar_group_name(name: &str) -> bool {
    name.starts_with("FowlCsar_")
}

pub(crate) fn is_fowl_csar_unit_unit(unit: &Unit) -> bool {
    unit.get_group()
        .ok()
        .and_then(|g| g.get_name().ok())
        .is_some_and(|name| is_fowl_csar_group_name(name.as_str()))
}

fn is_fowl_csar_unit(lua: MizLua, pilot_unit: &DcsOid<ClassUnit>) -> bool {
    Unit::get_instance(lua, pilot_unit)
        .ok()
        .is_some_and(|u| is_fowl_csar_unit_unit(&u))
}

fn destroy_csar_spawn_group(lua: MizLua, ucid: &Ucid, csar: &CsarDowned) {
    let (group_name, _) = csar_spawn_names(ucid, csar.ejected_at);
    if let Ok(group) = Group::get_by_name(lua, group_name.as_str()) {
        if let Err(e) = group.destroy() {
            warn!("csar: failed to destroy spawn group {group_name}: {e:?}");
        }
    }
}

fn lookup_csar_pilot_unit(
    lua: MizLua,
    ucid: &Ucid,
    csar: &CsarDowned,
) -> Result<Option<DcsOid<ClassUnit>>> {
    let (_, unit_name) = csar_spawn_names(ucid, csar.ejected_at);
    match Unit::get_by_name(lua, unit_name.as_str()) {
        Ok(unit) if unit.is_exist().unwrap_or(false) => Ok(Some(unit.object_id()?)),
        _ => Ok(None),
    }
}

fn refresh_csar_inst_from_unit(
    csar: &mut CsarDowned,
    unit: &Unit,
    now: DateTime<Utc>,
) -> Result<()> {
    csar.inst.position = unit.get_position()?;
    csar.inst.velocity = unit.get_velocity()?.0;
    csar.inst.in_air = unit.in_air()?;
    csar.inst.moved = Some(now);
    Ok(())
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

enum CsarPilotMissingAction {
    Rebound,
    Killed,
    AwaitRebind,
}

fn sync_csar_pilot_mark(
    msgs: &mut MsgQ,
    side: Side,
    player_name: &str,
    life_type_label: &str,
    csar: &mut CsarDowned,
) {
    if !csar.landed {
        if csar.circle_mark_id.is_some() || csar.point_mark_id.is_some() {
            delete_csar_marks(msgs, csar);
        }
        return;
    }
    if csar.circle_mark_id.is_some() && csar.point_mark_id.is_some() {
        return;
    }
    let pos = LuaVec3(csar.inst.position.p.0);
    let label = csar_pilot_mark_label(player_name, life_type_label);
    let mark_body = csar_pilot_mark_message(player_name, life_type_label);
    let circle_id = MarkId::new();
    let spec = CircleSpec {
        center: pos,
        radius: CSAR_LANDED_CIRCLE_RADIUS_M,
        color: csar_circle_color(),
        fill_color: csar_circle_fill_color(),
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
        label,
        Some(mark_body),
    );
    msgs.set_markup_color(point_id, csar_side_color(side));
    csar.circle_mark_id = Some(circle_id);
    csar.point_mark_id = Some(point_id);
    info!(
        "csar: mark synced for {player_name} ({life_type_label}) at {:?}",
        csar.inst.position.p.0
    );
}

impl Db {
    pub fn csar_enabled(&self) -> bool {
        self.ephemeral.cfg.csar.enabled
    }

    fn csar_pilot_template_name(&self, side: Side) -> Option<&str> {
        let cfg = &self.ephemeral.cfg.csar;
        match side {
            Side::Red => cfg.pilot_template_red.as_ref().map(|s| s.as_str()),
            Side::Blue => cfg.pilot_template_blue.as_ref().map(|s| s.as_str()),
            Side::Neutral => cfg
                .pilot_template_blue
                .as_ref()
                .or(cfg.pilot_template_red.as_ref())
                .map(|s| s.as_str()),
        }
    }

    fn spawn_csar_pilot_unit(
        &self,
        lua: MizLua,
        ucid: &Ucid,
        side: Side,
        csar: &CsarDowned,
    ) -> Result<Option<DcsOid<ClassUnit>>> {
        let idx = &self.ephemeral.miz_idx;
        let mut last_err = None;
        if let Some(template) = self.csar_pilot_template_name(side) {
            match try_spawn_csar_pilot_from_template(lua, idx, ucid, side, csar, template) {
                Ok(id) => return Ok(Some(id)),
                Err(e) => {
                    warn!("csar: spawn from template {template} failed: {e:?}");
                    last_err = Some(e);
                }
            }
            destroy_csar_spawn_group(lua, ucid, csar);
        }
        for type_name in csar_infantry_type_candidates(side) {
            match try_spawn_csar_pilot_infantry(lua, ucid, side, csar, type_name) {
                Ok(id) => return Ok(Some(id)),
                Err(e) => {
                    warn!("csar: spawn as infantry {type_name} failed: {e:?}");
                    last_err = Some(e);
                }
            }
        }
        destroy_csar_spawn_group(lua, ucid, csar);
        for type_name in csar_infantry_type_candidates(side) {
            match try_spawn_csar_pilot_infantry(lua, ucid, side, csar, type_name) {
                Ok(id) => return Ok(Some(id)),
                Err(e) => {
                    warn!(
                        "csar: respawn as infantry {type_name} after group cleanup failed: {e:?}"
                    );
                    last_err = Some(e);
                }
            }
        }
        if let Some(e) = last_err {
            Err(e)
        } else {
            Ok(None)
        }
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
        if let Some(ucid) = self.ephemeral.csar_pilot_unit.get(pilot_unit).copied() {
            return Some(ucid);
        }
        self.persisted
            .players
            .clone()
            .into_iter()
            .find_map(|(ucid, p)| {
                p.csar_downed
                    .iter()
                    .any(|c| c.pilot_unit == *pilot_unit)
                    .then_some(*ucid)
            })
    }

    fn apply_csar_pilot_rebind(
        &mut self,
        ucid: &Ucid,
        old_id: &DcsOid<ClassUnit>,
        new_id: DcsOid<ClassUnit>,
    ) -> bool {
        if let Some(owner) = self.ephemeral.csar_pilot_unit.get(&new_id) {
            if *owner != *ucid {
                warn!(
                    "csar: pilot unit {new_id:?} claimed by {owner:?}, not {ucid:?}"
                );
                return false;
            }
        }
        if self.persisted.players.get(ucid).is_some_and(|p| {
            p.csar_downed
                .iter()
                .any(|c| c.pilot_unit == new_id && c.pilot_unit != *old_id)
        }) {
            warn!(
                "csar: pilot unit {new_id:?} already tracked for another downed pilot {ucid:?}"
            );
            return false;
        }
        self.ephemeral.csar_pilot_unit.remove(old_id);
        self.ephemeral
            .csar_pilot_unit
            .insert(new_id.clone(), *ucid);
        if let Some(player) = self.persisted.players.get_mut_cow(ucid) {
            if let Some(entry) = player
                .csar_downed
                .iter_mut()
                .find(|c| c.pilot_unit == *old_id)
            {
                entry.pilot_unit = new_id.clone();
                entry.pilot_unit_stale = false;
            }
        }
        info!("csar: rebound downed pilot unit for {ucid:?} to {new_id:?}");
        true
    }

    fn refresh_csar_inst_for_pilot(
        &mut self,
        lua: MizLua,
        ucid: &Ucid,
        pilot_unit: &DcsOid<ClassUnit>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let unit = Unit::get_instance(lua, pilot_unit)?;
        if let Some(player) = self.persisted.players.get_mut_cow(ucid) {
            if let Some(entry) = player
                .csar_downed
                .iter_mut()
                .find(|c| c.pilot_unit == *pilot_unit)
            {
                if !entry.landed {
                    refresh_csar_inst_from_unit(entry, &unit, now)?;
                }
            }
        }
        Ok(())
    }

    fn respawn_csar_pilot_unit(
        &mut self,
        lua: MizLua,
        ucid: &Ucid,
        side: Side,
        old_id: &DcsOid<ClassUnit>,
        csar: &CsarDowned,
    ) -> Result<Option<DcsOid<ClassUnit>>> {
        let Some(new_id) = self.spawn_csar_pilot_unit(lua, ucid, side, csar)? else {
            warn!(
                "csar: respawn failed for {ucid:?} ({})",
                csar.life_type
            );
            return Ok(None);
        };
        if self.apply_csar_pilot_rebind(ucid, old_id, new_id.clone()) {
            Ok(Some(new_id))
        } else {
            Self::destroy_csar_pilot_unit(lua, &new_id);
            Ok(None)
        }
    }

    fn handle_csar_pilot_unit_missing(
        &mut self,
        lua: MizLua,
        ucid: &Ucid,
        side: Side,
        pilot_unit: &DcsOid<ClassUnit>,
        csar: &CsarDowned,
        now: DateTime<Utc>,
    ) -> Result<CsarPilotMissingAction> {
        if let Some(new_id) = lookup_csar_pilot_unit(lua, ucid, csar)? {
            if new_id != *pilot_unit {
                if self.apply_csar_pilot_rebind(ucid, pilot_unit, new_id.clone()) {
                    self.refresh_csar_inst_for_pilot(lua, ucid, &new_id, now)?;
                    return Ok(CsarPilotMissingAction::Rebound);
                }
            } else if Self::csar_pilot_unit_alive(lua, &new_id) {
                return Ok(CsarPilotMissingAction::AwaitRebind);
            }
        }
        if let Some(new_id) = self.respawn_csar_pilot_unit(lua, ucid, side, pilot_unit, csar)? {
            self.refresh_csar_inst_for_pilot(lua, ucid, &new_id, now)?;
            return Ok(CsarPilotMissingAction::Rebound);
        }
        if let Some(player) = self.persisted.players.get_mut_cow(ucid) {
            if let Some(entry) = player
                .csar_downed
                .iter_mut()
                .find(|c| c.pilot_unit == *pilot_unit)
            {
                entry.pilot_unit_stale = true;
            }
        }
        Ok(CsarPilotMissingAction::AwaitRebind)
    }

    pub fn on_csar_pilot_killed(&mut self, pilot_unit: &DcsOid<ClassUnit>, ucid: &Ucid) {
        let Some(player) = self.persisted.players.get_mut_cow(ucid) else {
            return;
        };
        let Some(idx) = player
            .csar_downed
            .iter()
            .position(|c| c.pilot_unit == *pilot_unit)
        else {
            return;
        };
        let mut csar = player.csar_downed.remove(idx);
        delete_csar_marks(self.ephemeral.msgs(), &mut csar);
        self.ephemeral.csar_pilot_unit.remove(pilot_unit);
        self.ephemeral.dirty();
        info!(
            "csar: downed pilot unit killed for {ucid:?} ({})",
            csar.life_type
        );
    }

    pub fn csar_active_ucids(&self) -> SmallVec<[Ucid; 16]> {
        self.persisted
            .players
            .clone()
            .into_iter()
            .filter_map(|(ucid, p)| (!p.csar_downed.is_empty()).then_some(*ucid))
            .collect()
    }

    fn csar_downed_ucids(&self) -> SmallVec<[Ucid; 16]> {
        self.csar_active_ucids()
    }

    /// After load or new mission: respawn pilot units, redraw marks.
    pub fn refresh_csar_after_load(&mut self, lua: MizLua) {
        if !self.csar_enabled() {
            return;
        }
        self.ephemeral.csar_pilot_unit.clear();
        for ucid in self.csar_downed_ucids() {
            let Some(side) = self.persisted.players.get(&ucid).map(|p| p.side) else {
                continue;
            };
            if let Some(player) = self.persisted.players.get_mut_cow(&ucid) {
                for csar in &mut player.csar_downed {
                    csar.circle_mark_id = None;
                    csar.point_mark_id = None;
                    csar.pilot_unit_stale = false;
                }
            }
            let len = self
                .persisted
                .players
                .get(&ucid)
                .map(|p| p.csar_downed.len())
                .unwrap_or(0);
            let mut claimed: FxHashSet<DcsOid<ClassUnit>> = FxHashSet::default();
            for idx in 0..len {
                self.ensure_csar_pilot_at_index(lua, &ucid, side, idx, &mut claimed);
            }
        }
        self.rebuild_csar_marks();
        self.ephemeral.dirty();
    }

    fn csar_pilot_unit_alive(lua: MizLua, pilot_unit: &DcsOid<ClassUnit>) -> bool {
        Unit::get_instance(lua, pilot_unit)
            .ok()
            .and_then(|u| u.is_exist().ok())
            .unwrap_or(false)
    }

    fn ensure_csar_pilot_at_index(
        &mut self,
        lua: MizLua,
        ucid: &Ucid,
        side: Side,
        idx: usize,
        claimed: &mut FxHashSet<DcsOid<ClassUnit>>,
    ) {
        let csar_snap = {
            let Some(player) = self.persisted.players.get(ucid) else {
                return;
            };
            let Some(csar) = player.csar_downed.get(idx) else {
                return;
            };
            csar.clone()
        };
        if let Ok(Some(existing)) = lookup_csar_pilot_unit(lua, ucid, &csar_snap) {
            if !claimed.contains(&existing) {
                if let Some(player) = self.persisted.players.get_mut_cow(ucid) {
                    if let Some(csar) = player.csar_downed.get_mut(idx) {
                        csar.pilot_unit = existing.clone();
                        csar.pilot_unit_stale = false;
                    }
                }
                claimed.insert(existing.clone());
                self.ephemeral.csar_pilot_unit.insert(existing, *ucid);
                info!(
                    "csar: relinked downed pilot for {ucid:?} ({}) after load",
                    csar_snap.life_type
                );
                return;
            }
        }
        if Self::csar_pilot_unit_alive(lua, &csar_snap.pilot_unit) {
            if is_fowl_csar_unit(lua, &csar_snap.pilot_unit) && !claimed.contains(&csar_snap.pilot_unit)
            {
                if let Some(player) = self.persisted.players.get_mut_cow(ucid) {
                    if let Some(csar) = player.csar_downed.get_mut(idx) {
                        csar.pilot_unit_stale = false;
                    }
                }
                claimed.insert(csar_snap.pilot_unit.clone());
                self.ephemeral
                    .csar_pilot_unit
                    .insert(csar_snap.pilot_unit.clone(), *ucid);
                return;
            }
            Self::destroy_csar_pilot_unit(lua, &csar_snap.pilot_unit);
        }
        match self.spawn_csar_pilot_unit(lua, ucid, side, &csar_snap) {
            Ok(Some(new_id)) if !claimed.contains(&new_id) => {
                if let Some(player) = self.persisted.players.get_mut_cow(ucid) {
                    if let Some(csar) = player.csar_downed.get_mut(idx) {
                        csar.pilot_unit = new_id.clone();
                    }
                }
                claimed.insert(new_id.clone());
                self.ephemeral
                    .csar_pilot_unit
                    .insert(new_id, *ucid);
                info!(
                    "csar: restored downed pilot for {ucid:?} ({}) after load",
                    csar_snap.life_type
                );
            }
            Ok(None) => {
                if let Some(player) = self.persisted.players.get_mut_cow(ucid) {
                    if let Some(csar) = player.csar_downed.get_mut(idx) {
                        csar.pilot_unit_stale = true;
                    }
                }
                warn!(
                    "csar: no pilot type to respawn for {ucid:?} ({})",
                    csar_snap.life_type
                );
            }
            Ok(Some(_)) => {
                if let Some(player) = self.persisted.players.get_mut_cow(ucid) {
                    if let Some(csar) = player.csar_downed.get_mut(idx) {
                        csar.pilot_unit_stale = true;
                    }
                }
                warn!(
                    "csar: respawn id collision for {ucid:?} ({})",
                    csar_snap.life_type
                );
            }
            Err(e) => {
                if let Some(player) = self.persisted.players.get_mut_cow(ucid) {
                    if let Some(csar) = player.csar_downed.get_mut(idx) {
                        csar.pilot_unit_stale = true;
                    }
                }
                warn!(
                    "csar: respawn failed for {ucid:?} ({}): {e:?}",
                    csar_snap.life_type
                );
            }
        }
    }

    fn find_csar_for_landing(&self, landing_pos: &Position3) -> Option<(Ucid, usize)> {
        let mut best: Option<(Ucid, usize, f64)> = None;
        for ucid in self.csar_active_ucids() {
            let Some(player) = self.persisted.players.get(&ucid) else {
                continue;
            };
            for (idx, csar) in player.csar_downed.iter().enumerate() {
                if csar.landed {
                    continue;
                }
                let dist = (csar.inst.position.p.0 - landing_pos.p.0).magnitude_squared();
                if best.as_ref().is_none_or(|(_, _, d)| dist < *d) {
                    best = Some((ucid, idx, dist));
                }
            }
        }
        best.map(|(ucid, idx, _)| (ucid, idx))
    }

    pub fn on_csar_landing_after_ejection(
        &mut self,
        lua: MizLua,
        now: DateTime<Utc>,
        landing_pos: Position3,
    ) -> Result<bool> {
        let Some((ucid, idx)) = self.find_csar_for_landing(&landing_pos) else {
            return Ok(false);
        };
        let side = match self.persisted.players.get(&ucid) {
            Some(p) => p.side,
            None => return Ok(false),
        };
        let (old_id, mut csar_snap) = {
            let Some(p) = self.persisted.players.get(&ucid) else {
                return Ok(false);
            };
            let Some(c) = p.csar_downed.get(idx) else {
                return Ok(false);
            };
            (c.pilot_unit.clone(), c.clone())
        };
        Self::destroy_csar_pilot_unit(lua, &old_id);
        self.ephemeral.csar_pilot_unit.remove(&old_id);
        csar_snap.inst.position = landing_pos;
        csar_snap.inst.in_air = false;
        csar_snap.inst.moved = Some(now);
        let Some(new_id) = self.spawn_csar_pilot_unit(lua, &ucid, side, &csar_snap)? else {
            warn!(
                "csar: landing respawn failed for {ucid:?} ({})",
                csar_snap.life_type
            );
            if let Some(player) = self.persisted.players.get_mut_cow(&ucid) {
                if let Some(c) = player.csar_downed.get_mut(idx) {
                    c.inst.position = landing_pos;
                    c.inst.in_air = false;
                    c.inst.moved = Some(now);
                    c.pilot_unit_stale = true;
                    set_csar_pilot_landed(c, now, &ucid);
                }
            }
            self.sync_csar_marks_for_ucid(&ucid);
            self.ephemeral.dirty();
            return Ok(true);
        };
        if let Some(player) = self.persisted.players.get_mut_cow(&ucid) {
            if let Some(c) = player.csar_downed.get_mut(idx) {
                c.pilot_unit = new_id.clone();
                c.inst = csar_snap.inst;
                c.pilot_unit_stale = false;
                set_csar_pilot_landed(c, now, &ucid);
            }
        }
        self.ephemeral
            .csar_pilot_unit
            .insert(new_id.clone(), ucid);
        if let Ok(unit) = Unit::get_instance(lua, &new_id) {
            prepare_csar_pilot_unit(&unit, true);
        }
        self.sync_csar_marks_for_ucid(&ucid);
        self.ephemeral.dirty();
        Ok(true)
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
        let side = match self.persisted.players.get(ucid) {
            Some(p) => p.side,
            None => return Ok(false),
        };
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
            if let Some(new_id) = lookup_csar_pilot_unit(lua, ucid, &csar)? {
                if new_id != old_id && self.apply_csar_pilot_rebind(ucid, &old_id, new_id) {
                    rebound = true;
                }
            } else if self
                .respawn_csar_pilot_unit(lua, ucid, side, &old_id, &csar)?
                .is_some()
            {
                rebound = true;
            }
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

    fn sync_csar_marks_for_ucid(&mut self, ucid: &Ucid) {
        let Some(player) = self.persisted.players.get(ucid) else {
            return;
        };
        if player.csar_downed.is_empty() {
            return;
        }
        let side = player.side;
        let name = player.name.clone();
        let per_type: FxHashMap<LifeType, u32> = player
            .csar_downed
            .iter()
            .fold(FxHashMap::default(), |mut m, c| {
                *m.entry(c.life_type).or_default() += 1;
                m
            });
        let len = player.csar_downed.len();
        let life_labels: SmallVec<[std::string::String; 4]> = {
            let mut ordinals: FxHashMap<LifeType, u32> = FxHashMap::default();
            player
                .csar_downed
                .iter()
                .map(|c| {
                    let base = self.life_type_panel_label(c.life_type);
                    if per_type.get(&c.life_type).copied().unwrap_or(0) > 1 {
                        let n = ordinals.entry(c.life_type).or_insert(0);
                        *n += 1;
                        format!("{base} #{n}").to_string()
                    } else {
                        base.to_string()
                    }
                })
                .collect()
        };
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
        let dcs_pilot_id = pilot.object_id()?;
        let aircraft_id = aircraft.object_id()?;
        let slot = aircraft.slot()?;
        let side = self
            .persisted
            .players
            .get(&ucid)
            .map(|p| p.side)
            .context("csar ejection: no such player")?;
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
        let pilot_type_name = csar_pilot_type_name(pilot).unwrap_or_default();
        let pilot_category = aircraft
            .get_group()
            .and_then(|g| g.get_category())
            .unwrap_or(GroupCategory::Airplane);
        let eject_position = inst.position;
        let mut csar = CsarDowned {
            pilot_unit: dcs_pilot_id.clone(),
            life_type,
            aircraft_type: typ,
            ejected_at: now,
            landed: false,
            inst,
            eject_position: Some(eject_position),
            circle_mark_id: None,
            point_mark_id: None,
            pilot_unit_stale: false,
            pilot_type_name,
            pilot_category,
        };
        let tracked_id = match self.spawn_csar_pilot_unit(lua, &ucid, side, &csar)? {
            Some(spawned_id) => {
                Self::destroy_csar_pilot_unit(lua, &dcs_pilot_id);
                csar.pilot_unit = spawned_id.clone();
                if let Ok(unit) = Unit::get_instance(lua, &spawned_id) {
                    prepare_csar_pilot_unit(&unit, false);
                    let _ = refresh_csar_inst_from_unit(&mut csar, &unit, now);
                }
                info!(
                    "csar: spawned coalition pilot on ejection for {ucid:?} at {spawned_id:?}"
                );
                spawned_id
            }
            None => {
                warn!(
                    "csar: spawn on ejection failed for {ucid:?} ({life_type}), keeping DCS pilot"
                );
                prepare_csar_pilot_unit(pilot, false);
                csar.pilot_unit.clone()
            }
        };
        if self.persisted.players.get(&ucid).is_some_and(|p| {
            p.csar_downed.iter().any(|c| c.pilot_unit == tracked_id)
        }) {
            warn!(
                "csar: pilot unit {tracked_id:?} already tracked for {ucid:?}, skipping duplicate registration"
            );
            if tracked_id != csar.pilot_unit {
                Self::destroy_csar_pilot_unit(lua, &tracked_id);
            }
            return Ok(());
        }
        let player = self
            .persisted
            .players
            .get_mut_cow(&ucid)
            .context("csar ejection: no such player")?;
        player.csar_downed.push(csar);
        self.ephemeral
            .csar_pilot_unit
            .insert(tracked_id.clone(), ucid);
        self.ephemeral.dirty();
        info!("csar: registered {life_type} downed pilot {ucid:?} at {tracked_id:?}");
        Ok(())
    }

    pub fn update_csar_downed_positions(
        &mut self,
        lua: MizLua,
        now: DateTime<Utc>,
        ucid: &Ucid,
    ) -> Result<()> {
        let (side, pilot_units) = match self.persisted.players.get(ucid) {
            Some(p) if !p.csar_downed.is_empty() => {
                (
                    p.side,
                    p.csar_downed
                        .iter()
                        .map(|c| c.pilot_unit.clone())
                        .collect::<SmallVec<[DcsOid<ClassUnit>; 4]>>(),
                )
            }
            _ => return Ok(()),
        };
        let coord = Coord::singleton(lua)?;
        for pilot_unit in pilot_units {
            let csar = match self.persisted.players.get(ucid) {
                Some(p) => p
                    .csar_downed
                    .iter()
                    .find(|c| c.pilot_unit == pilot_unit)
                    .cloned(),
                None => None,
            };
            let Some(csar) = csar else {
                continue;
            };
            let instance = {
                let by_name = lookup_csar_pilot_unit(lua, ucid, &csar)?;
                if let Some(by_name) = by_name {
                    Unit::get_instance(lua, &by_name).ok()
                } else {
                    Unit::get_instance(lua, &pilot_unit).ok()
                }
            };
            let Some(instance) = instance.filter(|u| u.is_exist().unwrap_or(false)) else {
                let _ = self.handle_csar_pilot_unit_missing(
                    lua,
                    ucid,
                    side,
                    &pilot_unit,
                    &csar,
                    now,
                )?;
                continue;
            };
            prepare_csar_pilot_unit(&instance, csar.landed);
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
            if csar.landed {
                continue;
            }
            let inst = &mut csar.inst;
            let moved = (inst.position.p.0 - pos.p.0).magnitude_squared() > 1.0;
            if moved {
                inst.position = pos;
                inst.velocity = velocity;
                inst.in_air = in_air;
                inst.moved = Some(now);
                self.ephemeral.stat(bfprotocols::stats::Stat::Position {
                    id: EnId::Player(*ucid),
                    pos: bfprotocols::stats::Pos {
                        pos: coord.lo_to_ll(inst.position.p)?,
                        velocity: inst.velocity,
                    },
                });
            }
        }
        let _ = self.rebind_stale_csar_pilots(lua, ucid)?;
        self.ephemeral.dirty();
        Ok(())
    }

    pub fn update_all_csar_pilots(&mut self, lua: MizLua, now: DateTime<Utc>) {
        if !self.csar_enabled() {
            return;
        }
        self.flush_pending_csar_destroys(lua);
        for ucid in self.csar_active_ucids() {
            if let Err(e) = self.update_csar_downed_positions(lua, now, &ucid) {
                warn!("csar position update failed for {ucid:?}: {e:?}");
            }
        }
    }

    pub fn rebuild_csar_marks(&mut self) {
        if !self.csar_enabled() {
            return;
        }
        for ucid in self.csar_downed_ucids() {
            self.sync_csar_marks_for_ucid(&ucid);
        }
    }
}
