use super::{
    group::{DeployKind, SpawnedUnit},
    objective::Objective,
    Db,
};
use crate::{
    group, group_mut, objective, objective_mut,
    spawnctx::SpawnCtx,
};
use anyhow::{anyhow, bail, Context, Result};
use bfprotocols::{
    cfg::{ActionKind, AiPlaneCfg, AiPlaneKind, UnitTag},
    db::{
        group::GroupId,
        objective::{ObjectiveId, ObjectiveKind},
    },
};
use chrono::{DateTime, Duration, Utc};
use compact_str::{format_compact, CompactString};
use rand::{Rng, thread_rng};
use dcso3::{
    airbase::{Airbase, AirbaseId},
    centroid2d,
    coalition::{Side, Static},
    controller::{
        ActionTyp, AiOption, AirOption, AltType, Controller, MissionPoint, OrbitPattern,
        PointType, Task, TurnMethod,
    },
    env::miz::{self, GroupInfo, GroupKind, MizIndex, UnitId},
    group::Group,
    land::{Land, SurfaceType},
    net::Ucid,
    object::{DcsObject, Object, ObjectCategory},
    perf::record_perf,
    static_object::StaticObject,
    unit::Unit,
    warehouse::LiquidType,
    weapon::WeaponFlag,
    world::{SearchVolume, World},
    LuaEnv, LuaVec2, LuaVec3, MizLua, String, Vector2, Vector3,
};
use fxhash::{FxHashMap, FxHashSet};
use mlua::{Table, Value};
use serde_derive::{Deserialize, Serialize};
use smallvec::{smallvec, SmallVec};
use std::f64;

/// Min distance² (m²) from action mark to hub for fixed-wing airbase spawn (CAP not from overflight mark).
pub(super) const MIN_MARK_HUB_DIST_SQ: f64 = 100_000_000.;
/// Min distance² for heli using airfield parking (~1 km).
const MIN_MARK_HUB_AIRFIELD_HELI_DIST_SQ: f64 = 1_000_000.;

fn objective_is_heli_spawn_hub(db: &Db, obj: &Objective) -> bool {
    match obj.kind {
        ObjectiveKind::Airbase | ObjectiveKind::Fob | ObjectiveKind::Farp { .. } => true,
        ObjectiveKind::Logistics => objective_has_airfield_hub(db, obj),
        _ => false,
    }
}

fn min_mark_hub_dist_sq(obj: &Objective, plane_kind: AiPlaneKind) -> f64 {
    match (plane_kind, &obj.kind) {
        (AiPlaneKind::FixedWing, ObjectiveKind::Airbase | ObjectiveKind::Logistics) => {
            MIN_MARK_HUB_DIST_SQ
        }
        (AiPlaneKind::Helicopter, ObjectiveKind::Airbase) => MIN_MARK_HUB_AIRFIELD_HELI_DIST_SQ,
        _ => 0.,
    }
}

/// Max distance² (m²) between Fob and deployed FARP zone centers to share helipad slots (~5 km).
const NEARBY_FARP_HELIPAD_MAX_DIST_SQ: f64 = 25_000_000.;
/// Helipad search radius² from FOB/OFO zone center (~15 km).
const FO_HELIPAD_SEARCH_RADIUS_SQ: f64 = 225_000_000.;

fn objectives_near(a: &Objective, b: &Objective, max_dist_sq: f64) -> bool {
    na::distance_squared(&a.zone.pos().into(), &b.zone.pos().into()) <= max_dist_sq
}

fn helipads_near_point(
    lua: MizLua,
    db: &Db,
    side: Side,
    center: Vector2,
    max_dist_sq: f64,
) -> Vec<HubSlot> {
    let mut out = Vec::new();
    let mut seen = FxHashSet::default();
    for (_, unit) in db.persisted.units.into_iter() {
        if unit.side != side || unit.dead {
            continue;
        }
        if na::distance_squared(&unit.pos.into(), &center.into()) > max_dist_sq {
            continue;
        }
        if !is_helipad_facility(unit.typ.0.as_str()) {
            continue;
        }
        let Some(slot_id) = helipad_facility_id(lua, unit.name.as_str()) else {
            continue;
        };
        if !seen.insert(slot_id) {
            continue;
        }
        out.push(HubSlot {
            kind: HubSlotKind::Helipad,
            slot_id,
            pos: unit.pos,
            heading: unit.heading,
            baro_alt: None,
            term_type: None,
            heading_from_spot: true,
            link_unit: None,
        });
    }
    out
}

fn helipad_slots_for_heli_hub(
    lua: MizLua,
    db: &Db,
    hub: &Objective,
    side: Side,
) -> Result<Vec<HubSlot>> {
    let mut out = helipad_slots_in_zone(lua, db, hub, side)?;
    let mut seen: FxHashSet<i64> = out.iter().map(|s| s.slot_id).collect();
    let center = hub.zone.pos();
    if matches!(hub.kind, ObjectiveKind::Fob | ObjectiveKind::Logistics) {
        for slot in helipads_near_point(lua, db, side, center, FO_HELIPAD_SEARCH_RADIUS_SQ) {
            if seen.insert(slot.slot_id) {
                out.push(slot);
            }
        }
    }
    if matches!(hub.kind, ObjectiveKind::Fob) {
        for (_, farp) in db.persisted.objectives.into_iter() {
            if farp.owner != side || !matches!(farp.kind, ObjectiveKind::Farp { .. }) {
                continue;
            }
            for slot in helipad_slots_in_zone(lua, db, farp, side)? {
                if seen.contains(&slot.slot_id) {
                    continue;
                }
                if hub.zone.contains(slot.pos)
                    || objectives_near(hub, farp, NEARBY_FARP_HELIPAD_MAX_DIST_SQ)
                    || na::distance_squared(&slot.pos.into(), &center.into())
                        <= FO_HELIPAD_SEARCH_RADIUS_SQ
                {
                    seen.insert(slot.slot_id);
                    out.push(slot);
                }
            }
        }
    }
    Ok(out)
}

fn airbase_point_pos(lua: MizLua, ab_oid: &dcso3::object::DcsOid<dcso3::airbase::ClassAirbase>) -> Result<Vector2> {
    let ab = Airbase::get_instance(lua, ab_oid)?;
    let p = ab.get_point()?;
    Ok(Vector2::new(p.x, p.z))
}

