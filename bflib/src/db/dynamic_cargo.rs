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
    group::DeployKind,
    logistics::{dcs_liquid_kg_to_fowl_tons, objective_liquids_stored_as_tons},
    Db, MapM, MapS,
};
use anyhow::{anyhow, bail, Context, Result};
use bfprotocols::cfg::DynamicCargoDeliveryCfg;
use bfprotocols::db::objective::{ObjectiveId, ObjectiveKind};
use chrono::prelude::*;
use compact_str::format_compact;
use dcso3::{
    azumith2d_to,
    coalition::{Coalition, Side, Static},
    country::Country,
    env::miz::Miz,
    land::Land,
    net::Ucid,
    object::{DcsObject, DcsOid, ObjectCategory},
    radians_to_degrees,
    static_object::StaticObject,
    unit::{ClassUnit, Unit},
    warehouse::{LiquidType, Warehouse},
    world::{SearchVolume, World},
    LuaEnv, LuaVec2, LuaVec3, MizLua, String, Vector2, Vector3,
};
use fxhash::{FxHashMap, FxHashSet};
use log::{info, warn};
use mlua::{FromLua, Value};
use serde_derive::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DynamicCargoCrate {
    pub index: u64,
    pub name: String,
    pub side: Side,
    pub spawner: Ucid,
    pub source: ObjectiveId,
    pub type_name: String,
    pub x: f64,
    pub y: f64,
    pub alt: f64,
    #[serde(default)]
    pub equipment: MapS<String, u32>,
    #[serde(default)]
    pub liquids: MapS<LiquidType, u32>,
    /// Source warehouse was debited for this crate (D1 checkout).
    #[serde(default = "default_true")]
    pub source_checked_out: bool,
    /// Last known ED cargo mass (kg); used when the static is already gone (absorb).
    #[serde(default)]
    pub last_weight_kg: f64,
    /// Last player who had this crate aboard (F8 / sling); deliverer on DCS absorb.
    #[serde(default)]
    pub last_carrier: Option<Ucid>,
    /// Left a transport while airborne / freefall (CDS airdrop). No prune-as-absorb stats.
    #[serde(default)]
    pub air_dropped: bool,
    /// After parachute landing: respawned once with `canCargo` so DCS sling/F8 can see it.
    #[serde(default)]
    pub airdrop_rehooked: bool,
}

fn default_true() -> bool {
    true
}

/// AGL above this while not aboard ⇒ mark `air_dropped`.
const AIR_DROP_AGL_M: f64 = 40.0;
/// AGL at/below this after air drop ⇒ grounded; eligible for canCargo rehook.
const GROUND_AGL_M: f64 = 8.0;
/// Wait for parachute / API gap before treating airdrop as lost (no stock restore).
const AIRDROP_CHUTE_GRACE: chrono::Duration = chrono::Duration::seconds(180);
/// Slow-tick misses of `get_by_name` before treating crate as gone (parachute name glitch).
const PRUNE_MISS_LIMIT: u8 = 4;
/// One `searchObjects` after airdrop: DCS chute copies + impact pos.
const AIRDROP_ORPHAN_RADIUS_M: f64 = 150.0;
/// Crash dump / bay cargo near the wreck (same as Fowl `crate_static_ed_carrier`).
const AIRFRAME_LOSS_CARGO_RADIUS_M: f64 = 80.0;
/// DCS may dump bay cargo a few seconds after Crash / UnitLost.
const AIRFRAME_LOSS_CARGO_PURGE_DELAY: chrono::Duration = chrono::Duration::seconds(3);

fn cargo_agl_m(lua: MizLua, x: f64, y: f64, alt: f64) -> Option<f64> {
    let land = Land::singleton(lua).ok()?;
    let ground = land.get_height(LuaVec2(Vector2::new(x, y))).ok()?;
    Some(alt - ground)
}

#[derive(Clone)]
struct NearbyCargoHit {
    name: String,
    x: f64,
    y: f64,
    alt: f64,
}

fn collect_nearby_cargo(lua: MizLua, x: f64, y: f64, radius: f64) -> Vec<NearbyCargoHit> {
    let hits = Arc::new(Mutex::new(Vec::new()));
    let Ok(world) = World::singleton(lua) else {
        return Vec::new();
    };
    let alt = Land::singleton(lua)
        .ok()
        .and_then(|l| l.get_height(LuaVec2(Vector2::new(x, y))).ok())
        .unwrap_or(0.);
    let vol = SearchVolume::Sphere {
        point: LuaVec3(Vector3::new(x, alt + 2., y)),
        radius,
    };
    let acc = hits.clone();
    let _ = world.search_objects(
        ObjectCategory::Cargo,
        vol,
        mlua::Value::Nil,
        move |_, obj, _| {
            let Ok(n) = obj.get_name() else {
                return Ok(true);
            };
            let Ok(pt) = obj.get_point() else {
                return Ok(true);
            };
            acc.lock().unwrap().push(NearbyCargoHit {
                name: n,
                x: pt.0.x,
                y: pt.0.z,
                alt: pt.0.y,
            });
            Ok(true)
        },
    );
    hits.lock().unwrap().clone()
}

impl DynamicCargoCrate {
    fn pos(&self) -> Vector2 {
        Vector2::new(self.x, self.y)
    }
}

#[derive(Debug, Clone, Default)]
pub struct ToStockResult {
    pub crates: u32,
    pub destination: String,
}

#[derive(Debug, Clone)]
pub struct NearbyDynamicCargo {
    pub type_name: String,
    pub name: String,
    pub distance: f64,
    pub heading: f64,
    pub loaded: bool,
    pub damaged: bool,
}

/// MOOSE `DYNAMICCARGO.AircraftDimensions` (half-box around unit origin).
struct DynamicCargoAircraftDim {
    width: f64,
    /// Forward half-extent from unit origin to nose (MOOSE cargo bay OBB).
    length: f64,
    ropelength: f64,
    /// Unit origin → cabin reference along +forward (0 = cockpit at origin).
    cabin_forward_m: f64,
}

/// Meters ahead of cabin reference for all Fowl crate nose-line placement.
const FOWL_CRATE_AHEAD_OF_CABIN_M: f64 = 15.;

fn dynamic_cargo_aircraft_dim(type_name: &str) -> Option<DynamicCargoAircraftDim> {
    match type_name {
        "CH-47Fbl1" => Some(DynamicCargoAircraftDim {
            width: 4.,
            length: 11.,
            ropelength: 30.,
            cabin_forward_m: 0.,
        }),
        "Mi-8MTV2" | "Mi-8MT" => Some(DynamicCargoAircraftDim {
            width: 6.,
            length: 15.,
            ropelength: 30.,
            cabin_forward_m: 0.,
        }),
        "UH-1H" => Some(DynamicCargoAircraftDim {
            width: 2.5,
            length: 5.,
            ropelength: 30.,
            cabin_forward_m: 0.,
        }),
        "Mi-24P" => Some(DynamicCargoAircraftDim {
            width: 3.,
            length: 6.,
            ropelength: 30.,
            cabin_forward_m: 0.,
        }),
        "C-130J-30" => Some(DynamicCargoAircraftDim {
            width: 4.,
            length: 35.,
            ropelength: 0.,
            // Origin amidships; cockpit well forward of CG.
            cabin_forward_m: 18.,
        }),
        _ => None,
    }
}

/// Cabin reference along +forward from DCS unit origin.
fn fowl_crate_cabin_forward_m(type_name: &str) -> f64 {
    dynamic_cargo_aircraft_dim(type_name)
        .map(|d| d.cabin_forward_m)
        .unwrap_or(0.)
}

/// Nose-line distance from unit origin: cabin reference + fixed ahead.
pub(crate) fn fowl_crate_nose_m(type_name: &str) -> f64 {
    fowl_crate_cabin_forward_m(type_name) + FOWL_CRATE_AHEAD_OF_CABIN_M
}

pub(crate) fn fowl_crate_spawn_nose_m(type_name: &str) -> f64 {
    fowl_crate_nose_m(type_name)
}

pub(crate) fn fowl_crate_nose_distance_m(type_name: &str) -> f64 {
    fowl_crate_nose_m(type_name)
}

/// Airframes that use ED F8 / bay for Fowl crates when listed in CFG `shared_ed_cargo_airframes`.
pub fn uses_shared_ed_cargo_bay(cfg: &DynamicCargoDeliveryCfg, typ: &str) -> bool {
    cfg.enabled && cfg.shared_ed_cargo_airframes.contains(typ)
}

/// Airframes that can F8 or sling ED canCargo crates (geometry / weight detection).
pub fn is_ed_cargo_transport(typ: &str) -> bool {
    dynamic_cargo_aircraft_dim(typ).is_some()
}

impl Db {
    pub fn dynamic_cargo_enabled(&self) -> bool {
        self.ephemeral.cfg.dynamic_cargo_delivery.enabled
    }

    fn dynamic_cargo_cfg(&self) -> &DynamicCargoDeliveryCfg {
        &self.ephemeral.cfg.dynamic_cargo_delivery
    }

    pub fn clear_dynamic_cargo_if_disabled(&mut self) {
        if !self.dynamic_cargo_enabled() {
            if self.persisted.dynamic_cargo_crates.len() > 0
                || self.persisted.dynamic_cargo_next_index.len() > 0
            {
                self.persisted.dynamic_cargo_crates = MapM::default();
                self.persisted.dynamic_cargo_next_index = MapS::default();
                self.ephemeral.dynamic_cargo_registered_at.clear();
                self.ephemeral.dynamic_cargo_miss_count.clear();
                self.ephemeral.dynamic_cargo_rehook_in_progress.clear();
                self.ephemeral.dynamic_cargo_air_dropped_at.clear();
                self.ephemeral.dirty();
            }
        }
    }

