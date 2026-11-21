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
    env::miz::{self, GroupInfo, GroupKind, Miz, MizIndex, TriggerZone, UnitId},
    group::{ClassGroup, Group, GroupCategory},
    land::Land,
    object::{DcsObject, DcsOid, ObjectCategory},
    perf::record_perf,
    static_object::StaticObject,
    unit::Unit,
    world::{SearchVolume, World},
};
use fxhash::FxHashMap;
use log::info;
use mlua::{prelude::*, Value};
use serde_derive::{Deserialize, Serialize};

const DESPAWN_COMBAT_ENGAGE: Duration = Duration::seconds(45);
pub(super) const DESPAWN_COMBAT_RETRY: Duration = Duration::seconds(30);

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

/// MOOSE SPAWNSTATIC defaults when ME omits `shape_name`.
fn default_heliport_shape(typ: &str) -> &'static str {
    match typ {
        "Invisible FARP" => "invisiblefarp",
        "SINGLE_HELIPAD" => "FARP",
        "FARP" => "FARPS",
        _ => "FARP",
    }
}

/// ATC fields from ME heliport unit (per-pad / per-coalition template).
#[derive(Clone, Debug)]
struct HeliportAtc {
    frequency: Option<f64>,
    callsign_id: Option<i64>,
    modulation: Option<i64>,
    allow_hot_start: Option<bool>,
}

impl HeliportAtc {
    fn from_me_unit(unit: &miz::Unit<'_>) -> Self {
        let frequency = unit
            .raw_get::<_, f64>("heliport_frequency")
            .ok()
            .or_else(|| {
                unit.raw_get::<_, i64>("heliport_frequency")
                    .ok()
                    .map(|hz| hz as f64)
            });
        Self {
            frequency,
            callsign_id: unit.raw_get::<_, i64>("heliport_callsign_id").ok(),
            modulation: unit.raw_get::<_, i64>("heliport_modulation").ok(),
            allow_hot_start: unit.raw_get::<_, bool>("allowHotStart").ok(),
        }
    }

    fn apply_to_table(&self, tbl: &mlua::Table<'_>) -> Result<()> {
        if let Some(f) = self.frequency {
            tbl.raw_set("heliport_frequency", f)?;
        }
        if let Some(c) = self.callsign_id {
            tbl.raw_set("heliport_callsign_id", c)?;
        }
        if let Some(m) = self.modulation {
            tbl.raw_set("heliport_modulation", m)?;
        }
        if let Some(h) = self.allow_hot_start {
            tbl.raw_set("allowHotStart", h)?;
        }
        Ok(())
    }
}

struct HeliportBlueprint {
    me_name: String,
    me_unit_id: Option<i64>,
    me_pos: Vector2,
    heading: f64,
    typ: String,
    shape: String,
    atc: HeliportAtc,
}