fn hub_mark_dist_sq(
    lua: MizLua,
    db: &Db,
    obj: &Objective,
    side: Side,
    plane_kind: AiPlaneKind,
    mark_pos: Vector2,
) -> Result<f64> {
    match plane_kind {
        AiPlaneKind::Helicopter => {
            let slots = helipad_slots_for_heli_hub(lua, db, obj, side)?;
            if let Some(d) = slots
                .iter()
                .map(|s| na::distance_squared(&s.pos.into(), &mark_pos.into()))
                .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            {
                return Ok(d);
            }
        }
        AiPlaneKind::FixedWing if objective_has_airfield_hub(db, obj)
            || objective_has_operational_carrier(lua, db, obj) =>
        {
            if objective_is_naval_carrier(db, obj) {
                if let Some(slot) = carrier_fallback_deck_slot(lua, db, obj)? {
                    return Ok(na::distance_squared(&slot.pos.into(), &mark_pos.into()));
                }
            }
            if let Some(ab_oid) = hub_airbase_oid(lua, db, obj.id)? {
                if let Ok(spots) = parking_spots(lua, &ab_oid) {
                    if let Some(d) = spots
                        .iter()
                        .map(|s| na::distance_squared(&s.pos.into(), &mark_pos.into()))
                        .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                    {
                        return Ok(d);
                    }
                }
                let ab_pos = airbase_point_pos(lua, &ab_oid)?;
                return Ok(na::distance_squared(&ab_pos.into(), &mark_pos.into()));
            }
            if let Some(slot) = carrier_fallback_deck_slot(lua, db, obj)? {
                return Ok(na::distance_squared(&slot.pos.into(), &mark_pos.into()));
            }
        }
        AiPlaneKind::FixedWing => {}
    }
    Ok(na::distance_squared(&obj.zone.pos().into(), &mark_pos.into()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HubSelectMode {
    #[default]
    Spawn,
    /// RTB / bingo: nearest landable hub, no warehouse stock checks, no min mark distance.
    Landing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AiAirPhase {
    #[default]
    Legacy,
    Bootstrap,
    OnMission,
    RtbInbound,
    TaxiToParking,
    Servicing,
    /// Drone: warehouse refuel on parking before `-action start`.
    Refueling,
    AwaitingLaunch,
    Departing,
    /// Duration expiry: all members landed, engines off, awaiting DCS removal.
    ShutdownParked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HubSlotKind {
    Parking,
    Helipad,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HubSlot {
    pub kind: HubSlotKind,
    /// DCS parking term index or helipad unit id.
    pub slot_id: i64,
    pub pos: Vector2,
    pub heading: f64,
    /// BARO MSL from `getParking` `vTerminalPos.y` when known.
    #[serde(default)]
    pub baro_alt: Option<f64>,
    /// `getParking` `Term_Type` when known (open apron vs hangar, etc.).
    #[serde(default)]
    pub term_type: Option<i64>,
    #[serde(default)]
    heading_from_spot: bool,
    /// Naval carrier deck: DCS ship `unitId` for `linkUnit` / `helipadId`.
    #[serde(default)]
    pub link_unit: Option<i64>,
}

fn default_alt_typ() -> AltType {
    AltType::BARO
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveMissionSnapshot {
    pub pos: Vector2,
    pub alt: f64,
    #[serde(default = "default_alt_typ")]
    pub alt_typ: AltType,
    pub speed: f64,
    pub racetrack: bool,
    pub destination: Option<Vector2>,
}

impl Default for ActiveMissionSnapshot {
    fn default() -> Self {
        Self {
            pos: Vector2::default(),
            alt: 0.,
            alt_typ: AltType::BARO,
            speed: 0.,
            racetrack: false,
            destination: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LoadoutLine {
    pub name: String,
    pub requested: u32,
    pub loaded: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AiAirState {
    pub phase: AiAirPhase,
    pub hub: Option<ObjectiveId>,
    pub hub_slots: Vec<HubSlot>,
    pub active_mission: ActiveMissionSnapshot,
    pub rtb_hold: bool,
    pub bootstrap_retries: u8,
    /// Saw on ground during Bootstrap before treating as airborne.
    #[serde(default)]
    pub bootstrap_grounded: bool,
    #[serde(default)]
    pub bootstrap_mission_pushed: bool,
    #[serde(default)]
    pub refuel_mission_pushed: bool,
    /// One DCS group per airframe (ME parking spawn).
    #[serde(default)]
    pub dcs_spawn_names: Vec<String>,
    /// Spawn template cfg; kept after `ActionKind::Rtb` for hub selection and servicing.
    #[serde(default)]
    pub plane_cfg: Option<AiPlaneCfg>,
    #[serde(default)]
    pub mission_kind: AiAirMissionKind,
    #[serde(default = "Utc::now")]
    pub phase_since: DateTime<Utc>,
    /// Duration elapsed: RTB shutdown only; players cannot extend via commands.
    #[serde(default)]
    pub duration_shutdown: bool,
    /// Any member destroyed (combat, crash, collision) — never re-spawn from persist.
    #[serde(default)]
    pub attrition: bool,
}

#[derive(Debug, Clone)]
pub struct HubPick {
    pub oid: ObjectiveId,
    pub slots: Vec<HubSlot>,
    pub airbase_id: Option<AirbaseId>,
    /// Hub zone center — ME group/route/unit anchor (DCS snaps via `parking`).
    pub anchor: Vector2,
    /// BARO reference altitude for ME-style parking spawn.
    pub baro_alt: f64,
}

#[derive(Debug, Clone)]
pub struct RtbRequest {
    pub group: GroupId,
    pub hub: Option<ObjectiveId>,
    pub hold: bool,
    /// Bingo auto-RTB keeps CAP/SEAD/etc. so the cycle can resume the same mission.
    pub preserve_mission_kind: bool,
    /// Campaign `duration` expiry: land, park off, let DCS despawn; no persist delete until gone.
    pub duration_shutdown: bool,
}

pub(super) fn slot_claim_key(oid: ObjectiveId, slot: &HubSlot) -> (ObjectiveId, HubSlotKind, i64) {
    (oid, slot.kind, slot.slot_id)
}

pub(super) fn claimed_hub_slots(db: &Db) -> FxHashSet<(ObjectiveId, HubSlotKind, i64)> {
    claimed_hub_slots_excluding(db, None)
}

pub(super) fn claimed_hub_slots_excluding(
    db: &Db,
    except: Option<GroupId>,
) -> FxHashSet<(ObjectiveId, HubSlotKind, i64)> {
    let mut set = FxHashSet::default();
    for gid in db.persisted.actions.into_iter() {
        if except == Some(*gid) {
            continue;
        }
        let Some(group) = db.persisted.groups.get(gid) else {
            continue;
        };
        let DeployKind::Action { ai_air, .. } = &group.origin else {
            continue;
        };
        let Some(hub) = ai_air.hub else {
            continue;
        };
        for slot in &ai_air.hub_slots {
            set.insert(slot_claim_key(hub, slot));
        }
    }
    set
}

fn objective_has_airfield_hub(db: &Db, obj: &Objective) -> bool {
    db.ephemeral.airbase_by_oid.contains_key(&obj.id)
}

fn farp_pad_template(obj: &Objective) -> Option<&String> {
    match &obj.kind {
        ObjectiveKind::Farp { pad_template, .. } => Some(pad_template),
        _ => None,
    }
}

fn objective_is_naval_carrier(db: &Db, obj: &Objective) -> bool {
    farp_pad_template(obj)
        .is_some_and(|pad| db.ephemeral.global_pad_templates.contains(pad.as_str()))
}

fn carrier_ship_unit_id(lua: MizLua, db: &Db, pad_template: &str) -> Result<Option<i64>> {
    for (_, group) in db.persisted.groups.into_iter() {
        if group.template_name.as_str() != pad_template {
            continue;
        }
        let Ok(g) = Group::get_by_name(lua, group.name.as_str()) else {
            continue;
        };
        if !g.is_exist()? {
            continue;
        }
        let u = g.get_unit(1).context("carrier hull unit")?;
        if !u.is_exist()? {
            continue;
        }
        return Ok(Some(u.id()?.inner()));
    }
    Ok(None)
}

fn objective_has_live_carrier_airbase(lua: MizLua, db: &Db, obj: &Objective) -> bool {
    if !objective_is_naval_carrier(db, obj) {
        return false;
    }
    let Some(pad) = farp_pad_template(obj) else {
        return false;
    };
    Airbase::get_by_name(lua, pad.clone())
        .and_then(|ab| ab.is_exist())
        .unwrap_or(false)
}

fn objective_has_operational_carrier(lua: MizLua, db: &Db, obj: &Objective) -> bool {
    if !objective_is_naval_carrier(db, obj) {
        return false;
    }
    if objective_has_live_carrier_airbase(lua, db, obj) {
        return true;
    }
    let Some(pad) = farp_pad_template(obj) else {
        return false;
    };
    if carrier_ship_unit_id(lua, db, pad).ok().flatten().is_some() {
        return true;
    }
    // TISP-deployed carriers: deck airbase exists once the pad is moved to the ship.
    Airbase::get_by_name(lua, pad.clone())
        .and_then(|ab| ab.is_exist())
        .unwrap_or(false)
}

fn carrier_fallback_deck_slot(lua: MizLua, db: &Db, obj: &Objective) -> Result<Option<HubSlot>> {
    let Some(pad) = farp_pad_template(obj) else {
        return Ok(None);
    };
    let Some(ship_id) = carrier_ship_unit_id(lua, db, pad)? else {
        return Ok(None);
    };
    if let Ok(ab) = Airbase::get_by_name(lua, pad.clone()) {
        if ab.is_exist()? {
            if let Ok(p) = ab.get_point() {
                let pos = Vector2::new(p.x, p.z);
                return Ok(Some(HubSlot {
                    kind: HubSlotKind::Parking,
                    slot_id: 1,
                    pos,
                    heading: 0.,
                    baro_alt: Some(p.y.round()),
                    term_type: Some(TERM_RUNWAY),
                    heading_from_spot: false,
                    link_unit: Some(ship_id),
                }));
            }
        }
    }
    let Ok(u) = Unit::get_by_name(lua, pad) else {
        return Ok(None);
    };
    if !u.is_exist()? {
        return Ok(None);
    }
    let pt = u.get_point()?;
    let pos = Vector2::new(pt.x, pt.z);
    Ok(Some(HubSlot {
        kind: HubSlotKind::Parking,
        slot_id: 1,
        pos,
        heading: 0.,
        baro_alt: Some(pt.y.round()),
        term_type: Some(TERM_RUNWAY),
        heading_from_spot: false,
        link_unit: Some(ship_id),
    }))
}

fn hub_airbase_oid(
    lua: MizLua,
    db: &Db,
    oid: ObjectiveId,
) -> Result<Option<dcso3::object::DcsOid<dcso3::airbase::ClassAirbase>>> {
    if let Some(ab) = db.ephemeral.airbase_by_oid.get(&oid) {
        return Ok(Some(ab.clone()));
    }
    let obj = objective!(db, oid)?;
    if let Some(pad) = farp_pad_template(obj) {
        if objective_is_naval_carrier(db, obj) {
            if let Ok(ab) = Airbase::get_by_name(lua, pad.clone()) {
                if ab.is_exist()? {
                    return Ok(ab.object_id().ok());
                }
            }
        }
    }
    Ok(None)
}

fn hub_supports_ai_air(lua: MizLua, db: &Db, obj: &Objective, kind: AiPlaneKind) -> bool {
    match kind {
        AiPlaneKind::Helicopter => {
            objective_is_heli_spawn_hub(db, obj)
                || objective_has_operational_carrier(lua, db, obj)
        }
        AiPlaneKind::FixedWing => {
            obj.is_airbase()
                || objective_has_airfield_hub(db, obj)
                || objective_has_operational_carrier(lua, db, obj)
                || db
                    .ephemeral
                    .cfg
                    .extra_fixed_wing_objectives
                    .contains(&obj.name)
        }
    }
}

fn hub_candidate_filter<'a>(
    lua: MizLua<'a>,
    db: &'a Db,
    side: Side,
    kind: AiPlaneKind,
    mode: HubSelectMode,
) -> impl Iterator<Item = &'a Objective> + 'a {
    db.persisted.objectives.into_iter().filter_map(move |(_, obj)| {
        if obj.owner != side {
            return None;
        }
        if matches!(obj.kind, ObjectiveKind::Production) {
            return None;
        }
        if mode == HubSelectMode::Spawn && obj.threatened {
            let deployed_farp_hub = matches!(obj.kind, ObjectiveKind::Farp { .. })
                && (objective_has_airfield_hub(db, obj)
                    || objective_has_operational_carrier(lua, db, obj));
            if !deployed_farp_hub {
                return None;
            }
        }
        if mode == HubSelectMode::Spawn && obj.captureable() {
            let naval_spawn_hub = objective_is_naval_carrier(db, obj)
                && objective_has_operational_carrier(lua, db, obj);
            let mobile_farp_heli = matches!(obj.kind, ObjectiveKind::Farp { .. })
                && matches!(kind, AiPlaneKind::Helicopter);
            if !naval_spawn_hub && !mobile_farp_heli {
                return None;
            }
        }
        if hub_supports_ai_air(lua, db, obj, kind) {
            Some(obj)
        } else {
            None
        }
    })
}

pub(super) fn dcs_spawn_names_for(db: &Db, gid: GroupId) -> Result<Vec<String>> {
    let group = group!(db, gid)?;
    match &group.origin {
        DeployKind::Action { ai_air, .. } if !ai_air.dcs_spawn_names.is_empty() => {
            Ok(ai_air.dcs_spawn_names.clone())
        }
        _ => Ok(vec![group.name.clone()]),
    }
}

pub(super) fn finish_hub_pick(
    lua: MizLua,
    db: &Db,
    oid: ObjectiveId,
    slots: Vec<HubSlot>,
    airbase_id: Option<AirbaseId>,
) -> Result<HubPick> {
    let zone_anchor = hub_zone_pos(db, oid)?;
    let anchor = slots.first().map(|s| s.pos).unwrap_or(zone_anchor);
    let baro_alt = hub_airfield_baro_alt(lua, anchor, &slots)?;
    Ok(HubPick {
        oid,
        slots,
        airbase_id,
        anchor,
        baro_alt,
    })
}

fn cfg_me_spawn_template<'lua>(
    spctx: &SpawnCtx<'lua>,
    idx: &MizIndex,
    side: Side,
    cfg_template: &str,
    cfg_unit_name: &str,
) -> Result<GroupInfo<'lua>> {
    let mut template = spctx.get_template(idx, GroupKind::Any, side, cfg_template)?;
    trim_template_to_one_unit(&mut template, cfg_unit_name)?;
    Ok(template)
}

fn trim_template_to_one_unit<'lua>(
    template: &mut GroupInfo<'lua>,
    keep_template_name: &str,
) -> Result<()> {
    let units = template.group.units().context("units")?;
    let mut i = 1i64;
    while (i as usize) <= units.len() {
        let unit = units.get(i)?;
        if unit.name()?.as_str() != keep_template_name {
            units.remove(i)?;
        } else {
            i += 1;
        }
    }
    if units.len() == 0 {
        bail!("cfg template missing unit {keep_template_name}");
    }
    Ok(())
}

/// ME default speed on TakeOffParking waypoints (~500 km/h).
const ME_PARKING_WAYPOINT_SPEED: f64 = 138.888_888_888_89;
/// ME default heli speed on parking / helipad (~150 km/h).
const ME_HELI_PARKING_SPEED: f64 = 41.666_666_666_667;

fn hub_airfield_baro_alt(lua: MizLua, anchor: Vector2, slots: &[HubSlot]) -> Result<f64> {
    if let Some(slot) = slots.iter().find(|s| s.baro_alt.is_some()) {
        return Ok(slot.baro_alt.unwrap().round());
    }
    let ground = Land::singleton(lua)?.get_height(LuaVec2(anchor))?;
    Ok(ground.round())
}

pub(super) fn template_unit_types<'lua>(
    spctx: &SpawnCtx<'lua>,
    idx: &MizIndex,
    side: Side,
    template_name: &str,
) -> Result<Vec<String>> {
    let tpl = spctx.get_template_ref(idx, GroupKind::Any, side, template_name)?;
    let mut out = Vec::new();
    for u in tpl.group.units()? {
        let u = u?;
        out.push(u.typ()?);
    }
    Ok(out)
}

/// Warehouse airframe keys for spawn checks (SETTINGS-Ai export map overrides template unit types).
pub(super) fn spawn_airframe_types<'lua>(
    db: &Db,
    spctx: &SpawnCtx<'lua>,
    idx: &MizIndex,
    side: Side,
    template_name: &str,
    unit_count: usize,
) -> Result<Vec<String>> {
    let export = &db.ephemeral.fowl_miz_export;
    if let Some(airframe) = export.ai_template_airframes.get(template_name) {
        return Ok(vec![String::from(airframe.as_str()); unit_count.max(1)]);
    }
    template_unit_types(spctx, idx, side, template_name)
}

fn warehouse_has_airframes(
    lua: MizLua,
    db: &Db,
    obj: &Objective,
    types: &[String],
    count: usize,
) -> Result<bool> {
    let wh = hub_airbase_oid(lua, db, obj.id)
        .ok()
        .flatten()
        .and_then(|ab_oid| Airbase::get_instance(lua, &ab_oid).ok())
        .and_then(|ab| ab.get_warehouse().ok())
        .or_else(|| {
            farp_pad_template(obj).and_then(|pad| {
                Airbase::get_by_name(lua, pad.clone())
                    .ok()
                    .and_then(|ab| ab.get_warehouse().ok())
            })
        });
    for typ in types.iter().take(count) {
        let virtual_n = obj
            .warehouse
            .equipment
            .get(typ.as_str())
            .map(|i| i.stored)
            .unwrap_or(0);
        if virtual_n >= 1 {
            continue;
        }
        if let Some(wh) = &wh {
            if wh.get_item_count(typ.clone()).unwrap_or(0) >= 1 {
                continue;
            }
        }
        return Ok(false);
    }
    Ok(true)
}

fn resolve_parking_airbase_id(lua: MizLua, db: &Db, hub: &HubPick) -> Result<AirbaseId> {
    if let Some(id) = hub.airbase_id {
        return Ok(id);
    }
    if let Some(id) = hub_airbase_id(db, lua, hub.oid)? {
        return Ok(id);
    }
    let obj = objective!(db, hub.oid)?;
    if let Ok(ab) = Airbase::get_by_name(lua, obj.name.clone()) {
        if ab.is_exist()? {
            return ab.get_id();
        }
    }
    bail!(
        "parking spawn missing airbase id for objective {:?}",
        obj.name
    );
}

fn apply_me_airfield_unit<'lua>(
    lua: MizLua<'lua>,
    unit: &miz::Unit<'lua>,
    slot: &HubSlot,
    hub: &HubPick,
    db: &Db,
) -> Result<()> {
    unit.set_alt(hub.baro_alt)?;
    unit.raw_set("alt_type", AltType::BARO)?;
    unit.raw_set("speed", ME_PARKING_WAYPOINT_SPEED)?;
    unit.set_pos(hub.anchor)?;
    let ab_id = resolve_parking_airbase_id(lua, db, hub)?;
    unit.raw_set("airdromeId", ab_id.inner())?;
    unit.raw_remove("parking")?;
    unit.raw_remove("parking_id")?;
    unit.raw_remove("helipadId")?;
    unit.raw_remove("linkUnit")?;
    unit.raw_remove("manualHeading")?;
    unit.raw_remove("heading")?;
    unit.raw_remove("psi")?;
    Ok(())
}

fn apply_me_helipad_unit<'lua>(
    lua: MizLua<'lua>,
    unit: &miz::Unit<'lua>,
    slot: &HubSlot,
    hub: &HubPick,
    _pad_index: usize,
) -> Result<()> {
    let alt = hub_slot_baro_alt(lua, slot, hub.baro_alt)?;
    unit.set_alt(alt)?;
    unit.raw_set("alt_type", AltType::BARO)?;
    unit.raw_set("speed", ME_HELI_PARKING_SPEED)?;
    unit.set_pos(slot.pos)?;
    if slot.heading_from_spot && slot.heading.abs() > 0.01 {
        unit.set_heading(slot.heading)?;
        unit.raw_set("psi", -slot.heading)?;
        unit.raw_set("manualHeading", true)?;
    } else {
        unit.set_heading(0.)?;
        unit.raw_set("psi", 0f64)?;
        unit.raw_remove("manualHeading")?;
    }
    unit.raw_set("helipadId", slot.slot_id)?;
    unit.raw_set("linkUnit", slot.slot_id)?;
    unit.raw_set("ropeLength", 15i64)?;
    unit.raw_remove("airdromeId")?;
    unit.raw_remove("parking")?;
    unit.raw_remove("parking_id")?;
    Ok(())
}

fn apply_me_carrier_deck_unit<'lua>(
    lua: MizLua<'lua>,
    unit: &miz::Unit<'lua>,
    slot: &HubSlot,
    hub: &HubPick,
) -> Result<()> {
    let Some(ship_id) = slot.link_unit else {
        bail!("carrier deck slot missing link_unit");
    };
    let alt = hub_slot_baro_alt(lua, slot, hub.baro_alt)?;
    unit.set_alt(alt)?;
    unit.raw_set("alt_type", AltType::BARO)?;
    unit.raw_set("speed", ME_PARKING_WAYPOINT_SPEED)?;
    unit.set_pos(slot.pos)?;
    unit.raw_set("parking", "1")?;
    unit.raw_set("parking_id", "1")?;
    unit.raw_remove("airdromeId")?;
    unit.raw_remove("helipadId")?;
    unit.raw_remove("linkUnit")?;
    unit.raw_remove("manualHeading")?;
    unit.raw_remove("heading")?;
    unit.raw_remove("psi")?;
    let _ = ship_id;
    Ok(())
}

pub(super) fn apply_parking_to_template_unit<'lua>(
    lua: MizLua<'lua>,
    db: &Db,
    unit: &miz::Unit<'lua>,
    slot: &HubSlot,
    hub: &HubPick,
    pad_index: usize,
) -> Result<()> {
    if slot.link_unit.is_some() {
        return apply_me_carrier_deck_unit(lua, unit, slot, hub);
    }
    match slot.kind {
        HubSlotKind::Parking => apply_me_airfield_unit(lua, unit, slot, hub, db),
        HubSlotKind::Helipad => apply_me_helipad_unit(lua, unit, slot, hub, pad_index),
    }
}

fn warehouse_has_fuel(lua: MizLua, db: &Db, oid: ObjectiveId, types: &[String]) -> Result<bool> {
    let obj = objective!(db, oid)?;
    let Some(ab_oid) = hub_airbase_oid(lua, db, oid)? else {
        return Ok(true);
    };
    let wh = Airbase::get_instance(lua, &ab_oid)?
        .get_warehouse()
        .context("warehouse")?;
    if types.is_empty() {
        for liq in LiquidType::ALL {
            if wh.get_liquid_amount(liq).unwrap_or(0) > 0 {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    let need_kg = spawn_fuel_kg_total(types);
    let liq = spawn_liquid_type(types.first().map(AsRef::as_ref).unwrap_or(""));
    let avail = wh.get_liquid_amount(liq).unwrap_or(0);
    if avail >= need_kg {
        return Ok(true);
    }
    for (liq, inv) in &obj.warehouse.liquids {
        if inv.stored > 0 {
            let kg = wh.get_liquid_amount(*liq).unwrap_or(0);
            if kg >= need_kg {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

const DEFAULT_JET_INTERNAL_FUEL_KG: u32 = 4500;
const DEFAULT_HELI_INTERNAL_FUEL_KG: u32 = 3000;
const DRONE_PREDATOR_FUEL_KG: u32 = 500;
const DRONE_WINGLOONG_FUEL_KG: u32 = 1000;

fn spawn_liquid_type(airframe: &str) -> LiquidType {
    if airframe.contains("Predator")
        || airframe.contains("RQ-1")
        || airframe.contains("FW-190")
        || airframe.contains("P-51")
        || airframe.contains("Bf 109")
        || airframe.contains("Spitfire")
        || airframe.contains("Yak-52")
    {
        LiquidType::Avgas
    } else {
        LiquidType::JetFuel
    }
}

fn spawn_fuel_kg_per_airframe(typ: &str) -> u32 {
    if typ.contains("Predator") || typ.contains("RQ-1") {
        DRONE_PREDATOR_FUEL_KG
    } else if typ.contains("WingLoong") {
        DRONE_WINGLOONG_FUEL_KG
    } else if typ.contains("Mi-8")
        || typ.contains("Mi-24")
        || typ.contains("Ka-50")
        || typ.contains("UH-1")
        || typ.contains("SA342")
    {
        DEFAULT_HELI_INTERNAL_FUEL_KG
    } else {
        DEFAULT_JET_INTERNAL_FUEL_KG
    }
}

fn spawn_fuel_kg_total(types: &[String]) -> u32 {
    types.iter().map(|t| spawn_fuel_kg_per_airframe(t)).sum()
}

fn read_unit_fuel_kg(unit: &miz::Unit<'_>, cap_kg: u32) -> u32 {
    if let Ok(payload) = unit.raw_get::<_, Table>("payload") {
        if let Ok(f) = payload.raw_get::<_, f64>("fuel") {
            if f > 1. {
                return f.round().clamp(0., f64::from(cap_kg)) as u32;
            }
            if f > 0. {
                return (f * f64::from(cap_kg)).round().clamp(0., f64::from(cap_kg)) as u32;
            }
            return 0;
        }
    }
    match unit.raw_get::<_, f64>("fuel") {
        Ok(f) if f > 1. => f.round().clamp(0., f64::from(cap_kg)) as u32,
        Ok(f) if f > 0. => (f * f64::from(cap_kg)).round().clamp(0., f64::from(cap_kg)) as u32,
        Ok(_) => 0,
        Err(_) => cap_kg,
    }
}

fn apply_me_template_fuel_kg(unit: &miz::Unit<'_>, fuel_kg: u32) -> Result<()> {
    let fuel = f64::from(fuel_kg);
    unit.raw_set("fuel", fuel)?;
    if let Ok(pl) = unit.raw_get::<_, Table>("payload") {
        pl.raw_set("fuel", fuel)?;
    }
    Ok(())
}

pub(super) fn apply_me_template_fuel_fraction(
    unit: &miz::Unit<'_>,
    airframe_type: &str,
    frac: f32,
) -> Result<()> {
    let cap = spawn_fuel_kg_per_airframe(airframe_type);
    let kg = ((frac.clamp(0., 1.) * cap as f32).round() as u32).min(cap);
    apply_me_template_fuel_kg(unit, kg)
}

/// Min AGL to resume ai air in flight after campaign persist reload.
const PERSIST_RESUME_MIN_AGL_M: f64 = 80.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AiAirPersistSpawn {
    NewDeploy,
    PersistGround,
    PersistInAir,
}

fn persisted_unit_airborne(lua: MizLua, su: &SpawnedUnit) -> Result<bool> {
    let ground = Land::singleton(lua)?.get_height(LuaVec2(su.pos))?;
    Ok((su.position.p.y - ground) > PERSIST_RESUME_MIN_AGL_M)
}

pub(super) fn should_resume_airborne(
    lua: MizLua,
    db: &Db,
    gid: GroupId,
    phase: AiAirPhase,
) -> Result<bool> {
    if !matches!(
        phase,
        AiAirPhase::OnMission | AiAirPhase::RtbInbound | AiAirPhase::Departing
    ) {
        return Ok(false);
    }
    let group = group!(db, gid)?;
    let mut any = false;
    for uid in group.units.into_iter() {
        let Some(u) = db.persisted.units.get(uid) else {
            continue;
        };
        if u.dead {
            continue;
        }
        any = true;
        if !persisted_unit_airborne(lua, u)? {
            return Ok(false);
        }
    }
    Ok(any)
}

fn prepare_me_in_air_group<'lua>(
    template: &mut GroupInfo<'lua>,
    dcs_name: &str,
    anchor: Vector2,
) -> Result<()> {
    template.group.set("lateActivation", false)?;
    template.group.set("hidden", false)?;
    template.group.set("uncontrolled", false)?;
    template.group.raw_set("uncontrollable", false)?;
    template.group.raw_set("dynSpawnTemplate", false)?;
    template.group.set_name(String::from(dcs_name))?;
    template.group.raw_remove("task")?;
    template.group.raw_set("taskSelected", false)?;
    template.group.raw_set("x", anchor.x)?;
    template.group.raw_set("y", anchor.y)?;
    Ok(())
}

fn apply_me_in_air_unit(
    unit: &miz::Unit<'_>,
    su: &SpawnedUnit,
    airframe_type: &str,
) -> Result<()> {
    unit.set_pos(su.pos)?;
    unit.set_alt(su.position.p.y)?;
    unit.set_heading(su.heading)?;
    if let Some(frac) = su.fuel_fraction {
        apply_me_template_fuel_fraction(unit, airframe_type, frac)?;
    }
    Ok(())
}

fn deduct_hub_liquid(
    lua: MizLua,
    db: &mut Db,
    hub: ObjectiveId,
    liq: LiquidType,
    kg: u32,
) -> Result<()> {
    let Some(ab_oid) = hub_airbase_oid(lua, db, hub)? else {
        bail!("hub {hub} has no DCS warehouse");
    };
    let wh = Airbase::get_instance(lua, &ab_oid)?
        .get_warehouse()
        .context("hub warehouse")?;
    let avail = wh.get_liquid_amount(liq)?;
    if avail < kg {
        bail!("insufficient {liq:?} at hub ({avail} < {kg} kg)");
    }
    wh.remove_liquid(liq, kg).context("remove_liquid")?;
    if let Ok(obj) = objective_mut!(db, hub) {
        if let Some(inv) = obj.warehouse.liquids.get_mut_cow(&liq) {
            inv.stored = wh.get_liquid_amount(liq)?;
        }
    }
    Ok(())
}

fn refuel_drone_by_respawn<'lua>(
    lua: MizLua<'lua>,
    db: &mut Db,
    spctx: &SpawnCtx<'lua>,
    idx: &MizIndex,
    gid: GroupId,
    hub: &HubPick,
    airframe_types: &[String],
) -> Result<()> {
    let (side, cfg_template, existing_dcs_names) = {
        let group = group!(db, gid)?;
        let existing = match &group.origin {
            DeployKind::Action { ai_air, .. } => ai_air.dcs_spawn_names.clone(),
            _ => vec![],
        };
        (group.side, group.template_name.clone(), existing)
    };
    let unit_pool: Vec<(String, String)> = {
        let group = group!(db, gid)?;
        group
            .units
            .into_iter()
            .filter_map(|uid| {
                db.persisted.units.get(uid).and_then(|u| {
                    if u.dead {
                        None
                    } else {
                        Some((u.name.clone(), u.template_name.clone()))
                    }
                })
            })
            .collect()
    };
    if unit_pool.is_empty() {
        bail!("no alive drone units to refuel for {gid}");
    }
    let mut fuel_per_unit = Vec::with_capacity(unit_pool.len());
    let mut total_kg = 0u32;
    for (i, _) in unit_pool.iter().enumerate() {
        let typ = airframe_types
            .get(i)
            .or(airframe_types.first())
            .map(|s| s.as_ref())
            .unwrap_or("");
        let kg = spawn_fuel_kg_per_airframe(typ);
        fuel_per_unit.push(kg);
        total_kg += kg;
    }
    let liq = spawn_liquid_type(
        airframe_types
            .first()
            .map(|s| s.as_ref())
            .unwrap_or(""),
    );
    deduct_hub_liquid(lua, db, hub.oid, liq, total_kg)?;
    let fowl_name = group!(db, gid)?.name.clone();
    db.ephemeral.ai_air_dcs_oids.remove(&gid);
    if let Some(old) = db.ephemeral.object_id_by_gid.remove(&gid) {
        db.ephemeral.gid_by_object_id.remove(&old);
    }
    let mut dcs_names = Vec::new();
    let mut oids = SmallVec::<[dcso3::object::DcsOid<dcso3::group::ClassGroup>; 4]>::new();
    for slot_i in 0..unit_pool.len() {
        let (su_name, cfg_unit_name) = &unit_pool[slot_i];
        let fuel_kg = fuel_per_unit[slot_i];
        let dcs_name = existing_dcs_names
            .get(slot_i)
            .cloned()
            .unwrap_or_else(|| String::from(format_compact!("{fowl_name}-{}", slot_i + 1)));
        if let Ok(g) = Group::get_by_name(lua, dcs_name.as_str()) {
            if g.is_exist()? {
                g.destroy()?;
            }
        }
        let slot = hub
            .slots
            .get(slot_i)
            .or(hub.slots.first())
            .ok_or_else(|| anyhow!("no hub slot for drone refuel respawn"))?;
        let mut template = cfg_me_spawn_template(
            spctx,
            idx,
            side,
            &cfg_template,
            cfg_unit_name.as_str(),
        )?;
        prepare_me_spawn_group(lua, &mut template, &dcs_name, hub, slot)?;
        let unit = template.group.units()?.get(1)?;
        unit.raw_remove("unitId")?;
        unit.raw_set("skill", "Excellent")?;
        unit.raw_set("onboard_num", random_onboard_num())?;
        apply_me_template_fuel_kg(&unit, fuel_kg)?;
        apply_parking_to_template_unit(lua, db, &unit, slot, hub, slot_i)?;
        unit.set_name(String::from(su_name.as_str()))?;
        let spawned = spctx
            .spawn(template)
            .context("respawn fueled drone")?;
        if let crate::spawnctx::Spawned::Group(g) = &spawned {
            oids.push(g.object_id()?);
        }
        apply_fowl_air_options_to_name(lua, &dcs_name)?;
        dcs_names.push(dcs_name);
    }
    db.ephemeral.ai_air_dcs_oids.insert(gid, oids.clone());
    if let Some(first) = oids.first() {
        db.ephemeral.object_id_by_gid.insert(gid, first.clone());
        db.ephemeral.gid_by_object_id.insert(first.clone(), gid);
    }
    let group = group_mut!(db, gid)?;
    if let DeployKind::Action { ai_air, .. } = &mut group.origin {
        ai_air.dcs_spawn_names = dcs_names.clone();
    }
    if let Err(e) = try_rearm_from_template(
        lua,
        db,
        spctx,
        idx,
        side,
        gid,
        &cfg_template,
        hub.oid,
        None,
        true,
    ) {
        log::warn!("ai air {gid}: rearm after drone refuel respawn failed: {e:?}");
    }
    let fuel = flight_min_fuel(lua, &dcs_names)?;
    if fuel.map(|f| f < 0.05).unwrap_or(true) {
        bail!(
            "respawn refuel left drone empty (fuel {:?}%)",
            fuel.map(|f| (f * 100.) as u32)
        );
    }
    log::info!(
        "ai air {gid}: drone refueled by respawn ({:.0}% fuel, {total_kg} kg {liq:?})",
        fuel.unwrap_or(0.) * 100.
    );
    Ok(())
}

pub(super) fn is_hub_ai_air_action(db: &Db, gid: GroupId) -> bool {
    let Ok(group) = group!(db, gid) else {
        return false;
    };
    match &group.origin {
        DeployKind::Action { ai_air, .. } => ai_air.hub.is_some() && !ai_air.hub_slots.is_empty(),
        _ => false,
    }
}

pub(super) fn ai_air_spawn_on_carrier_deck(origin: &DeployKind) -> bool {
    match origin {
        DeployKind::Action { ai_air, .. } => ai_air
            .hub_slots
            .first()
            .is_some_and(|s| s.link_unit.is_some()),
        _ => false,
    }
}

fn template_unit_fuel_fraction(unit: &miz::Unit<'_>) -> f64 {
    let cap = unit
        .typ()
        .map(|t| spawn_fuel_kg_per_airframe(t.as_ref()))
        .unwrap_or(DEFAULT_JET_INTERNAL_FUEL_KG);
    let kg = read_unit_fuel_kg(unit, cap);
    if cap == 0 {
        return 0.;
    }
    (f64::from(kg) / f64::from(cap)).clamp(0., 1.)
}

pub(super) fn template_spawn_fuel_kg_total<'lua>(
    spctx: &SpawnCtx<'lua>,
    idx: &MizIndex,
    side: Side,
    template_name: &str,
) -> Result<u32> {
    let tpl = spctx.get_template_ref(idx, GroupKind::Any, side, template_name)?;
    let mut total = 0u32;
    for u in tpl.group.units()? {
        let u = u?;
        let typ = u.typ()?;
        let frac = template_unit_fuel_fraction(&u);
        let cap = spawn_fuel_kg_per_airframe(typ.as_ref());
        total += (frac * f64::from(cap)).round() as u32;
    }
    Ok(total.max(1))
}

fn optional_vec2_from_pos_table(tbl: &Table) -> Option<Vector2> {
    let x: f64 = tbl.raw_get("x").ok()?;
    let z: f64 = tbl.raw_get("z").or_else(|_| tbl.raw_get("y")).ok()?;
    Some(Vector2::new(x, z))
}

fn parking_spot_baro_alt(spot: &Table) -> Option<f64> {
    for key in ["vTerminalPos", "vPos", "position"] {
        if let Ok(pos) = spot.raw_get::<_, Table>(key) {
            if let Ok(y) = pos.raw_get::<_, f64>("y") {
                return Some(y);
            }
        }
    }
    None
}

fn parking_spot_pos(spot: &Table) -> Option<Vector2> {
    for key in ["vTerminalPos", "vPos", "position"] {
        if let Ok(pos) = spot.raw_get::<_, Table>(key) {
            if let Some(v) = optional_vec2_from_pos_table(&pos) {
                return Some(v);
            }
        }
    }
    let x: f64 = spot.raw_get("x").ok()?;
    let z: f64 = spot.raw_get("z").or_else(|_| spot.raw_get("y")).ok()?;
    Some(Vector2::new(x, z))
}

/// DCS carrier deck parking often reports term -1 and coords at the map origin / magnetic-variation anchor.
const INVALID_HUB_SLOT_RADIUS_SQ: f64 = 1_000_000.;

fn is_map_origin_slot_pos(pos: Vector2) -> bool {
    const ANCHORS: [(f64, f64); 2] = [(43., 33.), (0., 0.)];
    ANCHORS.iter().any(|(x, y)| {
        na::distance_squared(&pos.into(), &Vector2::new(*x, *y).into()) <= INVALID_HUB_SLOT_RADIUS_SQ
    })
}

fn is_invalid_hub_slot(slot: &HubSlot) -> bool {
    if slot.link_unit.is_some() {
        return is_map_origin_slot_pos(slot.pos);
    }
    if slot.slot_id < 0 {
        return true;
    }
    if is_map_origin_slot_pos(slot.pos) {
        return true;
    }
    if slot.baro_alt == Some(0.) && slot.slot_id <= 0 {
        return true;
    }
    false
}

fn hub_slot_is_land(lua: MizLua, pos: Vector2) -> bool {
    let Ok(land) = Land::singleton(lua) else {
        return true;
    };
    matches!(
        land.get_surface_type(LuaVec2(pos)),
        Ok(SurfaceType::Land | SurfaceType::Road | SurfaceType::Runway)
    )
}

fn filter_valid_hub_slots(mut pool: Vec<HubSlot>) -> Vec<HubSlot> {
    pool.retain(|s| !is_invalid_hub_slot(s));
    pool
}

pub(super) fn refresh_hub_slots(
    lua: MizLua,
    db: &Db,
    hub_oid: ObjectiveId,
    slots: &[HubSlot],
    kind: AiPlaneKind,
) -> Result<Vec<HubSlot>> {
    let obj = objective!(db, hub_oid)?;
    let naval = objective_is_naval_carrier(db, &obj);
    slots
        .iter()
        .map(|slot| refresh_hub_slot(lua, db, &obj, slot, kind, naval))
        .collect()
}

fn refresh_hub_slot(
    lua: MizLua,
    db: &Db,
    obj: &Objective,
    slot: &HubSlot,
    _kind: AiPlaneKind,
    _naval: bool,
) -> Result<HubSlot> {
    let naval = objective_is_naval_carrier(db, obj);
    if naval || slot.link_unit.is_some() {
        if let Some(deck) = carrier_fallback_deck_slot(lua, db, obj)? {
            let mut s = slot.clone();
            s.pos = deck.pos;
            s.baro_alt = deck.baro_alt;
            s.link_unit = deck.link_unit;
            s.term_type = deck.term_type.or(s.term_type);
            if s.slot_id < 0 {
                s.slot_id = deck.slot_id;
            }
            return Ok(s);
        }
    }
    if is_invalid_hub_slot(slot) {
        if let Some(deck) = carrier_fallback_deck_slot(lua, db, obj)? {
            return Ok(deck);
        }
    }
    Ok(slot.clone())
}

const TERM_RUNWAY: i64 = 16;
const TERM_HELIPAD: i64 = 40;
const TERM_HARDENED_SHELTER: i64 = 68;
const TERM_OPEN_SHELTER: i64 = 72;
const TERM_SMALL_SHELTER: i64 = 100;
const TERM_OPEN: i64 = 104;

fn parking_allowed_for_kind(term: Option<i64>, kind: AiPlaneKind, naval_carrier: bool) -> bool {
    let t = term.unwrap_or(TERM_OPEN);
    match kind {
        AiPlaneKind::FixedWing => {
            if naval_carrier {
                matches!(t, TERM_OPEN | TERM_OPEN_SHELTER | TERM_RUNWAY)
            } else {
                matches!(t, TERM_OPEN | TERM_OPEN_SHELTER)
            }
        }
        AiPlaneKind::Helicopter => matches!(t, TERM_HELIPAD | TERM_OPEN | TERM_OPEN_SHELTER),
    }
}

fn parking_sort_key(term: Option<i64>, kind: AiPlaneKind, naval_carrier: bool) -> u8 {
    let t = term.unwrap_or(255);
    match kind {
        AiPlaneKind::FixedWing => {
            if naval_carrier && t == TERM_RUNWAY {
                return 0;
            }
            match t {
                TERM_OPEN => 0,
                TERM_OPEN_SHELTER => 1,
                _ => 255,
            }
        }
        AiPlaneKind::Helicopter => match t {
            TERM_HELIPAD => 0,
            TERM_OPEN => 1,
            TERM_OPEN_SHELTER => 2,
            _ => 255,
        },
    }
}

fn parking_spot_term_type(spot: &Table) -> Option<i64> {
    spot.raw_get("Term_Type").ok()
}

/// ME `heading` from `getParking` (`psi` / `fPsi` are negated into ME heading).
fn parking_spot_heading(spot: &Table) -> Option<f64> {
    if let Ok(psi) = spot.raw_get::<_, f64>("psi") {
        return Some(-psi);
    }
    if let Ok(psi) = spot.raw_get::<_, f64>("fPsi") {
        return Some(-psi);
    }
    if let Ok(h) = spot.raw_get::<_, f64>("heading") {
        return Some(h);
    }
    if let Ok(course) = spot.raw_get::<_, f64>("course") {
        return Some(course);
    }
    None
}

fn distinct_parking_slots(pool: Vec<HubSlot>, needed: usize) -> Vec<HubSlot> {
    let mut out: Vec<HubSlot> = Vec::with_capacity(needed);
    for s in pool {
        if out.iter().any(|o| o.slot_id == s.slot_id) {
            continue;
        }
        if out
            .iter()
            .any(|o| na::distance_squared(&o.pos.into(), &s.pos.into()) < 4.)
        {
            continue;
        }
        out.push(s);
        if out.len() >= needed {
            break;
        }
    }
    out
}

fn push_parking_spot(out: &mut Vec<HubSlot>, spot: Table, fallback_term: i64) {
    let Some(pos) = parking_spot_pos(&spot) else {
        return;
    };
    let term: i64 = spot
        .raw_get("Term_Index")
        .or_else(|_| spot.raw_get("vTerminalIdx"))
        .or_else(|_| spot.raw_get("term"))
        .unwrap_or(fallback_term);
    let (heading, heading_from_spot) = match parking_spot_heading(&spot) {
        Some(h) => (h, true),
        None => (0., false),
    };
    out.push(HubSlot {
        kind: HubSlotKind::Parking,
        slot_id: term,
        pos,
        heading,
        baro_alt: parking_spot_baro_alt(&spot),
        term_type: parking_spot_term_type(&spot),
        heading_from_spot,
        link_unit: None,
    });
}

fn parking_spots(lua: MizLua, ab_oid: &dcso3::object::DcsOid<dcso3::airbase::ClassAirbase>) -> Result<Vec<HubSlot>> {
    let ab = Airbase::get_instance(lua, ab_oid)?;
    let parking = ab.get_parking(true)?;
    let mut out = Vec::new();
    let len = parking.raw_len();
    for i in 1..=len {
        let Ok(spot) = parking.raw_get::<_, Table>(i) else {
            continue;
        };
        push_parking_spot(&mut out, spot, i as i64);
    }
    if out.is_empty() {
        parking.for_each(|_: Value, spot: Table| {
            let i = out.len() as i64 + 1;
            push_parking_spot(&mut out, spot, i);
            Ok(())
        })?;
    }
    Ok(filter_valid_hub_slots(out))
}

fn hub_slot_baro_alt(lua: MizLua, slot: &HubSlot, fallback: f64) -> Result<f64> {
    if let Some(a) = slot.baro_alt {
        return Ok(a.round());
    }
    Ok(Land::singleton(lua)?
        .get_height(LuaVec2(slot.pos))?
        .round()
        .max(fallback))
}

fn is_helipad_facility(typ: &str) -> bool {
    typ.contains("HELIPAD")
        || typ.contains("FARP")
        || typ.contains("Invisible")
        || typ.contains("FARPPAD")
}

/// ME helipad/FARP statics use `StaticObject`, not `Unit`.
fn helipad_facility_id(lua: MizLua, name: &str) -> Option<i64> {
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

fn helipad_heading_from_object(o: &Object<'_>) -> f64 {
    o.get_position()
        .map(|p| p.x.z.atan2(p.x.x))
        .unwrap_or(0.)
}

fn helipad_slot_from_dcs_object(
    _lua: MizLua,
    o: Object<'_>,
    zone: &super::objective::Zone,
    side: Side,
) -> Option<HubSlot> {
    if !o.is_exist().unwrap_or(false) {
        return None;
    }
    let typ = o.get_type_name().ok()?;
    if !is_helipad_facility(typ.as_str()) {
        return None;
    }
    let pt = o.get_point().ok()?;
    let pos = Vector2::new(pt.0.x, pt.0.z);
    if !zone.contains(pos) {
        return None;
    }
    let st = o.as_static().ok()?;
    if st.get_coalition().ok()? != side {
        return None;
    }
    let slot_id = i64::from(st.id().ok()?.inner());
    Some(HubSlot {
        kind: HubSlotKind::Helipad,
        slot_id,
        pos,
        heading: helipad_heading_from_object(&o),
        baro_alt: Some(pt.0.y),
        term_type: None,
        heading_from_spot: true,
        link_unit: None,
    })
}

fn helipad_slots_from_world(
    lua: MizLua,
    obj: &Objective,
    side: Side,
) -> Result<Vec<HubSlot>> {
    let world = World::singleton(lua)?;
    let land = Land::singleton(lua)?;
    let center = obj.zone.pos();
    let radius = obj.zone.radius() + 100.;
    let alt = land.get_height(LuaVec2(center))?;
    let vol = SearchVolume::Sphere {
        point: LuaVec3(Vector3::new(center.x, alt, center.y)),
        radius,
    };
    let found = std::sync::Arc::new(std::sync::Mutex::new(Vec::<HubSlot>::new()));
    let found_cb = found.clone();
    let zone = obj.zone;
    world.search_objects(ObjectCategory::Static, vol, Value::Nil, move |lua, o, _| {
        if let Some(slot) = helipad_slot_from_dcs_object(lua, o, &zone, side) {
            if let Ok(mut slots) = found_cb.lock() {
                if !slots.iter().any(|s| s.slot_id == slot.slot_id) {
                    slots.push(slot);
                }
            }
        }
        Ok(true)
    })?;
    let slots = found
        .lock()
        .map_err(|_| anyhow!("helipad scan lock poisoned"))?;
    Ok(slots.clone())
}

/// When the player mark lies on a TISP naval slot or carrier FARP zone, prefer that deck hub.
fn carrier_mark_priority_dist_sq(db: &Db, obj: &Objective, mark_pos: Vector2) -> Option<f64> {
    if !objective_is_naval_carrier(db, obj) {
        return None;
    }
    let Some(pad) = farp_pad_template(obj) else {
        return None;
    };
    if db
        .ephemeral
        .naval_slot_zones
        .get(pad.as_str())
        .is_some_and(|zone| zone.contains(mark_pos))
    {
        return Some(0.);
    }
    if obj.zone.contains(mark_pos) {
        return Some(0.);
    }
    None
}

fn helipad_slot_from_unit(lua: MizLua, unit: &SpawnedUnit, obj: &Objective) -> Option<HubSlot> {
    if unit.dead || !obj.zone.contains(unit.pos) {
        return None;
    }
    if !is_helipad_facility(unit.typ.0.as_str()) {
        return None;
    }
    let slot_id = helipad_facility_id(lua, unit.name.as_str())?;
    let baro_alt = Land::singleton(lua)
        .ok()
        .and_then(|land| land.get_height(LuaVec2(unit.pos)).ok());
    Some(HubSlot {
        kind: HubSlotKind::Helipad,
        slot_id,
        pos: unit.pos,
        heading: unit.heading,
        baro_alt,
        term_type: None,
        heading_from_spot: true,
        link_unit: None,
    })
}

fn helipad_slots_in_zone(
    lua: MizLua,
    db: &Db,
    obj: &Objective,
    side: Side,
) -> Result<Vec<HubSlot>> {
    let mut out = Vec::new();
    let mut seen = FxHashSet::default();
    if let Some(groups) = obj.groups.get(&side) {
        for gid in groups {
            let group = group!(db, gid)?;
            for uid in &group.units {
                let Some(unit) = db.persisted.units.get(uid) else {
                    continue;
                };
                if let Some(slot) = helipad_slot_from_unit(lua, unit, obj) {
                    if seen.insert(slot.slot_id) {
                        out.push(slot);
                    }
                }
            }
        }
    }
    for (_, unit) in db.persisted.units.into_iter() {
        if unit.side != side {
            continue;
        }
        if let Some(slot) = helipad_slot_from_unit(lua, unit, obj) {
            if seen.insert(slot.slot_id) {
                out.push(slot);
            }
        }
    }
    for slot in helipad_slots_from_world(lua, obj, side)? {
        if seen.insert(slot.slot_id) {
            out.push(slot);
        }
    }
    Ok(out)
}

fn free_slots_at_hub(
    lua: MizLua,
    db: &Db,
    obj: &Objective,
    side: Side,
    kind: AiPlaneKind,
    needed: usize,
    claimed: &FxHashSet<(ObjectiveId, HubSlotKind, i64)>,
) -> Result<Vec<HubSlot>> {
    let naval = objective_is_naval_carrier(db, obj);
    let mut pool = match kind {
        AiPlaneKind::Helicopter => {
            let mut helis = helipad_slots_for_heli_hub(lua, db, obj, side)?;
            if helis.is_empty() && matches!(obj.kind, ObjectiveKind::Farp { .. }) {
                helis.extend(helipads_near_point(
                    lua,
                    db,
                    side,
                    obj.zone.pos(),
                    FO_HELIPAD_SEARCH_RADIUS_SQ,
                ));
            }
            if helis.is_empty() && objective_has_operational_carrier(lua, db, obj) {
                if let Some(deck) = carrier_fallback_deck_slot(lua, db, obj)? {
                    helis.push(deck);
                }
            }
            if !helis.is_empty() {
                helis
            } else if objective_has_airfield_hub(db, obj)
                && matches!(obj.kind, ObjectiveKind::Airbase | ObjectiveKind::Logistics)
            {
                let Some(ab) = hub_airbase_oid(lua, db, obj.id)? else {
                    return Ok(vec![]);
                };
                parking_spots(lua, &ab)?
            } else {
                vec![]
            }
        }
        AiPlaneKind::FixedWing
            if objective_has_airfield_hub(db, obj)
                || objective_has_operational_carrier(lua, db, obj) =>
        {
            let mut pool = Vec::new();
            if naval {
                if let Some(slot) = carrier_fallback_deck_slot(lua, db, obj)? {
                    pool.push(slot);
                }
            }
            if let Some(ab) = hub_airbase_oid(lua, db, obj.id)? {
                pool.extend(parking_spots(lua, &ab)?);
            }
            pool = filter_valid_hub_slots(pool);
            if pool.is_empty() {
                if let Some(slot) = carrier_fallback_deck_slot(lua, db, obj)? {
                    pool.push(slot);
                }
            }
            pool
        }
        _ => vec![],
    };
    if naval {
        let ship_link = farp_pad_template(obj)
            .and_then(|pad| carrier_ship_unit_id(lua, db, pad).ok().flatten());
        for slot in &mut pool {
            slot.link_unit = ship_link;
        }
    }
    pool.retain(|s| !claimed.contains(&(obj.id, s.kind, s.slot_id)));
    pool.retain(|s| match s.kind {
        HubSlotKind::Helipad => true,
        HubSlotKind::Parking => parking_allowed_for_kind(s.term_type, kind, naval),
    });
    pool.retain(|s| {
        s.link_unit.is_some()
            || s.kind == HubSlotKind::Helipad
            || hub_slot_is_land(lua, s.pos)
    });
    pool.sort_by(|a, b| {
        a.slot_id
            .cmp(&b.slot_id)
            .then_with(|| parking_sort_key(a.term_type, kind, naval).cmp(&parking_sort_key(b.term_type, kind, naval)))
    });
    let picked = distinct_parking_slots(pool, needed);
    if picked.len() >= needed {
        Ok(picked)
    } else {
        Ok(vec![])
    }
}

pub(super) fn select_hub_for_ai(
    lua: MizLua,
    db: &Db,
    spctx: &SpawnCtx,
    idx: &MizIndex,
    side: Side,
    mark_pos: Vector2,
    plane: &AiPlaneCfg,
    unit_count: usize,
    mode: HubSelectMode,
) -> Result<HubPick> {
    let types = spawn_airframe_types(db, spctx, idx, side, &plane.template, unit_count)?;
    if types.is_empty() {
        bail!("template {} has no units", plane.template);
    }
    let claimed = claimed_hub_slots(db);
    let mut failures: SmallVec<[CompactString; 8]> = smallvec![];
    let mut candidates: Vec<(f64, &Objective)> = hub_candidate_filter(lua, db, side, plane.kind, mode)
        .filter_map(|obj| {
            let raw_dist = hub_mark_dist_sq(lua, db, obj, side, plane.kind, mark_pos).ok()?;
            if mode == HubSelectMode::Spawn
                && carrier_mark_priority_dist_sq(db, obj, mark_pos).is_none()
                && raw_dist <= min_mark_hub_dist_sq(obj, plane.kind)
            {
                return None;
            }
            let dist_sq = carrier_mark_priority_dist_sq(db, obj, mark_pos).unwrap_or(raw_dist);
            Some((dist_sq, obj))
        })
        .collect();
    candidates.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    for (dist_sq, obj) in candidates {
        let mut reason = Vec::new();
        if mode == HubSelectMode::Spawn {
            if !warehouse_has_airframes(lua, db, obj, &types, unit_count)? {
                reason.push("insufficient airframes");
            }
            if !warehouse_has_fuel(lua, db, obj.id, &types)? {
                reason.push("no fuel");
            }
        }
        let slots = free_slots_at_hub(lua, db, obj, side, plane.kind, unit_count, &claimed)?;
        if slots.len() < unit_count {
            reason.push("no free parking/helipad slots");
        }
        if reason.is_empty() {
            let airbase_id = hub_airbase_oid(lua, db, obj.id)?
                .and_then(|oid| Airbase::get_instance(lua, &oid).ok())
                .and_then(|ab| ab.get_id().ok());
            if !failures.is_empty() {
                log::info!(
                    "ai air hub {:?} for {} (skipped: {})",
                    obj.name,
                    plane.template,
                    failures.join("; ")
                );
            }
            if mode == HubSelectMode::Landing {
                log::info!(
                    "ai air landing hub {:?} for {} at {:.1} km",
                    obj.name,
                    plane.template,
                    dist_sq.sqrt() / 1000.
                );
            }
            return finish_hub_pick(lua, db, obj.id, slots, airbase_id);
        }
        failures.push(format_compact!(
            "{} — {}",
            obj.name,
            reason.join(", ")
        ));
    }
    bail!(
        "no friendly base can {} {}: {}",
        match mode {
            HubSelectMode::Spawn => "spawn",
            HubSelectMode::Landing => "land",
        },
        plane.template,
        failures.join("; ")
    );
}

fn lua_empty_combo_task(lua: MizLua) -> Result<Table> {
    let task = lua.inner().create_table()?;
    task.raw_set("id", "ComboTask")?;
    let params = lua.inner().create_table()?;
    params.raw_set("tasks", lua.inner().create_table()?)?;
    task.raw_set("params", params)?;
    Ok(task)
}

/// ME TakeOffParking route: hub anchor + airdromeId + parking term.
fn patch_me_airfield_route_lua<'lua>(
    lua: MizLua<'lua>,
    group: &miz::Group<'lua>,
    hub: &HubPick,
    slot: &HubSlot,
) -> Result<()> {
    let route: Table = match group.raw_get("route") {
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
    p1.raw_set("x", hub.anchor.x)?;
    p1.raw_set("y", hub.anchor.y)?;
    p1.raw_set("alt", hub.baro_alt)?;
    p1.raw_set("alt_type", "BARO")?;
    p1.raw_set("action", "From Parking Area")?;
    p1.raw_set("speed", ME_PARKING_WAYPOINT_SPEED)?;
    p1.raw_set("speed_locked", true)?;
    p1.raw_set("ETA", 0f64)?;
    p1.raw_set("ETA_locked", true)?;
    p1.raw_set("formation_template", "")?;
    p1.raw_set("task", lua_empty_combo_task(lua)?)?;
    let props = lua.inner().create_table()?;
    props.raw_set("addopt", lua.inner().create_table()?)?;
    p1.raw_set("properties", props)?;
    if let Some(ab) = hub.airbase_id {
        p1.raw_set("airdromeId", ab.inner())?;
    } else {
        p1.raw_set("airdromeId", Value::Nil)?;
    }
    p1.raw_set("helipadId", Value::Nil)?;
    p1.raw_set("linkUnit", Value::Nil)?;
    if let Some(parking) = hub_slot_parking_label(slot) {
        p1.raw_set("parking", parking.as_str())?;
    }
    p1.raw_set("timeReFuAr", ME_PARKING_REFUEL_REARM)?;
    points.raw_set(1, p1)?;
    route.raw_set("points", points)?;
    group.raw_set("route", route)?;
    Ok(())
}

fn patch_me_helipad_route_lua<'lua>(
    lua: MizLua<'lua>,
    group: &miz::Group<'lua>,
    hub: &HubPick,
    slot: &HubSlot,
) -> Result<()> {
    let route: Table = match group.raw_get("route") {
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
    p1.raw_set("x", slot.pos.x)?;
    p1.raw_set("y", slot.pos.y)?;
    p1.raw_set("alt", hub_slot_baro_alt(lua, slot, hub.baro_alt)?)?;
    p1.raw_set("alt_type", "BARO")?;
    p1.raw_set("action", "From Parking Area")?;
    p1.raw_set("speed", ME_HELI_PARKING_SPEED)?;
    p1.raw_set("speed_locked", true)?;
    p1.raw_set("ETA", 0f64)?;
    p1.raw_set("ETA_locked", true)?;
    p1.raw_set("formation_template", "")?;
    p1.raw_set("task", lua_empty_combo_task(lua)?)?;
    let props = lua.inner().create_table()?;
    props.raw_set("addopt", lua.inner().create_table()?)?;
    p1.raw_set("properties", props)?;
    p1.raw_set("airdromeId", Value::Nil)?;
    p1.raw_set("helipadId", slot.slot_id)?;
    p1.raw_set("linkUnit", slot.slot_id)?;
    p1.raw_set("timeReFuAr", ME_PARKING_REFUEL_REARM)?;
    points.raw_set(1, p1)?;
    route.raw_set("points", points)?;
    group.raw_set("route", route)?;
    Ok(())
}

/// ME carrier deck cold start (static `TTSN*` / `linkUnit` + `helipadId` pattern).
fn patch_me_carrier_deck_route_lua<'lua>(
    lua: MizLua<'lua>,
    group: &miz::Group<'lua>,
    hub: &HubPick,
    slot: &HubSlot,
) -> Result<()> {
    let Some(ship_id) = slot.link_unit else {
        bail!("carrier deck route missing link_unit");
    };
    let route: Table = match group.raw_get("route") {
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
    p1.raw_set("x", slot.pos.x)?;
    p1.raw_set("y", slot.pos.y)?;
    p1.raw_set("alt", hub_slot_baro_alt(lua, slot, hub.baro_alt)?)?;
    p1.raw_set("alt_type", "BARO")?;
    p1.raw_set("action", "From Parking Area")?;
    p1.raw_set("speed", ME_PARKING_WAYPOINT_SPEED)?;
    p1.raw_set("speed_locked", true)?;
    p1.raw_set("ETA", 0f64)?;
    p1.raw_set("ETA_locked", true)?;
    p1.raw_set("formation_template", "")?;
    p1.raw_set("task", lua_empty_combo_task(lua)?)?;
    let props = lua.inner().create_table()?;
    props.raw_set("addopt", lua.inner().create_table()?)?;
    p1.raw_set("properties", props)?;
    p1.raw_set("airdromeId", Value::Nil)?;
    p1.raw_set("helipadId", ship_id)?;
    p1.raw_set("linkUnit", ship_id)?;
    p1.raw_set("parking", "1")?;
    p1.raw_set("timeReFuAr", ME_PARKING_REFUEL_REARM)?;
    points.raw_set(1, p1)?;
    route.raw_set("points", points)?;
    group.raw_set("route", route)?;
    Ok(())
}

fn random_onboard_num() -> String {
    String::from(format_compact!("{:03}", thread_rng().gen_range(11..=999)))
}

fn prepare_me_spawn_group<'lua>(
    lua: MizLua<'lua>,
    template: &mut GroupInfo<'lua>,
    dcs_name: &str,
    hub: &HubPick,
    slot: &HubSlot,
) -> Result<()> {
    template.group.set("lateActivation", false)?;
    template.group.set("hidden", false)?;
    template.group.set("uncontrolled", false)?;
    template.group.raw_set("uncontrollable", false)?;
    template.group.raw_set("dynSpawnTemplate", false)?;
    template.group.set_name(String::from(dcs_name))?;
    // ME drone templates use Reconnaissance; parking cold-start needs a flyable task.
    template.group.raw_set("task", "CAS")?;
    template.group.raw_set("taskSelected", true)?;
    if slot.link_unit.is_some() {
        patch_me_carrier_deck_route_lua(lua, &template.group, hub, slot)?;
    } else {
        match slot.kind {
            HubSlotKind::Parking => patch_me_airfield_route_lua(lua, &template.group, hub, slot)?,
            HubSlotKind::Helipad => patch_me_helipad_route_lua(lua, &template.group, hub, slot)?,
        }
    }
    let group_pos = if slot.link_unit.is_some() {
        slot.pos
    } else {
        match slot.kind {
            HubSlotKind::Helipad => slot.pos,
            HubSlotKind::Parking => hub.anchor,
        }
    };
    template.group.raw_set("x", group_pos.x)?;
    template.group.raw_set("y", group_pos.y)?;
    Ok(())
}

/// DCS `timeReFuAr`: refuel and rearm on parking before takeoff (minutes).
const ME_PARKING_REFUEL_REARM: i64 = 3;

fn hub_slot_parking_label(slot: &HubSlot) -> Option<String> {
    match slot.kind {
        HubSlotKind::Parking if slot.link_unit.is_some() => Some(String::from("1")),
        HubSlotKind::Parking if slot.slot_id >= 0 => {
            Some(String::from(format_compact!("{}", slot.slot_id)))
        }
        _ => None,
    }
}

fn servicing_complete_wait() -> Duration {
    Duration::seconds(ME_PARKING_REFUEL_REARM * 60)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BootstrapMode {
    /// Cold spawn / first launch from hub parking.
    ColdSpawn,
    /// After `LandingReFuAr` servicing: `TakeOffParking` from hub (DCS assigns slot).
    PostService,
}

pub(super) fn bootstrap_route<'lua>(
    lua: MizLua<'lua>,
    db: &Db,
    hub: &HubPick,
    slot: &HubSlot,
    mode: BootstrapMode,
) -> Result<Vec<MissionPoint<'lua>>> {
    let time_re_fu_ar = match mode {
        BootstrapMode::PostService => None,
        BootstrapMode::ColdSpawn if fuel_available_at_hub(lua, db, hub.oid).unwrap_or(false) => {
            Some(ME_PARKING_REFUEL_REARM)
        }
        BootstrapMode::ColdSpawn => None,
    };
    let (point_typ, action, wpt_name) = (
        PointType::TakeOffParking,
        ActionTyp::Air(TurnMethod::FromParkingArea),
        match mode {
            BootstrapMode::ColdSpawn => "bootstrap",
            BootstrapMode::PostService => "relaunch",
        },
    );
    if let Some(ship_id) = slot.link_unit {
        return Ok(vec![MissionPoint {
            typ: point_typ,
            airdrome_id: None,
            helipad: Some(AirbaseId::from(ship_id)),
            time_re_fu_ar,
            link_unit: Some(UnitId::from(ship_id)),
            action: Some(action),
            pos: LuaVec2(slot.pos),
            alt: hub_slot_baro_alt(lua, slot, hub.baro_alt)?,
            alt_typ: Some(AltType::BARO),
            speed: ME_PARKING_WAYPOINT_SPEED,
            speed_locked: Some(true),
            eta: None,
            eta_locked: Some(true),
            name: Some(String::from(wpt_name)),
            parking: hub_slot_parking_label(slot),
            task: Box::new(Task::ComboTask(vec![])),
        }]);
    }
    let (airdrome_id, helipad, link_unit, speed) = match slot.kind {
        HubSlotKind::Parking => (
            hub.airbase_id,
            None,
            None,
            ME_PARKING_WAYPOINT_SPEED,
        ),
        HubSlotKind::Helipad => (
            None,
            Some(AirbaseId::from(slot.slot_id)),
            Some(UnitId::from(slot.slot_id)),
            ME_HELI_PARKING_SPEED,
        ),
    };
    let land_alt = match slot.kind {
        HubSlotKind::Helipad => hub_slot_baro_alt(lua, slot, hub.baro_alt)?,
        HubSlotKind::Parking => hub.baro_alt,
    };
    Ok(vec![MissionPoint {
        typ: point_typ,
        airdrome_id,
        helipad,
        time_re_fu_ar,
        link_unit,
        action: Some(action),
        pos: LuaVec2(slot.pos),
        alt: land_alt,
        alt_typ: Some(AltType::BARO),
        speed,
        speed_locked: Some(true),
        eta: None,
        eta_locked: Some(true),
        name: Some(String::from(wpt_name)),
        parking: hub_slot_parking_label(slot),
        task: Box::new(Task::ComboTask(vec![])),
    }])
}

fn refuel_parking_route<'lua>(
    lua: MizLua<'lua>,
    db: &Db,
    hub: &HubPick,
    slot: &HubSlot,
) -> Result<Vec<MissionPoint<'lua>>> {
    let time_re_fu_ar = if fuel_available_at_hub(lua, db, hub.oid).unwrap_or(false) {
        Some(ME_PARKING_REFUEL_REARM)
    } else {
        None
    };
    if let Some(ship_id) = slot.link_unit {
        return Ok(vec![MissionPoint {
            typ: PointType::TakeOffParking,
            airdrome_id: None,
            helipad: Some(AirbaseId::from(ship_id)),
            time_re_fu_ar,
            link_unit: Some(UnitId::from(ship_id)),
            action: Some(ActionTyp::Air(TurnMethod::FromParkingArea)),
            pos: LuaVec2(slot.pos),
            alt: hub_slot_baro_alt(lua, slot, hub.baro_alt)?,
            alt_typ: Some(AltType::BARO),
            speed: ME_PARKING_WAYPOINT_SPEED,
            speed_locked: Some(true),
            eta: None,
            eta_locked: Some(true),
            name: Some(String::from("refuel")),
            parking: hub_slot_parking_label(slot),
            task: Box::new(Task::ComboTask(vec![])),
        }]);
    }
    let (airdrome_id, helipad, link_unit, speed) = match slot.kind {
        HubSlotKind::Parking => (
            hub.airbase_id,
            None,
            None,
            ME_PARKING_WAYPOINT_SPEED,
        ),
        HubSlotKind::Helipad => (
            None,
            Some(AirbaseId::from(slot.slot_id)),
            Some(UnitId::from(slot.slot_id)),
            ME_HELI_PARKING_SPEED,
        ),
    };
    let land_alt = match slot.kind {
        HubSlotKind::Helipad => hub_slot_baro_alt(lua, slot, hub.baro_alt)?,
        HubSlotKind::Parking => hub.baro_alt,
    };
    Ok(vec![MissionPoint {
        typ: PointType::TakeOffParking,
        airdrome_id,
        helipad,
        time_re_fu_ar,
        link_unit,
        action: Some(ActionTyp::Air(TurnMethod::FromParkingArea)),
        pos: LuaVec2(slot.pos),
        alt: land_alt,
        alt_typ: Some(AltType::BARO),
        speed,
        speed_locked: Some(true),
        eta: None,
        eta_locked: Some(true),
        name: Some(String::from("refuel")),
        parking: hub_slot_parking_label(slot),
        task: Box::new(Task::ComboTask(vec![])),
    }])
}

fn hold_drone_on_parking(lua: MizLua, dcs_name: &str) -> Result<()> {
    let Ok(group) = Group::get_by_name(lua, dcs_name) else {
        return Ok(());
    };
    if !group.is_exist()? {
        return Ok(());
    }
    let Ok(con) = group.get_controller() else {
        return Ok(());
    };
    apply_fowl_air_controller_options(&con)?;
    let _ = con.set_task(Task::Hold);
    Ok(())
}

fn duration_shutdown_park(lua: MizLua, dcs_name: &str) -> Result<()> {
    hold_drone_on_parking(lua, dcs_name)?;
    if let Ok(group) = Group::get_by_name(lua, dcs_name) {
        if let Ok(con) = group.get_controller() {
            let _ = con.set_on_off(false);
        }
    }
    Ok(())
}

fn parking_await_route<'lua>(
    lua: MizLua<'lua>,
    db: &Db,
    hub: &HubPick,
    slot: &HubSlot,
) -> Result<Vec<MissionPoint<'lua>>> {
    let mut route = bootstrap_route(lua, db, hub, slot, BootstrapMode::PostService)?;
    if let Some(wpt) = route.first_mut() {
        wpt.task = Box::new(Task::Hold);
    }
    Ok(route)
}

