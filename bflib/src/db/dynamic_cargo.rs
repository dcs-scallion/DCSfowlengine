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
use compact_str::format_compact;
use dcso3::{
    coalition::{Coalition, Side, Static},
    country::Country,
    env::miz::Miz,
    net::Ucid,
    object::ObjectCategory,
    static_object::StaticObject,
    warehouse::{LiquidType, Warehouse},
    LuaEnv, MizLua, String, Vector2,
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
        "C-130J-30" => Some(DynamicCargoAircraftDim {
            width: 4.,
            length: 35.,
            ropelength: 0.,
        }),
        _ => None,
    }
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

    pub fn prune_missing_dynamic_cargo(&mut self, lua: MizLua) {
        if !self.dynamic_cargo_enabled() {
            return;
        }
        let gone: Vec<String> = self
            .persisted
            .dynamic_cargo_crates
            .into_iter()
            .filter_map(|(name, _)| {
                match StaticObject::get_by_name(lua, name.as_str()) {
                    Ok(Static::Static(st)) => match st.is_exist() {
                        Ok(true) => None,
                        _ => Some(name.clone()),
                    },
                    _ => Some(name.clone()),
                }
            })
            .collect();
        if gone.is_empty() {
            return;
        }
        let mut sync_oids: FxHashMap<ObjectiveId, ()> = FxHashMap::default();
        for name in gone {
            let Some(entry) = self.persisted.dynamic_cargo_crates.get(&name).cloned() else {
                continue;
            };
            // Keep source at checked-out level while crate still reserved.
            self.clamp_dynamic_cargo_checkout(entry.source);
            sync_oids.insert(entry.source, ());
            if let Some(pad) = self.objective_containing(entry.side, entry.pos()) {
                if pad != entry.source {
                    // DCS absorb credited the landing pad — strip it from Fowl then SyncTo.
                    if let Err(e) = self.debit_source_for_dynamic_cargo_checkout(
                        pad,
                        &entry.equipment,
                        &entry.liquids,
                    ) {
                        warn!("dynamic cargo absorb strip {pad:?}: {e:?}");
                    }
                }
                sync_oids.insert(pad, ());
            }
            info!(
                "dynamic cargo gone (DCS absorb or destroy): {} source={:?}",
                entry.name, entry.source
            );
            self.persisted.dynamic_cargo_crates.remove_cow(&name);
            self.clamp_dynamic_cargo_checkout(entry.source);
        }
        for oid in sync_oids.keys() {
            if let Err(e) = self.sync_objective_to_warehouse(lua, *oid) {
                warn!("dynamic cargo prune SyncTo {oid:?}: {e:?}");
            }
        }
        self.ephemeral.dirty();
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
        let names: Vec<String> = self
            .persisted
            .dynamic_cargo_crates
            .into_iter()
            .map(|(n, _)| n.clone())
            .collect();
        let mut dirty = false;
        for name in names {
            let Ok(Static::Static(st)) = StaticObject::get_by_name(lua, name.as_str()) else {
                continue;
            };
            let Ok(point) = st.get_point() else {
                continue;
            };
            let Ok((equipment, liquids)) = snapshot_cargo_warehouse(lua, &st) else {
                continue;
            };
            if let Some(entry) = self.persisted.dynamic_cargo_crates.get_mut_cow(&name) {
                entry.x = point.0.x;
                entry.y = point.0.z;
                entry.alt = point.0.y;
                entry.equipment = equipment;
                entry.liquids = liquids;
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
        let cfg_points = (
            self.dynamic_cargo_cfg().to_stock_points,
            self.dynamic_cargo_cfg().source_spawner_points,
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
            if cfg_points.0 > 0 {
                *point_awards.entry(*deliverer).or_default() += cfg_points.0 as i32;
            }
            if cfg_points.1 > 0 {
                *point_awards.entry(entry.spawner).or_default() += cfg_points.1 as i32;
            }
            self.persisted.dynamic_cargo_crates.remove_cow(&name);
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
}

/// MOOSE DynamicCargo: bay load = inside length×width while landed; sling = within rope 3D.
fn dynamic_cargo_is_aboard_transport(
    db: &Db,
    lua: MizLua,
    side: Side,
    cargo: &StaticObject,
) -> Result<bool> {
    let cargo_pt = cargo.get_point()?;
    let cargo2 = Vector2::new(cargo_pt.0.x, cargo_pt.0.z);
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
        let Some(dim) = dynamic_cargo_aircraft_dim(inst.typ.0.as_str()) else {
            continue;
        };
        let (hpt, landed) = match db.ephemeral.slot_instance_unit(lua, slot) {
            Ok(unit) => {
                let Ok(pt) = unit.get_point() else {
                    continue;
                };
                let landed = unit.in_air().map(|a| !a).unwrap_or(!inst.in_air);
                (pt.0, landed)
            }
            Err(_) => (
                inst.position.p.0,
                !inst.in_air,
            ),
        };
        let hpos = Vector2::new(hpt.x, hpt.z);
        let delta2 = (hpos - cargo2).magnitude();
        if landed && delta2 < dim.length && delta2 < dim.width {
            return Ok(true);
        }
        if !landed && dim.ropelength > 0. {
            let d3 = ((hpt.x - cargo_pt.0.x).powi(2)
                + (hpt.y - cargo_pt.0.y).powi(2)
                + (hpt.z - cargo_pt.0.z).powi(2))
            .sqrt();
            if d3 <= dim.ropelength + 1.0 {
                return Ok(true);
            }
        }
    }
    Ok(false)
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
