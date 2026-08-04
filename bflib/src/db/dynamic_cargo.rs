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
    logistics::{dcs_liquid_kg_to_fowl_tons, objective_liquids_stored_as_tons},
    Db, MapM, MapS,
};
use anyhow::{anyhow, bail, Context, Result};
use bfprotocols::cfg::DynamicCargoDeliveryCfg;
use bfprotocols::db::objective::{ObjectiveId, ObjectiveKind};
use chrono::prelude::*;
use compact_str::format_compact;
use dcso3::{
    coalition::{Coalition, Side, Static},
    country::Country,
    env::miz::Miz,
    net::Ucid,
    object::ObjectCategory,
    static_object::StaticObject,
    unit::Unit,
    warehouse::{LiquidType, Warehouse},
    LuaEnv, MizLua, String, Vector2, Vector3,
};
use fxhash::FxHashMap;
use log::{info, warn};
use mlua::{FromLua, Value};
use serde_derive::{Deserialize, Serialize};

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
}

fn default_true() -> bool {
    true
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

/// MOOSE `DYNAMICCARGO.AircraftDimensions` (half-box around unit origin).
struct DynamicCargoAircraftDim {
    width: f64,
    length: f64,
    ropelength: f64,
}

fn dynamic_cargo_aircraft_dim(type_name: &str) -> Option<DynamicCargoAircraftDim> {
    match type_name {
        "CH-47Fbl1" => Some(DynamicCargoAircraftDim {
            width: 4.,
            length: 11.,
            ropelength: 30.,
        }),
        "Mi-8MTV2" | "Mi-8MT" => Some(DynamicCargoAircraftDim {
            width: 6.,
            length: 15.,
            ropelength: 30.,
        }),
        "UH-1H" => Some(DynamicCargoAircraftDim {
            width: 2.5,
            length: 5.,
            ropelength: 30.,
        }),
        "Mi-24P" => Some(DynamicCargoAircraftDim {
            width: 3.,
            length: 6.,
            ropelength: 30.,
        }),
        "C-130J-30" => Some(DynamicCargoAircraftDim {
            width: 4.,
            length: 35.,
            ropelength: 0.,
        }),
        _ => None,
    }
}

/// Nose packing distance: past bay OBB so ground crates are not "aboard" / colliding.
pub(crate) fn fowl_crate_nose_distance_m(type_name: &str) -> f64 {
    const MIN_NOSE_M: f64 = 25.;
    const PAST_BAY_M: f64 = 5.;
    dynamic_cargo_aircraft_dim(type_name)
        .map(|d| (d.length + PAST_BAY_M).max(MIN_NOSE_M))
        .unwrap_or(MIN_NOSE_M)
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
        self.debit_source_for_dynamic_cargo_checkout(source, &equipment, &liquids)?;
        self.insert_dynamic_cargo_crate(
            lua,
            DynamicCargoCrate {
                index: 0,
                name,
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
            },
        )?;
        if let Err(e) = self.sync_objective_to_warehouse(lua, source) {
            warn!("dynamic cargo checkout SyncTo {source:?}: {e:?}");
        }
        Ok(())
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
            if let Err(e) = self.sync_objective_to_warehouse(lua, oid) {
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
                self.clamp_dynamic_cargo_checkout(entry.source);
                info!("dynamic cargo: removed Fowl crate registry entry {name}");
                self.ephemeral.dirty();
            }
        }
        const REGISTER_GRACE: chrono::Duration = chrono::Duration::seconds(15);
        let now = Utc::now();
        let gone: Vec<String> = self
            .persisted
            .dynamic_cargo_crates
            .into_iter()
            .filter_map(|(name, _)| {
                // Still in an ED bay — F8 load hides/moves the world static.
                if self.dynamic_cargo_name_on_any_board(lua, name.as_str()) {
                    return None;
                }
                let world_gone = match StaticObject::get_by_name(lua, name.as_str()) {
                    Ok(Static::Static(st)) => !st.is_exist().unwrap_or(false),
                    _ => true,
                };
                if !world_gone {
                    return None;
                }
                // Brief grace: fill/load UI can briefly drop the static.
                if self
                    .ephemeral
                    .dynamic_cargo_registered_at
                    .get(name)
                    .is_some_and(|t| now - *t < REGISTER_GRACE)
                {
                    return None;
                }
                Some(name.clone())
            })
            .collect();
        if gone.is_empty() {
            return;
        }
        let mut sync_oids: FxHashMap<ObjectiveId, ()> = FxHashMap::default();
        let mut point_awards: FxHashMap<Ucid, i32> = FxHashMap::default();
        let mut log_awards: FxHashMap<Ucid, ()> = FxHashMap::default();
        for name in gone {
            let Some(entry) = self.persisted.dynamic_cargo_crates.get(&name).cloned() else {
                continue;
            };
            self.clamp_dynamic_cargo_checkout(entry.source);
            sync_oids.insert(entry.source, ());
            let dest = self
                .resolve_dynamic_cargo_absorb_dest(&entry)
                .or_else(|| {
                    // Round-trip / multi-stop: transported crate absorbed at source pad.
                    entry.last_carrier.is_some().then_some(entry.source)
                });
            match dest {
                Some(pad) => {
                    if let Err(e) = self.credit_objective_from_dynamic_cargo(
                        pad,
                        &entry.equipment,
                        &entry.liquids,
                    ) {
                        warn!("dynamic cargo absorb credit {pad:?}: {e:?}");
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
                    sync_oids.insert(pad, ());
                }
                None => {
                    info!(
                        "dynamic cargo gone (no delivery): {} source={:?} pos=({:.0},{:.0}) carrier={:?} weight_kg={:.0}",
                        entry.name,
                        entry.source,
                        entry.x,
                        entry.y,
                        entry.last_carrier,
                        entry.last_weight_kg
                    );
                }
            }
            self.ephemeral.dynamic_cargo_registered_at.remove(&name);
            self.persisted.dynamic_cargo_crates.remove_cow(&name);
            self.clamp_dynamic_cargo_checkout(entry.source);
        }
        for oid in sync_oids.keys() {
            if let Err(e) = self.sync_objective_to_warehouse(lua, *oid) {
                warn!("dynamic cargo prune SyncTo {oid:?}: {e:?}");
            }
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
            if let Some(entry) = self.persisted.dynamic_cargo_crates.get_mut_cow(&name) {
                // Prefer carrier aircraft pos while aboard — cargo get_point can stay at source.
                if let Some((x, y, alt)) = carrier_pos {
                    entry.x = x;
                    entry.y = y;
                    entry.alt = alt;
                } else {
                    entry.x = point.0.x;
                    entry.y = point.0.z;
                    entry.alt = point.0.y;
                }
                entry.equipment = equipment;
                entry.liquids = liquids;
                if weight > 0. {
                    entry.last_weight_kg = weight;
                }
                if let Some(ucid) = carrier {
                    entry.last_carrier = Some(ucid);
                }
                dirty = true;
            }
        }
        if dirty {
            self.ephemeral.dirty();
        }
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
            let Some(entry) = self.persisted.dynamic_cargo_crates.get(&name).cloned() else {
                continue;
            };
            let Ok(Static::Static(st)) = StaticObject::get_by_name(lua, name.as_str()) else {
                self.persisted.dynamic_cargo_crates.remove_cow(&name);
                continue;
            };
            if dynamic_cargo_is_aboard_transport(self, lua, entry.side, &st)? {
                skipped_loaded += 1;
                continue;
            }
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
        self.sync_objective_to_warehouse(lua, dest_oid)
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

    /// Sum of ED warehouse dynamic cargo masses (kg) aboard this slot (excludes Fowl crates).
    pub fn loaded_warehouse_dynamic_cargo_weight_kg(
        &self,
        lua: MizLua,
        slot: &dcso3::net::SlotId,
    ) -> u32 {
        if !self.dynamic_cargo_enabled() {
            return 0;
        }
        let Some(ucid) = self.ephemeral.players_by_slot.get(slot) else {
            return 0;
        };
        let Some(player) = self.persisted.players.get(ucid) else {
            return 0;
        };
        let Some((_, Some(inst))) = player.current_slot.as_ref() else {
            return 0;
        };
        let side = player.side;
        let (hpt, fwd, landed, typ) = match self.ephemeral.slot_instance_unit(lua, slot) {
            Ok(unit) => {
                let Ok(pt) = unit.get_point() else {
                    return 0;
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
        let mut total = 0u32;
        for (name, entry) in &self.persisted.dynamic_cargo_crates {
            if entry.side != side {
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
                }
                _ => {}
            }
        }
        total
    }

    /// Sum of ED dynamic cargo crate masses (kg) currently aboard this slot's airframe.
    /// Warehouse registry crates + Fowl crates loaded via F8/sling.
    pub fn loaded_dynamic_cargo_weight_kg(&self, lua: MizLua, slot: &dcso3::net::SlotId) -> u32 {
        let warehouse = self.loaded_warehouse_dynamic_cargo_weight_kg(lua, slot);
        let fowl: u32 = self
            .fowl_crates_on_ed_bay(lua, slot)
            .into_iter()
            .map(|(_, w)| w)
            .sum();
        warehouse.saturating_add(fowl)
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
    if along.abs() <= dim.length && lat.abs() <= dim.width {
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