fn push_parking_await(
    lua: MizLua,
    db: &mut Db,
    spctx: &SpawnCtx,
    hub: &HubPick,
    slot: &HubSlot,
    dcs_name: &str,
) -> Result<()> {
    let Ok(group) = Group::get_by_name(lua, dcs_name) else {
        return Ok(());
    };
    if !group.is_exist()? {
        return Ok(());
    }
    let route = parking_await_route(lua, db, hub, slot)?;
    db.ai_air_push_mission_to_name(spctx, dcs_name, route, false)
}

fn push_drone_refuel_missions(
    lua: MizLua,
    db: &mut Db,
    spctx: &SpawnCtx,
    gid: GroupId,
    hub: &HubPick,
    dcs_names: &[String],
) -> Result<()> {
    for (i, dcs_name) in dcs_names.iter().enumerate() {
        let slot = hub
            .slots
            .get(i)
            .or(hub.slots.first())
            .ok_or_else(|| anyhow!("no hub slot for drone refuel"))?;
        let route = refuel_parking_route(lua, db, hub, slot)?;
        db.ai_air_push_mission_to_name(spctx, dcs_name, route, false)?;
    }
    let group = group_mut!(db, gid)?;
    if let DeployKind::Action { ai_air, .. } = &mut group.origin {
        ai_air.refuel_mission_pushed = true;
    }
    log::info!("ai air {gid}: drone refuel mission pushed (timeReFuAr on parking)");
    Ok(())
}

