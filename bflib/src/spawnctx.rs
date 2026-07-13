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

use crate::{db::persisted::Persisted, shots::ShotDb};
use anyhow::{Context, Result, anyhow, bail};
use bfprotocols::{db::group::GroupId, perf::PerfInner};
use chrono::{DateTime, Duration, Utc};
use compact_str::format_compact;
use dcso3::{
    change_heading, DeepClone, LuaEnv, LuaVec2, LuaVec3, MizLua, String, Vector2, Vector3,
    airbase::Airbase,
    coalition::{Coalition, Side, Static},
    controller::AltType,
    env::miz::{self, GroupInfo, GroupKind, Miz, MizIndex, Skill, TriggerZone, UnitId},
    group::{ClassGroup, Group, GroupCategory},
    land::Land,
    object::{DcsObject, DcsOid, ObjectCategory},
    perf::record_perf,
    static_object::StaticObject,
    trigger::Trigger,
    unit::Unit,
    world::{SearchVolume, World},
};
use fxhash::FxHashMap;
use log::info;
use mlua::{prelude::*, Value};
use serde_derive::{Deserialize, Serialize};

const DESPAWN_COMBAT_ENGAGE: Duration = Duration::seconds(45);
pub(super) const DESPAWN_COMBAT_RETRY: Duration = Duration::seconds(30);
const DEP_FARP_HELI_PARKING_SPEED: f64 = 41.666_666_666_667;
/// ME TakeOffParking: refuel/rearm on pad before takeoff (minutes).
const ME_PARKING_REFUEL_REARM: i64 = 3;

pub struct FarpPadMove<'lua> {
    pub spawned: Spawned<'lua>,
    pub helipad_ids: FxHashMap<String, i64>,
}

pub fn dep_farp_helipad_unit_name(pad_template: &str, slot_index: usize) -> String {
    String::from(format_compact!("{pad_template}-HP-{slot_index}"))
}

fn is_dep_farp_pad_helipad_name(name: &str, pad_template: &str) -> bool {
    name.starts_with(&format!("{pad_template}-HP-"))
}

fn me_empty_combo_task(lua: MizLua) -> Result<LuaTable> {
    let task = lua.inner().create_table()?;
    task.raw_set("id", "ComboTask")?;
    let params = lua.inner().create_table()?;
    params.raw_set("tasks", lua.inner().create_table()?)?;
    task.raw_set("params", params)?;
    Ok(task)
}

/// ME `helipadId` / `linkUnit`: spawned static helipad id (same as ai_air hub slots).
pub(crate) fn helipad_facility_id(lua: MizLua, name: &str) -> Option<i64> {
    if let Ok(Static::Static(st)) = StaticObject::get_by_name(lua, name) {
        if st.is_exist().unwrap_or(false) {
            if let Ok(id) = st.id() {
                return Some(i64::from(id.inner()));
            }
        }
    }
    let u = Unit::get_by_name(lua, name).ok()?;
    if !u.is_exist().unwrap_or(false) {
        return None;
    }
    u.id().ok().map(|id| i64::from(id.inner()))
}

fn resolve_dep_farp_helipad_link_id(
    lua: MizLua,
    helipad_name: &str,
    helipad_id_hint: Option<i64>,
) -> Result<i64> {
    if let Some(id) = helipad_id_hint {
        return Ok(id);
    }
    helipad_facility_id(lua, helipad_name).with_context(|| {
        format_compact!("DEP FARP helipad {helipad_name} missing world facility id")
    })
}

