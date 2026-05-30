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

use std::sync::Arc;

use super::{Db, ephemeral::SlotInfo, group::DeployKind, objective::ObjGroup};
use crate::{
    bg::Task,
    db::{
        MapS,
        logistics::{LogiStage, Warehouse},
        objective::{Objective, Zone},
    },
    group, group_health, group_mut,
    landcache::LandCache,
    objective_mut,
    spawnctx::{SpawnCtx, SpawnLoc},
    unit, unit_mut,
};
use anyhow::{Context, Result, anyhow, bail};
use bfprotocols::{
    cfg::{Cfg, Vehicle},
    db::{
        group::GroupId,
        objective::{ObjectiveId, ObjectiveKind},
    },
    fowl_miz_export::FowlMizExport,
    perf::PerfInner,
    stats::Stat,
};
use chrono::prelude::*;
use compact_str::{CompactString, format_compact};
use dcso3::{
    airbase::Airbase,
    centroid2d, coalition::Side, controller::{MissionPoint, PointType}, coord::Coord,
    env::miz::{Group, Miz, MizIndex, Skill, TriggerZone, TriggerZoneTyp, UnitId},
    land::Land, net::Net, object::{DcsObject, Object}, trigger::Trigger, LuaVec2, LuaVec3, MizLua,
    String,
    Vector2, Vector3,
};
use enumflags2::BitFlags;
use fxhash::FxHashSet;
use log::{debug, error, info, warn};
use smallvec::{smallvec, SmallVec};
use tokio::sync::mpsc::UnboundedSender;

impl Db {
    /// objectives are just trigger zones named according to type codes
    /// the first caracter is the type of the zone
    /// O - Objective
    /// G - Group within an objective
    /// T - Generic trigger zone, ignored by the engine
    ///
    /// Then a 2 character type code
    /// - AB: Airbase
    /// - FO: Fob
    /// - SA: Sam site
    /// - LO: Logistics Objective
    /// - PR: Production Objective
    ///
    /// Then a 1 character code for the default owner
    /// followed by the display name
    /// - R: Red
    /// - B: Blue
    /// - N: Neutral
    ///
    /// So e.g. Tblisi would be OABBTBLISI -> Objective, Airbase, Default to Blue, named Tblisi
    fn init_objective(&mut self, lua: MizLua, zone: TriggerZone, name: &str) -> Result<()> {
        fn side_and_name(s: &str) -> Result<(Side, String)> {
            if let Some(name) = s.strip_prefix("R") {
                Ok((Side::Red, String::from(name)))
            } else if let Some(name) = s.strip_prefix("B") {
                Ok((Side::Blue, String::from(name)))
            } else if let Some(name) = s.strip_prefix("N") {
                Ok((Side::Neutral, String::from(name)))
            } else {
                bail!("invalid default coalition {s} expected B, R, or N prefix")
            }
        }
        let (kind, owner, name) = if let Some(name) = name.strip_prefix("AB") {
            let (side, name) = side_and_name(name)?;
            (ObjectiveKind::Airbase, side, name)
        } else if let Some(name) = name.strip_prefix("FO") {
            let (side, name) = side_and_name(name)?;
            (ObjectiveKind::Fob, side, name)
        } else if let Some(name) = name.strip_prefix("LO") {
            let (side, name) = side_and_name(name)?;
            (ObjectiveKind::Logistics, side, name)
        } else if let Some(name) = name.strip_prefix("PR") {
            let (side, name) = side_and_name(name)?;
            (ObjectiveKind::Production, side, name)
        } else {
            bail!("invalid objective type for {name}, expected AB, FO, LO, or PR")
        };
        let id = ObjectiveId::new();
        let mut logistics_detached = false;
        let mut production_capacity = 0u16;
        for pr in zone.properties()? {
            let pr = pr?;
            if &*pr.key == "LOGISTICS_DETACHED" {
                let v = pr.value.to_ascii_lowercase();
                if &*v == "true" {
                    logistics_detached = true;
                } else if &*v == "false" {
                    logistics_detached = false;
                } else {
                    bail!("invalid value of LOGISTICS_DETACHED {v}")
                }
            } else if pr.key.eq_ignore_ascii_case("capacity") {
                production_capacity = pr
                    .value
                    .parse()
                    .with_context(|| format_compact!("invalid Capacity on objective {name}"))?;
            } else if &*pr.key == "include"
                || &*pr.key == "include_dyn_slots"
                || pr.key.eq_ignore_ascii_case("airbaseID")
            {
                // `include*` / `include_dyn_slots` / `airbaseID` are consumed by bftools (offline
                // assembly + editor hints). bflib ignores them here so missions start.
                continue;
            } else {
                bail!("invalid objective property {pr:?}")
            }
        }
        let zone = match zone.typ()? {
            TriggerZoneTyp::Quad(points) => Zone::Quad {
                pos: centroid2d([points.p0.0, points.p1.0, points.p2.0, points.p3.0]),
                points,
            },
            TriggerZoneTyp::Circle { radius } => Zone::Circle {
                pos: zone.pos()?,
                radius,
            },
        };
        let obj = Objective {
            id,
            spawned: false,
            enabled: false,
            threatened: false,
            zone,
            name: name.clone(),
            kind,
            owner,
            groups: MapS::new(),
            health: 0,
            logi: 0,
            supply: 0,
            fuel: 0,
            production: 100,
            production_repair: 0,
            production_capacity,
            production_hp_sum: u32::from(production_capacity) * 100,
            production_repair_need: 0,
            feed_hub: None,
            production_repair_due: Utc::now(),
            last_change_ts: Utc::now(),
            last_threatened_ts: Utc::now(),
            warehouse: Warehouse::default(),
            points: 0,
            logistics_detached,
            last_activate: DateTime::<Utc>::default(),
            // initialized by load
            threat_pos3: Vector3::default(),
        };
        if let ObjectiveKind::Logistics = obj.kind {
            self.persisted.logistics_hubs.insert_cow(id);
        }
        let pos = zone.pos();
        let llpos = Coord::singleton(lua)?.lo_to_ll(LuaVec3(Vector3::new(pos.x, 0., pos.y)))?;
        self.ephemeral.stat(Stat::Objective {
            name: name.clone(),
            id,
            kind: obj.kind.clone(),
            owner: obj.owner,
            pos: llpos,
        });
        self.persisted.objectives.insert_cow(id, obj);
        self.persisted.objectives_by_name.insert_cow(name, id);
        Ok(())
    }