fn push_bootstrap_missions(
    lua: MizLua,
    db: &mut Db,
    spctx: &SpawnCtx,
    gid: GroupId,
    hub: &HubPick,
    dcs_names: &[String],
) -> Result<()> {
    for (i, dcs_name) in dcs_names.iter().enumerate() {
        let slot = hub
            .slots
            .get(i)
            .or(hub.slots.first())
            .ok_or_else(|| anyhow!("no hub slot for bootstrap"))?;
        let route = bootstrap_route(lua, db, hub, slot, BootstrapMode::ColdSpawn)?;
        db.ai_air_push_mission_to_name(spctx, dcs_name, route, false)?;
    }
    let group = group_mut!(db, gid)?;
    if let DeployKind::Action { ai_air, .. } = &mut group.origin {
        ai_air.bootstrap_mission_pushed = true;
        ai_air.bootstrap_grounded = true;
    }
    log::info!("ai air {gid}: bootstrap takeoff pushed at spawn");
    Ok(())
}

fn template_weapon_ammo_keys(template_unit: &miz::Unit<'_>) -> Vec<std::string::String> {
    let Ok(payload) = template_unit.raw_get::<_, Table>("payload") else {
        return vec![];
    };
    let Ok(pylons) = payload.raw_get::<_, Table>("pylons") else {
        return vec![];
    };
    let mut keys = Vec::new();
    let mut push = |s: std::string::String| {
        if !s.is_empty() && !keys.iter().any(|k| k == &s) {
            keys.push(s);
        }
    };
    for pair in pylons.pairs::<Value, Table>() {
        let Ok((_, pylon)) = pair else {
            continue;
        };
        let Ok(clsid) = pylon
            .raw_get::<_, String>("CLSID")
            .or_else(|_| pylon.raw_get("clsid"))
        else {
            continue;
        };
        let uc = clsid.as_str().to_ascii_uppercase();
        if uc.contains("FUEL") || uc.contains("EXT") || uc.contains("DROP") {
            continue;
        }
        push(uc);
        push(clsid
            .as_str()
            .trim_matches(|c| c == '{' || c == '}')
            .to_ascii_uppercase());
        if let Ok(desc) = pylon.raw_get::<_, Table>("descriptor") {
            if let Ok(tn) = desc.raw_get::<_, String>("typeName") {
                push(tn.as_str().to_ascii_uppercase());
            }
            if let Ok(dn) = desc.raw_get::<_, String>("displayName") {
                push(dn.as_str().to_ascii_uppercase());
            }
        }
    }
    keys
}

fn ammo_matches_template_key(ammo: &dcso3::unit::Ammo<'_>, keys: &[std::string::String]) -> bool {
    let Ok(count) = ammo.count() else {
        return false;
    };
    if count == 0 {
        return false;
    }
    let typ = ammo.type_name().unwrap_or_default().to_ascii_uppercase();
    let display = ammo.display_name().unwrap_or_default().to_ascii_uppercase();
    keys.iter().any(|k| {
        typ == *k || display == *k || typ.contains(k.as_str()) || display.contains(k.as_str())
    })
}

fn flight_template_weapon_count(
    lua: MizLua,
    names: &[String],
    template_unit: &miz::Unit<'_>,
) -> Result<u32> {
    let keys = template_weapon_ammo_keys(template_unit);
    if keys.is_empty() {
        return Ok(0);
    }
    let mut total = 0u32;
    for name in names {
        let Ok(group) = Group::get_by_name(lua, name) else {
            continue;
        };
        for u in group.get_units()? {
            let u = u?;
            if !u.is_exist()? {
                continue;
            }
            for ammo in u.get_ammo()? {
                let ammo = ammo?;
                if ammo_matches_template_key(&ammo, &keys) {
                    total = total.saturating_add(ammo.count()?);
                }
            }
        }
    }
    Ok(total)
}

fn template_ag_weapon_slots(template_unit: &miz::Unit<'_>) -> u32 {
    let Ok(payload) = template_unit.raw_get::<_, Table>("payload") else {
        return 0;
    };
    let Ok(pylons) = payload.raw_get::<_, Table>("pylons") else {
        return 0;
    };
    let mut n = 0u32;
    for pair in pylons.pairs::<Value, Table>() {
        let Ok((_, pylon)) = pair else {
            continue;
        };
        let Ok(clsid) = pylon
            .raw_get::<_, String>("CLSID")
            .or_else(|_| pylon.raw_get("clsid"))
        else {
            continue;
        };
        let uc = clsid.to_ascii_uppercase();
        if uc.contains("FUEL") || uc.contains("EXT") || uc.contains("DROP") {
            continue;
        }
        let count: u32 = pylon.raw_get("count").unwrap_or(1);
        n = n.saturating_add(count);
    }
    n
}

fn ammo_weapon_flags(ammo: &dcso3::unit::Ammo<'_>) -> Option<u64> {
    ammo.try_weapon_flags()
}

fn is_alcm_store_ammo(ammo: &dcso3::unit::Ammo<'_>) -> bool {
    let typ = ammo.type_name().unwrap_or_default().to_ascii_uppercase();
    let display = ammo.display_name().unwrap_or_default().to_ascii_uppercase();
    if typ.contains("FLARE")
        || typ.contains("CHAFF")
        || typ.contains("DISPENSER")
        || display.contains("FLARE")
        || display.contains("CHAFF")
    {
        return false;
    }
    if let Some(flags) = ammo_weapon_flags(ammo) {
        if flags & WeaponFlag::BuiltInCannon as u64 != 0
            || flags & WeaponFlag::Cannons as u64 != 0
        {
            return false;
        }
    }
    typ.contains("KH")
        || typ.contains("CM-")
        || typ.contains("AGM")
        || typ.contains("ALCM")
        || display.contains("KH")
        || display.contains("ALCM")
}

pub(crate) fn unit_alcm_missile_count(_lua: MizLua, unit: &Unit) -> Result<u32> {
    let mut total = 0u32;
    if !unit.is_exist()? {
        return Ok(0);
    }
    for ammo in unit.get_ammo()? {
        let ammo = ammo?;
        if is_alcm_store_ammo(&ammo) {
            total = total.saturating_add(ammo.count()?);
        }
    }
    Ok(total)
}

pub(crate) fn flight_alcm_missile_count(lua: MizLua, db: &Db, gid: GroupId) -> Result<u32> {
    let dcs_names = dcs_spawn_names_for(db, gid)?;
    let mut total = 0u32;
    for name in &dcs_names {
        let Ok(group) = Group::get_by_name(lua, name) else {
            continue;
        };
        for u in group.get_units()? {
            let u = u?;
            total = total.saturating_add(unit_alcm_missile_count(lua, &u)?);
        }
    }
    Ok(total)
}

pub(super) fn snapshot_from_loiter(pos: Vector2, alt: f64, alt_typ: AltType, speed: f64, racetrack: bool) -> ActiveMissionSnapshot {
    ActiveMissionSnapshot {
        pos,
        alt,
        alt_typ,
        speed,
        racetrack,
        destination: None,
    }
}

pub(super) fn mission_from_snapshot<'lua>(snap: &ActiveMissionSnapshot, task: Task<'lua>) -> Vec<MissionPoint<'lua>> {
    vec![MissionPoint {
        typ: PointType::TurningPoint,
        airdrome_id: None,
        helipad: None,
        time_re_fu_ar: None,
        link_unit: None,
        action: Some(ActionTyp::Air(TurnMethod::FlyOverPoint)),
        pos: LuaVec2(snap.pos),
        alt: snap.alt,
        alt_typ: Some(snap.alt_typ.clone()),
        speed: snap.speed,
        speed_locked: None,
        eta: None,
        eta_locked: None,
        name: Some(String::from("mission")),
        parking: None,
        task: Box::new(task),
    }]
}

/// CAP-style orbit at `snap.pos` after an ingress point at `spawn_pos`.
pub(super) fn cap_orbit_mission<'lua>(
    snap: &ActiveMissionSnapshot,
    spawn_pos: Vector2,
    extra_tasks: Vec<Task<'lua>>,
) -> Vec<MissionPoint<'lua>> {
    let mut orbit_tasks = vec![Task::Orbit {
        pattern: OrbitPattern::Circle,
        point: Some(LuaVec2(snap.pos)),
        point2: None,
        speed: Some(snap.speed),
        altitude: Some(snap.alt),
    }];
    orbit_tasks.extend(extra_tasks);
    vec![
        MissionPoint {
            typ: PointType::TurningPoint,
            airdrome_id: None,
            helipad: None,
            time_re_fu_ar: None,
            link_unit: None,
            action: Some(ActionTyp::Air(TurnMethod::FlyOverPoint)),
            pos: LuaVec2(spawn_pos),
            alt: snap.alt,
            alt_typ: Some(snap.alt_typ.clone()),
            speed: snap.speed,
            speed_locked: None,
            eta: None,
            eta_locked: None,
            name: Some(String::from("ingress")),
            parking: None,
            task: Box::new(Task::ComboTask(vec![])),
        },
        MissionPoint {
            typ: PointType::TurningPoint,
            airdrome_id: None,
            helipad: None,
            time_re_fu_ar: None,
            link_unit: None,
            action: Some(ActionTyp::Air(TurnMethod::FlyOverPoint)),
            pos: LuaVec2(snap.pos),
            alt: snap.alt,
            alt_typ: Some(snap.alt_typ.clone()),
            speed: snap.speed,
            speed_locked: None,
            eta: None,
            eta_locked: None,
            name: Some(String::from("orbit")),
            parking: None,
            task: Box::new(Task::ComboTask(orbit_tasks)),
        },
    ]
}