    pub fn register_dynamic_cargo_static(&mut self, lua: MizLua, st: &StaticObject) -> Result<()> {
        if !self.dynamic_cargo_enabled() {
            return Ok(());
        }
        let obj = st.as_object().context("dynamic cargo as object")?;
        if obj.get_category()? != ObjectCategory::Cargo {
            return Ok(());
        }
        let name = st.get_name()?;
        // Fowl R/BCRATE (canCargo) must not enter the warehouse dynamic-cargo registry.
        if let Some(uid) = self.persisted.units_by_name.get(name.as_str()) {
            if let Some(unit) = self.persisted.units.get(uid) {
                if self.persisted.crates.contains(&unit.group) {
                    return Ok(());
                }
            }
        }
        if self.persisted.dynamic_cargo_crates.get(name.as_str()).is_some() {
            return Ok(());
        }
        let side = st.get_coalition()?;
        if side != Side::Blue && side != Side::Red {
            return Ok(());
        }
        let point = st.get_point()?.0;
        let pos = Vector2::new(point.x, point.z);
        let type_name = st.get_type_name().unwrap_or_else(|_| String::from("cargo"));
        let (spawner, source) = self.resolve_dynamic_cargo_spawner_and_source(lua, side, pos, &name)?;
        let (equipment, liquids) = snapshot_cargo_warehouse(lua, st)?;
        let snap_empty = equipment.len() == 0 && liquids.len() == 0;
        self.debit_source_for_dynamic_cargo_checkout(source, &equipment, &liquids)?;
        self.insert_dynamic_cargo_crate(
            lua,
            DynamicCargoCrate {
                index: 0,
                name: name.clone(),
                side,
                spawner,
                source,
                type_name,
                x: pos.x,
                y: pos.y,
                alt: point.y,
                equipment,
                liquids,
                source_checked_out: true,
                last_weight_kg: st.get_cargo_weight().unwrap_or(0.),
                last_carrier: None,
                air_dropped: false,
                airdrop_rehooked: false,
            },
        )?;
        // Birth often registers empty; SyncTo now would restore DCS stock (OLO exploit).
        // Fill-delta in refresh_dynamic_cargo_snapshots debits + SyncTo when content appears.
        if snap_empty {
            info!(
                "dynamic cargo registered {name}: empty warehouse snapshot, defer SyncTo until fill"
            );
        } else if let Err(e) = self.sync_objective_to_warehouse(lua, source, false) {
            warn!("dynamic cargo checkout SyncTo {source:?}: {e:?}");
        }
        Ok(())
    }

    pub(super) fn objective_has_open_dynamic_cargo_checkout(&self, oid: ObjectiveId) -> bool {
        self.persisted
            .dynamic_cargo_crates
            .into_iter()
            .any(|(_, c)| c.source == oid && c.source_checked_out)
    }

    fn resolve_dynamic_cargo_spawner_and_source(
        &self,
        _lua: MizLua,
        side: Side,
        pos: Vector2,
        cargo_name: &str,
    ) -> Result<(Ucid, ObjectiveId)> {
        let name_owner = cargo_name.split('|').next().unwrap_or("");
        let mut best: Option<(Ucid, ObjectiveId, f64)> = None;
        for (ucid, player, inst) in self.instanced_players() {
            if player.side != side {
                continue;
            }
            let ppos = Vector2::new(inst.position.p.0.x, inst.position.p.0.z);
            let dist = (ppos - pos).magnitude();
            let name_match = !name_owner.is_empty()
                && (player.name.as_str() == name_owner
                    || player.alts.into_iter().any(|a| a.as_str() == name_owner));
            let score = if name_match { dist * 0.01 } else { dist };
            let source = self
                .objective_containing(side, ppos)
                .or_else(|| self.objective_containing(side, pos))
                .or_else(|| {
                    inst.landed_at_objective
                        .and_then(|oid| self.persisted.objectives.get(&oid).map(|_| oid))
                });
            let Some(source) = source else {
                continue;
            };
            match best {
                None => best = Some((*ucid, source, score)),
                Some((_, _, best_score)) if score < best_score => {
                    best = Some((*ucid, source, score))
                }
                _ => {}
            }
        }
        if let Some((ucid, source, _)) = best {
            return Ok((ucid, source));
        }
        let source = self
            .objective_containing(side, pos)
            .ok_or_else(|| anyhow!("dynamic cargo {cargo_name}: no friendly objective nearby"))?;
        let ucid = self
            .persisted
            .players
            .into_iter()
            .find(|(_, p)| p.side == side)
            .map(|(u, _)| *u)
            .ok_or_else(|| anyhow!("dynamic cargo {cargo_name}: no player on {side:?}"))?;
        Ok((ucid, source))
    }

    fn objective_containing(&self, side: Side, pos: Vector2) -> Option<ObjectiveId> {
        self.persisted.objectives.into_iter().find_map(|(oid, obj)| {
            if obj.owner == side
                && obj.logi() > 0
                && !matches!(obj.kind, ObjectiveKind::Production)
                && obj.zone.contains(pos)
            {
                Some(*oid)
            } else {
                None
            }
        })
    }

    /// Dest for DCS absorb: last crate pos, else carrier / spawner objective (≠ source).
    fn resolve_dynamic_cargo_absorb_dest(
        &self,
        entry: &DynamicCargoCrate,
    ) -> Option<ObjectiveId> {
        if let Some(pad) = self.objective_containing(entry.side, entry.pos()) {
            if pad != entry.source {
                return Some(pad);
            }
        }
        let carrier = entry.last_carrier.unwrap_or(entry.spawner);
        self.player_cross_objective_for_absorb(entry.side, &carrier, entry.source)
    }

    fn player_cross_objective_for_absorb(
        &self,
        side: Side,
        ucid: &Ucid,
        source: ObjectiveId,
    ) -> Option<ObjectiveId> {
        let player = self.persisted.players.get(ucid)?;
        if player.side != side {
            return None;
        }
        let Some((_, Some(inst))) = player.current_slot.as_ref() else {
            return None;
        };
        if let Some(oid) = inst.landed_at_objective {
            if oid != source {
                let ok = self.persisted.objectives.get(&oid).is_some_and(|o| {
                    o.owner == side && o.logi() > 0 && !matches!(o.kind, ObjectiveKind::Production)
                });
                if ok {
                    return Some(oid);
                }
            }
        }
        let p = Vector2::new(inst.position.p.0.x, inst.position.p.0.z);
        let pad = self.objective_containing(side, p)?;
        (pad != source).then_some(pad)
    }

    fn player_instance_world_pos(&self, ucid: &Ucid) -> Option<(f64, f64, f64)> {
        let player = self.persisted.players.get(ucid)?;
        let (_, Some(inst)) = player.current_slot.as_ref()? else {
            return None;
        };
        Some((inst.position.p.0.x, inst.position.p.0.z, inst.position.p.0.y))
    }

    /// Checked-out equipment qty still in registered crates from this source.
    pub(super) fn dynamic_cargo_equipment_reserved(&self, oid: ObjectiveId, item: &str) -> u32 {
        self.persisted
            .dynamic_cargo_crates
            .into_iter()
            .filter(|(_, c)| c.source == oid && c.source_checked_out)
            .map(|(_, c)| c.equipment.get(item).copied().unwrap_or(0))
            .fold(0u32, |a, b| a.saturating_add(b))
    }

    /// Checked-out liquid qty (Fowl warehouse units) from crates sourced at `oid`.
    pub(super) fn dynamic_cargo_liquid_reserved(&self, oid: ObjectiveId, liq: &LiquidType) -> u32 {
        self.persisted
            .dynamic_cargo_crates
            .into_iter()
            .filter(|(_, c)| c.source == oid && c.source_checked_out)
            .filter_map(|(_, c)| c.liquids.get(liq).copied())
            .map(|kg| self.dynamic_cargo_liquid_qty_fowl(oid, kg))
            .fold(0u32, |a, b| a.saturating_add(b))
    }

    pub(super) fn dynamic_cargo_demand_room(stored: u32, capacity: u32, reserved: u32) -> u32 {
        let max_stored = capacity.saturating_sub(reserved);
        if stored <= max_stored {
            max_stored - stored
        } else {
            0
        }
    }

    fn debit_source_for_dynamic_cargo_checkout(
        &mut self,
        source: ObjectiveId,
        equipment: &MapS<String, u32>,
        liquids: &MapS<LiquidType, u32>,
    ) -> Result<()> {
        let liquids_tons = {
            let export = self.ephemeral.fowl_miz_export.as_ref();
            let obj = self
                .persisted
                .objectives
                .get(&source)
                .ok_or_else(|| anyhow!("dynamic cargo checkout: missing source {source:?}"))?;
            objective_liquids_stored_as_tons(export, obj)
        };
        let obj = self
            .persisted
            .objectives
            .get_mut_cow(&source)
            .ok_or_else(|| anyhow!("dynamic cargo checkout: missing source {source:?}"))?;
        for (item, qty) in equipment {
            if *qty == 0 {
                continue;
            }
            if let Some(inv) = obj.warehouse.equipment.get_mut_cow(item) {
                inv.stored = inv.stored.saturating_sub(*qty);
            }
        }
        for (liq, qty_kg) in liquids {
            if *qty_kg == 0 {
                continue;
            }
            let sub = if liquids_tons {
                dcs_liquid_kg_to_fowl_tons(*qty_kg).max(if *qty_kg > 0 { 1 } else { 0 })
            } else {
                *qty_kg
            };
            if sub == 0 {
                continue;
            }
            if let Some(inv) = obj.warehouse.liquids.get_mut_cow(liq) {
                inv.stored = inv.stored.saturating_sub(sub);
            }
        }
        self.ephemeral.dirty();
        Ok(())
    }