    /// Objective groups are trigger zones with the first character set to G. They are then a template
    /// name, followed by # and a number. They are associated with an objective by proximity.
    /// e.g. GRIRSRAD#001 would be the 1st instantiation of the template RIRSRAD, which must
    /// correspond to a group in the miz file. There is one special template name called (R|B|N)LOGI
    /// which corresponds to the logistics template for objectives
    fn init_objective_group(
        &mut self,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        _miz: &Miz,
        zone: TriggerZone,
        side: Side,
        name: &str,
    ) -> Result<()> {
        let pos = zone.pos()?;
        let obj = {
            let mut iter = self.persisted.objectives.into_iter();
            loop {
                match iter.next() {
                    None => bail!("group {:?} isn't associated with an objective", name),
                    Some((id, obj)) => {
                        if obj.zone.contains(pos) {
                            break *id;
                        }
                    }
                }
            }
        };
        let gid = self.add_group(
            spctx,
            idx,
            side,
            SpawnLoc::AtPos {
                pos,
                offset_direction: Vector2::default(),
                group_heading: 0.,
            },
            name,
            DeployKind::Objective { origin: obj },
            BitFlags::empty(),
            None,
            None,
        )?;
        let o = objective_mut!(self, obj)?;
        o.groups.get_or_default_cow(side).insert_cow(gid);
        let owner = o.owner;
        self.persisted.objectives_by_group.insert_cow(gid, obj);
        if side != owner {
            for uid in group!(self, gid)?.units.clone().into_iter() {
                unit_mut!(self, uid)?.dead = true;
            }
        }
        Ok(())
    }