fn weapon_bingo(
    lua: MizLua,
    db: &Db,
    gid: GroupId,
    dcs_names: &[String],
    template_unit: &miz::Unit<'_>,
    mission_kind: AiAirMissionKind,
) -> Result<bool> {
    if mission_kind == AiAirMissionKind::CruiseMissileSpawn {
        return Ok(flight_alcm_missile_count(lua, db, gid)? == 0);
    }
    let ag_slots = template_ag_weapon_slots(template_unit);
    if ag_slots == 0 {
        return Ok(false);
    }
    let count = flight_template_weapon_count(lua, dcs_names, template_unit)?;
    if count > 0 {
        return Ok(false);
    }
    // DCS ammo keys may not match template yet; avoid false bingo when unreadable.
    if !flight_has_readable_template_ammo(lua, dcs_names, template_unit)? {
        return Ok(false);
    }
    Ok(true)
}

fn flight_has_readable_template_ammo(
    lua: MizLua,
    names: &[String],
    template_unit: &miz::Unit<'_>,
) -> Result<bool> {
    let keys = template_weapon_ammo_keys(template_unit);
    if keys.is_empty() {
        return Ok(false);
    }
    for name in names {
        let Ok(group) = Group::get_by_name(lua, name) else {
            continue;
        };
        for u in group.get_units()? {
            let u = u?;
            if !u.is_exist()? {
                continue;
            }
            for ammo in u.get_ammo()? {
                let ammo = ammo?;
                if ammo_matches_template_key(&ammo, &keys) {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HubLandMode {
    CycleRtb,
    DurationShutdown,
}

pub(super) fn landing_hub_mission_point<'lua>(
    lua: MizLua<'lua>,
    hub: &HubPick,
    slot: &HubSlot,
    mode: HubLandMode,
) -> Result<MissionPoint<'lua>> {
    let land_alt = hub_slot_baro_alt(lua, slot, hub.baro_alt)?;
    if let Some(ship_id) = slot.link_unit {
        return Ok(MissionPoint {
            typ: PointType::Land,
            airdrome_id: None,
            helipad: Some(AirbaseId::from(ship_id)),
            time_re_fu_ar: None,
            link_unit: Some(UnitId::from(ship_id)),
            action: Some(ActionTyp::Air(TurnMethod::FlyOverPoint)),
            pos: LuaVec2(slot.pos),
            alt: land_alt,
            alt_typ: Some(AltType::BARO),
            speed: ME_PARKING_WAYPOINT_SPEED,
            speed_locked: Some(true),
            eta: None,
            eta_locked: Some(true),
            name: Some(String::from("rtb-land")),
            parking: None,
            task: Box::new(Task::ComboTask(vec![])),
        });
    }
    let (airdrome_id, helipad, link_unit, speed) = match slot.kind {
        HubSlotKind::Parking => (
            hub.airbase_id,
            None,
            None,
            ME_PARKING_WAYPOINT_SPEED,
        ),
        HubSlotKind::Helipad => (
            None,
            Some(AirbaseId::from(slot.slot_id)),
            Some(UnitId::from(slot.slot_id)),
            0.,
        ),
    };
    let (typ, time_re_fu_ar) = match mode {
        HubLandMode::CycleRtb => (PointType::LandingReFuAr, Some(ME_PARKING_REFUEL_REARM)),
        HubLandMode::DurationShutdown => (PointType::Land, None),
    };
    Ok(MissionPoint {
        typ,
        airdrome_id,
        helipad,
        time_re_fu_ar,
        link_unit,
        action: Some(ActionTyp::Air(TurnMethod::FlyOverPoint)),
        pos: LuaVec2(slot.pos),
        alt: land_alt,
        alt_typ: Some(AltType::BARO),
        speed,
        speed_locked: Some(true),
        eta: None,
        eta_locked: Some(true),
        name: Some(String::from("rtb-land")),
        parking: hub_slot_parking_label(slot),
        task: Box::new(Task::ComboTask(vec![])),
    })
}

/// Inbound RTB: hold mission altitude over the parking slot, then land.
pub(super) fn rtb_inbound_route<'lua>(
    lua: MizLua<'lua>,
    hub: &HubPick,
    slot: &HubSlot,
    inbound_alt: f64,
    inbound_alt_typ: AltType,
    mode: HubLandMode,
) -> Result<Vec<MissionPoint<'lua>>> {
    let land = landing_hub_mission_point(lua, hub, slot, mode)?;
    if mode == HubLandMode::DurationShutdown {
        return Ok(vec![land]);
    }
    let land_alt = land.alt;
    let approach_alt = inbound_alt.max(land_alt + 300.);
    Ok(vec![
        MissionPoint {
            typ: PointType::TurningPoint,
            airdrome_id: hub.airbase_id,
            helipad: land.helipad.clone(),
            time_re_fu_ar: None,
            link_unit: land.link_unit.clone(),
            action: Some(ActionTyp::Air(TurnMethod::FlyOverPoint)),
            pos: land.pos,
            alt: approach_alt,
            alt_typ: Some(inbound_alt_typ),
            speed: land.speed,
            speed_locked: Some(true),
            eta: None,
            eta_locked: Some(true),
            name: Some(String::from("rtb-approach")),
            parking: None,
            task: Box::new(Task::ComboTask(vec![])),
        },
        land,
    ])
}

pub(super) fn land_at_hub_route<'lua>(
    lua: MizLua<'lua>,
    hub: &HubPick,
    slot: &HubSlot,
    mode: HubLandMode,
) -> Result<Vec<MissionPoint<'lua>>> {
    Ok(vec![landing_hub_mission_point(lua, hub, slot, mode)?])
}

pub(super) fn group_on_ground(lua: MizLua, group_name: &str) -> Result<bool> {
    let g = Group::get_by_name(lua, group_name)?;
    let u = g.get_unit(1).context("unit 1")?;
    Ok(u.is_exist()? && !u.in_air()?)
}

pub(super) fn flight_on_ground(lua: MizLua, names: &[String]) -> Result<bool> {
    Ok(names
        .iter()
        .all(|n| group_on_ground(lua, n).unwrap_or(false)))
}

pub(super) fn group_in_air(lua: MizLua, group_name: &str) -> Result<bool> {
    let g = Group::get_by_name(lua, group_name)?;
    let u = g.get_unit(1).context("unit 1")?;
    Ok(u.is_exist()? && u.in_air()?)
}

pub(super) fn flight_any_in_air(lua: MizLua, names: &[String]) -> Result<bool> {
    Ok(names
        .iter()
        .any(|n| group_in_air(lua, n).unwrap_or(false)))
}

pub(super) fn flight_any_alive(lua: MizLua, names: &[String]) -> bool {
    names.iter().any(|n| {
        Group::get_by_name(lua, n)
            .ok()
            .and_then(|g| g.get_unit(1).ok())
            .and_then(|u| u.is_exist().ok())
            .unwrap_or(false)
    })
}

fn dcs_group_alive(lua: MizLua, name: &str) -> bool {
    Group::get_by_name(lua, name)
        .ok()
        .and_then(|g| g.is_exist().ok())
        .unwrap_or(false)
        && Group::get_by_name(lua, name)
            .ok()
            .and_then(|g| g.get_unit(1).ok())
            .and_then(|u| u.is_exist().ok())
            .unwrap_or(false)
}

pub(super) fn prune_alive_dcs_names(
    db: &mut Db,
    lua: MizLua,
    gid: GroupId,
    names: &[String],
) -> Vec<String> {
    let alive: Vec<String> = names
        .iter()
        .filter(|n| dcs_group_alive(lua, n))
        .cloned()
        .collect();
    if alive.len() != names.len() {
        if let Ok(group) = group_mut!(db, gid) {
            if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                ai_air.dcs_spawn_names = alive.clone();
            }
        }
    }
    alive
}

fn unit_on_ground_at_hub(
    lua: MizLua,
    name: &str,
    hub_pick: &HubPick,
    hub_pos: Vector2,
    hub_slots: &[HubSlot],
) -> bool {
    if !group_on_ground(lua, name).unwrap_or(false) {
        return false;
    }
    let Ok(pos) = group_center_pos(lua, name) else {
        return false;
    };
    near_point(pos, hub_pick.anchor, 3000.)
        || near_point(pos, hub_pos, 3000.)
        || hub_slots.iter().any(|s| near_point(pos, s.pos, 600.))
}

pub(super) fn flight_all_on_ground_at_hub(
    lua: MizLua,
    names: &[String],
    hub_pick: &HubPick,
    hub_pos: Vector2,
    hub_slots: &[HubSlot],
) -> bool {
    !names.is_empty()
        && names
            .iter()
            .all(|n| unit_on_ground_at_hub(lua, n, hub_pick, hub_pos, hub_slots))
}

fn park_shutdown_all(lua: MizLua, names: &[String]) -> Result<()> {
    for name in names {
        duration_shutdown_park(lua, name)?;
    }
    Ok(())
}

pub(super) fn ensure_player_may_control_ai_air(
    db: &Db,
    gid: GroupId,
    ucid: Option<&Ucid>,
) -> Result<()> {
    let group = group!(db, gid)?;
    let DeployKind::Action { ai_air, player, time, .. } = &group.origin else {
        return Ok(());
    };
    if ai_air.duration_shutdown {
        bail!("{gid} is shutting down after duration expiry");
    }
    let hours = db.ephemeral.cfg.ai_air_action_owner_hours.unwrap_or(24);
    if hours == 0 {
        return Ok(());
    }
    let Some(owner) = player.as_ref() else {
        return Ok(());
    };
    let Some(ucid) = ucid else {
        return Ok(());
    };
    if owner == ucid {
        return Ok(());
    }
    if Utc::now() - *time < Duration::hours(hours as i64) {
        bail!(
            "{gid} is owner-locked for {}h after deploy; only the deploying player may command it until then",
            hours
        );
    }
    Ok(())
}

fn ai_air_phase_label(phase: AiAirPhase) -> &'static str {
    match phase {
        AiAirPhase::Legacy => "legacy",
        AiAirPhase::Bootstrap => "bootstrap",
        AiAirPhase::OnMission => "on-mission",
        AiAirPhase::RtbInbound => "rtb-inbound",
        AiAirPhase::TaxiToParking => "taxi",
        AiAirPhase::Servicing => "servicing",
        AiAirPhase::Refueling => "refueling",
        AiAirPhase::AwaitingLaunch => "awaiting-launch",
        AiAirPhase::Departing => "departing",
        AiAirPhase::ShutdownParked => "shutdown-parked",
    }
}

fn action_air_duration_hours(db: &Db, gid: GroupId) -> Option<u32> {
    let group = db.persisted.groups.get(&gid)?;
    let DeployKind::Action { spec, ai_air, .. } = &group.origin else {
        return None;
    };
    ai_air
        .plane_cfg
        .as_ref()
        .and_then(|c| c.duration)
        .or_else(|| plane_cfg_from_action(&spec.kind).and_then(|c| c.duration))
}

fn action_air_duration_expired(db: &Db, gid: GroupId, now: DateTime<Utc>) -> bool {
    let Some(hours) = action_air_duration_hours(db, gid) else {
        return false;
    };
    let group = match db.persisted.groups.get(&gid) {
        Some(g) => g,
        None => return false,
    };
    let DeployKind::Action { time, .. } = &group.origin else {
        return false;
    };
    now - *time > Duration::hours(hours as i64)
}

fn duration_shutdown_side_msg(gid: GroupId, hub_name: Option<&str>) -> CompactString {
    match hub_name {
        Some(base) => format_compact!("{gid} duration expired — RTB to {base} for shutdown"),
        None => format_compact!(
            "{gid} duration expired — no allied base; orbiting until fuel exhausted"
        ),
    }
}

fn try_duration_shutdown_rtb(
    db: &mut Db,
    lua: MizLua,
    spctx: &SpawnCtx,
    idx: &MizIndex,
    gid: GroupId,
    side: Side,
) -> Result<()> {
    issue_rtb(
        db,
        lua,
        spctx,
        idx,
        side,
        RtbRequest {
            group: gid,
            hub: None,
            hold: false,
            preserve_mission_kind: true,
            duration_shutdown: true,
        },
    )
}

fn begin_duration_shutdown(
    db: &mut Db,
    lua: MizLua,
    spctx: &SpawnCtx,
    idx: &MizIndex,
    gid: GroupId,
    side: Side,
) -> Result<()> {
    {
        let group = group_mut!(db, gid)?;
        let DeployKind::Action { ai_air, .. } = &mut group.origin else {
            return Ok(());
        };
        if ai_air.duration_shutdown {
            return Ok(());
        }
        ai_air.duration_shutdown = true;
    }
    let hub_name = match try_duration_shutdown_rtb(db, lua, spctx, idx, gid, side) {
        Ok(()) => {
            let group = group!(db, gid)?;
            let DeployKind::Action { ai_air, .. } = &group.origin else {
                return Ok(());
            };
            ai_air
                .hub
                .and_then(|h| objective!(db, h).ok().map(|o| o.name.clone()))
        }
        Err(e) => {
            log::warn!("ai air {gid}: duration shutdown RTB failed: {e:#}");
            let group = group_mut!(db, gid)?;
            if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                set_phase(ai_air, AiAirPhase::OnMission);
            }
            None
        }
    };
    db.ephemeral
        .msgs()
        .panel_to_side(
            15,
            false,
            side,
            duration_shutdown_side_msg(gid, hub_name.as_ref().map(|s| s.as_str())),
        );
    Ok(())
}

/// DCS `Unit.getFuel()` fraction (0..1).
const BINGO_FUEL_FRAC: f32 = 0.25;
/// Ignore bingo checks right after entering `OnMission` (DCS ammo/fuel reads settle).
const ON_MISSION_BINGO_MIN: Duration = Duration::seconds(120);

pub(super) fn apply_fowl_air_controller_options(con: &Controller) -> Result<()> {
    con.set_option(AiOption::Air(AirOption::RtbOnBingo(false)))?;
    con.set_option(AiOption::Air(AirOption::RtbOnOutOfAmmo(false)))?;
    con.set_option(AiOption::Air(AirOption::JettTanksIfEmpty(true)))?;
    Ok(())
}

pub(super) fn apply_fowl_air_options_to_name(lua: MizLua, dcs_name: &str) -> Result<()> {
    let Ok(group) = Group::get_by_name(lua, dcs_name) else {
        return Ok(());
    };
    let Ok(con) = group.get_controller() else {
        return Ok(());
    };
    apply_fowl_air_controller_options(&con)
}

fn prepare_ai_unit_for_depart(lua: MizLua, dcs_name: &str) -> Result<()> {
    let Ok(group) = Group::get_by_name(lua, dcs_name) else {
        return Ok(());
    };
    if !group.is_exist()? {
        return Ok(());
    }
    let Ok(con) = group.get_controller() else {
        return Ok(());
    };
    apply_fowl_air_controller_options(&con)?;
    let _ = con.set_on_off(true);
    Ok(())
}

fn unit_ground_position(lua: MizLua, dcs_name: &str) -> Result<Option<Vector2>> {
    let Ok(group) = Group::get_by_name(lua, dcs_name) else {
        return Ok(None);
    };
    let Ok(unit) = group.get_unit(1) else {
        return Ok(None);
    };
    if !unit.is_exist()? || unit.in_air()? {
        return Ok(None);
    }
    let p = unit.get_point()?.0;
    Ok(Some(Vector2::new(p.x, p.z)))
}

fn nearest_parking_slot(
    lua: MizLua,
    ab_oid: &dcso3::object::DcsOid<dcso3::airbase::ClassAirbase>,
    pos: Vector2,
    kind: AiPlaneKind,
    naval_carrier: bool,
) -> Result<Option<HubSlot>> {
    let mut best: Option<(f64, HubSlot)> = None;
    for slot in parking_spots(lua, ab_oid)? {
        if !parking_allowed_for_kind(slot.term_type, kind, naval_carrier) {
            continue;
        }
        let d2 = na::distance_squared(&slot.pos.into(), &pos.into());
        match &best {
            None => best = Some((d2, slot)),
            Some((bd, _)) if d2 < *bd => best = Some((d2, slot)),
            _ => {}
        }
    }
    Ok(best.map(|(_, s)| s))
}

fn resolve_depart_slot(
    lua: MizLua,
    db: &Db,
    hub: &HubPick,
    dcs_name: &str,
    fallback: &HubSlot,
    kind: AiPlaneKind,
) -> Result<HubSlot> {
    let Some(pos) = unit_ground_position(lua, dcs_name)? else {
        return Ok(fallback.clone());
    };
    if fallback.link_unit.is_some() {
        let mut slot = fallback.clone();
        let Ok(obj) = objective!(db, hub.oid) else {
            slot.pos = pos;
            return Ok(slot);
        };
        if let Some(deck) = carrier_fallback_deck_slot(lua, db, &obj)? {
            slot.pos = deck.pos;
            slot.baro_alt = deck.baro_alt;
            slot.link_unit = deck.link_unit;
        } else {
            slot.pos = pos;
        }
        if let Ok(group) = Group::get_by_name(lua, dcs_name) {
            if let Ok(unit) = group.get_unit(1) {
                if let Ok(p) = unit.get_point() {
                    slot.baro_alt = Some(p.0.y);
                }
            }
        }
        return Ok(slot);
    }
    if let Some(ab_oid) = hub_airbase_oid(lua, db, hub.oid)? {
        let naval = objective!(db, hub.oid)
            .map(|o| objective_is_naval_carrier(db, o))
            .unwrap_or(false);
        if let Some(mut nearest) = nearest_parking_slot(lua, &ab_oid, pos, kind, naval)? {
            nearest.link_unit = fallback.link_unit;
            return Ok(nearest);
        }
    }
    let mut slot = fallback.clone();
    slot.pos = pos;
    if let Ok(group) = Group::get_by_name(lua, dcs_name) {
        if let Ok(unit) = group.get_unit(1) {
            if let Ok(p) = unit.get_point() {
                slot.baro_alt = Some(p.0.y);
            }
        }
    }
    Ok(slot)
}

fn push_post_service_depart<'lua>(
    lua: MizLua<'lua>,
    db: &mut Db,
    spctx: &SpawnCtx<'lua>,
    idx: &MizIndex,
    gid: GroupId,
    hub: &HubPick,
    hub_slots: &[HubSlot],
    dcs_names: &[String],
    kind: AiPlaneKind,
) -> Result<()> {
    let plane_kind = plane_cfg_for_ai_air(db, gid)?.kind;
    let refreshed = refresh_hub_slots(lua, db, hub.oid, hub_slots, plane_kind)?;
    {
        let group = group_mut!(db, gid)?;
        if let DeployKind::Action { ai_air, .. } = &mut group.origin {
            ai_air.hub_slots = refreshed.clone();
        }
    }
    let hub_pick = finish_hub_pick(lua, db, hub.oid, refreshed.clone(), hub.airbase_id)?;
    let orbit_route = db.regenerate_ai_air_mission(lua, spctx, idx, gid, false)?;
    for (i, dcs_name) in dcs_names.iter().enumerate() {
        prepare_ai_unit_for_depart(lua, dcs_name)?;
        let fallback = hub_pick
            .slots
            .get(i)
            .or(hub_pick.slots.first())
            .ok_or_else(|| anyhow!("no hub slot for depart"))?;
        let slot = resolve_depart_slot(lua, db, &hub_pick, dcs_name, fallback, kind)?;
        let route = if group_on_ground(lua, dcs_name).unwrap_or(false) {
            let mut takeoff =
                bootstrap_route(lua, db, &hub_pick, &slot, BootstrapMode::PostService)?;
            takeoff.extend(orbit_route.clone());
            takeoff
        } else {
            orbit_route.clone()
        };
        let airborne = !group_on_ground(lua, dcs_name).unwrap_or(false);
        db.ai_air_push_mission_to_name(spctx, dcs_name, route, airborne)?;
    }
    Ok(())
}

fn controller_has_active_task(lua: MizLua, dcs_name: &str) -> bool {
    let Ok(group) = Group::get_by_name(lua, dcs_name) else {
        return false;
    };
    if !group.is_exist().unwrap_or(false) {
        return false;
    }
    group
        .get_controller()
        .ok()
        .and_then(|c| c.has_task().ok())
        .unwrap_or(false)
}

fn ensure_airborne_orbit_task(
    lua: MizLua,
    db: &mut Db,
    spctx: &SpawnCtx,
    idx: &MizIndex,
    gid: GroupId,
    dcs_names: &[String],
) -> Result<()> {
    let phase = {
        let group = group!(db, gid)?;
        let DeployKind::Action { ai_air, .. } = &group.origin else {
            return Ok(());
        };
        ai_air.phase
    };
    if phase != AiAirPhase::OnMission {
        return Ok(());
    }
    for dcs_name in dcs_names {
        if !group_in_air(lua, dcs_name).unwrap_or(false) {
            continue;
        }
        if controller_has_active_task(lua, dcs_name) {
            continue;
        }
        let route = db.regenerate_ai_air_mission(lua, spctx, idx, gid, true)?;
        db.ai_air_push_mission_to_name(spctx, dcs_name, route, true)?;
        log::info!("ai air {gid}: re-pushed orbit (no active DCS task)");
    }
    Ok(())
}

fn ensure_ground_parking_task(
    lua: MizLua,
    db: &mut Db,
    spctx: &SpawnCtx,
    hub: &HubPick,
    slots: &[HubSlot],
    dcs_names: &[String],
) -> Result<()> {
    for (i, dcs_name) in dcs_names.iter().enumerate() {
        if !group_on_ground(lua, dcs_name).unwrap_or(false) {
            continue;
        }
        if controller_has_active_task(lua, dcs_name) {
            continue;
        }
        let slot = slots
            .get(i)
            .or(slots.first())
            .ok_or_else(|| anyhow!("no hub slot"))?;
        push_parking_await(lua, db, spctx, hub, slot, dcs_name)?;
    }
    Ok(())
}