    /// After SyncFrom: do not restore checked-out crate contents into Fowl stock.
    pub(super) fn clamp_dynamic_cargo_checkout_obj(
        obj: &mut super::objective::Objective,
        oid: ObjectiveId,
        crates: &MapM<String, DynamicCargoCrate>,
        export: &bfprotocols::fowl_miz_export::FowlMizExport,
    ) -> bool {
        let tons = objective_liquids_stored_as_tons(export, obj);
        let mut eq: FxHashMap<String, u32> = FxHashMap::default();
        let mut liq: FxHashMap<LiquidType, u32> = FxHashMap::default();
        for (_, c) in crates {
            if c.source != oid || !c.source_checked_out {
                continue;
            }
            for (name, qty) in &c.equipment {
                *eq.entry(name.clone()).or_default() =
                    eq.get(name).copied().unwrap_or(0).saturating_add(*qty);
            }
            for (typ, qty_kg) in &c.liquids {
                let add = if *qty_kg == 0 {
                    0
                } else if tons {
                    dcs_liquid_kg_to_fowl_tons(*qty_kg).max(1)
                } else {
                    *qty_kg
                };
                *liq.entry(*typ).or_default() =
                    liq.get(typ).copied().unwrap_or(0).saturating_add(add);
            }
        }
        let mut dirty = false;
        for (name, reserved) in eq {
            if let Some(inv) = obj.warehouse.equipment.get_mut_cow(&name) {
                let max_stored = inv.capacity.saturating_sub(reserved);
                if inv.stored > max_stored {
                    inv.stored = max_stored;
                    dirty = true;
                }
            }
        }
        for (typ, reserved) in liq {
            if let Some(inv) = obj.warehouse.liquids.get_mut_cow(&typ) {
                let max_stored = inv.capacity.saturating_sub(reserved);
                if inv.stored > max_stored {
                    inv.stored = max_stored;
                    dirty = true;
                }
            }
        }
        dirty
    }

    pub(super) fn clamp_dynamic_cargo_checkout(&mut self, oid: ObjectiveId) {
        if !self.dynamic_cargo_enabled() {
            return;
        }
        let dirty = {
            let Some(obj) = self.persisted.objectives.get_mut_cow(&oid) else {
                return;
            };
            Self::clamp_dynamic_cargo_checkout_obj(
                obj,
                oid,
                &self.persisted.dynamic_cargo_crates,
                self.ephemeral.fowl_miz_export.as_ref(),
            )
        };
        if dirty {
            self.ephemeral.dirty();
        }
    }

    fn dynamic_cargo_liquid_qty_fowl(&self, oid: ObjectiveId, qty_kg: u32) -> u32 {
        if qty_kg == 0 {
            return 0;
        }
        let tons = self
            .persisted
            .objectives
            .get(&oid)
            .map(|obj| {
                objective_liquids_stored_as_tons(self.ephemeral.fowl_miz_export.as_ref(), obj)
            })
            .unwrap_or(false);
        if tons {
            dcs_liquid_kg_to_fowl_tons(qty_kg).max(1)
        } else {
            qty_kg
        }
    }

    fn insert_dynamic_cargo_crate(&mut self, lua: MizLua, mut entry: DynamicCargoCrate) -> Result<()> {
        let cfg = self.dynamic_cargo_cfg();
        let max = cfg.maximum_dynamic_crates_per_coalition.max(1) as usize;
        let side = entry.side;
        let next = self
            .persisted
            .dynamic_cargo_next_index
            .get(&side)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        entry.index = next;
        self.persisted
            .dynamic_cargo_next_index
            .insert_cow(side, next);
        let name = entry.name.clone();
        self.persisted
            .dynamic_cargo_crates
            .insert_cow(name.clone(), entry);
        self.ephemeral
            .dynamic_cargo_registered_at
            .insert(name.clone(), Utc::now());
        info!("dynamic cargo registered {name} index={next} side={side:?}");
        while self.count_dynamic_cargo_side(side) > max {
            self.evict_oldest_dynamic_cargo(lua, side)?;
        }
        self.ephemeral.dirty();
        Ok(())
    }

    fn count_dynamic_cargo_side(&self, side: Side) -> usize {
        self.persisted
            .dynamic_cargo_crates
            .into_iter()
            .filter(|(_, c)| c.side == side)
            .count()
    }

    fn evict_oldest_dynamic_cargo(&mut self, lua: MizLua, side: Side) -> Result<()> {
        let oldest = self
            .persisted
            .dynamic_cargo_crates
            .into_iter()
            .filter(|(_, c)| c.side == side)
            .min_by_key(|(_, c)| c.index)
            .map(|(n, c)| (n.clone(), c.index));
        let Some((name, index)) = oldest else {
            return Ok(());
        };
        info!("dynamic cargo limit: destroy oldest {name} index={index} side={side:?}");
        let source = self
            .persisted
            .dynamic_cargo_crates
            .get(&name)
            .map(|c| c.source);
        if let Ok(Static::Static(st)) = StaticObject::get_by_name(lua, name.as_str()) {
            let _ = st.destroy();
        }
        self.persisted.dynamic_cargo_crates.remove_cow(&name);
        if let Some(oid) = source {
            if let Err(e) = self.sync_objective_to_warehouse(lua, oid, false) {
                warn!("dynamic cargo evict SyncTo {oid:?}: {e:?}");
            }
        }
        self.ephemeral.dirty();
        Ok(())
    }

    /// True if any player ED transport still lists this cargo name on board.
    fn dynamic_cargo_name_on_any_board(&self, lua: MizLua, name: &str) -> bool {
        for (slot, _) in &self.ephemeral.players_by_slot {
            let Ok(ac) = self.ephemeral.slot_instance_unit(lua, slot) else {
                continue;
            };
            if unit_has_cargo_named(&ac, name) {
                return true;
            }
        }
        false
    }

    fn player_carrier_in_air(&self, ucid: &Ucid) -> bool {
        self.persisted
            .players
            .get(ucid)
            .and_then(|p| p.current_slot.as_ref())
            .and_then(|(_, inst)| inst.as_ref())
            .map(|i| i.in_air)
            .unwrap_or(false)
    }

    fn destroy_dynamic_cargo_world_static(&self, lua: MizLua, name: &str) {
        if let Ok(Static::Static(st)) = StaticObject::get_by_name(lua, name) {
            let _ = st.destroy();
        }
    }

    fn cargo_live_xz(lua: MizLua, name: &str, fallback_x: f64, fallback_y: f64) -> (f64, f64) {
        if let Ok(Static::Static(st)) = StaticObject::get_by_name(lua, name) {
            if let Ok(p) = st.get_point() {
                return (p.0.x, p.0.z);
            }
        }
        (fallback_x, fallback_y)
    }

    fn unit_cargo_board_names(ac: &Unit) -> Vec<String> {
        let Ok(Some(cargos)) = ac.get_cargos_on_board() else {
            return Vec::new();
        };
        let mut names = Vec::new();
        let _ = cargos.for_each(|c| {
            let Ok(c) = c else {
                return Ok(());
            };
            if let Ok(name) = c.get_name() {
                names.push(name);
            }
            Ok(())
        });
        names
    }

    fn destroy_dynamic_cargo_lost_with_airframe(&mut self, lua: MizLua, name: &str) {
        info!("airframe lost: deleting dynamic cargo {name} (checkout kept, no restore)");
        self.destroy_dynamic_cargo_world_static(lua, name);
        self.remove_dynamic_cargo_registry_entry(name);
        self.ephemeral.dirty();
    }

    fn purge_dynamic_cargo_at_wreck(
        &mut self,
        lua: MizLua,
        ucid: &Ucid,
        wreck_x: f64,
        wreck_y: f64,
        unit: Option<&Unit>,
    ) {
        let radius_sq = AIRFRAME_LOSS_CARGO_RADIUS_M * AIRFRAME_LOSS_CARGO_RADIUS_M;
        let mut names: FxHashSet<String> = FxHashSet::default();
        if let Some(ac) = unit {
            for name in Self::unit_cargo_board_names(ac) {
                names.insert(name);
            }
        }
        let registered: Vec<(String, Option<Ucid>, bool, f64, f64)> = self
            .persisted
            .dynamic_cargo_crates
            .into_iter()
            .map(|(n, c)| {
                (
                    n.clone(),
                    c.last_carrier.clone(),
                    c.airdrop_rehooked,
                    c.x,
                    c.y,
                )
            })
            .collect();
        for (name, last_carrier, rehooked, x, y) in registered {
            if last_carrier.as_ref() != Some(ucid) || rehooked {
                continue;
            }
            let (lx, ly) = Self::cargo_live_xz(lua, name.as_str(), x, y);
            let dx = lx - wreck_x;
            let dy = ly - wreck_y;
            if dx * dx + dy * dy <= radius_sq {
                names.insert(name);
            }
        }
        for name in names {
            if self.persisted.dynamic_cargo_crates.get(&name).is_some() {
                self.destroy_dynamic_cargo_lost_with_airframe(lua, name.as_str());
            } else {
                self.destroy_dynamic_cargo_world_static(lua, name.as_str());
            }
        }
        let keep: FxHashSet<String> = self
            .persisted
            .dynamic_cargo_crates
            .into_iter()
            .map(|(n, _)| n.clone())
            .collect();
        for hit in collect_nearby_cargo(lua, wreck_x, wreck_y, AIRFRAME_LOSS_CARGO_RADIUS_M) {
            if keep.contains(&hit.name) {
                continue;
            }
            self.destroy_dynamic_cargo_world_static(lua, hit.name.as_str());
        }
    }

    /// Crash / UnitLost / Dead: destroy bay ED cargo. Skip PilotDead while the airframe still flies.
    pub fn destroy_dynamic_cargo_if_airframe_lost(
        &mut self,
        lua: MizLua,
        id: &DcsOid<ClassUnit>,
    ) {
        if !self.dynamic_cargo_enabled() {
            return;
        }
        if Self::object_airframe_flyable(lua, id) {
            return;
        }
        let Some(ucid) = self.player_in_unit(false, id) else {
            return;
        };
        let unit = Unit::get_instance(lua, id).ok();
        let wreck = unit
            .as_ref()
            .and_then(|u| u.get_point().ok())
            .map(|p| (p.0.x, p.0.z))
            .or_else(|| {
                self.player_instance_world_pos(&ucid)
                    .map(|(x, y, _)| (x, y))
            });
        let Some((wreck_x, wreck_y)) = wreck else {
            return;
        };
        self.purge_dynamic_cargo_at_wreck(lua, &ucid, wreck_x, wreck_y, unit.as_ref());
        if !self
            .ephemeral
            .pending_airframe_loss_dynamic_cargo
            .iter()
            .any(|(_, u, ..)| u == &ucid)
        {
            self.ephemeral.pending_airframe_loss_dynamic_cargo.push((
                Utc::now() + AIRFRAME_LOSS_CARGO_PURGE_DELAY,
                ucid,
                wreck_x,
                wreck_y,
            ));
        }
    }