    fn link_production_statics_from_miz(&mut self, miz: &Miz) -> Result<()> {
        use super::objective::ObjGroupClass;

        let factories = self.ephemeral.cfg.production_factory_units.clone();
        if factories.is_empty() {
            warn!("production_factory_units is empty; OPR factory statics will not be linked");
            return Ok(());
        }
        let mut linked = 0usize;
        let mut pending: SmallVec<[(Side, ObjectiveId, String, String, String, Vector2, f64); 32]> =
            smallvec![];
        for side in Side::ALL {
            let coa = miz.coalition(side)?;
            for country in coa.countries()? {
                let country = country?;
                for group in country.statics()? {
                    let group = group?;
                    let group_name = group.name()?;
                    let mut unit_name = None;
                    let mut unit_type = None;
                    let mut pos = None;
                    let mut heading = 0f64;
                    for u in group.units()? {
                        let u = u?;
                        unit_type = Some(u.typ()?);
                        unit_name = Some(u.name()?);
                        pos = Some(u.pos()?);
                        heading = u.heading().unwrap_or(0.);
                        break;
                    }
                    let (unit_type, unit_name, pos) = match (unit_type, unit_name, pos) {
                        (Some(t), Some(n), Some(p)) => (t, n, p),
                        _ => continue,
                    };
                    if !factories.contains(&String::from(unit_type.as_str())) {
                        continue;
                    }
                    if self.persisted.units_by_name.get(unit_name.as_str()).is_some() {
                        continue;
                    }
                    let oid = self
                        .persisted
                        .objectives
                        .into_iter()
                        .filter(|(_, obj)| {
                            obj.owner == side
                                && matches!(obj.kind, ObjectiveKind::Production)
                                && obj.zone.contains(pos)
                        })
                        .min_by(|(_, a), (_, b)| {
                            let da =
                                na::distance_squared(&a.zone.pos().into(), &pos.into());
                            let db =
                                na::distance_squared(&b.zone.pos().into(), &pos.into());
                            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                        })
                        .map(|(id, _)| *id);
                    let Some(oid) = oid else {
                        continue;
                    };
                    pending.push((
                        side,
                        oid,
                        group_name.clone(),
                        unit_name.clone(),
                        unit_type.clone(),
                        pos,
                        heading,
                    ));
                }
            }
        }
        for (side, oid, group_name, unit_name, unit_type, pos, heading) in pending {
            self.register_production_static_group(
                side, oid, group_name, unit_name, unit_type, pos, heading,
            )?;
            linked += 1;
        }
        if linked > 0 {
            let capacity: u32 = self
                .persisted
                .objectives
                .into_iter()
                .filter(|(_, o)| matches!(o.kind, ObjectiveKind::Production))
                .map(|(_, o)| o.production_capacity as u32)
                .sum();
            info!("linked {linked} production factory static(s) into OPR zones");
            if linked as u32 > capacity.saturating_mul(2).max(1) {
                warn!(
                    "linked {linked} factory static(s) but sum of OPR Capacity is {capacity}; \
                     check overlapping OPR zones or factory types matching non-factory buildings"
                );
            }
        }
        for (_, obj) in self.persisted.objectives.iter_mut_cow() {
            if !matches!(obj.kind, ObjectiveKind::Production) {
                continue;
            }
            if obj.production_capacity > 0 {
                continue;
            }
            let n = obj
                .groups
                .get(&obj.owner)
                .map(|gs| {
                    gs.into_iter()
                        .filter_map(|gid| group!(self, gid).ok())
                        .filter(|g| g.class == ObjGroupClass::Production)
                        .map(|g| g.units.len())
                        .sum::<usize>()
                })
                .unwrap_or(0) as u16;
            if n > 0 {
                obj.production_capacity = n;
            }
        }
        Ok(())
    }

    fn zone_from_trigger(zone: &TriggerZone<'_>) -> Result<Zone> {
        Ok(match zone.typ()? {
            TriggerZoneTyp::Quad(points) => Zone::Quad {
                pos: centroid2d([points.p0.0, points.p1.0, points.p2.0, points.p3.0]),
                points,
            },
            TriggerZoneTyp::Circle { radius } => Zone::Circle {
                pos: zone.pos()?,
                radius,
            },
        })
    }

    fn build_carrier_slot_maps(&mut self, miz: &Miz) -> Result<()> {
        self.ephemeral.pad_template_to_objective.clear();
        for (id, obj) in self.persisted.objectives.into_iter() {
            if let ObjectiveKind::Farp { pad_template, .. } = &obj.kind {
                self.ephemeral
                    .pad_template_to_objective
                    .insert(pad_template.clone(), *id);
            }
        }
        self.ephemeral.naval_slot_zones.clear();
        for zone in miz.triggers()? {
            let zone = zone?;
            let name = zone.name()?;
            let Some(pad) = name.strip_prefix("TTSN") else {
                continue;
            };
            self.ephemeral
                .naval_slot_zones
                .insert(String::from(pad), Self::zone_from_trigger(&zone)?);
        }
        Ok(())
    }