fn mission_kind_weapon_bingo(kind: AiAirMissionKind) -> bool {
    matches!(
        kind,
        AiAirMissionKind::Attackers | AiAirMissionKind::Sead | AiAirMissionKind::CruiseMissileSpawn
    )
}

fn mission_kind_one_shot(kind: AiAirMissionKind) -> bool {
    matches!(kind, AiAirMissionKind::PointToPoint)
}

const ONE_SHOT_RETURN_HUB_RADIUS_M: f64 = 8_000.;

fn group_min_fuel(lua: MizLua, group_name: &str) -> Result<Option<f32>> {
    let g = Group::get_by_name(lua, group_name)?;
    let mut min: Option<f32> = None;
    for u in g.get_units()? {
        let u = u?;
        if !u.is_exist()? {
            continue;
        }
        let f = u.get_fuel()?;
        min = Some(min.map(|m| m.min(f)).unwrap_or(f));
    }
    Ok(min)
}

fn flight_min_fuel(lua: MizLua, names: &[String]) -> Result<Option<f32>> {
    let mut min: Option<f32> = None;
    for n in names {
        if let Some(f) = group_min_fuel(lua, n)? {
            min = Some(min.map(|m| m.min(f)).unwrap_or(f));
        }
    }
    Ok(min)
}

fn is_pylon_store(flags: u64) -> bool {
    flags & WeaponFlag::ArmWeapon as u64 != 0
        || flags & WeaponFlag::AnyWeapon as u64 != 0
            && flags & WeaponFlag::BuiltInCannon as u64 == 0
            && flags & WeaponFlag::Cannons as u64 == 0
}

fn is_status_store(flags: Option<u64>, type_name: &str) -> bool {
    if let Some(f) = flags {
        if is_pylon_store(f) {
            return true;
        }
    }
    let uc = type_name.to_ascii_uppercase();
    !uc.is_empty()
        && !uc.contains("FUEL")
        && !uc.contains("EXT.TANK")
        && !uc.contains("DROP TANK")
        && !uc.contains("CHAFF")
        && !uc.contains("FLARE")
}

fn ammo_store_label(ammo: &dcso3::unit::Ammo<'_>) -> Option<String> {
    let display = ammo.display_name().unwrap_or_default();
    if !display.is_empty() && !display.starts_with('{') {
        return Some(display);
    }
    let typ = ammo.type_name().unwrap_or_default();
    if typ.is_empty() || typ.starts_with('{') {
        let inner = typ
            .trim_matches(|c| c == '{' || c == '}')
            .trim();
        if !inner.is_empty() && !inner.starts_with('{') {
            return Some(String::from(inner));
        }
        return None;
    }
    Some(typ)
}

fn group_store_ammo(lua: MizLua, group_name: &str) -> Result<FxHashMap<String, u32>> {
    let mut out = FxHashMap::default();
    let Ok(group) = Group::get_by_name(lua, group_name) else {
        return Ok(out);
    };
    for u in group.get_units()? {
        let u = u?;
        if !u.is_exist()? {
            continue;
        }
        for ammo in u.get_ammo()? {
            let ammo = ammo?;
            let count = ammo.count()?;
            if count == 0 {
                continue;
            }
            let typ = ammo.type_name().unwrap_or_default();
            let flags = ammo_weapon_flags(&ammo);
            if !is_status_store(flags, typ.as_str()) {
                continue;
            }
            let name = ammo_store_label(&ammo).unwrap_or(typ);
            if name.is_empty() {
                continue;
            }
            *out.entry(name).or_default() += count;
        }
    }
    Ok(out)
}

fn flight_store_ammo(lua: MizLua, names: &[String]) -> Result<Vec<(String, u32)>> {
    let mut out = FxHashMap::default();
    for n in names {
        for (name, count) in group_store_ammo(lua, n)? {
            *out.entry(name).or_default() += count;
        }
    }
    let mut stores: Vec<_> = out.into_iter().collect();
    stores.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(stores)
}

pub(super) fn issue_status(
    db: &mut Db,
    lua: MizLua,
    gid: GroupId,
    side: Side,
    ucid: &Ucid,
) -> Result<()> {
    ensure_player_may_control_ai_air(db, gid, Some(ucid))?;
    let (dcs_names, phase_label) = {
        let group = group!(db, gid)?;
        if group.side != side {
            bail!("wrong team");
        }
        let DeployKind::Action { ai_air, .. } = &group.origin else {
            bail!("{gid} is not an action group");
        };
        (dcs_spawn_names_for(db, gid)?, ai_air_phase_label(ai_air.phase))
    };
    let fuel = flight_min_fuel(lua, &dcs_names)?;
    let stores = flight_store_ammo(lua, &dcs_names)?;
    let fuel_str = match fuel {
        Some(f) => format_compact!("{}%", (f * 100.).round() as u32),
        None => CompactString::from("n/a"),
    };
    let mut msg = format_compact!("{gid} phase {phase_label}, fuel {fuel_str}");
    if stores.is_empty() {
        msg.push_str(", stores none");
    } else {
        msg.push_str(", stores ");
        msg.push_str(&format_stores_parts(&stores).join(", "));
    }
    db.ephemeral
        .panel_to_player(&db.persisted, 15, ucid, msg);
    Ok(())
}

pub(super) fn group_center_pos(lua: MizLua, group_name: &str) -> Result<Vector2> {
    let pos = Group::get_by_name(lua, group_name)?
        .get_unit(1)?
        .get_point()?;
    Ok(Vector2::new(pos.x, pos.z))
}

pub(super) fn flight_center_pos(lua: MizLua, names: &[String]) -> Result<Vector2> {
    let mut pts = Vec::new();
    for n in names {
        if let Ok(p) = group_center_pos(lua, n) {
            pts.push(p);
        }
    }
    if pts.is_empty() {
        bail!("no live ai air flight groups");
    }
    Ok(centroid2d(pts.iter().copied()))
}

fn aggregate_loadout_lines(lines: &[LoadoutLine]) -> FxHashMap<String, (u32, u32)> {
    let mut out: FxHashMap<String, (u32, u32)> = FxHashMap::default();
    for l in lines {
        let e = out.entry(l.name.clone()).or_insert((0, 0));
        e.0 = e.0.saturating_add(l.loaded);
        e.1 = e.1.saturating_add(l.requested);
    }
    out
}

fn format_stores_parts(stores: &[(String, u32)]) -> Vec<CompactString> {
    stores
        .iter()
        .map(|(name, count)| format_compact!("{name} x{count}"))
        .collect()
}

pub(super) fn panel_stores_report(
    db: &mut Db,
    lua: MizLua,
    ucid: &Ucid,
    gid: GroupId,
) -> Result<()> {
    let dcs_names = dcs_spawn_names_for(db, gid)?;
    let fuel = flight_min_fuel(lua, &dcs_names)?;
    let stores = flight_store_ammo(lua, &dcs_names)?;
    let fuel_str = match fuel {
        Some(f) => format_compact!("{}%", (f * 100.).round() as u32),
        None => CompactString::from("n/a"),
    };
    let mut msg = format_compact!("{gid} fuel {fuel_str}");
    if stores.is_empty() {
        msg.push_str(", stores none");
    } else {
        msg.push_str(", stores ");
        msg.push_str(&format_stores_parts(&stores).join(", "));
    }
    db.ephemeral
        .panel_to_player(&db.persisted, 15, ucid, msg);
    Ok(())
}

pub(super) fn panel_loadout_report(
    db: &mut Db,
    ucid: &Ucid,
    gid: GroupId,
    lines: &[LoadoutLine],
) {
    let partial: Vec<CompactString> = aggregate_loadout_lines(lines)
        .into_iter()
        .filter(|(_, (loaded, requested))| *loaded < *requested)
        .map(|(name, (loaded, requested))| format_compact!("{name} {loaded}/{requested}"))
        .collect();
    if partial.is_empty() {
        return;
    }
    let msg = format_compact!(
        "{gid} partial loadout: {}. Will launch with available stores; -action rearm {gid} for more",
        partial.join(", ")
    );
    db.ephemeral.panel_to_player(&db.persisted, 15, ucid, msg);
}

fn partial_loadout(lines: &[LoadoutLine]) -> bool {
    lines.iter().any(|l| l.loaded < l.requested)
}

fn warehouse_item_keys(clsid: &str, display_name: &str) -> Vec<std::string::String> {
    let mut out: Vec<std::string::String> = Vec::new();
    let mut push = |s: std::string::String| {
        if !s.is_empty() && !out.iter().any(|x| x == &s) {
            out.push(s);
        }
    };
    push(clsid.to_string());
    if let Some(inner) = clsid.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
        push(inner.to_string());
    }
    if !display_name.is_empty() {
        push(display_name.to_string());
    }
    push(format!("db/Weapons/ByCLSID/{clsid}"));
    out
}

fn warehouse_stock_for_keys(
    wh: &dcso3::warehouse::Warehouse<'_>,
    keys: &[std::string::String],
) -> (std::string::String, u32) {
    for key in keys {
        let count = wh.get_item_count(String::from(key.as_str())).unwrap_or(0);
        if count > 0 {
            return (key.clone(), count);
        }
    }
    (keys.first().cloned().unwrap_or_default(), 0)
}

fn apply_payload_to_dcs_group(lua: MizLua, dcs_name: &str, payload: &Table) -> Result<()> {
    let group = Group::get_by_name(lua, dcs_name)?;
    let unit = group.get_unit(1).context("unit 1")?;
    if !unit.is_exist()? {
        return Ok(());
    }
    unit.raw_set("payload", payload.clone())?;
    Ok(())
}

fn build_loadout_from_template<'lua>(
    lua: MizLua<'lua>,
    wh: &dcso3::warehouse::Warehouse<'lua>,
    obj: &mut Objective,
    template_unit: &miz::Unit<'lua>,
    deduct_from_warehouse: bool,
) -> Result<(Table<'lua>, Vec<LoadoutLine>)> {
    let payload: Table = template_unit
        .raw_get("payload")
        .unwrap_or_else(|_| lua.inner().create_table().unwrap());
    let pylons: Table = payload
        .raw_get("pylons")
        .unwrap_or_else(|_| lua.inner().create_table().unwrap());
    let out_payload = lua.inner().create_table()?;
    for pair in payload.clone().pairs::<Value, Value>() {
        let (k, v) = pair?;
        if k.as_str() != Some("pylons") {
            out_payload.raw_set(k, v)?;
        }
    }
    let out_pylons = lua.inner().create_table()?;
    let mut lines = Vec::new();
    for pair in pylons.pairs::<Value, Table>() {
        let (idx, pylon) = pair?;
        let clsid: String = pylon.raw_get("CLSID").or_else(|_| pylon.raw_get("clsid"))?;
        let requested: u32 = pylon.raw_get("count").unwrap_or(1);
        let display: String = pylon
            .raw_get::<_, Table>("descriptor")
            .and_then(|d| d.raw_get("displayName"))
            .unwrap_or_else(|_| clsid.clone());
        let type_name: Option<String> = pylon
            .raw_get::<_, Table>("descriptor")
            .and_then(|d| d.raw_get("typeName"))
            .ok();
        let mut keys = warehouse_item_keys(&clsid, &display);
        if let Some(tn) = type_name {
            keys.extend(warehouse_item_keys(&tn, &display));
        }
        let (wh_key, wh_count) = warehouse_stock_for_keys(wh, &keys);
        let loaded = requested.min(wh_count);
        if deduct_from_warehouse && loaded > 0 {
            let _ = wh.remove_item(String::from(wh_key.as_str()), loaded);
            if let Some(inv) = obj.warehouse.equipment.get_mut_cow(wh_key.as_str()) {
                inv.stored = wh.get_item_count(String::from(wh_key.as_str()))?;
            }
        }
        let mut out_pylon = pylon.clone();
        out_pylon.raw_set("count", loaded)?;
        out_pylons.raw_set(idx, out_pylon)?;
        lines.push(LoadoutLine {
            name: display,
            requested,
            loaded,
        });
    }
    out_payload.raw_set("pylons", out_pylons)?;
    Ok((out_payload, lines))
}

pub(super) fn try_rearm_from_template<'lua>(
    lua: MizLua,
    db: &mut Db,
    spctx: &SpawnCtx<'lua>,
    idx: &MizIndex,
    side: Side,
    gid: GroupId,
    template_name: &str,
    hub: ObjectiveId,
    player: Option<&Ucid>,
    deduct_from_warehouse: bool,
) -> Result<Vec<LoadoutLine>> {
    let tpl = spctx.get_template(idx, GroupKind::Any, side, template_name)?;
    let Some(ab_oid) = hub_airbase_oid(lua, db, hub)? else {
        return Ok(vec![]);
    };
    let wh = Airbase::get_instance(lua, &ab_oid)?.get_warehouse()?;
    let obj = objective_mut!(db, hub)?;
    let units = tpl.group.units()?;
    let template_unit = units.get(1)?;
    let (payload, lines) = build_loadout_from_template(
        lua,
        &wh,
        obj,
        &template_unit,
        deduct_from_warehouse,
    )?;
    let dcs_names = dcs_spawn_names_for(db, gid)?;
    for dcs_name in &dcs_names {
        apply_payload_to_dcs_group(lua, dcs_name, &payload)?;
    }
    if let Some(ucid) = player {
        panel_loadout_report(db, ucid, gid, &lines);
    }
    Ok(lines)
}

pub(super) fn arm_flight_from_template<'lua>(
    lua: MizLua,
    db: &mut Db,
    spctx: &SpawnCtx<'lua>,
    idx: &MizIndex,
    side: Side,
    gid: GroupId,
    template_name: &str,
    hub: ObjectiveId,
) -> Result<Vec<LoadoutLine>> {
    // Spawn-time payload shaping: keep pylon counts, but do not deduct warehouse stock here.
    // DCS `timeReFuAr` / `LandingReFuAr` will consume the hub warehouse.
    try_rearm_from_template(
        lua,
        db,
        spctx,
        idx,
        side,
        gid,
        template_name,
        hub,
        None,
        false,
    )
}

pub(super) fn notify_partial_loadout(
    db: &mut Db,
    gid: GroupId,
    ucid: Option<&Ucid>,
    lines: &[LoadoutLine],
) {
    if !partial_loadout(lines) {
        return;
    }
    if let Some(ucid) = ucid {
        panel_loadout_report(db, ucid, gid, lines);
    }
}

pub(super) fn hub_airbase_id(db: &Db, lua: MizLua, oid: ObjectiveId) -> Result<Option<AirbaseId>> {
    let Some(ab) = hub_airbase_oid(lua, db, oid)? else {
        return Ok(None);
    };
    Ok(Some(Airbase::get_instance(lua, &ab)?.get_id()?))
}

pub(super) fn hub_zone_pos(db: &Db, oid: ObjectiveId) -> Result<Vector2> {
    Ok(objective!(db, oid)?.zone.pos())
}

pub(super) fn set_phase(ai: &mut AiAirState, phase: AiAirPhase) {
    ai.phase = phase;
    ai.phase_since = Utc::now();
}

pub(super) fn fuel_available_at_hub(lua: MizLua, db: &Db, hub: ObjectiveId) -> Result<bool> {
    warehouse_has_fuel(lua, db, hub, &[])
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum AiAirMissionKind {
    #[default]
    Unknown,
    Fighters,
    Attackers,
    Sead,
    Tanker,
    Awacs,
    Drone,
    CruiseMissileSpawn,
    /// Bomber / Deployable / Paratrooper / Logistics: fly to target then RTB.
    PointToPoint,
}

pub(super) fn mission_kind_from_action(kind: &ActionKind) -> AiAirMissionKind {
    match kind {
        ActionKind::Fighters(_) => AiAirMissionKind::Fighters,
        ActionKind::Attackers(_) => AiAirMissionKind::Attackers,
        ActionKind::Sead(_) => AiAirMissionKind::Sead,
        ActionKind::Tanker(_) => AiAirMissionKind::Tanker,
        ActionKind::Awacs(_) => AiAirMissionKind::Awacs,
        ActionKind::Drone(_) => AiAirMissionKind::Drone,
        ActionKind::CruiseMissileSpawn(_) => AiAirMissionKind::CruiseMissileSpawn,
        ActionKind::Bomber(_)
        | ActionKind::Nuke(_)
        | ActionKind::Deployable(_)
        | ActionKind::Paratrooper(_)
        | ActionKind::LogisticsRepair(_)
        | ActionKind::LogisticsTransfer(_) => AiAirMissionKind::PointToPoint,
        _ => AiAirMissionKind::Unknown,
    }
}

fn mission_kind_from_template(template: &str) -> AiAirMissionKind {
    let t = template.to_ascii_uppercase();
    if t.contains("FIGHTER") {
        AiAirMissionKind::Fighters
    } else if t.contains("ATTACK") && t.contains("HELI") {
        AiAirMissionKind::Attackers
    } else if t.contains("ATTACK") {
        AiAirMissionKind::Attackers
    } else if t.contains("SEAD") {
        AiAirMissionKind::Sead
    } else if t.contains("TANKER") || t.contains("RTANKER") {
        AiAirMissionKind::Tanker
    } else if t.contains("AWACS") || t.contains("RAWACS") {
        AiAirMissionKind::Awacs
    } else if t.contains("DRONE") {
        AiAirMissionKind::Drone
    } else if t.contains("ALCM") || t.contains("BOMBER") {
        AiAirMissionKind::CruiseMissileSpawn
    } else {
        AiAirMissionKind::Unknown
    }
}

pub(super) fn ai_air_mission_kind(db: &Db, gid: GroupId) -> AiAirMissionKind {
    let Ok(group) = group!(db, gid) else {
        return AiAirMissionKind::Unknown;
    };
    let DeployKind::Action { spec, ai_air, .. } = &group.origin else {
        return AiAirMissionKind::Unknown;
    };
    if ai_air.mission_kind != AiAirMissionKind::Unknown {
        return ai_air.mission_kind;
    }
    let from_spec = mission_kind_from_action(&spec.kind);
    if from_spec != AiAirMissionKind::Unknown {
        return from_spec;
    }
    ai_air_template_name(db, gid)
        .map(|t| mission_kind_from_template(&t))
        .unwrap_or(AiAirMissionKind::Unknown)
}

fn ai_air_template_name(db: &Db, gid: GroupId) -> Option<String> {
    plane_cfg_for_ai_air(db, gid)
        .ok()
        .map(|c| c.template)
}

fn near_point(a: Vector2, b: Vector2, radius: f64) -> bool {
    na::distance_squared(&a.into(), &b.into()) <= radius * radius
}

fn group_has_live_persisted_units(db: &Db, gid: GroupId) -> bool {
    let Some(group) = db.persisted.groups.get(&gid) else {
        return false;
    };
    group.units.into_iter().any(|uid| {
        db.persisted
            .units
            .get(uid)
            .is_some_and(|u| !u.dead)
    })
}

fn group_has_dead_persisted_units(db: &Db, gid: GroupId) -> bool {
    let Some(group) = db.persisted.groups.get(&gid) else {
        return false;
    };
    group.units.into_iter().any(|uid| {
        db.persisted
            .units
            .get(uid)
            .is_some_and(|u| u.dead)
    })
}

/// Rehydrate only for idle ground phases (DCS idle despawn). Never mid-mission / after kills.
fn ai_air_rehydrate_allowed(phase: AiAirPhase) -> bool {
    matches!(
        phase,
        AiAirPhase::AwaitingLaunch
            | AiAirPhase::Servicing
            | AiAirPhase::ShutdownParked
            | AiAirPhase::Refueling
            | AiAirPhase::TaxiToParking
    )
}

/// Mark flight as lost after DCS reports unit destruction (kill, crash, collision).
pub(super) fn mark_ai_air_attrition(db: &mut Db, gid: GroupId) {
    let Some(group) = db.persisted.groups.get_mut_cow(&gid) else {
        return;
    };
    if let DeployKind::Action { ai_air, .. } = &mut group.origin {
        if ai_air.phase != AiAirPhase::Legacy {
            ai_air.attrition = true;
        }
    }
}

fn ai_air_has_attrition(db: &Db, gid: GroupId) -> bool {
    let Ok(group) = group!(db, gid) else {
        return false;
    };
    match &group.origin {
        DeployKind::Action { ai_air, .. } => ai_air.attrition,
        _ => false,
    }
}

fn ai_air_may_rehydrate(db: &Db, gid: GroupId, phase: AiAirPhase) -> bool {
    ai_air_rehydrate_allowed(phase)
        && !ai_air_has_attrition(db, gid)
        && !group_has_dead_persisted_units(db, gid)
}

fn finalize_ai_air_attrition(db: &mut Db, gid: GroupId) -> Result<()> {
    let uids: Vec<_> = {
        let group = group!(db, gid)?;
        group.units.into_iter().collect()
    };
    for uid in &uids {
        if let Some(u) = db.persisted.units.get_mut_cow(uid) {
            if !u.dead {
                u.dead = true;
                u.pos = u.spawn_pos;
                u.heading = u.spawn_heading;
                u.position = u.spawn_position;
            }
        }
    }
    db.ephemeral.dirty();
    if db.group_health(&gid)?.0 == 0 {
        if db.persisted.actions.contains(&gid) {
            if let DeployKind::Action { player, spec, .. } = &group!(db, gid)?.origin {
                if let Some((penalty, ucid)) = spec
                    .penalty
                    .and_then(|p| player.as_ref().map(|pl| (p, pl.clone())))
                {
                    db.adjust_points(
                        &ucid,
                        -(penalty as i32),
                        &format_compact!("for the loss of action group {gid}"),
                    );
                }
            }
        }
        db.delete_group(&gid)?;
    }
    Ok(())
}

fn try_rehydrate_vanished_ai_air(
    db: &mut Db,
    lua: MizLua,
    spctx: &SpawnCtx,
    idx: &MizIndex,
    gid: GroupId,
    hub: &HubPick,
    phase: AiAirPhase,
    perf: Option<&mut bfprotocols::perf::PerfInner>,
) -> Result<()> {
    log::warn!("ai air {gid}: DCS units missing, re-spawning from persist");
    spawn_ai_air_group(
        perf,
        db,
        spctx,
        idx,
        gid,
        hub,
        AiAirPersistSpawn::PersistGround,
    )?;
    let dcs_names = dcs_spawn_names_for(db, gid)?;
    match phase {
        AiAirPhase::AwaitingLaunch | AiAirPhase::ShutdownParked => {
            let group = group_mut!(db, gid)?;
            if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                set_phase(ai_air, AiAirPhase::AwaitingLaunch);
            }
            ensure_ground_parking_task(lua, db, spctx, hub, &hub.slots, &dcs_names)?;
        }
        AiAirPhase::Bootstrap => {
            push_bootstrap_missions(lua, db, spctx, gid, hub, &dcs_names)?;
        }
        AiAirPhase::OnMission | AiAirPhase::Departing | AiAirPhase::RtbInbound => {
            if flight_any_in_air(lua, &dcs_names).unwrap_or(false) {
                let route = db.regenerate_ai_air_mission(lua, spctx, idx, gid, true)?;
                for dcs_name in &dcs_names {
                    db.ai_air_push_mission_to_name(spctx, dcs_name, route.clone(), true)?;
                }
            } else {
                push_post_service_depart(
                    lua,
                    db,
                    spctx,
                    idx,
                    gid,
                    hub,
                    &hub.slots,
                    &dcs_names,
                    plane_cfg_for_ai_air(db, gid)?.kind,
                )?;
            }
        }
        AiAirPhase::Servicing | AiAirPhase::Refueling => {
            let group = group_mut!(db, gid)?;
            if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                set_phase(ai_air, AiAirPhase::Servicing);
            }
        }
        AiAirPhase::Legacy | AiAirPhase::TaxiToParking => {}
    }
    Ok(())
}