fn patch_dep_farp_slot_me_helipad_route<'lua>(
    lua: MizLua<'lua>,
    group: &miz::Group<'lua>,
    helipad_pos: Vector2,
    baro_alt: f64,
    helipad_id: i64,
) -> Result<()> {
    let route: LuaTable = match group.raw_get("route") {
        Ok(r) => r,
        Err(_) => {
            let r = lua.inner().create_table()?;
            group.raw_set("route", r.clone())?;
            r
        }
    };
    let points = lua.inner().create_table()?;
    let p1 = lua.inner().create_table()?;
    p1.raw_set("type", "TakeOffParking")?;
    p1.raw_set("x", helipad_pos.x)?;
    p1.raw_set("y", helipad_pos.y)?;
    p1.raw_set("alt", baro_alt)?;
    p1.raw_set("alt_type", "BARO")?;
    p1.raw_set("action", "From Parking Area")?;
    p1.raw_set("speed", DEP_FARP_HELI_PARKING_SPEED)?;
    p1.raw_set("speed_locked", true)?;
    p1.raw_set("ETA", 0f64)?;
    p1.raw_set("ETA_locked", true)?;
    p1.raw_set("formation_template", "")?;
    p1.raw_set("task", me_empty_combo_task(lua)?)?;
    let props = lua.inner().create_table()?;
    props.raw_set("addopt", lua.inner().create_table()?)?;
    p1.raw_set("properties", props)?;
    p1.raw_set("airdromeId", Value::Nil)?;
    p1.raw_set("helipadId", helipad_id)?;
    p1.raw_set("linkUnit", helipad_id)?;
    p1.raw_set("parking", Value::Nil)?;
    p1.raw_set("timeReFuAr", ME_PARKING_REFUEL_REARM)?;
    points.raw_set(1, p1)?;
    route.raw_set("points", points)?;
    group.raw_set("route", route)?;
    Ok(())
}

fn destroy_named_pad_unit(lua: MizLua, name: &str) {
    if let Ok(ab) = Airbase::get_by_name(lua, String::from(name)) {
        if ab.is_exist().unwrap_or(false) {
            let _ = ab.destroy();
            return;
        }
    }
    match StaticObject::get_by_name(lua, name) {
        Ok(Static::Airbase(ab)) if ab.is_exist().unwrap_or(false) => {
            let _ = ab.destroy();
        }
        Ok(Static::Static(st)) if st.is_exist().unwrap_or(false) => {
            let _ = st.destroy();
        }
        _ => {}
    }
}

fn dep_farp_helipad_template_heading(
    spctx: &SpawnCtx,
    idx: &MizIndex,
    side: Side,
    pad_template: &str,
    slot_index: usize,
) -> Result<f64> {
    let helipad_name = dep_farp_helipad_unit_name(pad_template, slot_index);
    let pad = spctx.get_template_ref(idx, GroupKind::Any, side, pad_template)?;
    for unit in pad.group.units()? {
        let unit = unit?;
        if unit.name()?.as_str() == helipad_name.as_str() {
            return unit.heading();
        }
    }
    for unit in pad.group.units()? {
        let unit = unit?;
        if unit.name()?.as_str() == pad_template {
            return unit.heading();
        }
    }
    Ok(0.)
}

fn dep_farp_helipad_template_pos(
    spctx: &SpawnCtx,
    idx: &MizIndex,
    side: Side,
    pad_template: &str,
    slot_index: usize,
) -> Result<Vector2> {
    let helipad_name = dep_farp_helipad_unit_name(pad_template, slot_index);
    let pad = spctx.get_template_ref(idx, GroupKind::Any, side, pad_template)?;
    for unit in pad.group.units()? {
        let unit = unit?;
        if unit.name()?.as_str() == helipad_name.as_str() {
            let p = unit.pos()?;
            return Ok(Vector2::new(p.x, p.y));
        }
    }
    bail!("pad template {pad_template} has no helipad unit {helipad_name}")
}