    pub fn process_pending_airframe_loss_dynamic_cargo(
        &mut self,
        lua: MizLua,
        now: DateTime<Utc>,
    ) {
        if self.ephemeral.pending_airframe_loss_dynamic_cargo.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.ephemeral.pending_airframe_loss_dynamic_cargo);
        let mut keep = Vec::new();
        for (due, ucid, wreck_x, wreck_y) in pending {
            if now < due {
                keep.push((due, ucid, wreck_x, wreck_y));
                continue;
            }
            self.purge_dynamic_cargo_at_wreck(lua, &ucid, wreck_x, wreck_y, None);
        }
        self.ephemeral.pending_airframe_loss_dynamic_cargo = keep;
    }

    fn dynamic_cargo_is_airframe_loss_dump(
        &self,
        lua: MizLua,
        entry: &DynamicCargoCrate,
    ) -> bool {
        if entry.airdrop_rehooked {
            return false;
        }
        let Some(ucid) = entry.last_carrier.as_ref() else {
            return false;
        };
        if self.player_current_airframe_flyable(lua, ucid) {
            return false;
        }
        let radius_sq = AIRFRAME_LOSS_CARGO_RADIUS_M * AIRFRAME_LOSS_CARGO_RADIUS_M;
        let (x, y) = Self::cargo_live_xz(lua, entry.name.as_str(), entry.x, entry.y);
        if let Some((wx, wy, _)) = self.player_instance_world_pos(ucid) {
            let dx = x - wx;
            let dy = y - wy;
            if dx * dx + dy * dy <= radius_sq {
                return true;
            }
        }
        for (_, u, wx, wy) in &self.ephemeral.pending_airframe_loss_dynamic_cargo {
            if u != ucid {
                continue;
            }
            let dx = x - wx;
            let dy = y - wy;
            if dx * dx + dy * dy <= radius_sq {
                return true;
            }
        }
        false
    }

    /// DCS chute copies (often a new name); purge unregistered cargo near the impact.
    fn destroy_airdrop_cargo_orphans(
        &self,
        lua: MizLua,
        keep_name: &str,
        x: f64,
        y: f64,
    ) {
        let registered: FxHashSet<String> = self
            .persisted
            .dynamic_cargo_crates
            .into_iter()
            .map(|(n, _)| n.clone())
            .collect();
        for hit in collect_nearby_cargo(lua, x, y, AIRDROP_ORPHAN_RADIUS_M) {
            if hit.name.as_str() == keep_name {
                continue;
            }
            if registered.contains(&hit.name) {
                continue;
            }
            self.destroy_dynamic_cargo_world_static(lua, hit.name.as_str());
        }
    }

    /// Debit source for newly appeared cargo warehouse qty (Birth often registers empty).
    fn debit_dynamic_cargo_snapshot_delta(
        &mut self,
        source: ObjectiveId,
        prev_eq: &MapS<String, u32>,
        prev_liq: &MapS<LiquidType, u32>,
        new_eq: &MapS<String, u32>,
        new_liq: &MapS<LiquidType, u32>,
    ) -> Result<bool> {
        let mut delta_eq = MapS::default();
        for (k, v) in new_eq {
            let prev = prev_eq.get(k).copied().unwrap_or(0);
            if *v > prev {
                delta_eq.insert_cow(k.clone(), *v - prev);
            }
        }
        let mut delta_liq = MapS::default();
        for (k, v) in new_liq {
            let prev = prev_liq.get(k).copied().unwrap_or(0);
            if *v > prev {
                delta_liq.insert_cow(*k, *v - prev);
            }
        }
        if delta_eq.len() == 0 && delta_liq.len() == 0 {
            return Ok(false);
        }
        self.debit_source_for_dynamic_cargo_checkout(source, &delta_eq, &delta_liq)?;
        Ok(true)
    }

    pub fn prune_missing_dynamic_cargo(&mut self, lua: MizLua) {
        if !self.dynamic_cargo_enabled() {
            return;
        }
        // Drop Fowl R/BCRATE entries wrongly registered while canCargo was on.
        let fowl_wrong: Vec<String> = self
            .persisted
            .dynamic_cargo_crates
            .into_iter()
            .filter_map(|(name, _)| {
                let uid = self.persisted.units_by_name.get(name.as_str())?;
                let unit = self.persisted.units.get(uid)?;
                if self.persisted.crates.contains(&unit.group) {
                    Some(name.clone())
                } else {
                    None
                }
            })
            .collect();
        for name in fowl_wrong {
            if let Some(entry) = self.persisted.dynamic_cargo_crates.remove_cow(&name) {
                self.ephemeral.dynamic_cargo_registered_at.remove(&name);
                self.ephemeral.dynamic_cargo_miss_count.remove(&name);
                self.ephemeral.dynamic_cargo_air_dropped_at.remove(&name);
                self.clamp_dynamic_cargo_checkout(entry.source);
                info!("dynamic cargo: removed Fowl crate registry entry {name}");
                self.ephemeral.dirty();
            }
        }
        const REGISTER_GRACE: chrono::Duration = chrono::Duration::seconds(90);
        let now = Utc::now();
        let mut gone: Vec<String> = Vec::new();
        let names: Vec<String> = self
            .persisted
            .dynamic_cargo_crates
            .into_iter()
            .map(|(n, _)| n.clone())
            .collect();
        for name in names {
            if self
                .ephemeral
                .dynamic_cargo_rehook_in_progress
                .contains(name.as_str())
            {
                continue;
            }
            // Still in an ED bay — F8 load hides/moves the world static.
            if self.dynamic_cargo_name_on_any_board(lua, name.as_str()) {
                self.ephemeral.dynamic_cargo_miss_count.remove(&name);
                continue;
            }
            let world_gone = match StaticObject::get_by_name(lua, name.as_str()) {
                Ok(Static::Static(st)) => !st.is_exist().unwrap_or(false),
                _ => true,
            };
            if !world_gone {
                self.ephemeral.dynamic_cargo_miss_count.remove(&name);
                continue;
            }
            let (carrier_ucid, spawner, already_dropped) = self
                .persisted
                .dynamic_cargo_crates
                .get(&name)
                .map(|e| (e.last_carrier.clone(), e.spawner, e.air_dropped))
                .unwrap_or((None, Ucid::default(), false));
            let track_ucid = carrier_ucid.clone().unwrap_or(spawner);
            let carrier_air = self.player_carrier_in_air(&track_ucid);
            let carrier_pos = self.player_instance_world_pos(&track_ucid);
            // Mark airdrop once; freeze drop pos from carrier/spawner (get_point often stays at source).
            if let Some(e) = self.persisted.dynamic_cargo_crates.get_mut_cow(&name) {
                let high =
                    cargo_agl_m(lua, e.x, e.y, e.alt).is_some_and(|a| a > AIR_DROP_AGL_M);
                if !e.air_dropped && (carrier_air || high) {
                    if let Some((x, y, alt)) = carrier_pos {
                        e.x = x;
                        e.y = y;
                        e.alt = alt;
                    }
                    if e.last_carrier.is_none() {
                        e.last_carrier = Some(track_ucid);
                    }
                    e.air_dropped = true;
                    self.ephemeral
                        .dynamic_cargo_air_dropped_at
                        .insert(name.clone(), now);
                    info!(
                        "dynamic cargo marked air_dropped: {} pos=({:.0},{:.0}) carrier_air={}",
                        name, e.x, e.y, carrier_air
                    );
                }
            }
            let air_dropped = already_dropped
                || self
                    .persisted
                    .dynamic_cargo_crates
                    .get(&name)
                    .map(|e| e.air_dropped)
                    .unwrap_or(false);
            let rehooked = self
                .persisted
                .dynamic_cargo_crates
                .get(&name)
                .map(|e| e.airdrop_rehooked)
                .unwrap_or(false);
            // Parachute in flight: keep registry; do not invent stock.
            if air_dropped && !rehooked {
                let dropped_at = self
                    .ephemeral
                    .dynamic_cargo_air_dropped_at
                    .get(&name)
                    .copied()
                    .unwrap_or(now);
                let chute_wait = now - dropped_at < AIRDROP_CHUTE_GRACE;
                let grounded = self
                    .persisted
                    .dynamic_cargo_crates
                    .get(&name)
                    .and_then(|e| cargo_agl_m(lua, e.x, e.y, e.alt))
                    .is_some_and(|a| a <= GROUND_AGL_M);
                if carrier_air && chute_wait && !grounded {
                    continue;
                }
                if !rehooked && (grounded || !chute_wait || !carrier_air) {
                    match self.rehook_airdropped_dynamic_cargo(lua, &name) {
                        Ok(()) => continue,
                        Err(e) => warn!("dynamic cargo airdrop rehook {name}: {e:?}"),
                    }
                }
            }
            // Fill / F8 load can drop the world static before getCargosOnBoard lists it.
            if self
                .ephemeral
                .dynamic_cargo_registered_at
                .get(&name)
                .is_some_and(|t| now - *t < REGISTER_GRACE)
            {
                continue;
            }
            let misses = self
                .ephemeral
                .dynamic_cargo_miss_count
                .entry(name.clone())
                .or_insert(0);
            *misses = misses.saturating_add(1);
            if *misses < PRUNE_MISS_LIMIT {
                continue;
            }
            gone.push(name);
        }
        if gone.is_empty() {
            return;
        }
        let mut sync_from_oids: FxHashMap<ObjectiveId, ()> = FxHashMap::default();
        let mut point_awards: FxHashMap<Ucid, i32> = FxHashMap::default();
        let mut log_awards: FxHashMap<Ucid, ()> = FxHashMap::default();
        for name in gone {
            let Some(entry) = self.persisted.dynamic_cargo_crates.get(&name).cloned() else {
                continue;
            };
            self.clamp_dynamic_cargo_checkout(entry.source);
            // Prefer live spawner/carrier pos — cargo get_point often stays at source after CDS.
            let mut entry = entry;
            if let Some((x, y, alt)) = entry
                .last_carrier
                .as_ref()
                .or(Some(&entry.spawner))
                .and_then(|u| self.player_instance_world_pos(u))
            {
                entry.x = x;
                entry.y = y;
                entry.alt = alt;
            }
            self.destroy_dynamic_cargo_world_static(lua, &name);

            // Stock re-enters warehouses only via DCS absorb (SyncFrom) or To stock.
            let dest = self.resolve_dynamic_cargo_absorb_dest(&entry);
            if dest.is_none() && entry.air_dropped {
                info!(
                    "dynamic cargo lost after airdrop (checkout kept, no restore): {} source={:?} pos=({:.0},{:.0}) carrier={:?} weight_kg={:.0}",
                    entry.name,
                    entry.source,
                    entry.x,
                    entry.y,
                    entry.last_carrier,
                    entry.last_weight_kg
                );
                self.remove_dynamic_cargo_registry_entry(&name);
                continue;
            }

            match dest {
                Some(pad) => {
                    // DCS already wrote warehouse stock; pull into virtual — do not Fowl-credit.
                    let synced = match self.sync_warehouse_to_objective(lua, pad) {
                        Ok(_) => {
                            sync_from_oids.insert(pad, ());
                            self.clamp_dynamic_cargo_checkout(pad);
                            info!(
                                "dynamic cargo absorb SyncFrom {pad:?} ({})",
                                entry.name
                            );
                            true
                        }
                        Err(e) => {
                            warn!("dynamic cargo absorb SyncFrom {pad:?}: {e:?}");
                            false
                        }
                    };
                    if synced {
                        let same_objective = pad == entry.source;
                        if same_objective {
                            info!(
                                "dynamic cargo DCS absorb same objective (no points/stats): {} -> {:?} carrier={:?} weight_kg={:.0}",
                                entry.name,
                                pad,
                                entry.last_carrier,
                                entry.last_weight_kg
                            );
                        } else {
                            let tons = delivery_tons(entry.last_weight_kg);
                            let cfg = self.dynamic_cargo_cfg();
                            let deliverer = entry.last_carrier.unwrap_or(entry.spawner);
                            let deliv_pts =
                                points_from_tons(tons, cfg.to_stock_points_per_ton);
                            let spawn_pts =
                                points_from_tons(tons, cfg.source_spawner_points_per_ton);
                            if deliv_pts != 0 {
                                *point_awards.entry(deliverer).or_default() += deliv_pts;
                            }
                            if spawn_pts != 0 {
                                *point_awards.entry(entry.spawner).or_default() += spawn_pts;
                            }
                            log_awards.insert(deliverer, ());
                            log_awards.insert(entry.spawner, ());
                            let dest_side = self
                                .persisted
                                .objectives
                                .get(&pad)
                                .map(|o| o.owner)
                                .unwrap_or(entry.side);
                            self.campaign_on_dynamic_cargo_delivery(
                                dest_side,
                                entry.last_weight_kg,
                                1,
                            );
                            info!(
                                "dynamic cargo DCS absorb delivery: {} -> {:?} tons={:.2} deliverer={:?} spawner={:?} carrier={:?} weight_kg={:.0}",
                                entry.name,
                                pad,
                                tons,
                                deliverer,
                                entry.spawner,
                                entry.last_carrier,
                                entry.last_weight_kg
                            );
                        }
                    }
                }
                None => {
                    info!(
                        "dynamic cargo lost (checkout kept, no restore): {} source={:?} pos=({:.0},{:.0}) carrier={:?} weight_kg={:.0}",
                        entry.name,
                        entry.source,
                        entry.x,
                        entry.y,
                        entry.last_carrier,
                        entry.last_weight_kg
                    );
                }
            }
            self.remove_dynamic_cargo_registry_entry(&name);
        }
        for (ucid, amount) in point_awards {
            if amount != 0 {
                self.adjust_points(&ucid, amount, "for dynamic cargo delivery (DCS absorb)");
            }
        }
        for ucid in log_awards.into_keys() {
            self.campaign_top10_on_logistics(ucid);
        }
        if let Err(e) = self.update_supply_status() {
            warn!("dynamic cargo absorb supply status: {e:?}");
        }
        self.ephemeral.dirty();
    }

    fn remove_dynamic_cargo_registry_entry(&mut self, name: &str) {
        let key: String = name.into();
        self.ephemeral.dynamic_cargo_registered_at.remove(&key);
        self.ephemeral.dynamic_cargo_miss_count.remove(&key);
        self.ephemeral.dynamic_cargo_rehook_in_progress.remove(&key);
        self.ephemeral.dynamic_cargo_air_dropped_at.remove(&key);
        if let Some(entry) = self.persisted.dynamic_cargo_crates.remove_cow(&key) {
            self.clamp_dynamic_cargo_checkout(entry.source);
        }
    }

    /// Dead / destroy of a registered ED warehouse crate (not Fowl R/BCRATE).
    /// Checkout stays debited (campaign loss). Returns true when handled.
    pub fn on_dynamic_cargo_static_destroyed(&mut self, _lua: MizLua, name: &str) -> bool {
        if !self.dynamic_cargo_enabled() {
            return false;
        }
        if self
            .ephemeral
            .dynamic_cargo_rehook_in_progress
            .contains(name)
        {
            return true;
        }
        let Some(entry) = self.persisted.dynamic_cargo_crates.get(name).cloned() else {
            return false;
        };
        info!(
            "dynamic cargo destroyed (checkout kept, no restore): {} source={:?} air_dropped={} weight_kg={:.0}",
            entry.name, entry.source, entry.air_dropped, entry.last_weight_kg
        );
        self.remove_dynamic_cargo_registry_entry(name);
        self.ephemeral.dirty();
        true
    }

    fn credit_objective_from_dynamic_cargo(
        &mut self,
        dest_oid: ObjectiveId,
        equipment: &MapS<String, u32>,
        liquids: &MapS<LiquidType, u32>,
    ) -> Result<()> {
        let liquids_tons = {
            let export = self.ephemeral.fowl_miz_export.as_ref();
            let obj = self
                .persisted
                .objectives
                .get(&dest_oid)
                .ok_or_else(|| anyhow!("missing destination objective"))?;
            objective_liquids_stored_as_tons(export, obj)
        };
        let obj = self
            .persisted
            .objectives
            .get_mut_cow(&dest_oid)
            .ok_or_else(|| anyhow!("missing destination objective"))?;
        for (item, qty) in equipment {
            if *qty == 0 {
                continue;
            }
            let inv = obj.warehouse.equipment.get_or_default_cow(item.clone());
            inv.stored = inv.stored.saturating_add(*qty);
            if inv.capacity == 0 {
                inv.capacity = 1;
            }
        }
        for (liq, qty_kg) in liquids {
            if *qty_kg == 0 {
                continue;
            }
            let add = if liquids_tons {
                dcs_liquid_kg_to_fowl_tons(*qty_kg).max(if *qty_kg > 0 { 1 } else { 0 })
            } else {
                *qty_kg
            };
            if add == 0 {
                continue;
            }
            let inv = obj.warehouse.liquids.get_or_default_cow(*liq);
            inv.stored = inv.stored.saturating_add(add);
            if inv.capacity == 0 {
                inv.capacity = 1;
            }
        }
        self.ephemeral.dirty();
        Ok(())
    }

    pub fn restore_dynamic_cargo_after_load(&mut self, lua: MizLua) -> Result<()> {
        if !self.dynamic_cargo_enabled() {
            self.clear_dynamic_cargo_if_disabled();
            return Ok(());
        }
        let entries: Vec<DynamicCargoCrate> = self
            .persisted
            .dynamic_cargo_crates
            .into_iter()
            .map(|(_, c)| c.clone())
            .collect();
        for entry in entries {
            match StaticObject::get_by_name(lua, entry.name.as_str()) {
                Ok(Static::Static(_)) => continue,
                _ => {}
            }
            if let Err(e) = self.respawn_dynamic_cargo_crate(lua, &entry) {
                warn!(
                    "dynamic cargo restore failed for {}: {e:?} (dropping registry entry)",
                    entry.name
                );
                self.persisted.dynamic_cargo_crates.remove_cow(&entry.name);
                self.ephemeral.dirty();
            }
        }
        Ok(())
    }

    fn respawn_dynamic_cargo_crate(&self, lua: MizLua, entry: &DynamicCargoCrate) -> Result<()> {
        let country = coalition_country_for_side(lua, entry.side)?;
        let coa = Coalition::singleton(lua)?;
        let tbl = lua.inner().create_table()?;
        tbl.raw_set("category", "Cargos")?;
        tbl.raw_set("type", entry.type_name.as_str())?;
        tbl.raw_set("name", entry.name.as_str())?;
        tbl.raw_set("x", entry.x)?;
        tbl.raw_set("y", entry.y)?;
        tbl.raw_set("alt", entry.alt)?;
        tbl.raw_set("heading", 0f64)?;
        tbl.raw_set("canCargo", true)?;
        let unit = dcso3::env::miz::Unit::from_lua(Value::Table(tbl), lua.inner())
            .map_err(|e| anyhow!("dynamic cargo unit table: {e}"))?;
        let spawned = coa
            .add_static_object(country, unit)
            .with_context(|| format_compact!("respawn dynamic cargo {}", entry.name))?;
        let Static::Static(st) = spawned else {
            bail!("dynamic cargo respawn returned airbase for {}", entry.name);
        };
        let wh = Warehouse::get_cargo_as_warehouse(lua, &st)
            .context("getCargoAsWarehouse after respawn")?;
        for (name, qty) in &entry.equipment {
            if *qty > 0 {
                wh.set_item(name.clone(), *qty)?;
            }
        }
        for (liq, qty) in &entry.liquids {
            if *qty > 0 {
                wh.set_liquid_amount(*liq, *qty)?;
            }
        }
        Ok(())
    }

    pub fn refresh_dynamic_cargo_snapshots(&mut self, lua: MizLua) {
        if !self.dynamic_cargo_enabled() {
            return;
        }
        let names: Vec<(String, Side)> = self
            .persisted
            .dynamic_cargo_crates
            .into_iter()
            .map(|(n, c)| (n.clone(), c.side))
            .collect();
        let mut dirty = false;
        let mut to_rehook: Vec<String> = Vec::new();
        let mut just_air_dropped: Vec<String> = Vec::new();
        let mut debit_sync: Vec<ObjectiveId> = Vec::new();
        for (name, side) in names {
            let Ok(Static::Static(st)) = StaticObject::get_by_name(lua, name.as_str()) else {
                continue;
            };
            let Ok(point) = st.get_point() else {
                continue;
            };
            let Ok((equipment, liquids)) = snapshot_cargo_warehouse(lua, &st) else {
                continue;
            };
            let weight = st.get_cargo_weight().unwrap_or(0.);
            let carrier = carrier_ucid_if_aboard(self, lua, side, &st);
            let carrier_pos = carrier.and_then(|u| self.player_instance_world_pos(&u));
            let aboard = carrier.is_some()
                || self.dynamic_cargo_name_on_any_board(lua, name.as_str());
            self.ephemeral.dynamic_cargo_miss_count.remove(&name);
            self.ephemeral
                .dynamic_cargo_registered_at
                .insert(name.clone(), Utc::now());

            let (source, spawner, prev_eq, prev_liq, source_checked_out) = {
                let Some(entry) = self.persisted.dynamic_cargo_crates.get(&name) else {
                    continue;
                };
                (
                    entry.source,
                    entry.spawner,
                    entry.equipment.clone(),
                    entry.liquids.clone(),
                    entry.source_checked_out,
                )
            };
            let snap_empty = equipment.len() == 0 && liquids.len() == 0;
            if source_checked_out && !snap_empty {
                match self.debit_dynamic_cargo_snapshot_delta(
                    source,
                    &prev_eq,
                    &prev_liq,
                    &equipment,
                    &liquids,
                ) {
                    Ok(true) => {
                        debit_sync.push(source);
                        info!(
                            "dynamic cargo checkout debit (fill delta): {name} source={source:?}"
                        );
                    }
                    Ok(false) => {}
                    Err(e) => warn!("dynamic cargo fill debit {name}: {e:?}"),
                }
            }

            let spawner_air = self.player_carrier_in_air(&spawner);
            let spawner_pos = self.player_instance_world_pos(&spawner);

            if let Some(entry) = self.persisted.dynamic_cargo_crates.get_mut_cow(&name) {
                // Prefer carrier aircraft pos while aboard — cargo get_point can stay at source.
                if let Some((x, y, alt)) = carrier_pos {
                    entry.x = x;
                    entry.y = y;
                    entry.alt = alt;
                } else if spawner_air {
                    if let Some((x, y, alt)) = spawner_pos {
                        entry.x = x;
                        entry.y = y;
                        entry.alt = alt;
                    }
                } else {
                    entry.x = point.0.x;
                    entry.y = point.0.z;
                    entry.alt = point.0.y;
                }
                if !snap_empty {
                    entry.equipment = equipment;
                    entry.liquids = liquids;
                }
                if weight > 0. {
                    entry.last_weight_kg = weight;
                }
                if let Some(ucid) = carrier {
                    entry.last_carrier = Some(ucid);
                } else if aboard {
                    entry.last_carrier = Some(spawner);
                }
                if let Some(agl) = cargo_agl_m(lua, entry.x, entry.y, entry.alt) {
                    if !aboard && agl > AIR_DROP_AGL_M && !entry.air_dropped {
                        entry.air_dropped = true;
                        just_air_dropped.push(name.clone());
                    }
                    if entry.air_dropped
                        && !entry.airdrop_rehooked
                        && !aboard
                        && agl <= GROUND_AGL_M
                    {
                        to_rehook.push(name.clone());
                    }
                }
                dirty = true;
            }
        }
        let drop_ts = Utc::now();
        for name in just_air_dropped {
            self.ephemeral
                .dynamic_cargo_air_dropped_at
                .insert(name, drop_ts);
        }
        for oid in debit_sync {
            if let Err(e) = self.sync_objective_to_warehouse(lua, oid, false) {
                warn!("dynamic cargo fill debit SyncTo {oid:?}: {e:?}");
            }
        }
        for name in to_rehook {
            if let Err(e) = self.rehook_airdropped_dynamic_cargo(lua, &name) {
                warn!("dynamic cargo airdrop rehook {name}: {e:?}");
            } else {
                dirty = true;
            }
        }
        if dirty {
            self.ephemeral.dirty();
        }
    }

    /// After parachute: respawn same type with `canCargo` at impact; then destroy DCS originals.
    fn rehook_airdropped_dynamic_cargo(&mut self, lua: MizLua, name: &str) -> Result<()> {
        let Some(mut entry) = self.persisted.dynamic_cargo_crates.get(name).cloned() else {
            return Ok(());
        };
        if entry.airdrop_rehooked || !entry.air_dropped {
            return Ok(());
        }
        if self.dynamic_cargo_is_airframe_loss_dump(lua, &entry) {
            self.destroy_dynamic_cargo_lost_with_airframe(lua, name);
            return Ok(());
        }
        if self.dynamic_cargo_name_on_any_board(lua, name) {
            return Ok(());
        }
        self.ephemeral
            .dynamic_cargo_rehook_in_progress
            .insert(name.into());
        let old_type = entry.type_name.clone();
        let named_pt = if let Ok(Static::Static(st)) = StaticObject::get_by_name(lua, name) {
            if let Ok((equipment, liquids)) = snapshot_cargo_warehouse(lua, &st) {
                if equipment.len() > 0 || liquids.len() > 0 {
                    entry.equipment = equipment;
                    entry.liquids = liquids;
                }
            }
            if let Ok(w) = st.get_cargo_weight() {
                if w > 0. {
                    entry.last_weight_kg = w;
                }
            }
            st.get_point().ok().map(|p| (p.0.x, p.0.z, p.0.y))
        } else {
            None
        };
        if let Some((x, y, alt)) = named_pt {
            entry.x = x;
            entry.y = y;
            entry.alt = alt;
        } else {
            let nearby = collect_nearby_cargo(lua, entry.x, entry.y, AIRDROP_ORPHAN_RADIUS_M);
            if let Some(hit) = nearby.iter().find(|h| {
                h.name.as_str() != name
                    && cargo_agl_m(lua, h.x, h.y, h.alt).is_some_and(|a| a <= GROUND_AGL_M)
            }) {
                entry.x = hit.x;
                entry.y = hit.y;
                entry.alt = hit.alt;
            }
        }
        if let Ok(land) = Land::singleton(lua) {
            if let Ok(ground) = land.get_height(LuaVec2(Vector2::new(entry.x, entry.y))) {
                entry.alt = ground + 1.0;
            }
        }
        let spawn_ok = self.respawn_dynamic_cargo_crate(lua, &entry);
        self.ephemeral.dynamic_cargo_rehook_in_progress.remove(name);
        spawn_ok.with_context(|| format_compact!("respawn after airdrop {name}"))?;
        self.destroy_airdrop_cargo_orphans(lua, name, entry.x, entry.y);
        if let Some(e) = self.persisted.dynamic_cargo_crates.get_mut_cow(name) {
            e.air_dropped = true;
            e.airdrop_rehooked = true;
            e.type_name = entry.type_name.clone();
            e.x = entry.x;
            e.y = entry.y;
            e.alt = entry.alt;
            e.equipment = entry.equipment;
            e.liquids = entry.liquids;
            e.last_weight_kg = entry.last_weight_kg;
        }
        self.ephemeral
            .dynamic_cargo_registered_at
            .insert(name.into(), Utc::now());
        self.ephemeral.dynamic_cargo_miss_count.remove(name);
        info!(
            "dynamic cargo airdrop rehook (canCargo): {name} type {old_type} pos=({:.0},{:.0})",
            entry.x, entry.y
        );
        Ok(())
    }

    pub fn list_nearby_dynamic_cargo(
        &self,
        lua: MizLua,
        st: &super::cargo::SlotStats,
    ) -> Result<Vec<NearbyDynamicCargo>> {
        if !self.dynamic_cargo_enabled() {
            return Ok(Vec::new());
        }
        let max_dist = self.ephemeral.cfg.crate_load_distance as f64;
        let max_dist_sq = max_dist * max_dist;
        let mut res: Vec<NearbyDynamicCargo> = Vec::new();
        for (name, entry) in &self.persisted.dynamic_cargo_crates {
            if entry.side != st.side {
                continue;
            }
            let (pos, damaged, loaded) =
                if let Ok(Static::Static(st_obj)) = StaticObject::get_by_name(lua, name.as_str()) {
                    let pos = st_obj
                        .get_point()
                        .map(|p| Vector2::new(p.0.x, p.0.z))
                        .unwrap_or_else(|_| entry.pos());
                    let damaged = st_obj
                        .try_get_life0()
                        .and_then(|life0| {
                            st_obj
                                .get_life()
                                .ok()
                                .map(|life| life > 0 && life < life0)
                        })
                        .unwrap_or(false);
                    let loaded = self.dynamic_cargo_name_on_any_board(lua, name.as_str());
                    (pos, damaged, loaded)
                } else {
                    (entry.pos(), false, false)
                };
            let dist_sq = (pos - st.point).magnitude_squared();
            if dist_sq > max_dist_sq {
                continue;
            }
            let distance = dist_sq.sqrt();
            let heading = radians_to_degrees(azumith2d_to(st.point, pos));
            res.push(NearbyDynamicCargo {
                type_name: entry.type_name.clone(),
                name: name.clone(),
                distance,
                heading,
                loaded,
                damaged,
            });
        }
        res.sort_by(|a, b| {
            a.distance
                .partial_cmp(&b.distance)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(res)
    }

    pub fn to_stock_dynamic_cargo(
        &mut self,
        lua: MizLua,
        slot: &dcso3::net::SlotId,
    ) -> Result<ToStockResult> {
        if !self.dynamic_cargo_enabled() {
            bail!("dynamic cargo delivery is disabled");
        }
        let st = super::cargo::SlotStats::get(self, lua, slot)?;
        let (dest_oid, dest_obj) = self.point_near_logistics_for_dynamic(st.side, st.point)?;
        let dest_name = dest_obj.name.clone();
        let max_dist = self.dynamic_cargo_cfg().to_stock_dynamic_crate_distance as f64;
        let max_dist_sq = max_dist * max_dist;
        let mut nearby: Vec<String> = self
            .persisted
            .dynamic_cargo_crates
            .into_iter()
            .filter(|(_, c)| c.side == st.side)
            .filter_map(|(name, c)| {
                let d = (c.pos() - st.point).magnitude_squared();
                (d <= max_dist_sq).then(|| name.clone())
            })
            .collect();
        if nearby.is_empty() {
            nearby = self
                .persisted
                .dynamic_cargo_crates
                .into_iter()
                .filter(|(_, c)| c.side == st.side)
                .filter_map(|(name, _)| {
                    let Ok(Static::Static(obj)) = StaticObject::get_by_name(lua, name.as_str())
                    else {
                        return None;
                    };
                    let Ok(p) = obj.get_point() else {
                        return None;
                    };
                    let pos = Vector2::new(p.0.x, p.0.z);
                    ((pos - st.point).magnitude_squared() <= max_dist_sq).then(|| name.clone())
                })
                .collect();
        }
        if nearby.is_empty() {
            bail!("no dynamic cargo crates within {max_dist} m");
        }
        self.to_stock_named_crates(lua, &st.ucid, dest_oid, dest_name, nearby)
    }

    fn point_near_logistics_for_dynamic(
        &self,
        side: Side,
        point: Vector2,
    ) -> Result<(ObjectiveId, &super::objective::Objective)> {
        self.persisted
            .objectives
            .into_iter()
            .find_map(|(oid, obj)| {
                if obj.owner == side
                    && obj.logi() > 0
                    && !matches!(obj.kind, ObjectiveKind::Production)
                    && obj.zone.contains(point)
                {
                    Some((*oid, obj))
                } else {
                    None
                }
            })
            .ok_or_else(|| anyhow!("not near friendly logistics"))
    }

    fn to_stock_named_crates(
        &mut self,
        lua: MizLua,
        deliverer: &Ucid,
        dest_oid: ObjectiveId,
        dest_name: String,
        names: Vec<String>,
    ) -> Result<ToStockResult> {
        let cfg_rates = (
            self.dynamic_cargo_cfg().to_stock_points_per_ton,
            self.dynamic_cargo_cfg().source_spawner_points_per_ton,
        );
        let liquids_tons = {
            let export = self.ephemeral.fowl_miz_export.as_ref();
            let obj = self
                .persisted
                .objectives
                .get(&dest_oid)
                .ok_or_else(|| anyhow!("missing destination objective"))?;
            objective_liquids_stored_as_tons(export, obj)
        };
        let mut result = ToStockResult {
            destination: dest_name,
            ..Default::default()
        };
        let mut point_awards: FxHashMap<Ucid, i32> = FxHashMap::default();
        let mut skipped_loaded = 0u32;
        let mut rejected_same_objective = 0u32;
        let mut delivered_weight_kg = 0f64;
        for name in names {
            if self.dynamic_cargo_name_on_any_board(lua, name.as_str()) {
                skipped_loaded += 1;
                continue;
            }
            let Some(entry) = self.persisted.dynamic_cargo_crates.get(&name).cloned() else {
                continue;
            };
            let Ok(Static::Static(st)) = StaticObject::get_by_name(lua, name.as_str()) else {
                self.persisted.dynamic_cargo_crates.remove_cow(&name);
                continue;
            };
            if entry.source == dest_oid {
                rejected_same_objective += 1;
                continue;
            }
            let weight_kg = st
                .get_cargo_weight()
                .ok()
                .filter(|w| *w > 0.)
                .unwrap_or(entry.last_weight_kg);
            let (equipment, liquids) = snapshot_cargo_warehouse(lua, &st).unwrap_or((
                entry.equipment.clone(),
                entry.liquids.clone(),
            ));
            {
                let obj = self
                    .persisted
                    .objectives
                    .get_mut_cow(&dest_oid)
                    .ok_or_else(|| anyhow!("missing destination objective"))?;
                for (item, qty) in &equipment {
                    if *qty == 0 {
                        continue;
                    }
                    let inv = obj.warehouse.equipment.get_or_default_cow(item.clone());
                    inv.stored = inv.stored.saturating_add(*qty);
                    if inv.capacity == 0 {
                        inv.capacity = 1;
                    }
                }
                for (liq, qty_kg) in &liquids {
                    if *qty_kg == 0 {
                        continue;
                    }
                    let add = if liquids_tons {
                        dcs_liquid_kg_to_fowl_tons(*qty_kg).max(if *qty_kg > 0 { 1 } else { 0 })
                    } else {
                        *qty_kg
                    };
                    if add == 0 {
                        continue;
                    }
                    let inv = obj.warehouse.liquids.get_or_default_cow(*liq);
                    inv.stored = inv.stored.saturating_add(add);
                    if inv.capacity == 0 {
                        inv.capacity = 1;
                    }
                }
            }
            let _ = st.destroy();
            let tons = delivery_tons(weight_kg);
            let deliv_pts = points_from_tons(tons, cfg_rates.0);
            let spawn_pts = points_from_tons(tons, cfg_rates.1);
            if deliv_pts != 0 {
                *point_awards.entry(*deliverer).or_default() += deliv_pts;
            }
            if spawn_pts != 0 {
                *point_awards.entry(entry.spawner).or_default() += spawn_pts;
            }
            self.persisted.dynamic_cargo_crates.remove_cow(&name);
            delivered_weight_kg += weight_kg.max(0.);
            result.crates += 1;
        }
        if result.crates == 0 {
            if rejected_same_objective > 0 && skipped_loaded == 0 {
                bail!(
                    "Supplies can only be unloaded at a different objective than where they were spawned"
                );
            }
            if skipped_loaded > 0 && rejected_same_objective == 0 {
                bail!(
                    "Unload dynamic cargo crates from the aircraft first (dynamic cargo menu)"
                );
            }
            if skipped_loaded > 0 && rejected_same_objective > 0 {
                bail!(
                    "Unload crates from the aircraft first; supplies can only be unloaded at a different objective than where they were spawned"
                );
            }
            bail!("no dynamic cargo crates could be stocked");
        }
        let dest_side = self
            .persisted
            .objectives
            .get(&dest_oid)
            .map(|o| o.owner)
            .unwrap_or(Side::Neutral);
        self.campaign_on_dynamic_cargo_delivery(dest_side, delivered_weight_kg, 1);
        self.sync_objective_to_warehouse(lua, dest_oid, false)
            .context("syncing destination warehouse after To stock")?;
        self.update_supply_status()
            .context("updating supply status after To stock")?;
        for (ucid, amount) in point_awards {
            if amount != 0 {
                self.adjust_points(&ucid, amount, "for dynamic cargo To stock");
                self.campaign_top10_on_logistics(ucid);
            }
        }
        self.ephemeral.dirty();
        Ok(result)
    }

    /// Sum of ED bay / sling dynamic cargo masses (kg) for List Cargo.
    /// Prefer `Unit.getCargosOnBoard` — F8-aboard crates keep pad `get_point`, so OBB alone misses warehouse stock.
    pub fn loaded_dynamic_cargo_weight_kg(&self, lua: MizLua, slot: &dcso3::net::SlotId) -> u32 {
        let mut total = 0u32;
        let mut counted: FxHashSet<String> = FxHashSet::default();
        if let Ok(ac) = self.ephemeral.slot_instance_unit(lua, slot) {
            if let Ok(Some(cargos)) = ac.get_cargos_on_board() {
                let _ = cargos.for_each(|c| {
                    let Ok(c) = c else {
                        return Ok(());
                    };
                    let Ok(name) = c.get_name() else {
                        return Ok(());
                    };
                    counted.insert(String::from(name.as_str()));
                    match c.get_cargo_weight() {
                        Ok(w) if w > 0. => {
                            total = total.saturating_add(w.round() as u32);
                        }
                        _ => {}
                    }
                    Ok(())
                });
            }
        }
        let Some(ucid) = self.ephemeral.players_by_slot.get(slot) else {
            return total;
        };
        let Some(player) = self.persisted.players.get(ucid) else {
            return total;
        };
        let Some((_, Some(inst))) = player.current_slot.as_ref() else {
            return total;
        };
        let side = player.side;
        // Sling / OBB: registry warehouse crates not listed on board (get_point usable).
        if self.dynamic_cargo_enabled() {
            let (hpt, fwd, landed, typ) = match self.ephemeral.slot_instance_unit(lua, slot) {
                Ok(unit) => {
                    let Ok(pt) = unit.get_point() else {
                        return total;
                    };
                    let landed = unit.in_air().map(|a| !a).unwrap_or(!inst.in_air);
                    let pos = unit.get_position().unwrap_or(inst.position);
                    (
                        pt.0,
                        Vector2::new(pos.x.x, pos.x.z),
                        landed,
                        inst.typ.0.clone(),
                    )
                }
                Err(_) => (
                    inst.position.p.0,
                    Vector2::new(inst.position.x.x, inst.position.x.z),
                    !inst.in_air,
                    inst.typ.0.clone(),
                ),
            };
            for (name, entry) in &self.persisted.dynamic_cargo_crates {
                if entry.side != side || counted.contains(name.as_str()) {
                    continue;
                }
                if let Some(uid) = self.persisted.units_by_name.get(name.as_str()) {
                    if let Some(unit) = self.persisted.units.get(uid) {
                        if self.persisted.crates.contains(&unit.group) {
                            continue;
                        }
                    }
                }
                let Ok(Static::Static(st)) = StaticObject::get_by_name(lua, name.as_str()) else {
                    continue;
                };
                match dynamic_cargo_is_aboard_unit(hpt, fwd, landed, typ.as_str(), &st) {
                    Ok(true) => {}
                    _ => continue,
                }
                match st.get_cargo_weight() {
                    Ok(w) if w > 0. => {
                        total = total.saturating_add(w.round() as u32);
                        counted.insert(name.clone());
                    }
                    _ => {}
                }
            }
        }
        // Dead Fowl still tagged ed_carrier (may be absent from getCargosOnBoard).
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
            if ed_carrier.as_ref() != Some(ucid) {
                continue;
            }
            let mut already = false;
            let mut any_dead = false;
            for uid in &group.units {
                let Some(unit) = self.persisted.units.get(uid) else {
                    continue;
                };
                if counted.contains(unit.name.as_str()) {
                    already = true;
                }
                if unit.dead {
                    any_dead = true;
                }
            }
            if already || !any_dead {
                continue;
            }
            total = total.saturating_add(spec.weight);
        }
        total
    }
}