/// Phase tick for persisted AI air action groups.
pub(super) fn advance_ai_air(
    lua: MizLua,
    db: &mut Db,
    spctx: &SpawnCtx,
    idx: &MizIndex,
    gid: GroupId,
    now: DateTime<Utc>,
    perf: Option<&mut bfprotocols::perf::PerfInner>,
) -> Result<()> {
    let (mut dcs_names, side, template_name, player, hub, phase, plane_kind, mission_kind, duration_shutdown) = {
        let group = group!(db, gid)?;
        let DeployKind::Action {
            ai_air,
            player,
            ..
        } = &group.origin
        else {
            return Ok(());
        };
        if ai_air.phase == AiAirPhase::Legacy {
            return Ok(());
        };
        let plane_kind = ai_air
            .plane_cfg
            .as_ref()
            .map(|c| c.kind)
            .or_else(|| plane_cfg_for_ai_air(db, gid).ok().map(|c| c.kind))
            .unwrap_or(AiPlaneKind::FixedWing);
        (
            dcs_spawn_names_for(db, gid)?,
            group.side,
            ai_air_template_name(db, gid),
            player.clone(),
            ai_air.hub,
            ai_air.phase,
            plane_kind,
            ai_air.mission_kind,
            ai_air.duration_shutdown,
        )
    };
    let Some(template_name) = template_name else {
        return Ok(());
    };
    dcs_names = prune_alive_dcs_names(db, lua, gid, &dcs_names);
    if !flight_any_alive(lua, &dcs_names) {
        if duration_shutdown {
            db.delete_group(&gid)?;
            return Ok(());
        }
        if !group_has_live_persisted_units(db, gid) {
            db.delete_group(&gid)?;
            return Ok(());
        }
        if !ai_air_may_rehydrate(db, gid, phase) {
            if ai_air_has_attrition(db, gid) || group_has_dead_persisted_units(db, gid) {
                log::info!("ai air {gid}: attrition — not re-spawning destroyed units");
            } else {
                log::warn!(
                    "ai air {gid}: all DCS units gone in phase {:?} — not re-spawning",
                    phase
                );
            }
            finalize_ai_air_attrition(db, gid)?;
            return Ok(());
        }
        if mission_kind_one_shot(ai_air_mission_kind(db, gid)) {
            return Ok(());
        }
        let Some(hub_oid) = hub else {
            return Ok(());
        };
        let airbase_id = hub_airbase_id(db, lua, hub_oid)?;
        let hub_slots = {
            let group = group!(db, gid)?;
            let DeployKind::Action { ai_air, .. } = &group.origin else {
                return Ok(());
            };
            ai_air.hub_slots.clone()
        };
        let refreshed = refresh_hub_slots(lua, db, hub_oid, &hub_slots, plane_kind)?;
        let hub_pick = finish_hub_pick(lua, db, hub_oid, refreshed, airbase_id)?;
        try_rehydrate_vanished_ai_air(
            db,
            lua,
            spctx,
            idx,
            gid,
            &hub_pick,
            phase,
            perf,
        )?;
        return Ok(());
    }
    if !duration_shutdown && action_air_duration_expired(db, gid, now) {
        begin_duration_shutdown(db, lua, spctx, idx, gid, side)?;
        return Ok(());
    }
    let Some(hub) = hub else {
        return Ok(());
    };
    if !db.ephemeral.object_id_by_gid.contains_key(&gid)
        && !db.ephemeral.ai_air_dcs_oids.contains_key(&gid)
    {
        return Ok(());
    }
    let hub_pos = hub_zone_pos(db, hub)?;
    let airbase_id = hub_airbase_id(db, lua, hub)?;
    let hub_slots = {
        let group = group!(db, gid)?;
        let DeployKind::Action { ai_air, .. } = &group.origin else {
            return Ok(());
        };
        ai_air.hub_slots.clone()
    };
    let refreshed_slots = refresh_hub_slots(lua, db, hub, &hub_slots, plane_kind)?;
    {
        let group = group_mut!(db, gid)?;
        if let DeployKind::Action { ai_air, .. } = &mut group.origin {
            ai_air.hub_slots = refreshed_slots.clone();
        }
    }
    let hub_pick = finish_hub_pick(lua, db, hub, refreshed_slots, airbase_id)?;
    let on_ground = flight_on_ground(lua, &dcs_names).unwrap_or(false);
    let in_air = flight_any_in_air(lua, &dcs_names).unwrap_or(false);
    let _pos = flight_center_pos(lua, &dcs_names).unwrap_or(hub_pos);

    match phase {
        AiAirPhase::Refueling => {
            if !on_ground {
                return Ok(());
            }
            let refuel_done = {
                let group = group!(db, gid)?;
                let DeployKind::Action { ai_air, .. } = &group.origin else {
                    return Ok(());
                };
                ai_air.refuel_mission_pushed
            };
            if !refuel_done {
                let template_name = ai_air_template_name(db, gid)
                    .ok_or_else(|| anyhow!("{gid} has no spawn template for refuel"))?;
                let types = spawn_airframe_types(
                    db,
                    spctx,
                    idx,
                    side,
                    &template_name,
                    dcs_names.len(),
                )?;
                if !warehouse_has_fuel(lua, db, hub, &types)? {
                    log::warn!("ai air {gid}: drone refuel skipped (no fuel at hub)");
                    if let Some(ucid) = player.as_ref() {
                        let base =
                            objective!(db, hub).map(|o| o.name.clone()).unwrap_or_default();
                        db.ephemeral.panel_to_player(
                            &db.persisted,
                            15,
                            ucid,
                            format_compact!(
                                "{gid} no fuel at {base}; fix hub stock or -action rearm {gid}"
                            ),
                        );
                    }
                    let group = group_mut!(db, gid)?;
                    if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                        set_phase(ai_air, AiAirPhase::AwaitingLaunch);
                    }
                    return Ok(());
                }
                match refuel_drone_by_respawn(lua, db, spctx, idx, gid, &hub_pick, &types) {
                    Ok(()) => {}
                    Err(e) => {
                        log::warn!(
                            "ai air {gid}: respawn refuel failed ({e:#}), trying timeReFuAr"
                        );
                        push_drone_refuel_missions(lua, db, spctx, gid, &hub_pick, &dcs_names)?;
                        let group = group_mut!(db, gid)?;
                        if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                            ai_air.refuel_mission_pushed = true;
                        }
                        return Ok(());
                    }
                }
                for dcs_name in &dcs_names {
                    hold_drone_on_parking(lua, dcs_name)?;
                }
                let fuel = flight_min_fuel(lua, &dcs_names)?;
                let group = group_mut!(db, gid)?;
                if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                    ai_air.refuel_mission_pushed = true;
                    set_phase(ai_air, AiAirPhase::AwaitingLaunch);
                }
                if let Some(ucid) = player.as_ref() {
                    let base = objective!(db, hub).map(|o| o.name.clone()).unwrap_or_default();
                    let pct = fuel.map(|f| (f * 100.) as u32).unwrap_or(0);
                    db.ephemeral.panel_to_player(
                        &db.persisted,
                        15,
                        ucid,
                        format_compact!(
                            "{gid} refueled at {base} ({pct}%); -action start {gid} to launch"
                        ),
                    );
                }
                return Ok(());
            }
            if now - {
                let group = group!(db, gid)?;
                let DeployKind::Action { ai_air, .. } = &group.origin else {
                    return Ok(());
                };
                ai_air.phase_since
            } < Duration::seconds(5)
            {
                return Ok(());
            }
            let fuel = flight_min_fuel(lua, &dcs_names)?;
            let done = fuel.map(|f| f > 0.05).unwrap_or(false);
            let timeout = now - {
                let group = group!(db, gid)?;
                let DeployKind::Action { ai_air, .. } = &group.origin else {
                    return Ok(());
                };
                ai_air.phase_since
            } > Duration::seconds(90);
            if !done && !timeout {
                return Ok(());
            }
            for dcs_name in &dcs_names {
                hold_drone_on_parking(lua, dcs_name)?;
            }
            if done {
                log::info!(
                    "ai air {gid}: drone refuel complete ({:.0}% fuel)",
                    fuel.unwrap_or(0.) * 100.
                );
            } else {
                log::warn!(
                    "ai air {gid}: drone refuel timed out (fuel {:?}%)",
                    fuel.map(|f| (f * 100.) as u32)
                );
            }
            let group = group_mut!(db, gid)?;
            if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                set_phase(ai_air, AiAirPhase::AwaitingLaunch);
            }
            if let Some(ucid) = player.as_ref() {
                let base = objective!(db, hub).map(|o| o.name.clone()).unwrap_or_default();
                let pct = fuel.map(|f| (f * 100.) as u32).unwrap_or(0);
                db.ephemeral.panel_to_player(
                    &db.persisted,
                    15,
                    ucid,
                    format_compact!(
                        "{gid} refueled at {base} ({pct}%); -action start {gid} to launch"
                    ),
                );
            }
        }
        AiAirPhase::Bootstrap => {
            let push_bootstrap = {
                let group = group!(db, gid)?;
                match &group.origin {
                    DeployKind::Action { ai_air, .. } => !ai_air.bootstrap_mission_pushed,
                    _ => false,
                }
            };
            if push_bootstrap && on_ground {
                let slots = {
                    let group = group!(db, gid)?;
                    let DeployKind::Action { ai_air, .. } = &group.origin else {
                        return Ok(());
                    };
                    ai_air.hub_slots.clone()
                };
                for (i, dcs_name) in dcs_names.iter().enumerate() {
                    let slot = slots
                        .get(i)
                        .or(slots.first())
                        .ok_or_else(|| anyhow!("no hub slot for bootstrap"))?;
                    let route = bootstrap_route(lua, db, &hub_pick, slot, BootstrapMode::ColdSpawn)?;
                    db.ai_air_push_mission_to_name(spctx, dcs_name, route, false)?;
                }
                let group = group_mut!(db, gid)?;
                if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                    ai_air.bootstrap_mission_pushed = true;
                }
            }
            if on_ground {
                let group = group_mut!(db, gid)?;
                if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                    ai_air.bootstrap_grounded = true;
                }
            }
            let grounded = {
                let group = group!(db, gid)?;
                match &group.origin {
                    DeployKind::Action { ai_air, .. } => ai_air.bootstrap_grounded,
                    _ => false,
                }
            };
            if in_air && grounded {
                let _snap = {
                    let group = group_mut!(db, gid)?;
                    let DeployKind::Action { ai_air, .. } = &mut group.origin else {
                        return Ok(());
                    };
                    let snap = ai_air.active_mission.clone();
                    set_phase(ai_air, AiAirPhase::OnMission);
                    snap
                };
                // After `timeReFuAr` / `LandingReFuAr`, DCS should have taken stock from
                // the hub warehouse; sync virtual stock so later spawns/servicing stay consistent.
                if let Err(e) = db.sync_warehouse_to_objective(lua, hub) {
                    log::warn!("ai air {gid}: warehouse sync after bootstrap failed: {e:#}");
                }
                let route = db.regenerate_ai_air_mission(lua, spctx, idx, gid, true)?;
                log::info!("ai air {gid}: airborne -> on-mission orbit ({} wpts)", route.len());
                db.ai_air_push_mission(spctx, gid, route, true)?;
            } else if on_ground {
                let retry = {
                    let group = group!(db, gid)?;
                    let DeployKind::Action { ai_air, .. } = &group.origin else {
                        return Ok(());
                    };
                    now - ai_air.phase_since > Duration::seconds(120)
                };
                if retry {
                    let group = group_mut!(db, gid)?;
                    if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                        ai_air.bootstrap_retries = ai_air.bootstrap_retries.saturating_add(1);
                        set_phase(ai_air, ai_air.phase);
                    }
                    let slots = {
                        let group = group!(db, gid)?;
                        let DeployKind::Action { ai_air, .. } = &group.origin else {
                            return Ok(());
                        };
                        ai_air.hub_slots.clone()
                    };
                    for (i, dcs_name) in dcs_names.iter().enumerate() {
                        let slot = slots
                            .get(i)
                            .or(slots.first())
                            .ok_or_else(|| anyhow!("no hub slot for bootstrap retry"))?;
                        let route = bootstrap_route(lua, db, &hub_pick, slot, BootstrapMode::ColdSpawn)?;
                        db.ai_air_push_mission_to_name(spctx, dcs_name, route, false)?;
                    }
                }
            }
        }
        AiAirPhase::OnMission => {
            if duration_shutdown {
                let retry = {
                    let group = group!(db, gid)?;
                    let DeployKind::Action { ai_air, .. } = &group.origin else {
                        return Ok(());
                    };
                    now - ai_air.phase_since >= Duration::seconds(120)
                };
                if retry && in_air {
                    if try_duration_shutdown_rtb(db, lua, spctx, idx, gid, side).is_err() {
                        let group = group_mut!(db, gid)?;
                        if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                            set_phase(ai_air, AiAirPhase::OnMission);
                        }
                    }
                }
            } else if mission_kind_one_shot(mission_kind) {
                if in_air {
                    let return_home = match &group!(db, gid)?.origin {
                        DeployKind::Action {
                            rtb: Some(origin),
                            ai_air,
                            ..
                        } if !ai_air.duration_shutdown => flight_center_pos(lua, &dcs_names)
                            .map(|p| near_point(p, *origin, ONE_SHOT_RETURN_HUB_RADIUS_M))
                            .unwrap_or(false),
                        _ => false,
                    };
                    if return_home {
                        log::info!("ai air {gid}: one-shot return near hub -> shutdown RTB");
                        let origin_hub = {
                            let group = group!(db, gid)?;
                            let DeployKind::Action { ai_air, .. } = &group.origin else {
                                return Ok(());
                            };
                            ai_air.hub
                        };
                        {
                            let group = group_mut!(db, gid)?;
                            if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                                ai_air.duration_shutdown = true;
                            }
                        }
                        issue_rtb(
                            db,
                            lua,
                            spctx,
                            idx,
                            side,
                            RtbRequest {
                                group: gid,
                                hub: origin_hub,
                                hold: false,
                                preserve_mission_kind: true,
                                duration_shutdown: true,
                            },
                        )?;
                        return Ok(());
                    }
                    ensure_airborne_orbit_task(lua, db, spctx, idx, gid, &dcs_names)?;
                }
            } else {
                let mut issued_rtb = false;
                let bingo_ready = {
                    let group = group!(db, gid)?;
                    let DeployKind::Action { ai_air, .. } = &group.origin else {
                        return Ok(());
                    };
                    now - ai_air.phase_since >= ON_MISSION_BINGO_MIN
                };
                if in_air && bingo_ready {
                    if let Some(fuel) = flight_min_fuel(lua, &dcs_names)? {
                        if fuel <= BINGO_FUEL_FRAC {
                            log::info!(
                                "ai air {gid}: bingo fuel ({:.0}%) -> RTB nearest hub",
                                fuel * 100.
                            );
                            issue_rtb(
                                db,
                                lua,
                                spctx,
                                idx,
                                side,
                                RtbRequest {
                                    group: gid,
                                    hub: None,
                                    hold: false,
                                    preserve_mission_kind: true,
                                    duration_shutdown: false,
                                },
                            )?;
                            issued_rtb = true;
                        }
                    }
                    if !issued_rtb && mission_kind_weapon_bingo(mission_kind) {
                        let tpl = spctx.get_template(idx, GroupKind::Any, side, &template_name)?;
                        let template_unit = tpl.group.units()?.get(1)?;
                        if weapon_bingo(lua, db, gid, &dcs_names, &template_unit, mission_kind)? {
                            log::info!("ai air {gid}: bingo weapons -> RTB nearest hub");
                            issue_rtb(
                                db,
                                lua,
                                spctx,
                                idx,
                                side,
                                RtbRequest {
                                    group: gid,
                                    hub: None,
                                    hold: false,
                                    preserve_mission_kind: true,
                                    duration_shutdown: false,
                                },
                            )?;
                            issued_rtb = true;
                        }
                    }
                }
                if !issued_rtb {
                    for dcs_name in &dcs_names {
                        let _ = apply_fowl_air_options_to_name(lua, dcs_name);
                    }
                    ensure_airborne_orbit_task(lua, db, spctx, idx, gid, &dcs_names)?;
                }
            }
        }
        AiAirPhase::RtbInbound => {
            if !flight_all_on_ground_at_hub(lua, &dcs_names, &hub_pick, hub_pos, &hub_pick.slots) {
                return Ok(());
            }
            if duration_shutdown {
                log::info!("ai air {gid}: all members at hub -> duration shutdown parked");
                park_shutdown_all(lua, &dcs_names)?;
                let group = group_mut!(db, gid)?;
                if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                    set_phase(ai_air, AiAirPhase::ShutdownParked);
                }
                return Ok(());
            }
            log::info!("ai air {gid}: on ground at hub -> servicing");
            let group = group_mut!(db, gid)?;
            if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                set_phase(ai_air, AiAirPhase::Servicing);
            }
        }
        AiAirPhase::Servicing => {
            let phase_since = {
                let group = group!(db, gid)?;
                let DeployKind::Action { ai_air, .. } = &group.origin else {
                    return Ok(());
                };
                ai_air.phase_since
            };
            if now - phase_since < Duration::seconds(3) {
                return Ok(());
            }
            if now - phase_since < servicing_complete_wait() {
                ensure_ground_parking_task(lua, db, spctx, &hub_pick, &hub_pick.slots, &dcs_names)?;
                return Ok(());
            }
            let fuel_ok = flight_min_fuel(lua, &dcs_names)?
                .map(|f| f > BINGO_FUEL_FRAC + 0.05)
                .unwrap_or(false);
            if !fuel_ok && now - phase_since < servicing_complete_wait() + Duration::seconds(90) {
                ensure_ground_parking_task(lua, db, spctx, &hub_pick, &hub_pick.slots, &dcs_names)?;
                return Ok(());
            }
            // DCS `LandingReFuAr` / `timeReFuAr` already refuels and re-arms; keep Fowl stock in sync.
            if let Err(e) = db.sync_warehouse_to_objective(lua, hub) {
                log::warn!("ai air {gid}: warehouse sync after servicing failed: {e:#}");
            }
            let (hold, no_fuel, panel) = {
                let group = group!(db, gid)?;
                let DeployKind::Action { ai_air, .. } = &group.origin else {
                    return Ok(());
                };
                let panel = if ai_air.rtb_hold
                    || !fuel_available_at_hub(lua, db, hub).unwrap_or(false)
                {
                    player.as_ref().map(|ucid| {
                        let base = objective!(db, hub).map(|o| o.name.clone()).unwrap_or_default();
                        (
                            ucid.clone(),
                            format_compact!("{gid} ready at {base}, awaiting -action start {gid}"),
                        )
                    })
                } else {
                    None
                };
                (
                    ai_air.rtb_hold,
                    !fuel_available_at_hub(lua, db, hub).unwrap_or(false),
                    panel,
                )
            };
            let departing = !hold && !no_fuel;
            {
                let group = group_mut!(db, gid)?;
                if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                    if hold || no_fuel {
                        set_phase(ai_air, AiAirPhase::AwaitingLaunch);
                    } else {
                        set_phase(ai_air, AiAirPhase::Departing);
                    }
                }
            }
            if let Some((ucid, msg)) = panel {
                db.ephemeral.panel_to_player(&db.persisted, 15, &ucid, msg);
            }
            if !departing {
                ensure_ground_parking_task(lua, db, spctx, &hub_pick, &hub_pick.slots, &dcs_names)?;
                return Ok(());
            }
            log::info!("ai air {gid}: servicing done -> depart (post-service takeoff + orbit)");
            let slots = {
                let group = group!(db, gid)?;
                let DeployKind::Action { ai_air, .. } = &group.origin else {
                    return Ok(());
                };
                ai_air.hub_slots.clone()
            };
            push_post_service_depart(
                lua,
                db,
                spctx,
                idx,
                gid,
                &hub_pick,
                &slots,
                &dcs_names,
                plane_kind,
            )?;
        }
        AiAirPhase::AwaitingLaunch => {
            ensure_ground_parking_task(lua, db, spctx, &hub_pick, &hub_pick.slots, &dcs_names)?;
        }
        AiAirPhase::Departing => {
            if in_air {
                let _snap = {
                    let group = group_mut!(db, gid)?;
                    let DeployKind::Action { ai_air, .. } = &mut group.origin else {
                        return Ok(());
                    };
                    let snap = ai_air.active_mission.clone();
                    set_phase(ai_air, AiAirPhase::OnMission);
                    snap
                };
                let route = db.regenerate_ai_air_mission(lua, spctx, idx, gid, true)?;
                log::info!("ai air {gid}: depart -> on-mission orbit ({} wpts)", route.len());
                db.ai_air_push_mission(spctx, gid, route, true)?;
            } else if on_ground {
                let phase_since = {
                    let group = group!(db, gid)?;
                    let DeployKind::Action { ai_air, .. } = &group.origin else {
                        return Ok(());
                    };
                    ai_air.phase_since
                };
                if now - phase_since > Duration::seconds(30) {
                    log::info!("ai air {gid}: depart retry (post-service takeoff + orbit)");
                    push_post_service_depart(
                        lua,
                        db,
                        spctx,
                        idx,
                        gid,
                        &hub_pick,
                        &hub_slots,
                        &dcs_names,
                        plane_kind,
                    )?;
                    let group = group_mut!(db, gid)?;
                    if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                        set_phase(ai_air, AiAirPhase::Departing);
                    }
                }
            }
        }
        AiAirPhase::TaxiToParking | AiAirPhase::Legacy => (),
        AiAirPhase::ShutdownParked => {
            park_shutdown_all(lua, &dcs_names)?;
            if duration_shutdown || mission_kind_one_shot(mission_kind) {
                let parked = now - {
                    let group = group!(db, gid)?;
                    let DeployKind::Action { ai_air, .. } = &group.origin else {
                        return Ok(());
                    };
                    ai_air.phase_since
                } > Duration::seconds(15);
                if parked {
                    log::info!("ai air {gid}: one-shot/shutdown parked -> removing from world");
                    db.delete_group(&gid)?;
                }
            }
        }
    }
    Ok(())
}