fn dep_farp_helipad_world_pos(
    lua: MizLua,
    side: Side,
    helipad_name: &str,
) -> Result<Vector2> {
    match StaticObject::get_by_name(lua, helipad_name) {
        Ok(Static::Static(obj)) if obj.is_exist()? => {
            let pt = obj.as_object()?.get_point()?;
            return Ok(Vector2::new(pt.0.x, pt.0.z));
        }
        Ok(Static::Airbase(ab)) if ab.is_exist()? => {
            let pt = ab.as_object()?.get_point()?;
            return Ok(Vector2::new(pt.0.x, pt.0.z));
        }
        _ => {}
    }
    if let Ok(ab) = Airbase::get_by_name(lua, String::from(helipad_name)) {
        if ab.is_exist()? {
            let pt = ab.get_point()?;
            return Ok(Vector2::new(pt.x, pt.z));
        }
    }
    let world = World::singleton(lua)?;
    for ab in world.get_airbases()? {
        let ab = ab?;
        if !ab.is_exist()? {
            continue;
        }
        let Ok(obj) = ab.as_object() else {
            continue;
        };
        if obj.get_name()?.as_str() == helipad_name {
            let pt = ab.get_point()?;
            return Ok(Vector2::new(pt.x, pt.z));
        }
    }
    let coalition = Coalition::singleton(lua)?;
    for st in coalition.get_static_objects(side)? {
        let st = st?;
        if !st.is_exist()? || st.get_name()?.as_str() != helipad_name {
            continue;
        }
        let pt = st.as_object()?.get_point()?;
        return Ok(Vector2::new(pt.0.x, pt.0.z));
    }
    if let Ok(u) = Unit::get_by_name(lua, helipad_name) {
        if u.is_exist()? {
            let p = u.get_point()?;
            return Ok(Vector2::new(p.0.x, p.0.z));
        }
    }
    bail!("DEP FARP helipad {helipad_name} has no world position")
}

fn default_speed() -> f64 {
    220.
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SpawnLoc {
    /// only for air units, obviously
    InAir {
        pos: Vector2,
        heading: f64,
        altitude: f64,
        #[serde(default = "default_speed")]
        speed: f64,
    },
    AtPos {
        /// the position of the player. the group will be offset in the
        /// direction offset_direction from this point by the group radius + 10 meters
        pos: Vector2,
        /// this should be a unit vector pointing in the direction
        /// you want to offset the group
        offset_direction: Vector2,
        /// rotate the group to this heading in radians
        group_heading: f64,
    },
    AtPosWithComponents {
        pos: Vector2,
        /// the position of sub components of the group by unit type
        component_pos: FxHashMap<String, Vector2>,
        /// rotate the group to this heading in radians
        group_heading: f64,
    },
    /// spawn the group as a direct translation from an original (provided) center
    /// to a new center. This is useful if you have statics, or multiple groups,
    /// and you want their relative positions to be preserved
    AtPosWithCenter {
        /// pos is the new center position of the group
        pos: Vector2,
        /// center is the original center of the group
        center: Vector2,
        /// added to each unit heading after translation (radians)
        heading_add: f64,
    },
    AtTrigger {
        name: String,
        /// rotate the group to this heading in radians
        group_heading: f64,
    },
}

impl Default for SpawnLoc {
    fn default() -> Self {
        Self::AtPos {
            pos: Vector2::new(0., 0.),
            offset_direction: Vector2::new(0., 0.),
            group_heading: 0.,
        }
    }
}

pub struct SpawnCtx<'lua> {
    coalition: Coalition<'lua>,
    miz: Miz<'lua>,
    lua: MizLua<'lua>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Despawn {
    Group(DcsOid<ClassGroup>),
    Static(String),
}

#[derive(Debug, Clone)]
pub enum Spawned<'lua> {
    Group(Group<'lua>),
    Static,
}

fn unit_damaged_in_dcs(unit: &dcso3::unit::Unit) -> bool {
    match (unit.get_life(), unit.get_life0()) {
        (Ok(life), Ok(life0)) => life0 > 0 && life < life0,
        _ => false,
    }
}

fn static_damaged_in_dcs(obj: &StaticObject) -> bool {
    match (obj.get_life(), obj.try_get_life0()) {
        (Ok(life), Some(life0)) => life < life0,
        _ => false,
    }
}