    fn pad_template_from_link_offset(
        miz: &Miz,
        idx: &MizIndex,
        slot: &Group,
    ) -> Result<Option<String>> {
        let link_offset = slot.raw_get::<_, bool>("linkOffset").unwrap_or(false);
        let route = slot.route()?;
        for point in route.points()? {
            let point: MissionPoint = point?;
            let link_id = point.link_unit.or_else(|| {
                point.helipad.map(|h| UnitId::from(h.inner()))
            });
            let Some(link_id) = link_id else {
                continue;
            };
            if !link_offset && point.typ != PointType::TakeOffParking {
                continue;
            }
            let Some(ifo) = miz.get_group_by_unit(idx, &link_id)? else {
                continue;
            };
            return Ok(Some(ifo.group.name()?));
        }
        Ok(None)
    }

    pub(crate) fn objective_id_for_carrier_airbase(
        &self,
        airbase_oid: &dcso3::object::DcsOid<dcso3::airbase::ClassAirbase>,
    ) -> Option<ObjectiveId> {
        self.ephemeral
            .airbases_by_oid
            .iter()
            .find(|(_, oids)| oids.iter().any(|o| o == airbase_oid))
            .map(|(oid, _)| *oid)
    }

    pub(crate) fn objective_for_slot_birth<'lua>(
        &self,
        lua: MizLua<'lua>,
        birth_place: Option<&Object<'lua>>,
        pos: Vector2,
    ) -> Option<ObjectiveId> {
        if let Some(place) = birth_place {
            if let Ok(place_oid) = place.object_id() {
                if let Ok(ab) = Airbase::get_instance_dyn(lua, &place_oid) {
                    if let Ok(ab_oid) = ab.object_id() {
                        if let Some(oid) = self.objective_id_for_carrier_airbase(&ab_oid) {
                            return Some(oid);
                        }
                    }
                }
            }
        }
        for (id, obj) in self.persisted.objectives.into_iter() {
            if obj.zone.contains(pos) {
                return Some(*id);
            }
        }
        for (pad, zone) in &self.ephemeral.naval_slot_zones {
            if zone.contains(pos) {
                if let Some(&oid) = self.ephemeral.pad_template_to_objective.get(pad) {
                    return Some(oid);
                }
            }
        }
        None
    }

    fn resolve_client_slot_objective(
        &self,
        miz: &Miz,
        idx: &MizIndex,
        pos: Vector2,
        slot: &Group,
    ) -> Result<Option<ObjectiveId>> {
        for (id, obj) in self.persisted.objectives.into_iter() {
            if obj.zone.contains(pos) {
                return Ok(Some(*id));
            }
        }
        for (pad, zone) in &self.ephemeral.naval_slot_zones {
            if zone.contains(pos) {
                if let Some(&oid) = self.ephemeral.pad_template_to_objective.get(pad) {
                    return Ok(Some(oid));
                }
            }
        }
        if let Some(pad) = Self::pad_template_from_link_offset(miz, idx, slot)? {
            if let Some(&oid) = self.ephemeral.pad_template_to_objective.get(&pad) {
                return Ok(Some(oid));
            }
        }
        Ok(None)
    }

    pub fn init_objective_slots(
        &mut self,
        miz: &Miz,
        idx: &MizIndex,
        side: Side,
        slot: Group,
    ) -> Result<()> {
        // Warehouse dyn templates and carrier-linked static slots use deck routes without ComboTask.
        if slot.raw_get::<_, bool>("dynSpawnTemplate").unwrap_or(false) {
            return Ok(());
        }
        let mut ground_start = false;
        if !slot.raw_get::<_, bool>("linkOffset").unwrap_or(false) {
            for point in slot.route()?.points()? {
                let point = point?;
                match point.typ {
                    PointType::TakeOffGround | PointType::TakeOffGroundHot => {
                        ground_start = true
                    }
                    PointType::Land
                    | PointType::TakeOff
                    | PointType::Custom(_)
                    | PointType::Nil
                    | PointType::TakeOffParking
                    | PointType::TurningPoint => (),
                }
            }
        }
        for unit in slot.units()? {
            let unit = unit?;
            let vehicle = Vehicle::from(unit.typ()?);
            self.ephemeral
                .cfg
                .check_vehicle_has_threat_distance(&vehicle)?;
            let Ok(skill) = unit.skill() else {
                continue;
            };
            if skill != Skill::Client {
                continue;
            }
            let id = unit.slot()?;
            let pos = unit.pos()?;
            let Some(obj) = self.resolve_client_slot_objective(miz, idx, pos, &slot)? else {
                info!(
                    "slot {:?} unit {:?} not associated with an objective",
                    slot.name()?,
                    unit.name()?
                );
                continue;
            };
            self.ephemeral.cfg.check_vehicle_has_life_type(&vehicle)?;
            self.ephemeral.slot_info.insert(
                id.clone(),
                SlotInfo {
                    typ: vehicle,
                    unit_name: unit.name()?,
                    objective: obj,
                    ground_start,
                    miz_gid: slot.id()?,
                    side,
                },
            );
        }
        Ok(())
    }

    pub fn init(
        lua: MizLua,
        cfg: Arc<Cfg>,
        idx: &MizIndex,
        miz: &Miz,
        to_bg: UnboundedSender<Task>,
        fowl_miz_export: Arc<FowlMizExport>,
    ) -> Result<Self> {
        let spctx = SpawnCtx::new(lua)?;
        let mut t = Self::default();
        t.ephemeral
            .set_cfg(miz, idx, cfg, to_bg, fowl_miz_export)?;
        let mut objective_names = FxHashSet::default();
        for zone in miz.triggers()? {
            let zone = zone?;
            let name = zone.name()?;
            if name.starts_with('O') {
                let stem = name
                    .strip_prefix('O')
                    .filter(|s| s.len() > 3)
                    .ok_or_else(|| anyhow!("malformed objective zone name {name}"))?;
                if !objective_names.insert(CompactString::from(stem)) {
                    bail!(
                        "duplicate objective zone stem O{stem} (second zone named {name})"
                    );
                }
                t.init_objective(lua, zone, stem)?
            }
        }
        t.build_carrier_slot_maps(miz)?;
        for side in Side::ALL {
            let coa = miz.coalition(side)?;
            for zone in miz.triggers()? {
                let zone = zone?;
                let name = zone.name()?;
                if let Some(name) = name.strip_prefix("G") {
                    let (template_side, name) = name.parse::<ObjGroup>()?.template(side);
                    if template_side == side {
                        t.init_objective_group(&spctx, idx, miz, zone, side, name.as_str())?
                    }
                } else if bfprotocols::miz_trigger::fowl_trigger_zone_name_valid(&name) {
                    () // O, T, SETTINGS-; G handled above
                } else {
                    bail!(
                        "invalid trigger zone type code {name}, expected {}",
                        bfprotocols::miz_trigger::FOWL_TRIGGER_ZONE_EXPECTED_PREFIXES_DISPLAY
                    )
                }
            }
            for country in coa.countries()? {
                let country = country?;
                for plane in country.planes()? {
                    let plane = plane?;
                    t.init_objective_slots(miz, idx, side, plane)?
                }
                for heli in country.helicopters()? {
                    let heli = heli?;
                    t.init_objective_slots(miz, idx, side, heli)?
                }
            }
        }
        let now = Utc::now();
        let ids = t
            .persisted
            .objectives
            .into_iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        t.link_production_statics_from_miz(miz)?;
        t.sync_production_static_uid_map(lua)
            .context("syncing production factory static object ids")?;
        for id in ids {
            t.update_objective_status(Some(lua), &id, now)?
        }
        t.refresh_hub_production_from_opr()
            .context("OPR feed hubs after factory link")?;
        t.seed_objective_warehouses_from_export(lua)
            .context("seeding objective warehouses from Fowl export")?;
        t.ephemeral.preserve_initial_warehouse_fill = true;
        t.ephemeral.defer_initial_hub_distribute = true;
        t.ephemeral.defer_initial_logistics_sync_to = true;
        t.ephemeral.dirty();
        Ok(t)
    }

    pub fn respawn_after_load(
        &mut self,
        lua: MizLua,
        perf: &mut PerfInner,
        idx: &MizIndex,
        miz: &Miz,
        landcache: &mut LandCache,
        spctx: &SpawnCtx,
    ) -> Result<()> {
        debug!("init slots");
        self.build_carrier_slot_maps(miz)?;
        self.sync_production_static_uid_map(lua)
            .context("syncing production factory static object ids after load")?;
        // migrate format changes
        if !self.persisted.migrated_v0 {
            self.persisted.migrated_v0 = true;
            self.ephemeral.dirty();
            for (oid, obj) in &self.persisted.objectives {
                for (_, groups) in &obj.groups {
                    for gid in groups {
                        let g = group_mut!(self, gid)?;
                        match &g.origin {
                            DeployKind::ObjectiveDeprecated => {
                                g.origin = DeployKind::Objective { origin: *oid };
                            }
                            _ => (),
                        }
                        for uid in &g.units {
                            let unit = unit_mut!(self, uid)?;
                            if unit.side != obj.owner {
                                unit.dead = true;
                            }
                        }
                    }
                }
            }
        }
        for side in Side::ALL {
            let coa = miz.coalition(side)?;
            for country in coa.countries()? {
                let country = country?;
                for plane in country.planes()? {
                    let plane = plane?;
                    self.init_objective_slots(miz, idx, side, plane)?
                }
                for heli in country.helicopters()? {
                    let heli = heli?;
                    self.init_objective_slots(miz, idx, side, heli)?
                }
            }
        }
        for name in &self.ephemeral.cfg.extra_fixed_wing_objectives {
            if !self.persisted.objectives_by_name.get(name).is_some() {
                bail!("extra_fixed_wing_objectives {name} does not match any objective")
            }
        }
        let mut spawn_deployed_and_logistics = || -> Result<()> {
            debug!("queue respawn deployables");
            let land = Land::singleton(spctx.lua())?;
            for gid in &self.persisted.deployed {
                self.ephemeral.push_spawn(*gid);
            }
            for gid in &self.persisted.crates {
                self.ephemeral.push_spawn(*gid);
            }
            for gid in &self.persisted.troops {
                self.ephemeral.push_spawn(*gid);
            }
            let actions: SmallVec<[GroupId; 16]> =
                SmallVec::from_iter(self.persisted.actions.into_iter().map(|g| *g));
            debug!("respawn actions");
            for gid in actions {
                if let Err(e) = self.respawn_action(perf, spctx, idx, gid) {
                    error!("failed to respawn action {e:?}");
                }
            }
            debug!("respawning farps");
            for (_, obj) in self.persisted.objectives.iter_mut_cow() {
                let pos = obj.zone.pos();
                let alt = land.get_height(LuaVec2(pos))? + 50.;
                obj.threat_pos3 = Vector3::new(pos.x, alt, pos.y);
                if let ObjectiveKind::Farp {
                    spec: _,
                    mobile,
                    pad_template,
                } = &obj.kind
                {
                    // ME DEP pad templates exist at editor coords after every mission load (`pad_live`).
                    // Ground DEP FARPs must still relocate to the persisted deploy position (same as
                    // `add_farp`); skipping when `pad_live` left the invisible pad off-map.
                    if *mobile {
                        spctx
                            .move_farp_pad(idx, obj.owner, &pad_template, pos)
                            .context("moving mobile farp pad")?;
                    } else {
                        spctx
                            .move_farp_pad(idx, obj.owner, &pad_template, pos)
                            .context("moving ground DEP FARP pad after load")?;
                        info!(
                            "respawned ground DEP FARP {:?} pad {:?} at persisted position",
                            obj.name, pad_template
                        );
                    }
                    self.ephemeral.set_pad_template_used(pad_template.clone());
                }
                if let Some(groups) = obj.groups.get(&obj.owner) {
                    for gid in groups {
                        let group = group!(self, gid)?;
                        if !obj.kind.is_farp() && !group.class.is_services() {
                            continue;
                        }
                        // Pad row is handled only in the FARP block above; spawning it again here
                        // duplicates the ME group (carriers jump back to template). Defenses use
                        // different template_name values than pad_template.
                        if let ObjectiveKind::Farp { pad_template: pt, .. } = &obj.kind {
                            if group.template_name.as_str() == pt.as_str() {
                                continue;
                            }
                        }
                        self.ephemeral.push_spawn(*gid)
                    }
                }
                // spawn left behind base defenses
                if let Some(groups) = obj.groups.get(&obj.owner.opposite()) {
                    for gid in groups {
                        if group_health!(self, gid)?.0 > 0 {
                            self.ephemeral.push_spawn(*gid);
                        }
                    }
                }
            }
            Ok(())
        };
        spawn_deployed_and_logistics().context("spawning deployed and logistics")?;
        // spawn everything before setting up warehouses, so that ship warehouses will also be set up correctly
        while self.ephemeral.spawnq_len() > 0 {
            self.ephemeral.process_spawn_queue(perf, &self.persisted, Utc::now(), idx, spctx)?
        }
        self.setup_warehouses_after_load(spctx.lua())
            .context("setting up warehouses")?;
        self.refresh_hub_production_from_opr()
            .context("OPR feed hubs before objective markup")?;
        let mut mark_deployed_and_logistics = || -> Result<()> {
            let groups = self
                .persisted
                .groups
                .into_iter()
                .map(|(gid, _)| *gid)
                .collect::<Vec<_>>();
            for gid in groups {
                self.mark_group(lua, &gid)?
            }
            for (_, obj) in &self.persisted.objectives {
                self.ephemeral.create_objective_markup(&self.persisted, obj)
            }
            Ok(())
        };
        mark_deployed_and_logistics().context("marking deployed and logistics")?;
        let net = Net::singleton(lua)?;
        let act = Trigger::singleton(lua)?.action()?;
        // spawn all the markup
        while self.ephemeral.msgs.len() > 0 {
            self.ephemeral.msgs.process(100, &net, &act);
        }
        let mut queue_check_close_enemies = || -> Result<()> {
            for (uid, unit) in &self.persisted.units {
                if !unit.dead {
                    self.ephemeral
                        .units_potentially_close_to_enemies
                        .insert(*uid);
                }
            }
            Ok(())
        };
        queue_check_close_enemies().context("queuing unit pos checks")?;
        self.cull_or_respawn_objectives(spctx.lua(), landcache, Utc::now())
            .context("initial cull or respawn")?;
        // return lives to pilots who were airborne on the last restart
        let airborne_players = self
            .persisted
            .players
            .into_iter()
            .filter_map(|(ucid, p)| p.airborne.and_then(|lt| Some((ucid.clone(), lt))))
            .collect::<Vec<_>>();
        for (ucid, lt) in airborne_players {
            let player = &mut self.persisted.players[&ucid];
            player.airborne = None;
            if let Some((_, lives)) = player.lives.get_mut_cow(&lt) {
                *lives += 1;
                if *lives >= self.ephemeral.cfg.default_lives[&lt].0 {
                    player.lives.remove_cow(&lt);
                }
                self.ephemeral.stat(Stat::Life {
                    id: ucid,
                    lives: player.lives.clone(),
                });
                self.ephemeral.dirty();
            }
        }
        Ok(())
    }

    /// Wall-clock time when `place_tisp_initial_ships` should run (new round only).
    pub fn schedule_tisp_initial_ship_placement(&mut self, at: DateTime<Utc>) {
        self.ephemeral.tisp_initial_after = Some(at);
    }

    pub fn try_run_deferred_tisp_initial_ships(
        &mut self,
        lua: MizLua,
        idx: &MizIndex,
        now: DateTime<Utc>,
    ) -> Result<()> {
        match self.ephemeral.tisp_initial_after {
            Some(at) if now >= at => {}
            _ => return Ok(()),
        }
        self.ephemeral.tisp_initial_after = None;
        self.ephemeral.defer_initial_hub_distribute = true;
        self.ephemeral.defer_initial_logistics_sync_to = true;
        let miz = Miz::singleton(lua)?;
        let spctx = SpawnCtx::new(lua)?;
        if let Err(e) = super::tisp_init::place_tisp_initial_ships(&miz, idx, self, &spctx)
            .context("deferred TISP naval ship placement")
        {
            error!(
                "deferred TISP naval ship placement failed (use -action if needed): {e:?}"
            );
            self.ephemeral.defer_initial_hub_distribute = false;
            return Ok(());
        }
        self.ephemeral.preserve_initial_warehouse_fill = true;
        self.setup_warehouses_after_load(lua)
            .context("warehouses after deferred TISP")?;
        self.ephemeral.defer_initial_hub_distribute = false;
        self.ephemeral.defer_initial_logistics_sync_to = true;
        let objectives = self
            .persisted
            .objectives
            .into_iter()
            .map(|(id, _)| *id)
            .collect();
        self.ephemeral.logistics_stage = LogiStage::SyncFromWarehouses { objectives };
        info!("warehouses re-synced after deferred TISP placement (bftools fill preserved)");
        self.ephemeral.dirty();
        Ok(())
    }
}
