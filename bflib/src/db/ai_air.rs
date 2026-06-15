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
    coalition::Side,
    controller::{
        ActionTyp, AltType, MissionPoint,
        PointType, Task, TurnMethod,
    },
    env::miz::{self, GroupInfo, GroupKind, MizIndex, UnitId},
    group::Group,
    land::Land,
    net::Ucid,
    object::DcsObject,
    perf::record_perf,
    unit::Unit,
    warehouse::LiquidType,
    LuaEnv, LuaVec2, MizLua, String, Vector2,
};
use fxhash::FxHashSet;
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

fn objectives_near(a: &Objective, b: &Objective, max_dist_sq: f64) -> bool {
    na::distance_squared(&a.zone.pos().into(), &b.zone.pos().into()) <= max_dist_sq
}

fn helipad_slots_for_heli_hub(
    lua: MizLua,
    db: &Db,
    hub: &Objective,
    side: Side,
) -> Result<Vec<HubSlot>> {
    let mut out = helipad_slots_in_zone(lua, db, hub, side)?;
    if !matches!(hub.kind, ObjectiveKind::Fob) {
        return Ok(out);
    }
    for (_, farp) in db.persisted.objectives.into_iter() {
        if farp.owner != side || !matches!(farp.kind, ObjectiveKind::Farp { .. }) {
            continue;
        }
        for slot in helipad_slots_in_zone(lua, db, farp, side)? {
            if out.iter().any(|s| s.slot_id == slot.slot_id) {
                continue;
            }
            if hub.zone.contains(slot.pos) || objectives_near(hub, farp, NEARBY_FARP_HELIPAD_MAX_DIST_SQ)
            {
                out.push(slot);
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
        AiPlaneKind::FixedWing if objective_has_airfield_hub(db, obj) => {
            if let Some(ab_oid) = db.ephemeral.airbase_by_oid.get(&obj.id) {
                if let Ok(spots) = parking_spots(lua, ab_oid) {
                    if let Some(d) = spots
                        .iter()
                        .map(|s| na::distance_squared(&s.pos.into(), &mark_pos.into()))
                        .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                    {
                        return Ok(d);
                    }
                }
                let ab_pos = airbase_point_pos(lua, ab_oid)?;
                return Ok(na::distance_squared(&ab_pos.into(), &mark_pos.into()));
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
    AwaitingLaunch,
    Departing,
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
}

pub(super) fn slot_claim_key(oid: ObjectiveId, slot: &HubSlot) -> (ObjectiveId, HubSlotKind, i64) {
    (oid, slot.kind, slot.slot_id)
}

pub(super) fn claimed_hub_slots(db: &Db) -> FxHashSet<(ObjectiveId, HubSlotKind, i64)> {
    let mut set = FxHashSet::default();
    for gid in db.persisted.actions.into_iter() {
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

fn hub_supports_ai_air(db: &Db, obj: &Objective, kind: AiPlaneKind) -> bool {
    match kind {
        AiPlaneKind::Helicopter => objective_is_heli_spawn_hub(db, obj),
        AiPlaneKind::FixedWing => {
            obj.is_airbase()
                || db
                    .ephemeral
                    .cfg
                    .extra_fixed_wing_objectives
                    .contains(&obj.name)
                || (matches!(obj.kind, ObjectiveKind::Logistics) && objective_has_airfield_hub(db, obj))
        }
    }
}

fn hub_candidate_filter<'a>(
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
        if mode == HubSelectMode::Spawn && (obj.captureable() || obj.threatened) {
            return None;
        }
        if hub_supports_ai_air(db, obj, kind) {
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
    let anchor = match slots.first() {
        Some(s) if s.kind == HubSlotKind::Helipad => s.pos,
        _ => zone_anchor,
    };
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
    let wh = db
        .ephemeral
        .airbase_by_oid
        .get(&obj.id)
        .and_then(|ab_oid| Airbase::get_instance(lua, ab_oid).ok())
        .and_then(|ab| ab.get_warehouse().ok());
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

fn apply_me_airfield_unit<'lua>(
    unit: &miz::Unit<'lua>,
    slot: &HubSlot,
    hub: &HubPick,
) -> Result<()> {
    unit.set_alt(hub.baro_alt)?;
    unit.raw_set("alt_type", AltType::BARO)?;
    unit.raw_set("speed", ME_PARKING_WAYPOINT_SPEED)?;
    unit.set_pos(hub.anchor)?;
    let ab_id = hub
        .airbase_id
        .ok_or_else(|| anyhow!("parking spawn missing airbase id"))?;
    unit.raw_set("airdromeId", ab_id.inner())?;
    unit.raw_set("parking", slot.slot_id.to_string())?;
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
) -> Result<()> {
    let alt = if let Some(a) = slot.baro_alt {
        a
    } else {
        Land::singleton(lua)?.get_height(LuaVec2(slot.pos))?
    };
    unit.set_alt(alt)?;
    unit.raw_set("alt_type", AltType::BARO)?;
    unit.raw_set("speed", 0f64)?;
    unit.set_pos(hub.anchor)?;
    unit.set_heading(slot.heading)?;
    unit.raw_set("psi", -slot.heading)?;
    unit.raw_set("manualHeading", true)?;
    unit.raw_set("helipadId", slot.slot_id)?;
    unit.raw_set("linkUnit", slot.slot_id)?;
    unit.raw_remove("airdromeId")?;
    unit.raw_remove("parking")?;
    unit.raw_remove("parking_id")?;
    Ok(())
}

pub(super) fn apply_parking_to_template_unit<'lua>(
    lua: MizLua<'lua>,
    unit: &miz::Unit<'lua>,
    slot: &HubSlot,
    hub: &HubPick,
) -> Result<()> {
    match slot.kind {
        HubSlotKind::Parking => apply_me_airfield_unit(unit, slot, hub),
        HubSlotKind::Helipad => apply_me_helipad_unit(lua, unit, slot, hub),
    }
}

fn warehouse_has_fuel(lua: MizLua, db: &Db, oid: ObjectiveId, types: &[String]) -> Result<bool> {
    let obj = objective!(db, oid)?;
    let Some(ab_oid) = db.ephemeral.airbase_by_oid.get(&oid) else {
        return Ok(true);
    };
    let wh = Airbase::get_instance(lua, ab_oid)?
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

fn spawn_liquid_type(airframe: &str) -> LiquidType {
    if airframe.contains("FW-190")
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
    if typ.contains("Mi-8")
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

fn template_unit_fuel_fraction(unit: &miz::Unit<'_>) -> f64 {
    if let Ok(payload) = unit.raw_get::<_, Table>("payload") {
        if let Ok(f) = payload.raw_get::<_, f64>("fuel") {
            if f > 0. {
                return f.clamp(0., 1.);
            }
        }
    }
    unit.raw_get::<_, f64>("fuel").unwrap_or(1.).clamp(0., 1.)
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

const TERM_RUNWAY: i64 = 16;
const TERM_HELIPAD: i64 = 40;
const TERM_HARDENED_SHELTER: i64 = 68;
const TERM_OPEN_SHELTER: i64 = 72;
const TERM_SMALL_SHELTER: i64 = 100;
const TERM_OPEN: i64 = 104;

fn parking_allowed_for_kind(term: Option<i64>, kind: AiPlaneKind) -> bool {
    let t = term.unwrap_or(TERM_OPEN);
    match kind {
        AiPlaneKind::FixedWing => matches!(t, TERM_OPEN | TERM_OPEN_SHELTER),
        AiPlaneKind::Helicopter => matches!(t, TERM_HELIPAD | TERM_OPEN | TERM_OPEN_SHELTER),
    }
}

fn parking_sort_key(term: Option<i64>, kind: AiPlaneKind) -> u8 {
    let t = term.unwrap_or(255);
    match kind {
        AiPlaneKind::FixedWing => match t {
            TERM_OPEN => 0,
            TERM_OPEN_SHELTER => 1,
            _ => 255,
        },
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
    Ok(out)
}

fn is_helipad_facility(typ: &str) -> bool {
    typ.contains("HELIPAD")
        || typ.contains("FARP")
        || typ.contains("Invisible")
        || typ.contains("FARPPAD")
}

fn helipad_slot_from_unit(lua: MizLua, unit: &SpawnedUnit, obj: &Objective) -> Option<HubSlot> {
    if unit.dead || !obj.zone.contains(unit.pos) {
        return None;
    }
    if !is_helipad_facility(unit.typ.0.as_str()) {
        return None;
    }
    let live = Unit::get_by_name(lua, unit.name.as_str()).ok()?;
    if !live.is_exist().ok()? {
        return None;
    }
    let slot_id = i64::from(live.id().ok()?.inner());
    Some(HubSlot {
        kind: HubSlotKind::Helipad,
        slot_id,
        pos: unit.pos,
        heading: unit.heading,
        baro_alt: None,
        term_type: None,
        heading_from_spot: true,
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
    let mut pool = match kind {
        AiPlaneKind::Helicopter => {
            let helis = helipad_slots_for_heli_hub(lua, db, obj, side)?;
            if !helis.is_empty() {
                helis
            } else if objective_has_airfield_hub(db, obj) {
                let Some(ab) = db.ephemeral.airbase_by_oid.get(&obj.id) else {
                    return Ok(vec![]);
                };
                parking_spots(lua, ab)?
            } else {
                vec![]
            }
        }
        AiPlaneKind::FixedWing if objective_has_airfield_hub(db, obj) => {
            let Some(ab) = db.ephemeral.airbase_by_oid.get(&obj.id) else {
                return Ok(vec![]);
            };
            parking_spots(lua, ab)?
        }
        _ => vec![],
    };
    pool.retain(|s| !claimed.contains(&(obj.id, s.kind, s.slot_id)));
    pool.retain(|s| match s.kind {
        HubSlotKind::Helipad => true,
        HubSlotKind::Parking => parking_allowed_for_kind(s.term_type, kind),
    });
    pool.sort_by_key(|s| parking_sort_key(s.term_type, kind));
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
    let mut candidates: Vec<(f64, &Objective)> = hub_candidate_filter(db, side, plane.kind, mode)
        .filter_map(|obj| {
            let dist_sq = hub_mark_dist_sq(lua, db, obj, side, plane.kind, mark_pos).ok()?;
            if mode == HubSelectMode::Spawn
                && dist_sq <= min_mark_hub_dist_sq(obj, plane.kind)
            {
                return None;
            }
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
            let airbase_id = db
                .ephemeral
                .airbase_by_oid
                .get(&obj.id)
                .and_then(|oid| Airbase::get_instance(lua, oid).ok())
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

/// ME TakeOffParking route: hub anchor + airdromeId, no parking on WP.
fn patch_me_airfield_route_lua<'lua>(
    lua: MizLua<'lua>,
    group: &miz::Group<'lua>,
    hub: &HubPick,
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
    p1.raw_set("timeReFuAr", Value::Nil)?;
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
    p1.raw_set("x", hub.anchor.x)?;
    p1.raw_set("y", hub.anchor.y)?;
    p1.raw_set("alt", hub.baro_alt)?;
    p1.raw_set("alt_type", "BARO")?;
    p1.raw_set("action", "From Parking Area")?;
    p1.raw_set("speed", 0f64)?;
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
    p1.raw_set("timeReFuAr", Value::Nil)?;
    points.raw_set(1, p1)?;
    route.raw_set("points", points)?;
    group.raw_set("route", route)?;
    Ok(())
}

fn random_onboard_num() -> String {
    String::from(format_compact!("{:03}", thread_rng().gen_range(10..=99)))
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
    match slot.kind {
        HubSlotKind::Parking => patch_me_airfield_route_lua(lua, &template.group, hub)?,
        HubSlotKind::Helipad => patch_me_helipad_route_lua(lua, &template.group, hub, slot)?,
    }
    template.group.raw_set("x", hub.anchor.x)?;
    template.group.raw_set("y", hub.anchor.y)?;
    Ok(())
}

/// DCS `timeReFuAr`: refuel and rearm on parking before takeoff.
const ME_PARKING_REFUEL_REARM: i64 = 3;

pub(super) fn bootstrap_route<'lua>(
    hub: &HubPick,
    slot: &HubSlot,
) -> Result<Vec<MissionPoint<'lua>>> {
    let (airdrome_id, helipad, link_unit, parking, speed) = match slot.kind {
        HubSlotKind::Parking => (hub.airbase_id, None, None, None, ME_PARKING_WAYPOINT_SPEED),
        HubSlotKind::Helipad => (
            None,
            Some(AirbaseId::from(slot.slot_id)),
            Some(UnitId::from(slot.slot_id)),
            None,
            0.,
        ),
    };
    Ok(vec![MissionPoint {
        typ: PointType::TakeOffParking,
        airdrome_id,
        helipad,
        time_re_fu_ar: Some(ME_PARKING_REFUEL_REARM),
        link_unit,
        action: Some(ActionTyp::Air(TurnMethod::FromParkingArea)),
        pos: LuaVec2(hub.anchor),
        alt: hub.baro_alt,
        alt_typ: Some(AltType::BARO),
        speed,
        speed_locked: Some(true),
        eta: None,
        eta_locked: Some(true),
        name: Some(String::from("bootstrap")),
        parking,
        task: Box::new(Task::ComboTask(vec![])),
    }])
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

pub(super) fn land_at_hub_route<'lua>(
    hub: &HubPick,
    slot: &HubSlot,
    alt: f64,
    alt_typ: AltType,
    speed: f64,
) -> Vec<MissionPoint<'lua>> {
    let (airdrome_id, helipad, link_unit) = match slot.kind {
        HubSlotKind::Parking => (hub.airbase_id, None, None),
        HubSlotKind::Helipad => (
            None,
            Some(AirbaseId::from(slot.slot_id)),
            Some(UnitId::from(slot.slot_id)),
        ),
    };
    vec![MissionPoint {
        typ: PointType::Land,
        airdrome_id,
        helipad,
        time_re_fu_ar: None,
        link_unit,
        action: Some(ActionTyp::Air(TurnMethod::FlyOverPoint)),
        pos: LuaVec2(hub.anchor),
        alt,
        alt_typ: Some(alt_typ),
        speed,
        speed_locked: None,
        eta: None,
        eta_locked: None,
        name: Some(String::from("rtb")),
        parking: None,
        task: Box::new(Task::ComboTask(vec![])),
    }]
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

/// DCS `Unit.getFuel()` fraction (0..1).
const BINGO_FUEL_FRAC: f32 = 0.25;

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

pub(super) fn panel_loadout_report(
    db: &mut Db,
    ucid: &Ucid,
    gid: GroupId,
    lines: &[LoadoutLine],
) {
    let full = lines.iter().all(|l| l.loaded >= l.requested);
    if full {
        return;
    }
    let mut parts: Vec<CompactString> = Vec::new();
    for l in lines {
        if l.loaded < l.requested {
            parts.push(format_compact!("{} {}/{}", l.name, l.loaded, l.requested));
        }
    }
    let msg = format_compact!("{gid} partial loadout: {}", parts.join(", "));
    db.ephemeral.panel_to_player(&db.persisted, 15, ucid, msg);
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
) -> Result<Vec<LoadoutLine>> {
    let tpl = spctx.get_template(idx, GroupKind::Any, side, template_name)?;
    let Some(ab_oid) = db.ephemeral.airbase_by_oid.get(&hub) else {
        return Ok(vec![]);
    };
    let wh = Airbase::get_instance(lua, ab_oid)?.get_warehouse()?;
    let obj = objective_mut!(db, hub)?;
    let mut lines = Vec::new();
    let units = tpl.group.units()?;
    let u = units.get(1)?;
    let payload: Table = u
        .raw_get("payload")
        .unwrap_or_else(|_| lua.inner().create_table().unwrap());
    let pylons: Table = payload
        .raw_get("pylons")
        .unwrap_or_else(|_| lua.inner().create_table().unwrap());
    let pairs = pylons.clone();
    for pair in pairs.pairs::<Value, Table>() {
        let (_, pylon) = pair?;
        let clsid: String = pylon.raw_get("CLSID").or_else(|_| pylon.raw_get("clsid"))?;
        let count: u32 = pylon.raw_get("count").unwrap_or(1);
        let name: String = pylon
            .raw_get::<_, Table>("descriptor")
            .and_then(|d| d.raw_get("displayName"))
            .unwrap_or_else(|_| clsid.clone());
        let wh_count = wh.get_item_count(clsid.clone()).unwrap_or(0);
        let load = count.min(wh_count);
        if load > 0 {
            let _ = wh.remove_item(clsid.clone(), load);
            if let Some(inv) = obj.warehouse.equipment.get_mut_cow(clsid.as_str()) {
                inv.stored = wh.get_item_count(clsid.clone())?;
            }
        }
        lines.push(LoadoutLine {
            name,
            requested: count,
            loaded: load,
        });
    }
    if let Some(ucid) = player {
        panel_loadout_report(db, ucid, gid, &lines);
    }
    Ok(lines)
}

pub(super) fn hub_airbase_id(db: &Db, lua: MizLua, oid: ObjectiveId) -> Result<Option<AirbaseId>> {
    let Some(ab) = db.ephemeral.airbase_by_oid.get(&oid) else {
        return Ok(None);
    };
    Ok(Some(Airbase::get_instance(lua, ab)?.get_id()?))
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
    } else if t.contains("CRUISE") {
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

/// Phase tick for persisted AI air action groups.
pub(super) fn advance_ai_air(
    lua: MizLua,
    db: &mut Db,
    spctx: &SpawnCtx,
    idx: &MizIndex,
    gid: GroupId,
    now: DateTime<Utc>,
) -> Result<()> {
    let (dcs_names, side, template_name, player, hub, phase) = {
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
        (
            dcs_spawn_names_for(db, gid)?,
            group.side,
            ai_air_template_name(db, gid),
            player.clone(),
            ai_air.hub,
            ai_air.phase,
        )
    };
    let Some(template_name) = template_name else {
        return Ok(());
    };
    let Some(hub) = hub else {
        return Ok(());
    };
    if !db.ephemeral.object_id_by_gid.contains_key(&gid)
        && !db.ephemeral.ai_air_dcs_oids.contains_key(&gid)
    {
        return Ok(());
    }
    if !flight_any_alive(lua, &dcs_names) {
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
    let hub_pick = finish_hub_pick(lua, db, hub, hub_slots.clone(), airbase_id)?;
    let on_ground = flight_on_ground(lua, &dcs_names).unwrap_or(false);
    let in_air = flight_any_in_air(lua, &dcs_names).unwrap_or(false);
    let pos = flight_center_pos(lua, &dcs_names).unwrap_or(hub_pos);

    match phase {
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
                    let route = bootstrap_route(&hub_pick, slot)?;
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
                let route = db.regenerate_ai_air_mission(lua, spctx, idx, gid)?;
                db.ai_air_push_mission(spctx, gid, route, true)?;
            } else if {
                let group = group!(db, gid)?;
                let DeployKind::Action { ai_air, .. } = &group.origin else {
                    return Ok(());
                };
                let elapsed = now - ai_air.phase_since;
                if in_air && !grounded {
                    elapsed > Duration::seconds(5)
                } else {
                    elapsed > Duration::seconds(120)
                }
            } {
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
                    let route = bootstrap_route(&hub_pick, slot)?;
                    db.ai_air_push_mission_to_name(spctx, dcs_name, route, false)?;
                }
            }
        }
        AiAirPhase::OnMission => {
            if in_air {
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
                            },
                        )?;
                    }
                }
            }
        }
        AiAirPhase::RtbInbound => {
            if !on_ground {
                return Ok(());
            }
            let at_hub = near_point(pos, hub_pick.anchor, 3000.)
                || near_point(pos, hub_pos, 3000.)
                || hub_slots
                    .iter()
                    .any(|s| near_point(pos, s.pos, 600.));
            if at_hub {
                log::info!("ai air {gid}: on ground at hub -> servicing");
                let group = group_mut!(db, gid)?;
                if let DeployKind::Action { ai_air, .. } = &mut group.origin {
                    set_phase(ai_air, AiAirPhase::Servicing);
                }
            }
        }
        AiAirPhase::Servicing => {
            if now - {
                let group = group!(db, gid)?;
                let DeployKind::Action { ai_air, .. } = &group.origin else {
                    return Ok(());
                };
                ai_air.phase_since
            } < Duration::seconds(15)
            {
                return Ok(());
            }
            let _ = try_rearm_from_template(
                lua,
                db,
                spctx,
                idx,
                side,
                gid,
                &template_name,
                hub,
                player.as_ref(),
            );
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
                return Ok(());
            }
            log::info!("ai air {gid}: servicing done -> depart (bootstrap takeoff)");
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
                    .ok_or_else(|| anyhow!("no hub slot for depart"))?;
                let route = bootstrap_route(&hub_pick, slot)?;
                db.ai_air_push_mission_to_name(spctx, dcs_name, route, false)?;
            }
        }
        AiAirPhase::AwaitingLaunch => (),
        AiAirPhase::Departing => {
            if in_air {
                let snap = {
                    let group = group_mut!(db, gid)?;
                    let DeployKind::Action { ai_air, .. } = &mut group.origin else {
                        return Ok(());
                    };
                    let snap = ai_air.active_mission.clone();
                    set_phase(ai_air, AiAirPhase::OnMission);
                    snap
                };
                let route = db.regenerate_ai_air_mission(lua, spctx, idx, gid)?;
                db.ai_air_push_mission(spctx, gid, route, true)?;
            }
        }
        AiAirPhase::TaxiToParking | AiAirPhase::Legacy => (),
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
            let airbase_id = hub_airbase_id(db, lua, h)?;
            let slots = {
                let group = group!(db, req.group)?;
                let n = group.units.len().max(1);
                let claimed = claimed_hub_slots(db);
                free_slots_at_hub(lua, db, obj, side, plane.kind, n, &claimed)?
            };
            if slots.len() < group!(db, req.group)?.units.len().max(1) {
                bail!("no free slots at {}", obj.name);
            }
            finish_hub_pick(lua, db, h, slots, airbase_id)?
        }
        None => {
            let n = group!(db, req.group)?.units.len().max(1);
            select_hub_for_ai(
                lua,
                db,
                spctx,
                idx,
                side,
                pos,
                &plane,
                n,
                HubSelectMode::Landing,
            )?
        }
    };
    let rtb_pos = hub_zone_pos(db, hub.oid)?;
    let snap = {
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
        set_phase(ai_air, AiAirPhase::RtbInbound);
        if !req.preserve_mission_kind {
            spec.kind = ActionKind::Rtb;
        }
        ai_air.active_mission.clone()
    };
    let rtb_slot = hub
        .slots
        .first()
        .ok_or_else(|| anyhow!("hub has no landing slot"))?;
    let route = land_at_hub_route(&hub, rtb_slot, snap.alt, snap.alt_typ.clone(), snap.speed);
    db.ai_air_push_mission(spctx, req.group, route, true)?;
    Ok(())
}

pub(super) fn issue_start(
    db: &mut Db,
    lua: MizLua,
    spctx: &SpawnCtx,
    gid: GroupId,
    side: Side,
) -> Result<()> {
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
    for (i, dcs_name) in dcs_names.iter().enumerate() {
        let slot = slots
            .get(i)
            .or(slots.first())
            .ok_or_else(|| anyhow!("no parking slot"))?;
        let route = bootstrap_route(&hub_pick, slot)?;
        db.ai_air_push_mission_to_name(spctx, dcs_name, route, false)?;
    }
    Ok(())
}

pub(super) fn spawn_ai_air_group<'lua>(
    perf: &mut bfprotocols::perf::PerfInner,
    db: &mut Db,
    spctx: &SpawnCtx<'lua>,
    idx: &MizIndex,
    gid: GroupId,
    hub: &HubPick,
) -> Result<()> {
    let ts = Utc::now();
    let lua = spctx.lua();
    let (fowl_name, side, cfg_template, existing_dcs_names) = {
        let group = group!(db, gid)?;
        let existing = match &group.origin {
            DeployKind::Action { ai_air, .. } => ai_air.dcs_spawn_names.clone(),
            _ => vec![],
        };
        (
            group.name.clone(),
            group.side,
            group.template_name.clone(),
            existing,
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
    for (slot_i, (su, cfg_unit_name)) in unit_pool.iter().enumerate() {
        let slot = hub
            .slots
            .get(slot_i)
            .ok_or_else(|| anyhow!("no hub slot for unit {}", slot_i + 1))?;
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
        prepare_me_spawn_group(lua, &mut template, &dcs_name, hub, slot)?;
        let unit = template.group.units()?.get(1)?;
        unit.raw_remove("unitId")?;
        unit.raw_set("skill", "Excellent")?;
        unit.raw_set("onboard_num", random_onboard_num())?;
        apply_parking_to_template_unit(lua, &unit, slot, hub)?;
        log::info!(
            "ai air {gid} unit {}: parking {} baro {:.0}m anchor [{:.0},{:.0}]",
            su.name,
            slot.slot_id,
            hub.baro_alt,
            hub.anchor.x,
            hub.anchor.y,
        );
        unit.set_name(su.name.clone())?;
        let spawned = spctx.spawn(template).context("spawn ai air unit")?;
        if let crate::spawnctx::Spawned::Group(g) = &spawned {
            oids.push(g.object_id()?);
        }
        dcs_names.push(dcs_name);
    }
    db.ephemeral.ai_air_dcs_oids.insert(gid, oids.clone());
    if let Some(first) = oids.first() {
        db.ephemeral.object_id_by_gid.insert(gid, first.clone());
        db.ephemeral.gid_by_object_id.insert(first.clone(), gid);
    }
    let group = group_mut!(db, gid)?;
    if let DeployKind::Action { ai_air, .. } = &mut group.origin {
        ai_air.dcs_spawn_names = dcs_names;
    }
    db.ephemeral.dirty();
    record_perf(&mut perf.spawn, ts);
    Ok(())
}