/// Hold scripted despawn while DCS may still be resolving damage on this group.
pub(super) fn despawn_defer_combat(
    lua: MizLua,
    shots: &ShotDb,
    persisted: &Persisted,
    now: DateTime<Utc>,
    gid: GroupId,
    despawn: &Despawn,
) -> bool {
    if shots.group_recently_engaged(gid, now, DESPAWN_COMBAT_ENGAGE) {
        return true;
    }
    if let Some(group) = persisted.groups.get(&gid) {
        let mut alive = 0usize;
        for uid in &group.units {
            if persisted.units.get(uid).is_some_and(|u| !u.dead) {
                alive += 1;
            }
        }
        if alive < group.units.len() {
            return true;
        }
    }
    match despawn {
        Despawn::Group(oid) => {
            let Ok(group) = Group::get_instance(lua, oid) else {
                return false;
            };
            let Ok(units) = group.get_units() else {
                return false;
            };
            let mut damaged = false;
            let _ = units.for_each(|u| {
                let u = u?;
                if unit_damaged_in_dcs(&u) {
                    damaged = true;
                }
                if let Ok(id) = u.object_id() {
                    if shots.unit_recently_engaged(&id, now, DESPAWN_COMBAT_ENGAGE) {
                        damaged = true;
                    }
                }
                Ok(())
            });
            damaged
        }
        Despawn::Static(name) => {
            let Ok(obj) = StaticObject::get_by_name(lua, name) else {
                return false;
            };
            match obj {
                Static::Airbase(_) => false,
                Static::Static(st) => static_damaged_in_dcs(&st),
            }
        }
    }
}

impl<'lua> SpawnCtx<'lua> {
    pub fn new(lua: MizLua<'lua>) -> Result<Self> {
        Ok(Self {
            coalition: Coalition::singleton(lua)?,
            miz: Miz::singleton(lua)?,
            lua,
        })
    }