fn ai_air_phase_template(kind: &ActionKind) -> Option<&AiPlaneCfg> {
    match kind {
        ActionKind::Tanker(c)
        | ActionKind::Fighters(c)
        | ActionKind::Attackers(c)
        | ActionKind::Sead(c)
        | ActionKind::CruiseMissileSpawn(c) => Some(c),
        ActionKind::Awacs(a) => Some(&a.plane),
        ActionKind::Drone(d) => Some(&d.plane),
        ActionKind::LogisticsRepair(c) | ActionKind::LogisticsTransfer(c) => Some(c),
        _ => None,
    }
}

pub(super) fn plane_cfg_from_action(kind: &ActionKind) -> Option<AiPlaneCfg> {
    ai_air_phase_template(kind).cloned()
}

pub(super) fn plane_cfg_for_ai_air(db: &Db, gid: GroupId) -> Result<AiPlaneCfg> {
    let group = group!(db, gid)?;
    let DeployKind::Action { spec, ai_air, .. } = &group.origin else {
        bail!("not an action aircraft");
    };
    if let Some(cfg) = ai_air.plane_cfg.as_ref() {
        return Ok(cfg.clone());
    }
    if let Some(cfg) = plane_cfg_from_action(&spec.kind) {
        return Ok(cfg);
    }
    if matches!(spec.kind, ActionKind::Rtb | ActionKind::Start) {
        let snap = &ai_air.active_mission;
        let first_uid = group.units.into_iter().next();
        let template = first_uid
            .and_then(|uid| db.persisted.units.get(uid))
            .map(|u| u.template_name.clone())
            .unwrap_or_else(|| group.name.clone());
        let kind = first_uid
            .and_then(|uid| db.persisted.units.get(uid))
            .map(|u| {
                if u.tags.contains(UnitTag::Helicopter) {
                    AiPlaneKind::Helicopter
                } else {
                    AiPlaneKind::FixedWing
                }
            })
            .unwrap_or(AiPlaneKind::FixedWing);
        return Ok(AiPlaneCfg {
            kind,
            duration: None,
            template,
            altitude: snap.alt,
            altitude_typ: snap.alt_typ.clone(),
            speed: snap.speed,
            freq: None,
        });
    }
    bail!("not an ai air unit")
}

fn rtb_hub_search_pos(lua: MizLua, db: &Db, gid: GroupId) -> Result<Vector2> {
    let group = group!(db, gid)?;
    let DeployKind::Action { ai_air, .. } = &group.origin else {
        bail!("not an action aircraft");
    };
    let live = flight_center_pos(lua, &dcs_spawn_names_for(db, gid)?).ok();
    let mission = (ai_air.active_mission.pos != Vector2::default()).then_some(ai_air.active_mission.pos);
    let use_mission = matches!(
        ai_air.phase,
        AiAirPhase::OnMission | AiAirPhase::Bootstrap | AiAirPhase::Legacy
    );
    match (live, mission) {
        (_, Some(m)) if use_mission => Ok(m),
        (Some(l), _) => Ok(l),
        (_, Some(m)) => Ok(m),
        _ => Err(anyhow!("no live ai air position for RTB")),
    }
}

pub(super) fn issue_rtb(
    db: &mut Db,
    lua: MizLua,
    spctx: &SpawnCtx,
    idx: &MizIndex,
    side: Side,
    req: RtbRequest,
) -> Result<()> {
    if !req.duration_shutdown {
        ensure_player_may_control_ai_air(db, req.group, None)?;
    }
    let land_mode = if req.duration_shutdown {
        HubLandMode::DurationShutdown
    } else {
        HubLandMode::CycleRtb
    };
    let plane = {
        let group = group!(db, req.group)?;
        if group.side != side {
            bail!("wrong team");
        }
        plane_cfg_for_ai_air(db, req.group)?
    };
    let pos = rtb_hub_search_pos(lua, db, req.group)?;
    let hub = match req.hub {
        Some(h) => {
            let obj = objective!(db, h)?;
            if obj.owner != side {
                bail!("base not owned");
            }
            log::info!("ai air rtb {} -> hub {:?} (explicit)", req.group, obj.name);
            let airbase_id = hub_airbase_id(db, lua, h)?;
            let n = group!(db, req.group)?.units.len().max(1);
            let slots = {
                let group = group!(db, req.group)?;
                let DeployKind::Action { ai_air, .. } = &group.origin else {
                    bail!("not action");
                };
                if ai_air.hub == Some(h) && ai_air.hub_slots.len() >= n {
                    ai_air.hub_slots.clone()
                } else {
                    let claimed = claimed_hub_slots_excluding(db, Some(req.group));
                    let picked =
                        free_slots_at_hub(lua, db, obj, side, plane.kind, n, &claimed)?;
                    if picked.len() < n {
                        bail!("no free slots at {}", obj.name);
                    }
                    picked
                }
            };
            finish_hub_pick(lua, db, h, slots, airbase_id)?
        }
        None => {
            let n = group!(db, req.group)?.units.len().max(1);
            let hub = select_hub_for_ai(
                lua,
                db,
                spctx,
                idx,
                side,
                pos,
                &plane,
                n,
                HubSelectMode::Landing,
            )?;
            log::info!(
                "ai air rtb {} -> hub {:?} (bingo/auto)",
                req.group,
                objective!(db, hub.oid).map(|o| o.name.as_str()).unwrap_or("?")
            );
            hub
        }
    };
    let refreshed_slots = refresh_hub_slots(lua, db, hub.oid, &hub.slots, plane.kind)?;
    let hub = finish_hub_pick(lua, db, hub.oid, refreshed_slots, hub.airbase_id)?;
    let (inbound_alt, inbound_alt_typ) = {
        let group = group!(db, req.group)?;
        let DeployKind::Action { ai_air, .. } = &group.origin else {
            bail!("not action");
        };
        (
            ai_air.active_mission.alt,
            ai_air.active_mission.alt_typ.clone(),
        )
    };
    let rtb_pos = hub
        .slots
        .first()
        .map(|s| s.pos)
        .unwrap_or_else(|| hub_zone_pos(db, hub.oid).unwrap_or_default());
    {
        let group = group_mut!(db, req.group)?;
        let DeployKind::Action { ai_air, rtb, spec, .. } = &mut group.origin else {
            bail!("not action");
        };
        if ai_air.active_mission.pos == Vector2::default() {
            let cfg = ai_air
                .plane_cfg
                .as_ref()
                .cloned()
                .or_else(|| plane_cfg_from_action(&spec.kind));
            if let Some(c) = cfg {
                ai_air.active_mission = snapshot_from_loiter(
                    pos,
                    c.altitude,
                    c.altitude_typ.clone(),
                    c.speed,
                    false,
                );
            }
        }
        if ai_air.plane_cfg.is_none() {
            if let Some(c) = plane_cfg_from_action(&spec.kind) {
                ai_air.plane_cfg = Some(c);
            }
        }
        if ai_air.mission_kind == AiAirMissionKind::Unknown {
            ai_air.mission_kind = mission_kind_from_action(&spec.kind);
        }
        *rtb = Some(rtb_pos);
        ai_air.hub = Some(hub.oid);
        ai_air.hub_slots = hub.slots.clone();
        ai_air.rtb_hold = req.hold;
        if req.duration_shutdown {
            ai_air.duration_shutdown = true;
        }
        set_phase(ai_air, AiAirPhase::RtbInbound);
        if !req.preserve_mission_kind {
            spec.kind = ActionKind::Rtb;
        }
    }
    let dcs_names = dcs_spawn_names_for(db, req.group)?;
    for (i, dcs_name) in dcs_names.iter().enumerate() {
        let slot = hub
            .slots
            .get(i)
            .or(hub.slots.first())
            .ok_or_else(|| anyhow!("hub has no landing slot"))?;
        let route = rtb_inbound_route(
            lua,
            &hub,
            slot,
            inbound_alt,
            inbound_alt_typ.clone(),
            land_mode,
        )?;
        log::info!(
            "ai air rtb {} -> {:?} slot {} airdrome {:?} parking {:?}",
            req.group,
            objective!(db, hub.oid).map(|o| o.name.as_str()).unwrap_or("?"),
            slot.slot_id,
            hub.airbase_id,
            route.first().and_then(|p| p.parking.as_deref()),
        );
        db.ai_air_push_mission_to_name(spctx, dcs_name, route, true)?;
    }
    Ok(())
}

pub(super) fn issue_rearm(
    db: &mut Db,
    lua: MizLua,
    spctx: &SpawnCtx,
    idx: &MizIndex,
    gid: GroupId,
    side: Side,
    ucid: &Ucid,
) -> Result<()> {
    ensure_player_may_control_ai_air(db, gid, Some(ucid))?;
    let (template_name, hub, phase) = {
        let group = group!(db, gid)?;
        if group.side != side {
            bail!("wrong team");
        }
        let DeployKind::Action { ai_air, .. } = &group.origin else {
            bail!("not an action aircraft");
        };
        if !matches!(
            ai_air.phase,
            AiAirPhase::AwaitingLaunch | AiAirPhase::Servicing
        ) {
            bail!("{gid} is not on the ground awaiting rearm");
        }
        (
            ai_air_template_name(db, gid),
            ai_air.hub.ok_or_else(|| anyhow!("no hub"))?,
            ai_air.phase,
        )
    };
    let template_name =
        template_name.ok_or_else(|| anyhow!("{gid} has no spawn template for rearm"))?;
    let dcs_names = dcs_spawn_names_for(db, gid)?;
    if !flight_on_ground(lua, &dcs_names)? {
        bail!("{gid} must be on the ground to rearm");
    }
    let lines = try_rearm_from_template(
        lua,
        db,
        spctx,
        idx,
        side,
        gid,
        &template_name,
        hub,
        Some(ucid),
        true,
    )?;
    notify_partial_loadout(db, gid, Some(ucid), &lines);
    if !partial_loadout(&lines) && phase == AiAirPhase::Servicing {
        let group = group_mut!(db, gid)?;
        if let DeployKind::Action { ai_air, .. } = &mut group.origin {
            set_phase(ai_air, AiAirPhase::AwaitingLaunch);
        }
        db.ephemeral.panel_to_player(
            &db.persisted,
            15,
            ucid,
            format_compact!("{gid} fully rearmed. -action start {gid} to launch"),
        );
    } else {
        db.ephemeral.panel_to_player(
            &db.persisted,
            15,
            ucid,
            format_compact!("{gid} fully rearmed. -action start {gid} to launch"),
        );
    }
    Ok(())
}

pub(super) fn issue_start(
    db: &mut Db,
    lua: MizLua,
    spctx: &SpawnCtx,
    idx: &MizIndex,
    gid: GroupId,
    side: Side,
    ucid: &Ucid,
) -> Result<()> {
    ensure_player_may_control_ai_air(db, gid, Some(ucid))?;
    {
        let group = group!(db, gid)?;
        if group.side != side {
            bail!("wrong team");
        }
        let DeployKind::Action { ai_air, .. } = &group.origin else {
            bail!("not an action aircraft");
        };
        if ai_air.phase != AiAirPhase::AwaitingLaunch {
            bail!("{gid} is not awaiting launch");
        }
    }
    let dcs_names = dcs_spawn_names_for(db, gid)?;
    if !flight_on_ground(lua, &dcs_names)? {
        bail!("{gid} must be on the ground to start");
    }
    let hub_oid = {
        let group = group!(db, gid)?;
        let DeployKind::Action { ai_air, .. } = &group.origin else {
            bail!("not action");
        };
        ai_air.hub.ok_or_else(|| anyhow!("no hub"))?
    };
    let slots = {
        let group = group!(db, gid)?;
        let DeployKind::Action { ai_air, .. } = &group.origin else {
            bail!("not action");
        };
        ai_air.hub_slots.clone()
    };
    let airbase_id = hub_airbase_id(db, lua, hub_oid)?;
    let hub_pick = finish_hub_pick(lua, db, hub_oid, slots.clone(), airbase_id)?;
    {
        let group = group_mut!(db, gid)?;
        if let DeployKind::Action { ai_air, .. } = &mut group.origin {
            ai_air.rtb_hold = false;
            set_phase(ai_air, AiAirPhase::Departing);
        }
    }
    let plane_kind = plane_cfg_for_ai_air(db, gid)?.kind;
    push_post_service_depart(
        lua,
        db,
        spctx,
        idx,
        gid,
        &hub_pick,
        &slots,
        &dcs_names,
        plane_kind,
    )?;
    Ok(())
}

pub(super) fn spawn_ai_air_group<'lua>(
    perf: Option<&mut bfprotocols::perf::PerfInner>,
    db: &mut Db,
    spctx: &SpawnCtx<'lua>,
    idx: &MizIndex,
    gid: GroupId,
    hub: &HubPick,
    mode: AiAirPersistSpawn,
) -> Result<()> {
    let ts = Utc::now();
    let lua = spctx.lua();
    let (fowl_name, side, cfg_template, existing_dcs_names, mission_kind) = {
        let group = group!(db, gid)?;
        let mission_kind = match &group.origin {
            DeployKind::Action { ai_air, .. } => ai_air.mission_kind,
            _ => AiAirMissionKind::Unknown,
        };
        let existing = match &group.origin {
            DeployKind::Action { ai_air, .. } => ai_air.dcs_spawn_names.clone(),
            _ => vec![],
        };
        (
            group.name.clone(),
            group.side,
            group.template_name.clone(),
            existing,
            mission_kind,
        )
    };
    let unit_pool: Vec<(&SpawnedUnit, String)> = {
        let group = group!(db, gid)?;
        group
            .units
            .into_iter()
            .filter_map(|uid| {
                db.persisted.units.get(uid).and_then(|u| {
                    if u.dead {
                        None
                    } else {
                        Some((u, u.template_name.clone()))
                    }
                })
            })
            .collect()
    };
    if unit_pool.is_empty() {
        bail!("no alive units to spawn for {gid}");
    }
    db.ephemeral.ai_air_dcs_oids.remove(&gid);
    if let Some(old) = db.ephemeral.object_id_by_gid.remove(&gid) {
        db.ephemeral.gid_by_object_id.remove(&old);
    }
    let mut dcs_names = Vec::new();
    let mut oids = SmallVec::<[dcso3::object::DcsOid<dcso3::group::ClassGroup>; 4]>::new();
    let flight_anchor = centroid2d(unit_pool.iter().map(|(su, _)| su.pos));
    for (slot_i, (su, cfg_unit_name)) in unit_pool.iter().enumerate() {
        let dcs_name = existing_dcs_names
            .get(slot_i)
            .cloned()
            .unwrap_or_else(|| String::from(format_compact!("{fowl_name}-{}", slot_i + 1)));
        let mut template = cfg_me_spawn_template(
            spctx,
            idx,
            side,
            &cfg_template,
            cfg_unit_name.as_str(),
        )?;
        let unit = template.group.units()?.get(1)?;
        let airframe_type = unit.typ()?;
        unit.raw_remove("unitId")?;
        unit.raw_set("skill", "Excellent")?;
        unit.raw_set("onboard_num", random_onboard_num())?;
        match mode {
            AiAirPersistSpawn::PersistInAir => {
                prepare_me_in_air_group(&mut template, &dcs_name, flight_anchor)?;
                apply_me_in_air_unit(&unit, su, airframe_type.as_ref())?;
                log::info!(
                    "ai air {gid} unit {}: in-air resume baro {:.0}m pos [{:.0},{:.0}] fuel {:?}%",
                    su.name,
                    su.position.p.y,
                    su.pos.x,
                    su.pos.y,
                    su.fuel_fraction.map(|f| (f * 100.) as u32)
                );
            }
            AiAirPersistSpawn::NewDeploy | AiAirPersistSpawn::PersistGround => {
                let slot = hub
                    .slots
                    .get(slot_i)
                    .ok_or_else(|| anyhow!("no hub slot for unit {}", slot_i + 1))?;
                prepare_me_spawn_group(lua, &mut template, &dcs_name, hub, slot)?;
                apply_parking_to_template_unit(lua, db, &unit, slot, hub, slot_i)?;
                match slot.kind {
                    HubSlotKind::Helipad => log::info!(
                        "ai air {gid} unit {}: helipad {} linkUnit {} pos [{:.0},{:.0}] baro {:.0}m",
                        su.name,
                        slot.slot_id,
                        slot.slot_id,
                        slot.pos.x,
                        slot.pos.y,
                        hub_slot_baro_alt(lua, slot, hub.baro_alt).unwrap_or(hub.baro_alt),
                    ),
                    HubSlotKind::Parking => log::info!(
                        "ai air {gid} unit {}: parking {} baro {:.0}m anchor [{:.0},{:.0}]",
                        su.name,
                        slot.slot_id,
                        hub.baro_alt,
                        hub.anchor.x,
                        hub.anchor.y,
                    ),
                }
                if mode == AiAirPersistSpawn::PersistGround {
                    if let Some(frac) = su.fuel_fraction {
                        apply_me_template_fuel_fraction(&unit, airframe_type.as_ref(), frac)?;
                    }
                }
            }
        }
        unit.set_name(su.name.clone())?;
        let spawned = spctx.spawn(template).context("spawn ai air unit")?;
        if let crate::spawnctx::Spawned::Group(g) = &spawned {
            oids.push(g.object_id()?);
        }
        let _ = apply_fowl_air_options_to_name(lua, &dcs_name);
        dcs_names.push(dcs_name);
    }
    db.ephemeral.ai_air_dcs_oids.insert(gid, oids.clone());
    if let Some(first) = oids.first() {
        db.ephemeral.object_id_by_gid.insert(gid, first.clone());
        db.ephemeral.gid_by_object_id.insert(first.clone(), gid);
    }
    let group = group_mut!(db, gid)?;
    if let DeployKind::Action { ai_air, .. } = &mut group.origin {
        ai_air.dcs_spawn_names = dcs_names.clone();
    }
    let player_ucid = {
        let group = group!(db, gid)?;
        match &group.origin {
            DeployKind::Action { player, .. } => player.clone(),
            _ => None,
        }
    };
    match mode {
        AiAirPersistSpawn::NewDeploy => {
            match arm_flight_from_template(lua, db, spctx, idx, side, gid, &cfg_template, hub.oid) {
                Err(e) => log::warn!("ai air {gid}: initial armament from warehouse failed: {e:?}"),
                Ok(lines) => {
                    notify_partial_loadout(db, gid, player_ucid.as_ref(), &lines);
                    if let Some(ucid) = player_ucid.as_ref() {
                        let _ = panel_stores_report(db, lua, ucid, gid);
                    }
                }
            }
            push_bootstrap_missions(lua, db, spctx, gid, hub, &dcs_names)?;
        }
        AiAirPersistSpawn::PersistGround => {
            if let Some(frac) = dcs_names
                .first()
                .and_then(|_| unit_pool.first())
                .and_then(|(su, _)| su.fuel_fraction)
            {
                log::info!(
                    "ai air {gid}: ground persist resume (fuel {:.0}%)",
                    frac * 100.
                );
            }
            match arm_flight_from_template(lua, db, spctx, idx, side, gid, &cfg_template, hub.oid) {
                Err(e) => log::warn!("ai air {gid}: persist ground armament failed: {e:?}"),
                Ok(lines) => {
                    notify_partial_loadout(db, gid, player_ucid.as_ref(), &lines);
                    if let Some(ucid) = player_ucid.as_ref() {
                        let _ = panel_stores_report(db, lua, ucid, gid);
                    }
                }
            }
        }
        AiAirPersistSpawn::PersistInAir => {
            log::info!("ai air {gid}: in-air persist resume ({} unit(s))", dcs_names.len());
            let route = db.regenerate_ai_air_mission(lua, spctx, idx, gid, true)?;
            log::info!(
                "ai air {gid}: in-air resume mission pushed ({} wpts)",
                route.len()
            );
            db.ai_air_push_mission(spctx, gid, route, true)?;
            let group = group_mut!(db, gid)?;
            if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                set_phase(ai_air, AiAirPhase::OnMission);
            }
        }
    }
    db.ephemeral.dirty();
    if let Some(perf) = perf {
        record_perf(&mut perf.spawn, ts);
    }
    Ok(())
}