/// MOOSE DynamicCargo bay footprint: oriented box (table values = half-extents from model origin)
/// plus sling sphere when airborne. Long bays (C-130) need OBB, not isotropic min(L,W).
pub(crate) fn dynamic_cargo_is_aboard_unit(
    unit_pt: Vector3,
    unit_forward_xz: Vector2,
    landed: bool,
    type_name: &str,
    cargo: &StaticObject,
) -> Result<bool> {
    let Some(dim) = dynamic_cargo_aircraft_dim(type_name) else {
        return Ok(false);
    };
    let cargo_pt = cargo.get_point()?;
    let cargo2 = Vector2::new(cargo_pt.0.x, cargo_pt.0.z);
    let hpos = Vector2::new(unit_pt.x, unit_pt.z);
    let rel = cargo2 - hpos;
    let flen = (unit_forward_xz.x * unit_forward_xz.x + unit_forward_xz.y * unit_forward_xz.y).sqrt();
    let fwd = if flen > 1e-3 {
        unit_forward_xz / flen
    } else {
        Vector2::new(1., 0.)
    };
    let right = Vector2::new(fwd.y, -fwd.x);
    let along = rel.x * fwd.x + rel.y * fwd.y;
    let lat = rel.x * right.x + rel.y * right.y;
    // Table length/width are half-extents (MOOSE CH-47 comment).
    let in_obb = if landed {
        // Ground: bay forward of origin only; closer spawn must not false-trigger aft box.
        along >= 0. && along <= dim.length && lat.abs() <= dim.width
    } else {
        along.abs() <= dim.length && lat.abs() <= dim.width
    };
    if in_obb {
        return Ok(true);
    }
    if !landed && dim.ropelength > 0. {
        let d3 = ((unit_pt.x - cargo_pt.0.x).powi(2)
            + (unit_pt.y - cargo_pt.0.y).powi(2)
            + (unit_pt.z - cargo_pt.0.z).powi(2))
        .sqrt();
        if d3 <= dim.ropelength + 1.0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Hook vs bay: cargo hanging below the airframe inside DCS rope length.
const SLING_BELOW_M: f64 = 2.5;

fn cargo_point_for_sling(lua: MizLua, ac: &Unit, crate_name: &str) -> Option<Vector3> {
    if let Ok(Static::Static(st)) = StaticObject::get_by_name(lua, crate_name) {
        if st.is_exist().unwrap_or(false) {
            if let Ok(p) = st.get_point() {
                return Some(p.0);
            }
        }
    }
    let Ok(Some(cargos)) = ac.get_cargos_on_board() else {
        return None;
    };
    let mut pt = None;
    let _ = cargos.for_each(|c| {
        let Ok(c) = c else {
            return Ok(());
        };
        if c.get_name()
            .map(|n| n.as_str() == crate_name)
            .unwrap_or(false)
        {
            pt = c.get_point().ok().map(|p| p.0);
        }
        Ok(())
    });
    pt
}

/// True when this Fowl crate is on the cargo hook, not in the F8 bay.
pub(crate) fn fowl_crate_is_on_sling(
    lua: MizLua,
    ac: &Unit,
    type_name: &str,
    crate_name: &str,
) -> bool {
    let Some(dim) = dynamic_cargo_aircraft_dim(type_name) else {
        return false;
    };
    if dim.ropelength <= 0. {
        return false;
    }
    let Ok(upt) = ac.get_point() else {
        return false;
    };
    let Some(cpt) = cargo_point_for_sling(lua, ac, crate_name) else {
        return false;
    };
    let below = upt.0.y - cpt.y;
    if below < SLING_BELOW_M {
        return false;
    }
    let d3 = ((upt.0.x - cpt.x).powi(2)
        + (upt.0.y - cpt.y).powi(2)
        + (upt.0.z - cpt.z).powi(2))
    .sqrt();
    d3 <= dim.ropelength + 1.0
}

impl Db {
    pub(crate) fn player_airframe_in_air(&self, lua: MizLua, ucid: &Ucid) -> bool {
        let Some(player) = self.persisted.players.get(ucid) else {
            return false;
        };
        let Some((slot, inst)) = player.current_slot.as_ref() else {
            return false;
        };
        if let Ok(u) = self.ephemeral.slot_instance_unit(lua, slot) {
            return u.in_air().unwrap_or(false);
        }
        inst.as_ref().map(|i| i.in_air).unwrap_or(false)
    }

    pub(crate) fn fowl_crate_is_slung_by_player(
        &self,
        lua: MizLua,
        ucid: &Ucid,
        crate_name: &str,
    ) -> bool {
        let Some(player) = self.persisted.players.get(ucid) else {
            return false;
        };
        let Some((slot, Some(inst))) = player.current_slot.as_ref() else {
            return false;
        };
        let Ok(ac) = self.ephemeral.slot_instance_unit(lua, slot) else {
            return false;
        };
        fowl_crate_is_on_sling(lua, &ac, inst.typ.as_str(), crate_name)
    }
}

/// True if `Unit.getCargosOnBoard` still lists this crate name (F8 / ghost slots).
pub(crate) fn unit_has_cargo_named(ac: &Unit, crate_name: &str) -> bool {
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
}

/// Count all entries returned by `Unit.getCargosOnBoard` (including nameless ghosts).
pub(crate) fn ed_bay_cargo_count(ac: &Unit) -> u32 {
    let Ok(Some(cargos)) = ac.get_cargos_on_board() else {
        return 0;
    };
    let mut n = 0u32;
    let _ = cargos.for_each(|c| {
        if c.is_ok() {
            n += 1;
        }
        Ok(())
    });
    n
}

fn delivery_tons(weight_kg: f64) -> f64 {
    (weight_kg / 1000.).max(1.)
}

fn points_from_tons(tons: f64, rate_per_ton: u32) -> i32 {
    (tons * rate_per_ton as f64).round() as i32
}

fn carrier_ucid_if_aboard(
    db: &Db,
    lua: MizLua,
    side: Side,
    cargo: &StaticObject,
) -> Option<Ucid> {
    let cargo_name = cargo.get_name().ok();
    for (slot, ucid) in &db.ephemeral.players_by_slot {
        let Some(player) = db.persisted.players.get(ucid) else {
            continue;
        };
        if player.side != side {
            continue;
        }
        let Some((_, Some(inst))) = player.current_slot.as_ref() else {
            continue;
        };
        if let (Some(cname), Ok(ac)) = (cargo_name.as_ref(), db.ephemeral.slot_instance_unit(lua, slot))
        {
            if unit_has_cargo_named(&ac, cname.as_str()) {
                return Some(*ucid);
            }
        }
        let (hpt, fwd, landed, typ) = match db.ephemeral.slot_instance_unit(lua, slot) {
            Ok(unit) => {
                let Ok(pt) = unit.get_point() else {
                    continue;
                };
                let landed = unit.in_air().map(|a| !a).unwrap_or(!inst.in_air);
                let pos = unit.get_position().unwrap_or(inst.position);
                (
                    pt.0,
                    Vector2::new(pos.x.x, pos.x.z),
                    landed,
                    inst.typ.0.as_str(),
                )
            }
            Err(_) => (
                inst.position.p.0,
                Vector2::new(inst.position.x.x, inst.position.x.z),
                !inst.in_air,
                inst.typ.0.as_str(),
            ),
        };
        if dynamic_cargo_is_aboard_unit(hpt, fwd, landed, typ, cargo).unwrap_or(false) {
            return Some(*ucid);
        }
    }
    None
}

fn dynamic_cargo_is_aboard_transport(
    db: &Db,
    lua: MizLua,
    side: Side,
    cargo: &StaticObject,
) -> Result<bool> {
    Ok(carrier_ucid_if_aboard(db, lua, side, cargo).is_some())
}

/// Player transport bay/sling footprint (any shared-dim airframe in slot).
pub(crate) fn dynamic_cargo_is_aboard_any_player_transport(
    db: &Db,
    lua: MizLua,
    side: Side,
    cargo: &StaticObject,
) -> Result<bool> {
    dynamic_cargo_is_aboard_transport(db, lua, side, cargo)
}

fn snapshot_cargo_warehouse(
    lua: MizLua,
    st: &StaticObject,
) -> Result<(MapS<String, u32>, MapS<LiquidType, u32>)> {
    let wh = Warehouse::get_cargo_as_warehouse(lua, st)?;
    let inv = wh.get_inventory(None)?;
    let mut equipment = MapS::default();
    let mut ingest = |items: dcso3::warehouse::ItemInventory<'_>| -> Result<()> {
        items.for_each(|name, qty| {
            if qty > 0 {
                equipment.insert_cow(name, qty);
            }
            Ok(())
        })
    };
    if let Ok(w) = inv.weapons() {
        ingest(w)?;
    }
    if let Ok(a) = inv.aircraft() {
        ingest(a)?;
    }
    let mut liquids = MapS::default();
    if let Ok(liq) = inv.liquids() {
        liq.for_each(|typ, qty| {
            if qty > 0 {
                liquids.insert_cow(typ, qty);
            }
            Ok(())
        })?;
    }
    Ok((equipment, liquids))
}

fn coalition_country_for_side(lua: MizLua, side: Side) -> Result<Country> {
    let miz = Miz::singleton(lua)?;
    let coa = miz.coalition(side)?;
    let countries = coa.countries()?;
    let country = countries
        .first()
        .with_context(|| format!("no countries for coalition {side:?}"))?;
    country.id()
}