fn fill_heliport_unit_table(
    tbl: &mlua::Table<'_>,
    bp: &HeliportBlueprint,
    live_name: &str,
    world: Vector2,
    alt: f64,
) -> Result<()> {
    tbl.raw_set("category", "Heliports")?;
    tbl.raw_set("type", bp.typ.as_str())?;
    tbl.raw_set("shape_name", bp.shape.as_str())?;
    tbl.raw_set("name", live_name)?;
    tbl.raw_set("x", world.x)?;
    tbl.raw_set("y", world.y)?;
    tbl.raw_set("alt", alt)?;
    tbl.raw_set("heading", bp.heading)?;
    tbl.raw_set("dead", false)?;
    tbl.raw_set("dynamicSpawn", true)?;
    // Reuse ME unitId so DCS keeps the pad warehouse (SKU + linkDynTempl) from bftools.
    if let Some(id) = bp.me_unit_id {
        tbl.raw_set("unitId", id)?;
    }
    bp.atc.apply_to_table(tbl)?;
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
    /// Static on a moving ship deck (MOOSE/Hoggit `linkUnit` + `offsets`).
    AtPosOnShip {
        pos: Vector2,
        group_heading: f64,
        altitude: f64,
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

/// Resolve DCS airbase for a pad template (LIVE name first, then ME name).
pub fn airbase_for_pad_template<'lua>(
    lua: MizLua<'lua>,
    pad_template: &str,
) -> Result<Airbase<'lua>> {
    let live = SpawnCtx::ground_dep_live_pad_name(pad_template);
    if let Ok(ab) = Airbase::get_by_name(lua, live.clone()) {
        if ab.is_exist().unwrap_or(false) {
            return Ok(ab);
        }
    }
    Airbase::get_by_name(lua, String::from(pad_template)).with_context(|| {
        format_compact!("getting airbase for pad template {pad_template} (tried {live})")
    })
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

    /// Live DCS airbase name for a ME pad unit / template (`{name}-LIVE`).
    pub fn ground_dep_live_pad_name(me_name: &str) -> String {
        String::from(format_compact!("{me_name}-LIVE"))
    }

    /// Ground DEP heliport: ME type/shape/ATC + relative pos; LIVE name for dyn slots.
    /// 1 pad → `addStaticObject`; multi-pad → one `addGroup(-1)`.
    /// Boat pads still translate the ME template group.
    pub fn move_farp_pad(
        &self,
        idx: &MizIndex,
        side: Side,
        pad_template: &str,
        pos: Vector2,
        layout_center: Option<Vector2>,
    ) -> Result<Spawned<'lua>> {
        let pad = self
            .get_template(idx, GroupKind::Any, side, &pad_template)
            .context("getting the pad")?;
        match GroupCategory::from_kind(pad.category) {
            Some(_) => {
                pad.group.set("hidden", false)?;
                pad.group.set("lateActivation", false)?;
                let units = pad.group.units().context("getting pad units")?;
                let n = units.len() as i64;
                if n < 1 {
                    bail!("pad template {pad_template} has no units");
                }
                let anchor_old = units.get(1).context("getting first pad unit")?.pos()?;
                let origin = layout_center.unwrap_or(anchor_old);
                let dx = pos.x - origin.x;
                let dy = pos.y - origin.y;
                for i in 1..=n {
                    let u = units.get(i).with_context(|| format_compact!("pad unit index {i}"))?;
                    let p = u.pos()?;
                    u.set_pos(Vector2::new(p.x + dx, p.y + dy))
                        .with_context(|| format_compact!("setting pad unit {i} position"))?;
                }
                self.spawn(pad).context("moving the pad")
            }
            None => {
                let units = pad.group.units().context("getting pad units")?;
                let mut blueprints: Vec<HeliportBlueprint> = Vec::new();
                for unit in units {
                    let unit = unit?;
                    let me_name = unit.name()?;
                    let me_unit_id = unit.id().ok().map(|id| id.inner());
                    destroy_named_pad_unit(self.lua(), me_name.as_str());
                    let typ = unit.typ().unwrap_or_else(|_| String::from("FARP"));
                    let shape = unit
                        .raw_get::<_, String>("shape_name")
                        .ok()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| String::from(default_heliport_shape(typ.as_str())));
                    let heading = unit.heading().unwrap_or(0.);
                    let me_pos = unit.pos()?;
                    let atc = HeliportAtc::from_me_unit(&unit);
                    blueprints.push(HeliportBlueprint {
                        me_name,
                        me_unit_id,
                        me_pos,
                        heading,
                        typ,
                        shape,
                        atc,
                    });
                }
                destroy_named_pad_unit(self.lua(), pad_template);
                if let Ok(g) = Group::get_by_name(self.lua(), pad_template) {
                    if g.is_exist().unwrap_or(false) {
                        let _ = g.destroy();
                    }
                }
                if blueprints.is_empty() {
                    bail!("pad template {pad_template} has no units");
                }
                let origin = layout_center.unwrap_or(blueprints[0].me_pos);
                let land = Land::singleton(self.lua)?;
                let live_group = Self::ground_dep_live_pad_name(pad_template);
                destroy_named_pad_unit(self.lua(), live_group.as_str());
                if let Ok(g) = Group::get_by_name(self.lua(), live_group.as_str()) {
                    if g.is_exist().unwrap_or(false) {
                        let _ = g.destroy();
                    }
                }

                if blueprints.len() == 1 {
                    let bp = &blueprints[0];
                    let world = Vector2::new(
                        bp.me_pos.x + (pos.x - origin.x),
                        bp.me_pos.y + (pos.y - origin.y),
                    );
                    let live_name = Self::ground_dep_live_pad_name(bp.me_name.as_str());
                    destroy_named_pad_unit(self.lua(), live_name.as_str());
                    let alt = land
                        .get_height(LuaVec2(world))
                        .context("land.getHeight for pad spawn")?;
                    let tbl = self
                        .lua
                        .inner()
                        .create_table()
                        .map_err(|e| anyhow!("create heliport unit table: {e}"))?;
                    fill_heliport_unit_table(&tbl, bp, live_name.as_str(), world, alt)?;
                    let unit = miz::Unit::from_lua(Value::Table(tbl), self.lua.inner())
                        .map_err(|e| anyhow!("wrap heliport unit table: {e}"))?;
                    self.coalition
                        .add_static_object(pad.country, unit)
                        .with_context(|| {
                            format_compact!("spawning clean heliport {live_name} typ={}", bp.typ)
                        })?;
                    info!(
                        "ground DEP pad: spawned {}/{} {live_name} via addStaticObject (unitId={:?} freq={:?} callsign={:?})",
                        bp.typ,
                        bp.shape,
                        bp.me_unit_id,
                        bp.atc.frequency,
                        bp.atc.callsign_id
                    );
                    return Ok(Spawned::Static);
                }

                // Multi-pad: one addGroup(-1) so DCS shares a single warehouse (ME multi-Invisible).
                let lua = self.lua.inner();
                let group_tbl = lua
                    .create_table()
                    .map_err(|e| anyhow!("create heliport group table: {e}"))?;
                let units_tbl = lua
                    .create_table()
                    .map_err(|e| anyhow!("create heliport units table: {e}"))?;
                let mut group_x = 0.;
                let mut group_y = 0.;
                for (i, bp) in blueprints.iter().enumerate() {
                    let world = Vector2::new(
                        bp.me_pos.x + (pos.x - origin.x),
                        bp.me_pos.y + (pos.y - origin.y),
                    );
                    if i == 0 {
                        group_x = world.x;
                        group_y = world.y;
                    }
                    let live_name = Self::ground_dep_live_pad_name(bp.me_name.as_str());
                    destroy_named_pad_unit(self.lua(), live_name.as_str());
                    let alt = land
                        .get_height(LuaVec2(world))
                        .context("land.getHeight for pad spawn")?;
                    let unit_tbl = lua
                        .create_table()
                        .map_err(|e| anyhow!("create heliport unit row: {e}"))?;
                    fill_heliport_unit_table(&unit_tbl, bp, live_name.as_str(), world, alt)?;
                    units_tbl.raw_set((i + 1) as i64, unit_tbl)?;
                }
                group_tbl.raw_set("name", live_group.as_str())?;
                group_tbl.raw_set("hidden", false)?;
                group_tbl.raw_set("visible", true)?;
                group_tbl.raw_set("lateActivation", false)?;
                group_tbl.raw_set("x", group_x)?;
                group_tbl.raw_set("y", group_y)?;
                group_tbl.raw_set("units", units_tbl)?;
                let group_data = miz::Group::from_lua(Value::Table(group_tbl), lua)
                    .map_err(|e| anyhow!("wrap heliport group table: {e}"))?;
                self.coalition
                    .add_farp_group(pad.country, group_data)
                    .with_context(|| {
                        format_compact!(
                            "spawning multi-pad heliport group {live_group} ({} pads)",
                            blueprints.len()
                        )
                    })?;
                info!(
                    "ground DEP pad: spawned {} pads as addGroup(-1) {live_group} (shared warehouse)",
                    blueprints.len()
                );
                Ok(Spawned::Static)
            }
        }
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
                // MOOSE SPAWNSTATIC: Template.alt = land.getHeight(map x/y)
                let pos = unit.pos().context("static unit pos")?;
                let alt = Land::singleton(self.lua)?
                    .get_height(LuaVec2(pos))
                    .context("land.getHeight for static spawn")?;
                unit.set_alt(alt).context("set static spawn alt")?;
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

    pub fn remove_junk(&self, point: Vector2, radius: f64) -> Result<()> {
        let alt = Land::singleton(self.lua)?.get_height(LuaVec2(point))?;
        let point = LuaVec3(Vector3::new(point.x, alt, point.y));
        let vol = SearchVolume::Sphere { point, radius };
        World::singleton(self.lua)?.remove_junk(vol)?;
        Ok(())
    }

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