    pub fn lua(&self) -> MizLua<'lua> {
        self.lua
    }

    pub fn get_template(
        &self,
        idx: &MizIndex,
        kind: GroupKind,
        side: Side,
        template_name: &str,
    ) -> Result<GroupInfo<'lua>> {
        let mut template = self
            .miz
            .get_group_by_name(idx, kind, side, template_name)?
            .ok_or_else(|| anyhow!("no such template {template_name}"))?;
        template.group = template.group.deep_clone(self.lua.inner())?;
        Ok(template)
    }

    /// get at template that you pinky promise not to modify
    pub fn get_template_ref<'a>(
        &'a self,
        idx: &MizIndex,
        kind: GroupKind,
        side: Side,
        template_name: &str,
    ) -> Result<GroupInfo<'a>> {
        self.miz
            .get_group_by_name(idx, kind, side, template_name)?
            .ok_or_else(|| anyhow!("no such template {template_name}"))
    }

    pub fn get_trigger_zone<'a>(&'a self, idx: &MizIndex, name: &str) -> Result<TriggerZone<'a>> {
        Ok(self
            .miz
            .get_trigger_zone(idx, name)?
            .ok_or_else(|| anyhow!("no such trigger zone {name}"))?)
    }

    pub fn move_farp_pad(
        &self,
        idx: &MizIndex,
        side: Side,
        pad_template: &str,
        pos: Vector2,
    ) -> Result<FarpPadMove<'lua>> {
        let pad = {
            let pad = self
                .get_template(idx, GroupKind::Any, side, &pad_template)
                .context("getting the pad")?;
            pad.group.set("hidden", false)?;
            pad.group.set("lateActivation", false)?;
            let units = pad.group.units().context("getting pad units")?;
            let n = units.len() as i64;
            if n < 1 {
                bail!("pad template {pad_template} has no units");
            }
            let anchor_old = units.get(1).context("getting first pad unit")?.pos()?;
            let dx = pos.x - anchor_old.x;
            let dy = pos.y - anchor_old.y;
            for i in 1..=n {
                let u = units.get(i).with_context(|| format_compact!("pad unit index {i}"))?;
                let p = u.pos()?;
                u.set_pos(Vector2::new(p.x + dx, p.y + dy))
                    .with_context(|| format_compact!("setting pad unit {i} position"))?;
            }
            pad
        };
        let mut helipad_ids = FxHashMap::default();
        match GroupCategory::from_kind(pad.category) {
            Some(_) => Ok(FarpPadMove {
                spawned: self.spawn(pad).context("moving the pad")?,
                helipad_ids,
            }),
            None => {
                for unit in pad.group.units().context("getting pad units")? {
                    let unit = unit?;
                    let unit_name = unit.name()?;
                    destroy_named_pad_unit(self.lua(), unit_name.as_str());
                    self.coalition
                        .add_static_object(pad.country, unit.clone())
                        .with_context(|| {
                            format_compact!("spawning pad static {unit_name}")
                        })?;
                }
                for unit in pad.group.units().context("getting pad units")? {
                    let unit = unit?;
                    let unit_name = unit.name()?;
                    if !is_dep_farp_pad_helipad_name(unit_name.as_str(), pad_template) {
                        continue;
                    }
                    let link_id = helipad_facility_id(self.lua(), unit_name.as_str())
                        .with_context(|| {
                            format_compact!(
                                "DEP FARP helipad {unit_name} missing world facility id after spawn"
                            )
                        })?;
                    info!(
                        "DEP FARP pad helipad {unit_name} linkUnit id {link_id} (world facility)"
                    );
                    helipad_ids.insert(unit_name, link_id);
                }
                Ok(FarpPadMove {
                    spawned: Spawned::Static,
                    helipad_ids,
                })
            }
        }
    }

    /// Bind a pool client slot to a deployed pad helipad (`TakeOffParking`, late activation).
    pub fn activate_dep_farp_static_slot(
        &self,
        idx: &MizIndex,
        side: Side,
        group_name: &str,
        pad_template: &str,
        slot_index: usize,
        pad_heading: f64,
        helipad_id_hint: Option<i64>,
    ) -> Result<Spawned<'lua>> {
        let helipad_name = dep_farp_helipad_unit_name(pad_template, slot_index);
        let helipad_pos = dep_farp_helipad_template_pos(
            self,
            idx,
            side,
            pad_template,
            slot_index,
        )
        .or_else(|_| dep_farp_helipad_world_pos(self.lua(), side, helipad_name.as_str()))?;
        let helipad_id = resolve_dep_farp_helipad_link_id(
            self.lua(),
            helipad_name.as_str(),
            helipad_id_hint,
        )?;
        let slot_heading = dep_farp_helipad_template_heading(
            self,
            idx,
            side,
            pad_template,
            slot_index,
        )
        .unwrap_or(pad_heading);
        info!(
            "DEP FARP slot {group_name} -> helipad {helipad_name} linkUnit id {helipad_id} (TakeOffParking)"
        );
        let land = Land::singleton(self.lua())?;
        let baro_alt = land
            .get_height(LuaVec2(helipad_pos))?
            .round()
            .max(0.);
        {
            let slot = self
                .get_template_ref(idx, GroupKind::Any, side, group_name)
                .with_context(|| format_compact!("getting DEP FARP static slot {group_name}"))?;
            slot.group.set("hidden", false)?;
            slot.group.set("lateActivation", false)?;
            slot.group.set("uncontrolled", false)?;
            slot.group.raw_set("uncontrollable", false)?;
            slot.group.raw_set("task", "CAS")?;
            slot.group.raw_set("taskSelected", true)?;
            slot.group.raw_set("x", helipad_pos.x)?;
            slot.group.raw_set("y", helipad_pos.y)?;
            patch_dep_farp_slot_me_helipad_route(
                self.lua(),
                &slot.group,
                helipad_pos,
                baro_alt,
                helipad_id,
            )?;
            for unit in slot.group.units()? {
                let unit = unit?;
                if unit.skill()? != Skill::Client {
                    continue;
                }
                unit.set_pos(helipad_pos)?;
                unit.set_alt(baro_alt)?;
                unit.raw_set("alt_type", AltType::BARO)?;
                unit.raw_set("speed", DEP_FARP_HELI_PARKING_SPEED)?;
                unit.raw_set("helipadId", helipad_id)?;
                unit.raw_set("linkUnit", helipad_id)?;
                unit.raw_set("ropeLength", 15i64)?;
                if slot_heading.abs() > 0.01 {
                    unit.set_heading(slot_heading)?;
                    unit.raw_set("psi", -slot_heading)?;
                    unit.raw_set("manualHeading", true)?;
                } else {
                    unit.set_heading(0.)?;
                    unit.raw_set("psi", 0f64)?;
                    unit.raw_remove("manualHeading")?;
                }
                unit.raw_remove("airdromeId")?;
                unit.raw_remove("parking")?;
                unit.raw_remove("parking_id")?;
            }
        }
        let trigger = Trigger::singleton(self.lua())?;
        let action = trigger.action()?;
        if let Ok(world) = Group::get_by_name(self.lua(), group_name) {
            if world.is_exist()? {
                action.deactivate_group(String::from(group_name))?;
            }
        }
        action.activate_group(String::from(group_name))?;
        let world = Group::get_by_name(self.lua(), group_name).with_context(|| {
            format_compact!("DEP FARP static slot {group_name} not active after activateGroup")
        })?;
        let _ = idx;
        Ok(Spawned::Group(world))
    }

    pub fn deactivate_dep_farp_static_slot_pool(
        &self,
        idx: &MizIndex,
        side: Side,
        group_name: &str,
    ) -> Result<()> {
        {
            let slot = self
                .get_template_ref(idx, GroupKind::Any, side, group_name)
                .with_context(|| format_compact!("getting DEP FARP static slot {group_name}"))?;
            slot.group.set("hidden", true)?;
        }
        let trigger = Trigger::singleton(self.lua())?;
        if let Ok(world) = Group::get_by_name(self.lua(), group_name) {
            if world.is_exist()? {
                trigger
                    .action()?
                    .deactivate_group(String::from(group_name))?;
            }
        }
        let _ = idx;
        Ok(())
    }

    pub fn spawn(&self, template: GroupInfo<'lua>) -> Result<Spawned<'lua>> {
        match GroupCategory::from_kind(template.category) {
            Some(category) => Ok(Spawned::Group(
                self.coalition
                    .add_group(template.country, category, template.group.clone())
                    .with_context(|| {
                        format_compact!("spawning group from template {:?}", template)
                    })?,
            )),
            None => {
                // static objects are not fed to addStaticObject as groups
                let unit: miz::Unit<'lua> = template
                    .group
                    .units()
                    .context("getting static group units")?
                    .first()
                    .context("getting first unit in static group")?
                    .clone();
                self.coalition
                    .add_static_object(template.country, unit)
                    .with_context(|| {
                        format_compact!("spawning static object from template {:?}", template)
                    })?;
                Ok(Spawned::Static)
            }
        }
    }

    pub fn despawn(&self, perf: &mut PerfInner, name: Despawn) -> Result<()> {
        let ts = Utc::now();
        match name {
            Despawn::Group(oid) => {
                match Group::get_instance(self.lua, &oid) {
                    Ok(group) => group.destroy()?,
                    Err(e) => info!("attempt to despawn invalid group {e:?}"),
                }
                record_perf(&mut perf.despawn, ts);
                Ok(())
            }
            Despawn::Static(name) => {
                match dcso3::static_object::StaticObject::get_by_name(self.lua, &*name) {
                    Ok(Static::Airbase(obj)) => obj.destroy()?,
                    Ok(Static::Static(obj)) => obj.destroy()?,
                    Err(e) => info!("attempt to despawn unknown static {} {}", name, e),
                }
                record_perf(&mut perf.despawn, ts);
                Ok(())
            }
        }
    }

    /*
    pub fn remove_junk(&self, point: Vector2, radius: f64) -> Result<()> {
        let alt = Land::singleton(self.lua)?.get_height(LuaVec2(point))?;
        let point = LuaVec3(Vector3::new(point.x, alt, point.y));
        let vol = SearchVolume::Sphere { point, radius };
        World::singleton(self.lua)?.remove_junk(vol)?;
        Ok(())
    }
    */

    #[allow(dead_code)]
    pub fn remove_scenery(&self, point: Vector2, radius: f64) -> Result<()> {
        let alt = Land::singleton(self.lua)?.get_height(LuaVec2(point))?;
        let point = LuaVec3(Vector3::new(point.x, alt, point.y));
        let vol = SearchVolume::Sphere { point, radius };
        World::singleton(self.lua)?.search_objects(
            ObjectCategory::Scenery,
            vol,
            Value::Nil,
            |_, o, _| {
                o.destroy()?;
                Ok(true)
            },
        )?;
        Ok(())
    }
}
