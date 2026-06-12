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
    ephemeral::{Equipment, Production, WarehouseResourceMeta},
    objective::Objective,
    persisted::Persisted,
    Db, Map, MapS, SetS,
};
use crate::{admin::WarehouseKind, maybe, objective, objective_mut, Task};
use anyhow::{anyhow, bail, Context, Result};
use bfprotocols::{
    cfg::Vehicle,
    db::objective::{ObjectiveId, ObjectiveKind},
    fowl_miz_export::{FowlMizExport, ObjectiveCoalitionStock, ObjectiveStockItem},
    perf::{Perf, PerfInner},
    stats::Stat,
    tisp::ship_pad_display_name,
};
use chrono::{prelude::*, Duration};
use compact_str::{format_compact, CompactString};
use dcso3::{
    airbase::Airbase,
    coalition::Side,
    land::{Land, SurfaceType},
    object::DcsObject,
    perf::record_perf,
    warehouse::{self, LiquidType},
    world::World,
    LuaVec2, MizLua, String, Vector2,
};
use fxhash::{FxHashMap, FxHashSet};
use log::{debug, error, info, warn};
use serde_derive::{Deserialize, Serialize};
use smallvec::{smallvec, SmallVec};

fn objective_airbase_on_water(lua: MizLua, obj: &Objective) -> Result<bool> {
    if !matches!(obj.kind, ObjectiveKind::Airbase) {
        return Ok(false);
    }
    let land = Land::singleton(lua)?;
    let st = land.get_surface_type(LuaVec2(obj.zone.pos()))?;
    Ok(matches!(
        st,
        SurfaceType::Water | SurfaceType::ShallowWater
    ))
}

fn scale_capacity_by_percent_floor(capacity: u32, pct: u8) -> u32 {
    capacity.saturating_mul(pct as u32) / 100
}
use std::{
    cmp::{max, min},
    collections::hash_map::Entry,
    mem,
    ops::{AddAssign, SubAssign},
    sync::Arc,
};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug)]
struct WarehouseSyncSkipped;

impl std::fmt::Display for WarehouseSyncSkipped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("warehouse sync skipped")
    }
}

impl std::error::Error for WarehouseSyncSkipped {}

fn warehouse_sync_skip(e: &anyhow::Error) -> bool {
    e.downcast_ref::<WarehouseSyncSkipped>().is_some()
}

#[derive(Debug, Clone)]
pub enum LogiStage {
    Complete {
        last_tick: DateTime<Utc>,
    },
    SyncFromWarehouses {
        objectives: SmallVec<[ObjectiveId; 128]>,
    },
    SyncToWarehouses {
        objectives: SmallVec<[ObjectiveId; 128]>,
    },
    ExecuteTransfers {
        transfers: Vec<Transfer>,
    },
    Init,
}

impl Default for LogiStage {
    fn default() -> Self {
        Self::Init
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Inventory {
    pub stored: u32,
    pub capacity: u32,
}

impl Inventory {
    pub fn percent(&self) -> Option<u8> {
        if self.capacity == 0 {
            None
        } else {
            let stored: f32 = self.stored as f32;
            let capacity: f32 = self.capacity as f32;
            Some(min(100, ((stored / capacity) * 100.) as u32) as u8)
        }
    }

    pub fn reduce(&mut self, percent: f32) -> u32 {
        if self.stored == 0 {
            0
        } else {
            let taken = max(1, (self.stored as f32 * percent) as u32);
            self.stored -= taken;
            taken
        }
    }
}

impl AddAssign<u32> for Inventory {
    fn add_assign(&mut self, rhs: u32) {
        let qty = self.stored + rhs;
        if qty > self.capacity {
            self.stored = self.capacity
        } else {
            self.stored = qty
        }
    }
}

impl SubAssign<u32> for Inventory {
    fn sub_assign(&mut self, rhs: u32) {
        if rhs > self.stored {
            self.stored = 0
        } else {
            self.stored = self.stored - rhs;
        }
    }
}

#[derive(Debug, Clone)]
enum TransferItem {
    Equipment(String),
    Liquid(LiquidType),
}

#[derive(Debug, Clone)]
pub struct Transfer {
    source: ObjectiveId,
    target: ObjectiveId,
    amount: u32,
    item: TransferItem,
}

impl Transfer {
    pub(super) fn target(&self) -> ObjectiveId {
        self.target
    }

    fn execute(&self, db: &mut Persisted, to_bg: &Option<UnboundedSender<Task>>) -> Result<()> {
        let src = db
            .objectives
            .get_mut_cow(&self.source)
            .ok_or_else(|| anyhow!("no such objective {:?}", self.source))?;
        match &self.item {
            TransferItem::Equipment(name) => {
                let d = &mut src.warehouse.equipment[name].stored;
                *d -= self.amount;
                if let Some(to_bg) = to_bg.as_ref() {
                    let _ = to_bg.send(Task::Stat(Stat::EquipmentInventory {
                        id: src.id,
                        item: name.clone(),
                        amount: *d,
                    }));
                }
            }
            TransferItem::Liquid(name) => {
                let d = &mut src.warehouse.liquids[name].stored;
                *d -= self.amount;
                if let Some(to_bg) = to_bg.as_ref() {
                    let _ = to_bg.send(Task::Stat(Stat::LiquidInventory {
                        id: src.id,
                        item: *name,
                        amount: *d,
                    }));
                }
            }
        }
        let dst = db
            .objectives
            .get_mut_cow(&self.target)
            .ok_or_else(|| anyhow!("no such objective {:?}", self.target))?;
        match &self.item {
            TransferItem::Equipment(name) => {
                let d = &mut dst
                    .warehouse
                    .equipment
                    .get_or_default_cow(name.clone())
                    .stored;
                *d += self.amount;
                if let Some(to_bg) = to_bg.as_ref() {
                    let _ = to_bg.send(Task::Stat(Stat::EquipmentInventory {
                        id: dst.id,
                        item: name.clone(),
                        amount: *d,
                    }));
                }
            }
            TransferItem::Liquid(name) => {
                let d = &mut dst.warehouse.liquids.get_or_default_cow(*name).stored;
                *d += self.amount;
                if let Some(to_bg) = to_bg.as_ref() {
                    let _ = to_bg.send(Task::Stat(Stat::LiquidInventory {
                        id: dst.id,
                        item: *name,
                        amount: *d,
                    }));
                }
            }
        }
        Ok(())
    }
}

struct Needed<'a> {
    oid: &'a ObjectiveId,
    obj: &'a Objective,
    demanded: u32,
    allocated: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Warehouse {
    pub(super) base_equipment: Map<String, Inventory>,
    pub(super) equipment: Map<String, Inventory>,
    pub(super) liquids: MapS<LiquidType, Inventory>,
    pub(super) supplier: Option<ObjectiveId>,
    pub(super) destination: SetS<ObjectiveId>,
}

fn sync_obj_to_warehouse(
    obj: &Objective,
    warehouse: &warehouse::Warehouse,
    export_farp_liquids_tons: bool,
) -> Result<()> {
    let perf = unsafe { Perf::get_mut() };
    let perf = Arc::make_mut(&mut perf.inner);
    for (item, inv) in &obj.warehouse.equipment {
        perf.logistics_items.insert((item.clone(), obj.id));
        let current = warehouse
            .get_item_count(item.clone())
            .with_context(|| format_compact!("getting item count for {item}"))?;
        if current < inv.stored {
            warehouse
                .add_item(item.clone(), inv.stored - current)
                .with_context(|| format_compact!("adding item {item}"))?;
        } else if current > inv.stored {
            warehouse
                .remove_item(item.clone(), current - inv.stored)
                .with_context(|| format_compact!("removing item {item}"))?;
        }
    }
    for (name, inv) in &obj.warehouse.liquids {
        let target_kg = if export_farp_liquids_tons {
            fowl_liquid_tons_to_dcs_kg(inv.stored)
        } else {
            inv.stored
        };
        let current = warehouse
            .get_liquid_amount(*name)
            .with_context(|| format_compact!("getting liquid amount for {name:?}"))?;
        if current < target_kg {
            warehouse
                .add_liquid(*name, target_kg - current)
                .with_context(|| format_compact!("adding liquid {name:?}"))?;
        } else if current > target_kg {
            warehouse
                .remove_liquid(*name, current - target_kg)
                .with_context(|| format_compact!("removing liquid {name:?}"))?;
        }
    }
    Ok(())
}

fn sync_warehouse_to_obj(
    obj: &mut Objective,
    warehouse: &warehouse::Warehouse,
    export_farp_liquids_tons: bool,
) -> Result<()> {
    for (name, inv) in obj.warehouse.equipment.iter_mut_cow() {
        inv.stored = warehouse.get_item_count(name.clone())?;
    }
    for (name, inv) in obj.warehouse.liquids.iter_mut_cow() {
        let kg = warehouse.get_liquid_amount(*name)?;
        inv.stored = if export_farp_liquids_tons {
            dcs_liquid_kg_to_fowl_tons(kg)
        } else {
            kg
        };
    }
    Ok(())
}

/// Drop DCS rows not in virtual; optional full profile row registration (capture reseed).
fn apply_virtual_warehouse_to_dcs<'lua>(
    obj: &Objective,
    warehouse: &warehouse::Warehouse<'lua>,
    export_farp_liquids_tons: bool,
    establish_profile_rows: bool,
) -> Result<()> {
    let inv = warehouse
        .get_inventory(None)
        .context("warehouse getInventory for virtual apply")?;
    let trim_equipment = |items: warehouse::ItemInventory<'_>| -> Result<()> {
        items.for_each(|name, qty| {
            if qty == 0 {
                return Ok(());
            }
            let in_virtual = obj.warehouse.equipment.get(name.as_str()).is_some();
            if !in_virtual {
                warehouse
                    .remove_item(name.clone(), qty)
                    .with_context(|| format_compact!("remove_item orphan {name}"))?;
                return Ok(());
            }
            let Some(inv) = obj.warehouse.equipment.get(name.as_str()) else {
                return Ok(());
            };
            let target = inv.stored;
            if qty != target {
                warehouse
                    .set_item(name.clone(), target)
                    .with_context(|| format_compact!("set_item {name} to {target}"))?;
            }
            Ok(())
        })
    };
    trim_equipment(inv.weapons().context("warehouse weapons for virtual apply")?)?;
    trim_equipment(inv.aircraft().context("warehouse aircraft for virtual apply")?)?;
    for typ in LiquidType::ALL {
        let target = obj
            .warehouse
            .liquids
            .get(&typ)
            .map(|i| {
                if export_farp_liquids_tons {
                    fowl_liquid_tons_to_dcs_kg(i.stored)
                } else {
                    i.stored
                }
            })
            .unwrap_or(0);
        let current = warehouse
            .get_liquid_amount(typ)
            .with_context(|| format_compact!("get_liquid_amount {typ:?}"))?;
        if obj.warehouse.liquids.get(&typ).is_none() {
            if current > 0 {
                warehouse
                    .remove_liquid(typ, current)
                    .with_context(|| format_compact!("remove_liquid orphan {typ:?}"))?;
            }
        } else if current != target {
            warehouse
                .set_liquid_amount(typ, target)
                .with_context(|| format_compact!("set_liquid_amount {typ:?} to {target} kg"))?;
        }
    }
    if establish_profile_rows {
        for (name, inv) in &obj.warehouse.equipment {
            warehouse
                .set_item(name.clone(), inv.stored)
                .with_context(|| format_compact!("establish warehouse item {name}"))?;
        }
        for (typ, inv) in &obj.warehouse.liquids {
            let kg = if export_farp_liquids_tons {
                fowl_liquid_tons_to_dcs_kg(inv.stored)
            } else {
                inv.stored
            };
            warehouse
                .set_liquid_amount(*typ, kg)
                .with_context(|| format_compact!("establish warehouse liquid {typ:?}"))?;
        }
    }
    Ok(())
}

/// Ground DEP FARP: ME pad may still carry full stock; drop DCS rows not in virtual or above virtual `stored`.
fn reconcile_dcs_warehouse_to_virtual<'lua>(
    obj: &Objective,
    warehouse: &warehouse::Warehouse<'lua>,
) -> Result<()> {
    apply_virtual_warehouse_to_dcs(obj, warehouse, true, false)
}

fn equipment_capacity_for_discovered_row(
    obj: &Objective,
    name: &str,
    stored: u32,
    export: &FowlMizExport,
    resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
    whcfg: Option<&bfprotocols::cfg::WarehouseConfig>,
    production: Option<&Production>,
    on_water: bool,
) -> u32 {
    if let Some(profile) = objective_coalition_stock_for_objective(export, obj) {
        let item = if objective_is_ground_dep_farp(obj) {
            profile_export_equipment_item(profile, name, resource_meta)
        } else {
            profile.equipment.get(name)
        };
        if let Some(item) = item {
            if item.baseline > 0 {
                return item.baseline;
            }
        }
    }
    if let Some(inv) = obj.warehouse.equipment.get(name) {
        if inv.capacity > 0 {
            return inv.capacity.max(stored);
        }
    }
    if let (Some(whcfg), Some(prod)) = (whcfg, production) {
        if let Some(eq) = prod.equipment.get(name) {
            return whcfg
                .capacity(&obj.kind, on_water, eq.production)
                .max(stored);
        }
    }
    stored.max(1)
}

fn liquid_capacity_for_discovered_row(
    obj: &Objective,
    typ: LiquidType,
    stored: u32,
    export: &FowlMizExport,
    whcfg: Option<&bfprotocols::cfg::WarehouseConfig>,
    production: Option<&Production>,
    on_water: bool,
) -> u32 {
    if let Some(profile) = objective_coalition_stock_for_objective(export, obj) {
        for (key, liq) in &profile.liquids {
            if liquid_type_from_export_key(key).ok() == Some(typ) && liq.baseline > 0 {
                return liq.baseline;
            }
        }
    }
    if let Some(inv) = obj.warehouse.liquids.get(&typ) {
        if inv.capacity > 0 {
            return inv.capacity.max(stored);
        }
    }
    if let (Some(whcfg), Some(prod)) = (whcfg, production) {
        if let Some(qty) = prod.liquids.get(&typ) {
            return whcfg.capacity(&obj.kind, on_water, *qty).max(stored);
        }
    }
    stored.max(1)
}

/// FARP pads: virtual rows keyed by pad template when runtime objective name differs from export.
fn discover_dcs_warehouse_into_obj(
    obj: &mut Objective,
    warehouse: &warehouse::Warehouse,
    export: &FowlMizExport,
    resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
    whcfg: Option<&bfprotocols::cfg::WarehouseConfig>,
    production: Option<&Production>,
    on_water: bool,
) -> Result<()> {
    let inv = warehouse
        .get_inventory(None)
        .context("warehouse getInventory for virtual hydrate")?;
    let profile = objective_coalition_stock_for_objective(export, obj);
    let mut ingest_equipment =
        |items: warehouse::ItemInventory<'_>| -> Result<()> {
            items.for_each(|name, stored| {
                if stored == 0 {
                    return Ok(());
                }
                let meta = resource_meta.get(&name).copied();
                if !equipment_allowed_for_objective(
                    export,
                    obj,
                    obj.owner,
                    name.as_str(),
                    meta,
                ) {
                    return Ok(());
                }
                if let Some(profile) = profile {
                    if !discover_equipment_allowed_by_export_profile(
                        obj,
                        profile,
                        name.as_str(),
                        resource_meta,
                    ) && !profile.equipment.is_empty()
                    {
                        return Ok(());
                    }
                }
                let capacity = equipment_capacity_for_discovered_row(
                    obj,
                    name.as_str(),
                    stored,
                    export,
                    resource_meta,
                    whcfg,
                    production,
                    on_water,
                );
                let row = obj
                    .warehouse
                    .equipment
                    .get_or_default_cow(String::from(name.as_str()));
                row.stored = stored;
                row.capacity = capacity.max(stored);
                Ok(())
            })
        };
    ingest_equipment(inv.weapons().context("warehouse weapon inventory")?)?;
    ingest_equipment(inv.aircraft().context("warehouse aircraft inventory")?)?;
    inv.liquids()
        .context("warehouse liquid inventory")?
        .for_each(|typ, stored| {
            if stored == 0 {
                return Ok(());
            }
            if let Some(profile) = profile {
                let keep = profile.liquids.keys().any(|k| {
                    liquid_type_from_export_key(k).ok() == Some(typ)
                });
                if !profile.liquids.is_empty() && !keep {
                    return Ok(());
                }
            }
            let capacity = liquid_capacity_for_discovered_row(
                obj,
                typ,
                stored,
                export,
                whcfg,
                production,
                on_water,
            );
            let row = obj.warehouse.liquids.get_or_default_cow(typ);
            row.stored = stored;
            row.capacity = capacity.max(stored);
            Ok(())
        })?;
    Ok(())
}

fn get_supplier<'lua>(lua: MizLua<'lua>, template: String) -> Result<warehouse::Warehouse<'lua>> {
    Airbase::get_by_name(lua, template.clone())
        .with_context(|| format_compact!("getting airbase {}", template))?
        .get_warehouse()
        .context("getting warehouse")
}

fn export_has_any_weapon_allowlist(export: &FowlMizExport) -> bool {
    !export.blue_weapon_ws.is_empty() || !export.red_weapon_ws.is_empty()
}

fn export_has_objective_stock(export: &FowlMizExport) -> bool {
    !export.objective_stock.is_empty()
}

fn objective_coalition_stock_for_side<'a>(
    export: &'a FowlMizExport,
    objective_name: &str,
    side: Side,
) -> Option<&'a ObjectiveCoalitionStock> {
    let by_coa = export.objective_stock.get(objective_name)?;
    Some(match side {
        Side::Blue => &by_coa.blue,
        Side::Red => &by_coa.red,
        Side::Neutral => return None,
    })
}

/// Pozemní DEP FARP (`DEPBFARPPAD*`); not naval deck pads or carrier airbases.
fn objective_is_ground_dep_farp(obj: &Objective) -> bool {
    matches!(obj.kind, ObjectiveKind::Farp { mobile: false, .. })
}

/// Export `objective_stock` for mobile naval pads (allowlist / defaults), not DEP pads.
fn farp_export_lookup_keys(obj: &Objective) -> Option<SmallVec<[String; 3]>> {
    let ObjectiveKind::Farp {
        mobile: true,
        pad_template,
        ..
    } = &obj.kind
    else {
        return None;
    };
    let mut keys: SmallVec<[String; 3]> = smallvec![];
    let display = ship_pad_display_name(pad_template.as_str());
    if display.as_str() != obj.name.as_str() {
        keys.push(String::from(display));
    }
    if pad_template.as_str() != obj.name.as_str() {
        keys.push(pad_template.clone());
    }
    if keys.is_empty() {
        None
    } else {
        Some(keys)
    }
}

fn dep_farp_export_profile<'a>(
    export: &'a FowlMizExport,
    obj: &Objective,
) -> Option<&'a ObjectiveCoalitionStock> {
    if !objective_is_ground_dep_farp(obj) {
        return None;
    }
    let ObjectiveKind::Farp { pad_template, .. } = &obj.kind else {
        return None;
    };
    objective_coalition_stock_for_side(export, pad_template.as_str(), obj.owner).or_else(|| {
        objective_coalition_stock_for_side(export, obj.name.as_str(), obj.owner)
    })
}

/// Ground DEP FARP with per-pad export stock: 20% initial fill, DCS sync in tons, virtual prune.
fn objective_is_ground_dep_farp_export(export: &FowlMizExport, obj: &Objective) -> bool {
    dep_farp_export_profile(export, obj).is_some()
}

/// Persisted deploy: virtual rows already set (e.g. after 20% init); do not hydrate from ME pad template on load.
fn dep_farp_has_persisted_virtual_stock(obj: &Objective) -> bool {
    if !objective_is_ground_dep_farp(obj) {
        return false;
    }
    for (_, inv) in &obj.warehouse.equipment {
        if inv.capacity > 0 {
            return true;
        }
    }
    for (_, inv) in &obj.warehouse.liquids {
        if inv.capacity > 0 {
            return true;
        }
    }
    false
}

fn objective_coalition_stock_for_objective<'a>(
    export: &'a FowlMizExport,
    obj: &Objective,
) -> Option<&'a ObjectiveCoalitionStock> {
    if let Some(stock) =
        objective_coalition_stock_for_side(export, obj.name.as_str(), obj.owner)
    {
        return Some(stock);
    }
    let keys = farp_export_lookup_keys(obj)?;
    for key in keys {
        if let Some(stock) = objective_coalition_stock_for_side(export, key.as_str(), obj.owner) {
            return Some(stock);
        }
    }
    None
}

fn objective_defaults_for_objective<'a>(
    export: &'a FowlMizExport,
    obj: &Objective,
    side: Side,
) -> Option<(&'a [std::string::String], &'a [[i32; 4]])> {
    if let Some(d) = objective_defaults_for_side(export, obj.name.as_str(), side) {
        return Some(d);
    }
    let keys = farp_export_lookup_keys(obj)?;
    for key in keys {
        if let Some(d) = objective_defaults_for_side(export, key.as_str(), side) {
            return Some(d);
        }
    }
    None
}

fn liquid_type_from_export_key(key: &str) -> Result<LiquidType> {
    match key {
        "jet_fuel" => Ok(LiquidType::JetFuel),
        "gasoline" => Ok(LiquidType::Avgas),
        "diesel" => Ok(LiquidType::Diesel),
        "methanol_mixture" => Ok(LiquidType::MW50),
        _ => bail!("unknown liquid key in Fowl export: {key}"),
    }
}

/// Fowl export / bftools `InitFuel` for FARPs: metric tons; DCS `getLiquidAmount` / `setLiquidAmount`: kg.
const FOWL_LIQUID_TONS_TO_DCS_KG: u32 = 1000;

fn fowl_liquid_tons_to_dcs_kg(tons: u32) -> u32 {
    tons.saturating_mul(FOWL_LIQUID_TONS_TO_DCS_KG)
}

fn dcs_liquid_kg_to_fowl_tons(kg: u32) -> u32 {
    kg / FOWL_LIQUID_TONS_TO_DCS_KG
}

/// DCS ME / warehouse liquid list order; unknown `LiquidType` variants follow sorted by discriminant.
const FUELS_INFOBAR_ORDER: [LiquidType; 4] = [
    LiquidType::JetFuel,
    LiquidType::Avgas,
    LiquidType::MW50,
    LiquidType::Diesel,
];

fn objective_liquid_stored_tons(stored_in_tons: bool, stored: u32) -> u32 {
    if stored_in_tons {
        stored
    } else {
        dcs_liquid_kg_to_fowl_tons(stored)
    }
}

/// Non-zero liquid stocks for F10 fuel infobar (`50+20`), metric tons; `0` when empty.
pub(super) fn objective_warehouse_fuel_infobar_amounts(
    export: &FowlMizExport,
    obj: &Objective,
) -> (CompactString, u8) {
    let stored_in_tons = objective_is_ground_dep_farp_export(export, obj);
    let mut out = CompactString::new("");
    let mut first = true;
    let mut kinds = 0u8;
    let mut push = |tons: u32| {
        if tons == 0 {
            return;
        }
        kinds = kinds.saturating_add(1);
        if !first {
            out.push('+');
        }
        out.push_str(format_compact!("{tons}").as_str());
        first = false;
    };
    for typ in FUELS_INFOBAR_ORDER {
        let Some(inv) = obj.warehouse.liquids.get(&typ) else {
            continue;
        };
        push(objective_liquid_stored_tons(stored_in_tons, inv.stored));
    }
    let mut extra: SmallVec<[(LiquidType, u32); 4]> = smallvec![];
    for (typ, inv) in obj.warehouse.liquids.into_iter() {
        if FUELS_INFOBAR_ORDER.contains(&typ) {
            continue;
        }
        let tons = objective_liquid_stored_tons(stored_in_tons, inv.stored);
        if tons > 0 {
            extra.push((*typ, tons));
        }
    }
    extra.sort_by_key(|(typ, _)| *typ as u8);
    for (_, tons) in extra {
        push(tons);
    }
    let amounts = if first {
        CompactString::from("0")
    } else {
        out
    };
    (amounts, kinds)
}

fn parse_export_ws_type_key(key: &str) -> Option<[i32; 4]> {
    let inner = key.strip_prefix("wsType [")?.strip_suffix(']')?;
    let parts: Vec<i32> = inner
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    match parts.as_slice() {
        [a, b, c, d] => Some([*a, *b, *c, *d]),
        _ => None,
    }
}

fn resolve_export_equipment_dcs_name(
    export_key: &str,
    resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
) -> String {
    if let Some(quad) = parse_export_ws_type_key(export_key) {
        for (name, meta) in resource_meta {
            if meta.quad == Some(quad) {
                return name.clone();
            }
        }
    }
    String::from(export_key)
}

fn profile_export_equipment_item<'a>(
    profile: &'a ObjectiveCoalitionStock,
    dcs_name: &str,
    resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
) -> Option<&'a ObjectiveStockItem> {
    profile.equipment.get(dcs_name).or_else(|| {
        profile.equipment.iter().find_map(|(key, item)| {
            (resolve_export_equipment_dcs_name(key, resource_meta).as_str() == dcs_name).then_some(item)
        })
    })
}

fn profile_export_equipment_has(
    profile: &ObjectiveCoalitionStock,
    dcs_name: &str,
    resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
) -> bool {
    profile_export_equipment_item(profile, dcs_name, resource_meta).is_some()
}

fn discover_equipment_allowed_by_export_profile(
    obj: &Objective,
    profile: &ObjectiveCoalitionStock,
    dcs_name: &str,
    resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
) -> bool {
    if objective_is_ground_dep_farp(obj) {
        return profile_export_equipment_has(profile, dcs_name, resource_meta);
    }
    // Carriers / airbases: do not filter DCS rows by export wsType keys (hydrate from ME warehouse).
    true
}

/// DCS `wsType_Weapon` branch (`l1 == 4`): missile 4, bomb 5, shell 6, NURS 7, torpedo 8 (see DCS modding wsType list). Fowl `campaign_cfg` used 5/6 for some buckets; runtime `getResourceMap` often uses **7** for Hydra-class rockets.
fn ws_type_is_ordnance_allowlist_target(quad: &[i32; 4]) -> bool {
    quad[0] == 4 && (4..=8).contains(&quad[1])
}

/// Effective allowlist per coalition. Empty `blue_weapon_ws` with non-empty red → use red for blue (symmetric campaigns); otherwise blue-only missions keep full blue stock when blue list is empty.
fn side_allowlist<'a>(export: &'a FowlMizExport, side: Side) -> &'a [[i32; 4]] {
    match side {
        Side::Blue => {
            if export.blue_weapon_ws.is_empty() && !export.red_weapon_ws.is_empty() {
                &export.red_weapon_ws
            } else {
                &export.blue_weapon_ws
            }
        }
        Side::Red => &export.red_weapon_ws,
        Side::Neutral => &[],
    }
}

/// Rows the coalition allowlist may restrict: explicit export quads, or ordnance-shaped `wsType` (incl. misclassified resource-map rows). Pods/MISC use other `l2` values — skipped.
fn row_subject_to_weapon_allowlist(export: &FowlMizExport, quad: &[i32; 4]) -> bool {
    if export.blue_weapon_ws.iter().any(|w| w == quad)
        || export.red_weapon_ws.iter().any(|w| w == quad)
    {
        return true;
    }
    ws_type_is_ordnance_allowlist_target(quad)
}

/// Ordnance / export-listed rows: empty side allowlist → no filter for that coalition; else quad must be listed.
fn weapon_allowed_by_fowl_export<'lua>(
    export: &FowlMizExport,
    side: Side,
    typ: &warehouse::WSType<'lua>,
) -> Result<bool> {
    if !export_has_any_weapon_allowlist(export) {
        return Ok(true);
    }
    let quad = typ
        .quad()
        .context("reading wsType quad for Fowl weapon allowlist")?;
    if !row_subject_to_weapon_allowlist(export, &quad) {
        return Ok(true);
    }
    let list = side_allowlist(export, side);
    if list.is_empty() {
        return Ok(true);
    }
    Ok(list.iter().any(|w| *w == quad))
}

fn objective_defaults_for_side<'a>(
    export: &'a FowlMizExport,
    objective_name: &str,
    side: Side,
) -> Option<(&'a [std::string::String], &'a [[i32; 4]])> {
    let defaults = export.objective_defaults.get(objective_name)?;
    match side {
        Side::Blue => Some((&defaults.blue_aircraft, &defaults.blue_weapon_ws)),
        Side::Red => Some((&defaults.red_aircraft, &defaults.red_weapon_ws)),
        Side::Neutral => None,
    }
}

fn equipment_allowed_for_objective(
    export: &FowlMizExport,
    obj: &Objective,
    side: Side,
    _name: &str,
    meta: Option<WarehouseResourceMeta>,
) -> bool {
    let Some((_, allowed_ws)) = objective_defaults_for_objective(export, obj, side) else {
        return true;
    };
    if let Some(meta) = meta {
        // Aircraft allowlist is enforced by DCS linkDynTempl per helipad; skip here.
        if meta.is_aircraft {
            return true;
        }
        if let Some(quad) = meta.quad {
            if row_subject_to_weapon_allowlist(export, &quad) {
                return allowed_ws.iter().any(|w| *w == quad);
            }
        }
    }
    true
}

fn build_resource_meta_map(lua: MizLua) -> Result<FxHashMap<String, WarehouseResourceMeta>> {
    let mut out: FxHashMap<String, WarehouseResourceMeta> = FxHashMap::default();
    let map = warehouse::Warehouse::get_resource_map(lua).context("getting resource map")?;
    map.for_each(|name, typ| {
        let quad = typ.quad().ok();
        let is_aircraft = typ.category().map(|c| c.is_aircraft()).unwrap_or(false);
        out.insert(name, WarehouseResourceMeta { quad, is_aircraft });
        Ok(())
    })
    .context("building resource meta map")?;
    Ok(out)
}

fn allowed_weapon_quads(export: &FowlMizExport, side: Side) -> FxHashSet<[i32; 4]> {
    side_allowlist(export, side).iter().copied().collect()
}

/// Prune via warehouse inventory + cached resource map (not full DCS catalog scan).
fn prune_disallowed_dcs_weapon_stock<'lua>(
    warehouse: &warehouse::Warehouse<'lua>,
    export: &FowlMizExport,
    side: Side,
    resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
) -> Result<()> {
    if !export_has_any_weapon_allowlist(export) {
        return Ok(());
    }
    if side_allowlist(export, side).is_empty() {
        return Ok(());
    }
    let allowed = allowed_weapon_quads(export, side);
    let inv = warehouse
        .get_inventory(None)
        .context("warehouse getInventory for prune")?;
    let weapons = inv.weapons().context("warehouse weapon inventory")?;
    weapons
        .for_each(|name, qty| {
            if qty == 0 {
                return Ok(());
            }
            let Some(meta) = resource_meta.get(&name) else {
                return Ok(());
            };
            let Some(quad) = meta.quad else {
                return Ok(());
            };
            if !row_subject_to_weapon_allowlist(export, &quad) {
                return Ok(());
            }
            if allowed.contains(&quad) {
                return Ok(());
            }
            warehouse
                .remove_item(name.clone(), qty)
                .with_context(|| format_compact!("remove_item {name}"))?;
            Ok(())
        })
        .context("pruning disallowed weapon stock from DCS")?;
    Ok(())
}

pub(super) fn hub_to_objective_distance_km(hub: &Objective, dest: &Objective) -> f64 {
    na::distance(&hub.zone.pos().into(), &dest.zone.pos().into()) / 1000.
}

pub(super) fn clear_virtual_resupply_efficiency_cache(
    cache: &mut FxHashMap<(ObjectiveId, ObjectiveId), u8>,
) {
    cache.clear();
}

pub(super) fn invalidate_virtual_resupply_efficiency_for(
    cache: &mut FxHashMap<(ObjectiveId, ObjectiveId), u8>,
    oid: ObjectiveId,
) {
    cache.retain(|(hub, dest), _| *hub != oid && *dest != oid);
}

pub(super) fn virtual_resupply_delivery_efficiency_cached(
    cache: &mut FxHashMap<(ObjectiveId, ObjectiveId), u8>,
    cfg: &bfprotocols::cfg::Cfg,
    hub: &Objective,
    dest: &Objective,
) -> u8 {
    if !cfg.virtual_resupply {
        return 100;
    }
    let key = (hub.id, dest.id);
    if let Some(&eff) = cache.get(&key) {
        return eff;
    }
    let eff = cfg
        .virtual_resupply_decay
        .efficiency_at_distance_km(hub_to_objective_distance_km(hub, dest));
    cache.insert(key, eff);
    eff
}

fn scale_by_delivery_efficiency(base: u32, efficiency_pct: u8) -> u32 {
    if efficiency_pct == 0 || base == 0 {
        return 0;
    }
    let scaled = base.saturating_mul(efficiency_pct as u32) / 100;
    if scaled == 0 { 1 } else { scaled }
}

fn nearest_logistics_hub_filtered(
    persisted: &Persisted,
    owner: Side,
    pos: Vector2,
    hub_ok: impl Fn(&Objective) -> bool,
) -> Option<ObjectiveId> {
    persisted
        .logistics_hubs
        .into_iter()
        .filter_map(|hid| {
            let hub = persisted.objectives.get(hid)?;
            if hub.owner != owner || !hub_ok(hub) {
                return None;
            }
            let dist = na::distance_squared(&pos.into(), &hub.zone.pos().into());
            Some((*hid, dist))
        })
        .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(hid, _)| hid)
}

pub(super) fn nearest_logistics_hub(
    persisted: &Persisted,
    owner: Side,
    pos: Vector2,
) -> Option<ObjectiveId> {
    nearest_logistics_hub_filtered(persisted, owner, pos, |hub| hub.is_normal_logistics_hub())
}

pub(super) fn nearest_normal_logistics_hub(
    persisted: &Persisted,
    owner: Side,
    pos: Vector2,
) -> Option<ObjectiveId> {
    nearest_logistics_hub_filtered(persisted, owner, pos, |hub| hub.is_normal_logistics_hub())
}

impl Db {
    pub(super) fn warehouse_sync_objective_ids(&self) -> Vec<ObjectiveId> {
        self.persisted
            .objectives
            .into_iter()
            .filter(|(_, obj)| !matches!(obj.kind, ObjectiveKind::Production))
            .map(|(id, _)| *id)
            .collect()
    }

    fn scale_production_amount(base: u32, production: u8) -> u32 {
        if production == 0 || base == 0 {
            return 0;
        }
        let scaled = base.saturating_mul(production as u32) / 100;
        if scaled == 0 { 1 } else { scaled }
    }

    pub(super) fn refresh_hub_production_from_opr(&mut self) -> Result<()> {
        let mut sums: FxHashMap<ObjectiveId, (u32, u32)> = FxHashMap::default();
        let opr_ids: Vec<ObjectiveId> = self
            .persisted
            .objectives
            .into_iter()
            .filter(|(_, obj)| matches!(obj.kind, ObjectiveKind::Production))
            .map(|(id, _)| *id)
            .collect();
        for oid in opr_ids {
            let (owner, pos, production, capacity) = {
                let obj = objective!(self, oid)?;
                (
                    obj.owner,
                    obj.zone.pos(),
                    obj.production,
                    obj.production_capacity.max(1),
                )
            };
            let hub = nearest_logistics_hub(&self.persisted, owner, pos);
            let obj = objective_mut!(self, oid)?;
            obj.feed_hub = hub;
            if let Some(hid) = hub {
                let e = sums.entry(hid).or_insert((0, 0));
                e.0 = e.0.saturating_add(u32::from(production) * u32::from(capacity));
                e.1 = e.1.saturating_add(u32::from(capacity));
            }
        }
        for hid in &self.persisted.logistics_hubs {
            let hub = objective_mut!(self, hid)?;
            if let Some((weighted, cap_sum)) = sums.get(hid).copied() {
                let denom = cap_sum.max(1) as u64;
                let numer = weighted as u64;
                // Round up to whole percents, so steps like 79 (floor) don't show
                // when the "true" ratio is 79.1+.
                let prod = ((numer + denom - 1) / denom).min(100);
                hub.production = prod as u8;
            } else {
                hub.production = 0;
            }
        }
        Ok(())
    }

    fn warehouse_resource_meta_cache(
        &mut self,
        lua: MizLua,
    ) -> Result<Arc<FxHashMap<String, WarehouseResourceMeta>>> {
        if self.ephemeral.warehouse_resource_meta.is_none() {
            let map = build_resource_meta_map(lua).context("warehouse resource meta cache")?;
            self.ephemeral.warehouse_resource_meta = Some(Arc::new(map));
        }
        Ok(Arc::clone(
            self.ephemeral
                .warehouse_resource_meta
                .as_ref()
                .expect("warehouse_resource_meta just set"),
        ))
    }

    fn init_resource_map(&mut self, lua: MizLua) -> Result<()> {
        let whcfg = match self.ephemeral.cfg.warehouse.as_ref() {
            None => return Ok(()),
            Some(w) => w,
        };
        if self.ephemeral.production_by_side.is_empty() {
            let map =
                warehouse::Warehouse::get_resource_map(lua).context("getting resource map")?;
            map.for_each(|name, typ| {
                let category = typ.category().context("getting category")?;
                let export = self.ephemeral.fowl_miz_export.as_ref();
                for side in Side::ALL {
                    let template = match whcfg.supply_source.get(&side) {
                        Some(tmpl) => tmpl,
                        None => continue, // side didn't produce anything, bummer
                    };
                    let w = get_supplier(lua, template.clone())
                        .with_context(|| format_compact!("getting supplier {template}"))?;
                    if !weapon_allowed_by_fowl_export(export, side, &typ)? {
                        continue;
                    }
                    let production =
                        Arc::make_mut(self.ephemeral.production_by_side.entry(side).or_default());
                    let qty = w
                        .get_item_count(name.clone())
                        .with_context(|| format_compact!("getting {name} from the warehouse"))?;
                    if qty > 0 {
                        production
                            .equipment
                            .insert(name.clone(), Equipment { production: qty });
                        if category.is_aircraft() {
                            let vehicle = Vehicle::from(name.clone());
                            self.ephemeral
                                .cfg
                                .check_vehicle_has_threat_distance(&vehicle)?;
                            self.ephemeral
                                .cfg
                                .check_vehicle_has_life_type(&vehicle)?;
                        }
                    }
                    for name in LiquidType::ALL {
                        let qty = w.get_liquid_amount(name).context("getting liquid amount")?;
                        if qty > 0 {
                            production.liquids.insert(name, qty);
                        }
                    }
                }
                Ok(())
            })
            .context("iterating resource map")?
        }
        Ok(())
    }

    pub(super) fn init_farp_warehouse(&mut self, lua: MizLua, oid: &ObjectiveId) -> Result<()> {
        if self.ephemeral.cfg.warehouse.is_none() {
            return Ok(());
        }
        let initial_stock_pct = self
            .ephemeral
            .cfg
            .warehouse
            .as_ref()
            .map(|w| w.dynamic_farps_initial_stock_percentage)
            .unwrap_or(100)
            .min(100);
        let export = Arc::clone(&self.ephemeral.fowl_miz_export);
        let obj_read = objective!(self, oid)?;
        if !objective_is_ground_dep_farp(obj_read) {
            bail!(
                "init_farp_warehouse: {:?} is not a ground DEP FARP (naval pads use DCS ME stock)",
                obj_read.name
            );
        }
        let profile = dep_farp_export_profile(export.as_ref(), obj_read).cloned();
        let resource_meta = self
            .warehouse_resource_meta_cache(lua)
            .context("resource map for DEP FARP export stock")?;
        let obj = objective_mut!(self, oid)?;
        if let Some(profile) = profile {
            obj.warehouse = Warehouse::default();
            for (name, item) in &profile.equipment {
                if item.baseline == 0 {
                    continue;
                }
                let dcs_name =
                    resolve_export_equipment_dcs_name(name.as_str(), resource_meta.as_ref());
                let meta = resource_meta.get(&dcs_name).copied();
                if !equipment_allowed_for_objective(
                    export.as_ref(),
                    obj,
                    obj.owner,
                    name.as_str(),
                    meta,
                ) {
                    continue;
                }
                obj.warehouse.equipment.insert_cow(
                    dcs_name,
                    Inventory {
                        stored: scale_capacity_by_percent_floor(item.baseline, initial_stock_pct),
                        capacity: item.baseline,
                    },
                );
            }
            for (key, liq) in &profile.liquids {
                if liq.baseline == 0 {
                    continue;
                }
                let typ = liquid_type_from_export_key(key)
                    .with_context(|| format_compact!("FARP objective {:?}", obj.name))?;
                obj.warehouse.liquids.insert_cow(
                    typ,
                    Inventory {
                        stored: scale_capacity_by_percent_floor(liq.baseline, initial_stock_pct),
                        capacity: liq.baseline,
                    },
                );
            }
            log::info!(
                "ground DEP FARP {:?} ({:?}): warehouse from export pad profile ({}% initial stock)",
                oid,
                obj.name,
                initial_stock_pct
            );
            return Ok(());
        }
        let whcfg = self.ephemeral.cfg.warehouse.as_ref().unwrap();
        let production = match self.ephemeral.production_by_side.get(&obj.owner) {
            Some(q) => Arc::clone(q),
            None => return Ok(()),
        };
        for (name, equip) in &production.equipment {
            let capacity = whcfg.capacity(&obj.kind, false, equip.production);
            let inv = Inventory {
                stored: scale_capacity_by_percent_floor(capacity, initial_stock_pct),
                capacity,
            };
            obj.warehouse.equipment.insert_cow(name.clone(), inv);
        }
        for (name, qty) in &production.liquids {
            let capacity = whcfg.capacity(&obj.kind, false, *qty);
            let inv = Inventory {
                stored: scale_capacity_by_percent_floor(capacity, initial_stock_pct),
                capacity,
            };
            obj.warehouse.liquids.insert_cow(*name, inv);
        }
        warn!(
            "dynamic FARP {:?} ({:?}): no export profile for pad; using coalition BINVENTORY/RINVENTORY ({}% stock)",
            oid,
            obj.name,
            initial_stock_pct
        );
        Ok(())
    }

    /// Push virtual DEP FARP stock (tons→kg) into DCS and prune ME template overflow.
    pub(super) fn apply_dep_farp_virtual_stock_to_dcs(
        &mut self,
        lua: MizLua,
        oid: &ObjectiveId,
    ) -> Result<()> {
        let export = self.ephemeral.fowl_miz_export.as_ref();
        let obj = objective!(self, oid)?;
        if !objective_is_ground_dep_farp_export(export, obj) {
            return Ok(());
        }
        self.sync_objective_to_warehouse(lua, *oid)
            .context("applying ground DEP FARP virtual stock to DCS warehouse")?;
        Ok(())
    }

    fn mark_dep_farp_virtual_authoritative(&mut self, oid: ObjectiveId) {
        let until = Utc::now() + Duration::seconds(90);
        self.ephemeral
            .dep_farp_authoritative_until
            .insert(oid, until);
    }

    fn dep_farp_skip_dcs_hydrate(&self, oid: ObjectiveId) -> bool {
        self.ephemeral
            .dep_farp_authoritative_until
            .get(&oid)
            .is_some_and(|until| Utc::now() < *until)
    }

    /// Initial DEP FARP fill only: [`Self::init_farp_warehouse`] at `dynamicFARPs_InitialStockPercentage`, then DCS sync.
    /// Later hub resupply uses normal `capacity` like any other objective (`virtual_resupply`).
    pub(super) fn finish_dynamic_farp_warehouse(
        &mut self,
        lua: MizLua,
        oid: &ObjectiveId,
    ) -> Result<()> {
        let pct = self
            .ephemeral
            .cfg
            .warehouse
            .as_ref()
            .map(|w| w.dynamic_farps_initial_stock_percentage)
            .unwrap_or(100)
            .min(100);
        let name = objective!(self, oid)?.name.clone();
        self.init_farp_warehouse(lua, oid)
            .context("initializing dynamic FARP warehouse")?;
        self.mark_dep_farp_virtual_authoritative(*oid);
        self.apply_dep_farp_virtual_stock_to_dcs(lua, oid)
            .context("applying ground DEP FARP initial stock to DCS warehouse")?;
        self.update_supply_status()
            .context("updating supply/fuel after FARP warehouse init")?;
        info!(
            "ground DEP FARP {:?} ({:?}): DCS warehouse set to {}% export stock (infobar supply/fuel updated)",
            oid, name, pct
        );
        Ok(())
    }

    /// Fowl 2.0: virtual rows from export baselines; DCS amounts applied in [`Self::setup_warehouses_after_load`].
    pub(super) fn seed_objective_warehouses_from_export(&mut self, lua: MizLua) -> Result<()> {
        self.init_resource_map(lua)
            .context("initializing resource map")?;
        if self.ephemeral.cfg.warehouse.is_none() {
            return Ok(());
        }
        let export = self.ephemeral.fowl_miz_export.as_ref();
        if !export_has_objective_stock(export) {
            bail!(
                "Fowl: mission export has no objective_stock (rebuild with FowlTools schema v5)"
            );
        }
        let resource_meta =
            build_resource_meta_map(lua).context("resource map for objective stock seed")?;
        for (_oid, obj) in self.persisted.objectives.iter_mut_cow() {
            obj.warehouse = Warehouse::default();
            if matches!(obj.kind, ObjectiveKind::Production) {
                continue;
            }
            if objective_is_ground_dep_farp(obj) {
                let Some(profile) = dep_farp_export_profile(export, obj) else {
                    warn!(
                        "ground DEP FARP {:?}: no export pad profile for {:?}",
                        obj.name,
                        obj.owner
                    );
                    continue;
                };
                for (name, item) in &profile.equipment {
                    if item.baseline == 0 {
                        continue;
                    }
                    let dcs_name =
                        resolve_export_equipment_dcs_name(name.as_str(), &resource_meta);
                    let meta = resource_meta.get(&dcs_name).copied();
                    if !equipment_allowed_for_objective(
                        export,
                        obj,
                        obj.owner,
                        name.as_str(),
                        meta,
                    ) {
                        continue;
                    }
                    obj.warehouse.equipment.insert_cow(
                        dcs_name,
                        Inventory {
                            stored: 0,
                            capacity: item.baseline,
                        },
                    );
                }
                for (key, liq) in &profile.liquids {
                    if liq.baseline == 0 {
                        continue;
                    }
                    let typ = liquid_type_from_export_key(key)
                        .with_context(|| format_compact!("DEP FARP {:?}", obj.name))?;
                    obj.warehouse.liquids.insert_cow(
                        typ,
                        Inventory {
                            stored: 0,
                            capacity: liq.baseline,
                        },
                    );
                }
                continue;
            }
            let Some(profile) =
                objective_coalition_stock_for_side(export, obj.name.as_str(), obj.owner)
            else {
                warn!(
                    "objective {:?}: no objective_stock profile for owner {:?}",
                    obj.name, obj.owner
                );
                continue;
            };
            for (name, item) in &profile.equipment {
                let dcs_name =
                    resolve_export_equipment_dcs_name(name.as_str(), &resource_meta);
                let meta = resource_meta.get(&dcs_name).copied();
                if !equipment_allowed_for_objective(
                    export,
                    obj,
                    obj.owner,
                    name.as_str(),
                    meta,
                ) {
                    continue;
                }
                if item.baseline == 0 {
                    continue;
                }
                obj.warehouse.equipment.insert_cow(
                    dcs_name,
                    Inventory {
                        stored: 0,
                        capacity: item.baseline,
                    },
                );
            }
            for (key, liq) in &profile.liquids {
                if liq.baseline == 0 {
                    continue;
                }
                let typ = liquid_type_from_export_key(key)
                    .with_context(|| format_compact!("objective {:?}", obj.name))?;
                obj.warehouse.liquids.insert_cow(
                    typ,
                    Inventory {
                        stored: 0,
                        capacity: liq.baseline,
                    },
                );
            }
        }
        self.ephemeral.dirty();
        Ok(())
    }

    /// Replace virtual warehouse rows with the export profile for `obj.owner`.
    fn apply_export_profile_to_objective_virtual_warehouse(
        obj: &mut Objective,
        export: &FowlMizExport,
        resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
    ) -> Result<bool> {
        if matches!(obj.kind, ObjectiveKind::Production) {
            return Ok(false);
        }
        let profile = if objective_is_ground_dep_farp(obj) {
            dep_farp_export_profile(export, obj)
        } else {
            objective_coalition_stock_for_objective(export, obj)
        };
        let Some(profile) = profile else {
            return Ok(false);
        };
        let supplier = obj.warehouse.supplier;
        obj.warehouse = Warehouse {
            supplier,
            ..Warehouse::default()
        };
        for (name, item) in &profile.equipment {
            if item.baseline == 0 {
                continue;
            }
            let dcs_name = resolve_export_equipment_dcs_name(name.as_str(), resource_meta);
            let meta = resource_meta.get(&dcs_name).copied();
            if !equipment_allowed_for_objective(export, obj, obj.owner, name.as_str(), meta) {
                continue;
            }
            obj.warehouse.equipment.insert_cow(
                dcs_name,
                Inventory {
                    stored: 0,
                    capacity: item.baseline,
                },
            );
        }
        for (key, liq) in &profile.liquids {
            if liq.baseline == 0 {
                continue;
            }
            let typ = liquid_type_from_export_key(key)
                .with_context(|| format_compact!("capture warehouse {:?}", obj.name))?;
            obj.warehouse.liquids.insert_cow(
                typ,
                Inventory {
                    stored: 0,
                    capacity: liq.baseline,
                },
            );
        }
        Ok(true)
    }

    fn apply_capture_spoils_to_virtual(
        obj: &mut Objective,
        equipment: &FxHashMap<String, u32>,
        liquids: &FxHashMap<LiquidType, u32>,
    ) {
        for (name, inv) in obj.warehouse.equipment.iter_mut_cow() {
            if let Some(&stored) = equipment.get(name.as_str()) {
                inv.stored = stored.min(inv.capacity);
            }
        }
        for (typ, inv) in obj.warehouse.liquids.iter_mut_cow() {
            if let Some(&stored) = liquids.get(typ) {
                inv.stored = stored.min(inv.capacity);
            }
        }
    }

    pub(super) fn setup_warehouses_after_load(&mut self, lua: MizLua) -> Result<()> {
        self.init_resource_map(lua)
            .context("initializing resource map")?;
        let resource_meta = self
            .warehouse_resource_meta_cache(lua)
            .context("resource map for objective defaults")?;
        let whcfg = match self.ephemeral.cfg.warehouse.as_ref() {
            Some(cfg) => cfg,
            None => return Ok(()),
        };
        let export = self.ephemeral.fowl_miz_export.as_ref();
        let _map = warehouse::Warehouse::get_resource_map(lua).context("getting resource map")?;
        let world = World::singleton(lua).context("getting world")?;
        self.ephemeral.airbase_by_oid.clear();
        self.ephemeral.airbases_by_oid.clear();
        let mut objective_whid: FxHashMap<ObjectiveId, String> = FxHashMap::default();
        let mut load_and_sync_airbases = || -> Result<()> {
            world
                .get_airbases()
                .context("getting airbases")?
                .for_each(|airbase| {
                    let airbase = airbase.context("getting airbase")?;
                    let name = airbase.as_object()?.get_name()?;
                    log::info!("setting up airbase {name}");
                    if !airbase.is_exist()? {
                        return Ok(()); // can happen when farps get recycled
                    }
                    let pos3 = airbase.get_point().context("getting airbase position")?;
                    let pos = Vector2::new(pos3.x, pos3.z);
                    airbase
                        .auto_capture(false)
                        .context("setting airbase autocapture")?;
                    let oid = self
                        .persisted
                        .objectives
                        .into_iter()
                        .find(|(_, obj)| obj.zone.contains(pos));
                    let w = airbase
                        .get_warehouse()
                        .context("getting airbase warehouse")?;
                    let whid = w.whid().context("getting airbase warehouse id")?;
                    let (oid, obj) = match oid {
                        Some((oid, obj)) => {
                            airbase
                                .set_coalition(obj.owner)
                                .context("setting airbase owner")?;
                            (*oid, obj)
                        }
                        None if !self.ephemeral.global_pad_templates.contains(&name) => {
                            // Do not mutate DCS item tables for non-objective airbases.
                            // Bulk zeroing can remove aircraft rows (and their dynamic template links).
                            return Ok(());
                        }
                        None => {
                            log::info!("airbase {name} has no objective");
                            return Ok(());
                        }
                    };
                    match self.ephemeral.airbase_by_oid.entry(oid) {
                        Entry::Vacant(e) => {
                            let airbase_oid =
                                airbase.object_id().context("getting airbase object_id")?;
                            e.insert(airbase_oid.clone());
                            self.ephemeral
                                .airbases_by_oid
                                .insert(oid, smallvec![airbase_oid]);
                            objective_whid.insert(oid, whid);
                        }
                        Entry::Occupied(_) => {
                            match objective_whid.get(&oid) {
                                Some(expected) if expected == &whid => {
                                    let airbase_oid =
                                        airbase.object_id().context("getting airbase object_id")?;
                                    let oairbases = self
                                        .ephemeral
                                        .airbases_by_oid
                                        .entry(oid)
                                        .or_insert_with(|| smallvec![]);
                                    if !oairbases.contains(&airbase_oid) {
                                        oairbases.push(airbase_oid);
                                    }
                                }
                                Some(_) => {
                                    bail!(
                                        "multiple warehouses inside the trigger zone of {}",
                                        obj.name
                                    )
                                }
                                None => bail!("missing warehouse key while processing {}", obj.name),
                            }
                        }
                    }
                    Ok(())
                })
        };
        load_and_sync_airbases().context("loading and syncing airbases")?;
        // Dynamic FARPs (TISP / -action): zone center is a heuristic; DCS ship airbase `getPoint`
        // can sit outside the circle after `move_farp_pad`. Pair by ME pad group name like `add_farp`.
        let mut pair_farp_airbases_by_pad = || -> Result<()> {
            for (oid, obj) in &self.persisted.objectives {
                if self.ephemeral.airbase_by_oid.contains_key(oid) {
                    continue;
                }
                let ObjectiveKind::Farp { pad_template, .. } = &obj.kind else {
                    continue;
                };
                let airbase = Airbase::get_by_name(lua, pad_template.clone()).with_context(|| {
                    format_compact!(
                        "FARP objective {:?}: expected DCS airbase named {:?}",
                        obj.name,
                        pad_template
                    )
                })?;
                if !airbase.is_exist()? {
                    bail!(
                        "FARP objective {:?}: airbase {:?} not spawned",
                        obj.name,
                        pad_template
                    );
                }
                airbase
                    .auto_capture(false)
                    .context("setting airbase autocapture")?;
                airbase
                    .set_coalition(obj.owner)
                    .context("setting airbase coalition")?;
                let w = airbase
                    .get_warehouse()
                    .context("getting airbase warehouse")?;
                let whid = w.whid().context("getting airbase warehouse id")?;
                let airbase_oid = airbase
                    .object_id()
                    .context("getting airbase object_id")?;
                if self
                    .ephemeral
                    .airbase_by_oid
                    .values()
                    .any(|existing| existing == &airbase_oid)
                {
                    bail!(
                        "FARP pad {:?} already paired to another objective (duplicate pad name?)",
                        pad_template
                    );
                }
                if let Entry::Vacant(e) = self.ephemeral.airbase_by_oid.entry(*oid) {
                    e.insert(airbase_oid.clone());
                    self.ephemeral
                        .airbases_by_oid
                        .insert(*oid, smallvec![airbase_oid]);
                    objective_whid.insert(*oid, whid);
                }
            }
            Ok(())
        };
        pair_farp_airbases_by_pad().context("pairing FARP objectives to ship airbases by pad name")?;
        let mut adjust_warehouses_for_miz_changes = || -> Result<()> {
            let use_export_stock = export_has_objective_stock(export);
            for (_oid, obj) in self.persisted.objectives.iter_mut_cow() {
                if matches!(obj.kind, ObjectiveKind::Production) {
                    continue;
                }
                let mut del_eq: SmallVec<[String; 8]> = smallvec![];
                let mut del_l: SmallVec<[LiquidType; 4]> = smallvec![];
                if use_export_stock {
                    if objective_is_ground_dep_farp(obj) {
                        let Some(profile) = dep_farp_export_profile(export, obj) else {
                            continue;
                        };
                        for (name, _) in &obj.warehouse.equipment {
                            let mut keep = profile_export_equipment_has(
                                profile,
                                name.as_str(),
                                resource_meta.as_ref(),
                            );
                            if keep {
                                let meta = resource_meta.get(name).copied();
                                keep = equipment_allowed_for_objective(
                                    export,
                                    obj,
                                    obj.owner,
                                    name.as_str(),
                                    meta,
                                );
                            }
                            if !keep {
                                del_eq.push(name.clone());
                            }
                        }
                        for name in del_eq {
                            obj.warehouse.equipment.remove_cow(&name);
                        }
                        for (liq, _) in &obj.warehouse.liquids {
                            let keep = profile.liquids.keys().any(|k| {
                                liquid_type_from_export_key(k).ok() == Some(*liq)
                            });
                            if !keep {
                                del_l.push(*liq);
                            }
                        }
                        for liq in del_l {
                            obj.warehouse.liquids.remove_cow(&liq);
                        }
                        for (name, item) in &profile.equipment {
                            if item.baseline == 0 {
                                continue;
                            }
                            let dcs_name = resolve_export_equipment_dcs_name(
                                name.as_str(),
                                resource_meta.as_ref(),
                            );
                            let meta = resource_meta.get(&dcs_name).copied();
                            if !equipment_allowed_for_objective(
                                export,
                                obj,
                                obj.owner,
                                name.as_str(),
                                meta,
                            ) {
                                continue;
                            }
                            let inv = obj.warehouse.equipment.get_or_default_cow(dcs_name);
                            inv.capacity = item.baseline;
                        }
                        for (key, liq) in &profile.liquids {
                            if liq.baseline == 0 {
                                continue;
                            }
                            let typ = liquid_type_from_export_key(key)?;
                            let inv = obj.warehouse.liquids.get_or_default_cow(typ);
                            inv.capacity = liq.baseline;
                        }
                    } else {
                        let Some(profile) = objective_coalition_stock_for_side(
                            export,
                            obj.name.as_str(),
                            obj.owner,
                        ) else {
                            continue;
                        };
                        for (name, _) in &obj.warehouse.equipment {
                            let mut keep = profile_export_equipment_has(
                                profile,
                                name.as_str(),
                                resource_meta.as_ref(),
                            );
                            if keep {
                                let meta = resource_meta.get(name).copied();
                                keep = equipment_allowed_for_objective(
                                    export,
                                    obj,
                                    obj.owner,
                                    name.as_str(),
                                    meta,
                                );
                            }
                            if !keep {
                                del_eq.push(name.clone());
                            }
                        }
                        for name in del_eq {
                            obj.warehouse.equipment.remove_cow(&name);
                        }
                        for (liq, _) in &obj.warehouse.liquids {
                            let keep = profile.liquids.keys().any(|k| {
                                liquid_type_from_export_key(k).ok() == Some(*liq)
                            });
                            if !keep {
                                del_l.push(*liq);
                            }
                        }
                        for liq in del_l {
                            obj.warehouse.liquids.remove_cow(&liq);
                        }
                        for (name, item) in &profile.equipment {
                            if item.baseline == 0 {
                                continue;
                            }
                            let dcs_name = resolve_export_equipment_dcs_name(
                                name.as_str(),
                                resource_meta.as_ref(),
                            );
                            let meta = resource_meta.get(&dcs_name).copied();
                            if !equipment_allowed_for_objective(
                                export,
                                obj,
                                obj.owner,
                                name.as_str(),
                                meta,
                            ) {
                                continue;
                            }
                            let inv = obj.warehouse.equipment.get_or_default_cow(dcs_name);
                            inv.capacity = item.baseline;
                        }
                        for (key, liq) in &profile.liquids {
                            if liq.baseline == 0 {
                                continue;
                            }
                            let typ = liquid_type_from_export_key(key)?;
                            let inv = obj.warehouse.liquids.get_or_default_cow(typ);
                            inv.capacity = liq.baseline;
                        }
                    }
                } else if let Some(prod) = self.ephemeral.production_by_side.get(&obj.owner) {
                    let on_water = objective_airbase_on_water(lua, obj)?;
                    for (name, _) in &obj.warehouse.equipment {
                        let mut keep = prod.equipment.contains_key(name);
                        if keep {
                            let meta = resource_meta.get(name).copied();
                            keep =
                                equipment_allowed_for_objective(export, obj, obj.owner, name.as_str(), meta);
                        }
                        if !keep {
                            del_eq.push(name.clone());
                        }
                    }
                    for name in del_eq {
                        obj.warehouse.equipment.remove_cow(&name);
                    }
                    for (liq, _) in &obj.warehouse.liquids {
                        if !prod.liquids.contains_key(liq) {
                            del_l.push(*liq);
                        }
                    }
                    for liq in del_l {
                        obj.warehouse.liquids.remove_cow(&liq);
                    }
                    for (name, eqip) in &prod.equipment {
                        let meta = resource_meta.get(name).copied();
                        if !equipment_allowed_for_objective(
                            export,
                            obj,
                            obj.owner,
                            name.as_str(),
                            meta,
                        ) {
                            continue;
                        }
                        let capacity = whcfg.capacity(&obj.kind, on_water, eqip.production);
                        let inv = obj.warehouse.equipment.get_or_default_cow(name.clone());
                        inv.capacity = capacity;
                    }
                    for (name, prod) in &prod.liquids {
                        let capacity = whcfg.capacity(&obj.kind, on_water, *prod);
                        let inv = obj.warehouse.liquids.get_or_default_cow(*name);
                        inv.capacity = capacity;
                    }
                }
            }
            Ok(())
        };
        adjust_warehouses_for_miz_changes().context("adjusting warehouses for miz changes")?;
        let mut missing = vec![];
        for (oid, obj) in &self.persisted.objectives {
            if matches!(obj.kind, ObjectiveKind::Production) {
                continue;
            }
            if !self.ephemeral.airbase_by_oid.contains_key(oid) {
                missing.push(obj.name.clone());
            }
        }
        if !missing.is_empty() {
            bail!("objectives missing a warehouse {:?}", missing)
        }
        let sync_oids: Vec<ObjectiveId> = self.ephemeral.airbase_by_oid.keys().copied().collect();
        let preserve_fill = self.ephemeral.preserve_initial_warehouse_fill;
        for oid in sync_oids {
            if matches!(objective!(self, oid)?.kind, ObjectiveKind::Production) {
                continue;
            }
            self.sync_warehouse_to_objective(lua, oid)
                .with_context(|| format_compact!("seed virtual stock from DCS warehouse for {:?}", oid))?;
            if !preserve_fill {
                self.sync_objective_to_warehouse(lua, oid).with_context(|| {
                    format_compact!("Fowl export: prune/sync DCS warehouse for {:?}", oid)
                })?;
            }
        }
        if preserve_fill {
            self.ephemeral.preserve_initial_warehouse_fill = false;
            info!(
                "new campaign: preserved bftools ME warehouse stock (virtual sync from DCS only, no prune/sync-to)"
            );
        }
        let mut dep_farp_restore: SmallVec<[(ObjectiveId, String); 8]> = smallvec![];
        for oid in &self.persisted.farps {
            let obj = objective!(self, oid)?;
            if dep_farp_has_persisted_virtual_stock(obj) {
                dep_farp_restore.push((*oid, obj.name.clone()));
            }
        }
        for (oid, name) in dep_farp_restore {
            if let Err(e) = self.apply_dep_farp_virtual_stock_to_dcs(lua, &oid) {
                error!(
                    "failed to restore persisted ground DEP FARP {:?} ({:?}) warehouse to DCS: {e:?}",
                    oid, name
                );
            } else {
                info!(
                    "restored persisted ground DEP FARP {:?} ({:?}) virtual warehouse to DCS",
                    oid, name
                );
            }
        }
        self.update_supply_status()
            .context("updating supply status")?;
        self.setup_supply_lines()
            .context("setting up supply lines")?;
        Ok(())
    }

    pub fn admin_tick_now(&mut self) {
        match &mut self.ephemeral.logistics_stage {
            LogiStage::Init
            | LogiStage::SyncFromWarehouses { .. }
            | LogiStage::SyncToWarehouses { .. }
            | LogiStage::ExecuteTransfers { .. } => (),
            LogiStage::Complete { last_tick } => {
                *last_tick = DateTime::<Utc>::MIN_UTC;
            }
        }
    }

    pub fn admin_deliver_now(&mut self) {
        self.admin_tick_now();
        self.persisted.logistics_ticks_since_delivery = u32::MAX;
    }

    pub fn logistics_step(
        &mut self,
        lua: MizLua,
        perf: &mut PerfInner,
        ts: DateTime<Utc>,
    ) -> Result<()> {
        if let Some((tick, ticks_per_delivery)) = self
            .ephemeral
            .cfg
            .warehouse
            .as_ref()
            .map(|w| (w.tick, w.ticks_per_delivery))
        {
            self.refresh_hub_production_from_opr()
                .context("refreshing hub production before logistics step")?;
            let freq = Duration::minutes(tick as i64);
            let start_ts = Utc::now();
            match &mut self.ephemeral.logistics_stage {
                LogiStage::Init => {
                    let objectives = self.warehouse_sync_objective_ids().into();
                    self.ephemeral.logistics_stage = if self
                        .ephemeral
                        .defer_initial_logistics_sync_to
                    {
                        LogiStage::SyncFromWarehouses { objectives }
                    } else {
                        LogiStage::SyncToWarehouses { objectives }
                    };
                }
                LogiStage::Complete { last_tick } if ts - *last_tick >= freq => {
                    let objectives = self.warehouse_sync_objective_ids().into();
                    self.ephemeral.logistics_stage = LogiStage::SyncFromWarehouses { objectives };
                }
                LogiStage::Complete { last_tick: _ } => (),
                LogiStage::SyncFromWarehouses { objectives } => match objectives.pop() {
                    Some(oid) => {
                        let start_ts = Utc::now();
                        if let Err(e) = self.sync_warehouse_to_objective(lua, oid) {
                            if !warehouse_sync_skip(&e) {
                                error!("failed to sync objective {oid} from warehouse {:?}", e)
                            }
                        }
                        record_perf(&mut perf.logistics_sync_from, start_ts);
                    }
                    None => {
                        let sts = Utc::now();
                        let transfers = if self.persisted.logistics_ticks_since_delivery
                            >= ticks_per_delivery
                        {
                            self.persisted.logistics_ticks_since_delivery = 0;
                            let v = match self.deliver_production() {
                                Ok(v) => v,
                                Err(e) => {
                                    error!("failed to deliver production {:?}", e);
                                    vec![]
                                }
                            };
                            record_perf(&mut perf.logistics_deliver, sts);
                            v
                        } else {
                            self.persisted.logistics_ticks_since_delivery += 1;
                            let v = if self.ephemeral.defer_initial_hub_distribute {
                                info!(
                                    "new campaign: skipping hub-to-objective distribute (bftools warehouse fill)"
                                );
                                vec![]
                            } else {
                                match self.deliver_supplies_from_logistics_hubs() {
                                    Ok(v) => v,
                                    Err(e) => {
                                        error!("failed to deliver supplies from hubs {:?}", e);
                                        vec![]
                                    }
                                }
                            };
                            record_perf(&mut perf.logistics_distribute, sts);
                            v
                        };
                        self.ephemeral.logistics_stage = LogiStage::ExecuteTransfers { transfers };
                    }
                },
                LogiStage::ExecuteTransfers { transfers } if transfers.is_empty() => {
                    let st = Utc::now();
                    if self.ephemeral.defer_initial_logistics_sync_to {
                        self.ephemeral.defer_initial_logistics_sync_to = false;
                        info!(
                            "new campaign: skipping logistics sync-to DCS (bftools ME warehouse fill)"
                        );
                        self.update_supply_status()
                            .context("supply status after bootstrap")?;
                        self.ephemeral.logistics_stage = LogiStage::Complete { last_tick: ts };
                    } else {
                        self.balance_logistics_hubs()?;
                        let objectives = self.warehouse_sync_objective_ids().into();
                        self.ephemeral.logistics_stage =
                            LogiStage::SyncToWarehouses { objectives };
                    }
                    record_perf(&mut perf.logistics_transfer, st);
                }
                LogiStage::ExecuteTransfers { transfers } => {
                    let st = Utc::now();
                    while let Some(tr) = transfers.pop() {
                        if let Err(e) = tr.execute(&mut self.persisted, &self.ephemeral.to_bg) {
                            error!("executing transfer {:?} {e:?}", tr)
                        }
                        if Utc::now() - st > Duration::milliseconds(6) {
                            break;
                        }
                    }
                    record_perf(&mut perf.logistics_transfer, st);
                }
                LogiStage::SyncToWarehouses { objectives } => match objectives.pop() {
                    None => self.ephemeral.logistics_stage = LogiStage::Complete { last_tick: ts },
                    Some(oid) => {
                        let start_ts = Utc::now();
                        if let Err(e) = self.sync_objective_to_warehouse(lua, oid) {
                            if !warehouse_sync_skip(&e) {
                                error!("failed to sync objective {oid} to warehouse {:?}", e)
                            }
                        }
                        record_perf(&mut perf.logistics_sync_to, start_ts);
                    }
                },
            }
            record_perf(&mut perf.logistics, start_ts);
        }
        Ok(())
    }

    pub(super) fn capture_warehouse(&mut self, lua: MizLua, oid: ObjectiveId) -> Result<()> {
        if self.ephemeral.cfg.warehouse.is_none() {
            return Ok(());
        }
        let export = Arc::clone(&self.ephemeral.fowl_miz_export);
        if export_has_objective_stock(export.as_ref()) {
            let resource_meta = self
                .warehouse_resource_meta_cache(lua)
                .context("resource meta for capture warehouse")?;
            let (preserved_fuel, spoils_eq, spoils_liq) = {
                let obj = objective!(self, oid)?;
                let mut spoils_eq: FxHashMap<String, u32> = FxHashMap::default();
                for (k, v) in &obj.warehouse.equipment {
                    spoils_eq.insert(k.clone(), v.stored);
                }
                let mut spoils_liq: FxHashMap<LiquidType, u32> = FxHashMap::default();
                for (k, v) in &obj.warehouse.liquids {
                    spoils_liq.insert(*k, v.stored);
                }
                (obj.fuel, spoils_eq, spoils_liq)
            };
            let reseeded = {
                let obj = objective_mut!(self, oid)?;
                Self::apply_export_profile_to_objective_virtual_warehouse(
                    obj,
                    export.as_ref(),
                    resource_meta.as_ref(),
                )?
            };
            if reseeded {
                {
                    let obj = objective_mut!(self, oid)?;
                    Self::apply_capture_spoils_to_virtual(obj, &spoils_eq, &spoils_liq);
                }
                match self.sync_objective_to_warehouse(lua, oid) {
                    Ok((obj, wh)) => {
                        let dep_farp = objective_is_ground_dep_farp_export(export.as_ref(), obj);
                        if let Err(e) = apply_virtual_warehouse_to_dcs(
                            obj,
                            &wh,
                            dep_farp,
                            true,
                        ) {
                            error!("apply DCS warehouse after capture {oid}: {e:?}");
                        }
                    }
                    Err(e) if warehouse_sync_skip(&e) => (),
                    Err(e) => error!("sync warehouse after capture {oid}: {e:?}"),
                }
                {
                    let obj = objective_mut!(self, oid)?;
                    obj.fuel = preserved_fuel;
                }
                self.update_supply_status()
                    .context("supply status after capture warehouse reseed")?;
                {
                    let obj = objective_mut!(self, oid)?;
                    if obj.fuel != preserved_fuel {
                        obj.fuel = preserved_fuel;
                        self.ephemeral.stat(Stat::ObjectiveSupply {
                            id: oid,
                            supply: obj.supply,
                            fuel: preserved_fuel,
                        });
                    }
                }
                info!(
                    "capture warehouse {:?}: reseeded to {:?} profile with spoils (fuel {}% preserved)",
                    objective!(self, oid)?.name,
                    objective!(self, oid)?.owner,
                    preserved_fuel
                );
                return Ok(());
            }
            if objective!(self, oid)?.is_occupied_logistics_hub() {
                warn!(
                    "capture warehouse {:?}: no objective_stock profile for {:?}; \
                     rebuild Fowl export or check sortie _fowl_export.json",
                    objective!(self, oid)?.name,
                    objective!(self, oid)?.owner
                );
            }
        }
        let whcfg = self.ephemeral.cfg.warehouse.as_ref().unwrap();
        let obj = objective_mut!(self, oid)?;
        let other_production = match self.ephemeral.production_by_side.get(&obj.owner.opposite()) {
            Some(q) => Arc::clone(q),
            None => Arc::new(Production::default()),
        };
        let production = match self.ephemeral.production_by_side.get(&obj.owner) {
            Some(q) => Arc::clone(q),
            None => return Ok(()),
        };
        let map = warehouse::Warehouse::get_resource_map(lua).context("getting resource map")?;
        let on_water = objective_airbase_on_water(lua, obj)?;
        let kind = &obj.kind;
        map.for_each(|name, _| {
            match production.equipment.get(&name) {
                Some(equip) => {
                    let inv = obj.warehouse.equipment.get_or_default_cow(name);
                    inv.capacity = whcfg.capacity(kind, on_water, equip.production);
                }
                None => {
                    if let Some(_) = other_production.equipment.get(&name) {
                        let inv = obj.warehouse.equipment.get_or_default_cow(name);
                        inv.stored = 0;
                        inv.capacity = 0;
                    }
                }
            }
            Ok(())
        })?;
        for name in LiquidType::ALL {
            match production.liquids.get(&name) {
                Some(qty) => {
                    let inv = obj.warehouse.liquids.get_or_default_cow(name);
                    inv.capacity = whcfg.capacity(kind, on_water, *qty);
                }
                None => {
                    if let Some(_) = other_production.liquids.get(&name) {
                        let inv = obj.warehouse.liquids.get_or_default_cow(name);
                        inv.stored = 0;
                        inv.capacity = 0;
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn compute_supplier(&self, obj: &Objective) -> Result<Option<ObjectiveId>> {
        Ok(self
            .persisted
            .logistics_hubs
            .into_iter()
            .fold(Ok::<_, anyhow::Error>(None), |acc, id| {
                let logi = objective!(self, id)?;
                if obj.logistics_detached || logi.owner != obj.owner {
                    acc
                } else {
                    let dist =
                        na::distance_squared(&obj.zone.pos().into(), &logi.zone.pos().into());
                    match acc {
                        Err(e) => Err(e),
                        Ok(None) => Ok(Some((dist, *id))),
                        Ok(Some((pdist, _))) if dist < pdist => Ok(Some((dist, *id))),
                        Ok(Some((dist, id))) => Ok(Some((dist, id))),
                    }
                }
            })?
            .map(|(_, id)| id))
    }

    pub fn setup_supply_lines(&mut self) -> Result<()> {
        clear_virtual_resupply_efficiency_cache(&mut self.ephemeral.hub_delivery_efficiency);
        let mut suppliers: SmallVec<[(ObjectiveId, Option<ObjectiveId>); 64]> = smallvec![];
        for (oid, obj) in &self.persisted.objectives {
            match obj.kind {
                ObjectiveKind::Logistics => (),
                ObjectiveKind::Production => (),
                ObjectiveKind::Airbase | ObjectiveKind::Farp { .. } | ObjectiveKind::Fob => {
                    let hub = self.compute_supplier(obj)?;
                    suppliers.push((*oid, hub));
                }
            }
        }
        let mut current: FxHashMap<ObjectiveId, SetS<ObjectiveId>> = FxHashMap::default();
        for oid in &self.persisted.logistics_hubs {
            let obj = objective_mut!(self, oid)?;
            current.insert(*oid, mem::take(&mut obj.warehouse.destination));
        }
        for (oid, supplier) in suppliers {
            let obj = objective_mut!(self, oid)?;
            obj.warehouse.supplier = supplier;
            if let Some(id) = supplier {
                objective_mut!(self, id)?
                    .warehouse
                    .destination
                    .insert_cow(oid);
            }
        }
        for (oid, current) in current {
            let obj = objective!(self, oid)?;
            if obj.warehouse.destination != current {
                self.ephemeral.create_objective_markup(&self.persisted, obj)
            }
        }
        Ok(())
    }

    pub fn deliver_production(&mut self) -> Result<Vec<Transfer>> {
        if self.ephemeral.cfg.warehouse.is_none() {
            return Ok(vec![]);
        }
        self.refresh_hub_production_from_opr()
            .context("refreshing OPR to OLO production map")?;
        self.setup_supply_lines()
            .context("setting up supply lines")?;
        let mut deliver_produced_supplies = || -> Result<()> {
            for side in Side::ALL {
                let production = match self.ephemeral.production_by_side.get(&side) {
                    Some(e) => e,
                    None => continue,
                };
                for oid in &self.persisted.logistics_hubs {
                    let logi = objective_mut!(self, oid)?;
                    if logi.owner == side && logi.is_normal_logistics_hub() {
                        for (name, inv) in logi.warehouse.equipment.iter_mut_cow() {
                            if let Some(eq) = production.equipment.get(name) {
                                *inv += Self::scale_production_amount(eq.production, logi.production);
                            }
                        }
                        for (name, inv) in logi.warehouse.liquids.iter_mut_cow() {
                            if let Some(pr) = production.liquids.get(name) {
                                *inv += Self::scale_production_amount(*pr, logi.production);
                            }
                        }
                    }
                }
            }
            Ok(())
        };
        deliver_produced_supplies().context("delivering produced supplies")?;
        self.ephemeral.dirty();
        self.deliver_supplies_from_logistics_hubs()
            .context("delivering supplies from logistics hubs")
    }

    pub fn sync_vehicle_at_obj(
        &mut self,
        lua: MizLua,
        oid: ObjectiveId,
        typ: Vehicle,
    ) -> Result<()> {
        let obj = objective_mut!(self, oid)?;
        let id = maybe!(self.ephemeral.airbase_by_oid, oid, "airbase")?;
        let wh = Airbase::get_instance(lua, id)
            .context("getting airbase")?
            .get_warehouse()
            .context("getting warehouse")?;
        if let Some(inv) = obj.warehouse.equipment.get_mut_cow(&typ.0) {
            inv.stored = wh.get_item_count(typ.0).context("getting item")?;
            self.ephemeral.dirty();
        }
        Ok(())
    }

    pub fn deliver_supplies_from_logistics_hubs(&mut self) -> Result<Vec<Transfer>> {
        if !self.ephemeral.cfg.virtual_resupply {
            return Ok(vec![]);
        }
        self.update_supply_status()
            .context("updating supply status")?;
        let mut transfers: Vec<Transfer> = vec![];
        for lid in &self.persisted.logistics_hubs {
            let logi = objective!(self, lid)?;
            let hub_destinations = || {
                logi.warehouse
                    .destination
                    .into_iter()
                    .filter_map(|oid| Some((oid, self.persisted.objectives.get(oid)?)))
                    .filter(|(_, obj)| logi.owner == obj.owner)
            };
            let mut needed_equipment: SmallVec<[Needed; 64]> = hub_destinations()
                .filter(|(_, obj)| obj.supply < 100)
                .map(|(oid, obj)| Needed {
                    oid,
                    obj,
                    demanded: 0,
                    allocated: 0,
                })
                .collect();
            let mut needed_liquid: SmallVec<[Needed; 64]> = hub_destinations()
                .filter(|(_, obj)| obj.fuel < 100)
                .map(|(oid, obj)| Needed {
                    oid,
                    obj,
                    demanded: 0,
                    allocated: 0,
                })
                .collect();
            macro_rules! schedule_transfers {
                ($typ:expr, $from:ident, $get:ident, $needed:ident) => {
                    for (name, inv) in &logi.warehouse.$from {
                        if inv.stored == 0 {
                            continue;
                        }
                        $needed.sort_by(|n0, n1| {
                            let i0 = n0.obj.$get(name);
                            let i1 = n1.obj.$get(name);
                            i0.stored.cmp(&i1.stored)
                        });
                        let mut total_demanded = 0;
                        for n in &mut $needed {
                            let inv = n.obj.$get(name);
                            let demanded = if inv.stored <= inv.capacity {
                                inv.capacity - inv.stored
                            } else {
                                0
                            };
                            total_demanded += demanded;
                            n.demanded = demanded;
                            n.allocated = 0;
                        }
                        let mut have = inv.stored;
                        let mut total_filled = 0;
                        while have > 0 && total_filled < total_demanded {
                            for n in &mut $needed {
                                if have == 0 {
                                    break;
                                }
                                let allocation = max(1, have >> 3);
                                let amount = min(allocation, n.demanded - n.allocated);
                                n.allocated += amount;
                                total_filled += amount;
                                have -= amount;
                            }
                        }
                        for n in &$needed {
                            if n.allocated > 0 {
                                let amount = scale_by_delivery_efficiency(
                                    n.allocated,
                                    virtual_resupply_delivery_efficiency_cached(
                                        &mut self.ephemeral.hub_delivery_efficiency,
                                        &self.ephemeral.cfg,
                                        logi,
                                        n.obj,
                                    ),
                                );
                                if amount == 0 {
                                    continue;
                                }
                                transfers.push(Transfer {
                                    source: *lid,
                                    target: *n.oid,
                                    amount,
                                    item: $typ(name.clone()),
                                })
                            }
                        }
                    }
                };
            }
            schedule_transfers!(
                TransferItem::Equipment,
                equipment,
                get_equipment,
                needed_equipment
            );
            schedule_transfers!(TransferItem::Liquid, liquids, get_liquids, needed_liquid);
        }
        for occ_id in self
            .persisted
            .logistics_hubs
            .into_iter()
            .copied()
            .collect::<Vec<_>>()
        {
            let (occ_owner, occ_pos, need_supply, need_fuel) = {
                let occ = objective!(self, occ_id)?;
                if !occ.is_occupied_logistics_hub() {
                    continue;
                }
                (
                    occ.owner,
                    occ.zone.pos(),
                    occ.supply < 100,
                    occ.fuel < 100,
                )
            };
            if !need_supply && !need_fuel {
                continue;
            }
            let Some(supplier_id) = nearest_normal_logistics_hub(
                &self.persisted,
                occ_owner,
                occ_pos,
            ) else {
                continue;
            };
            if supplier_id == occ_id {
                continue;
            }
            let logi = objective!(self, supplier_id)?;
            let occ = objective!(self, occ_id)?;
            let mut needed_equipment: SmallVec<[Needed; 1]> = if need_supply {
                smallvec![Needed {
                    oid: &occ_id,
                    obj: occ,
                    demanded: 0,
                    allocated: 0,
                }]
            } else {
                smallvec![]
            };
            let mut needed_liquid: SmallVec<[Needed; 1]> = if need_fuel {
                smallvec![Needed {
                    oid: &occ_id,
                    obj: occ,
                    demanded: 0,
                    allocated: 0,
                }]
            } else {
                smallvec![]
            };
            macro_rules! schedule_occupied_transfers {
                ($typ:expr, $from:ident, $get:ident, $needed:ident) => {
                    for (name, inv) in &logi.warehouse.$from {
                        if inv.stored == 0 {
                            continue;
                        }
                        $needed.sort_by(|n0, n1| {
                            let i0 = n0.obj.$get(name);
                            let i1 = n1.obj.$get(name);
                            i0.stored.cmp(&i1.stored)
                        });
                        let mut total_demanded = 0;
                        for n in &mut $needed {
                            let inv = n.obj.$get(name);
                            let demanded = if inv.stored <= inv.capacity {
                                inv.capacity - inv.stored
                            } else {
                                0
                            };
                            total_demanded += demanded;
                            n.demanded = demanded;
                            n.allocated = 0;
                        }
                        let mut have = inv.stored;
                        let mut total_filled = 0;
                        while have > 0 && total_filled < total_demanded {
                            for n in &mut $needed {
                                if have == 0 {
                                    break;
                                }
                                let allocation = max(1, have >> 3);
                                let amount = min(allocation, n.demanded - n.allocated);
                                n.allocated += amount;
                                total_filled += amount;
                                have -= amount;
                            }
                        }
                        for n in &$needed {
                            if n.allocated > 0 {
                                let amount = scale_by_delivery_efficiency(n.allocated, 100);
                                if amount == 0 {
                                    continue;
                                }
                                transfers.push(Transfer {
                                    source: supplier_id,
                                    target: occ_id,
                                    amount,
                                    item: $typ(name.clone()),
                                })
                            }
                        }
                    }
                };
            }
            schedule_occupied_transfers!(
                TransferItem::Equipment,
                equipment,
                get_equipment,
                needed_equipment
            );
            schedule_occupied_transfers!(
                TransferItem::Liquid,
                liquids,
                get_liquids,
                needed_liquid
            );
        }
        Ok(transfers)
    }

    fn balance_logistics_hubs(&mut self) -> Result<()> {
        if !self.ephemeral.cfg.virtual_resupply {
            return Ok(());
        }
        struct Needed<'a> {
            oid: &'a ObjectiveId,
            obj: &'a Objective,
            had: u32,
            have: u32,
        }
        for side in Side::ALL {
            let mut transfers: Vec<Transfer> = vec![];
            macro_rules! schedule_transfers {
                ($typ:expr, $from:ident, $get:ident) => {{
                    let mut needed: SmallVec<[Needed; 16]> = self
                        .persisted
                        .logistics_hubs
                        .into_iter()
                        .filter_map(|lid| {
                            let obj = &self.persisted.objectives[lid];
                            if obj.owner != side || obj.is_occupied_logistics_hub() {
                                None
                            } else {
                                Some(Needed {
                                    oid: lid,
                                    obj,
                                    had: 0,
                                    have: 0,
                                })
                            }
                        })
                        .collect();
                    if needed.len() < 2 {
                        continue;
                    }
                    let items = needed[0].obj.warehouse.$from.clone();
                    for (name, _) in &items {
                        let mean = {
                            let sum: u32 = needed
                                .iter_mut()
                                .map(|n| {
                                    n.have = n.obj.$get(name).stored;
                                    n.had = n.have;
                                    n.had
                                })
                                .sum();
                            sum / needed.len() as u32
                        };
                        if mean >> 2 == 0 {
                            continue;
                        }
                        needed.sort_by(|n0, n1| n0.had.cmp(&n1.had));
                        let mut take = needed.len() - 1;
                        for i in 0..needed.len() {
                            if needed[i].have + 1 >= mean {
                                break;
                            }
                            while needed[i].have + 1 < mean {
                                while take > i && needed[take].have <= mean {
                                    take -= 1;
                                }
                                if take == i {
                                    break;
                                }
                                let need = mean - needed[i].have;
                                let available = needed[take].have - mean;
                                let xfer = min(need, available);
                                needed[i].have += xfer;
                                needed[take].have -= xfer;
                                transfers.push(Transfer {
                                    source: *needed[take].oid,
                                    target: *needed[i].oid,
                                    amount: xfer,
                                    item: $typ(name.clone()),
                                });
                            }
                        }
                    }
                }};
            }
            schedule_transfers!(TransferItem::Equipment, equipment, get_equipment);
            schedule_transfers!(TransferItem::Liquid, liquids, get_liquids);
            for tr in transfers.drain(..) {
                tr.execute(&mut self.persisted, &self.ephemeral.to_bg)
                    .with_context(|| format_compact!("executing transfer {:?}", tr))?
            }
            self.ephemeral.dirty();
        }
        self.update_supply_status()?;
        Ok(())
    }

    fn update_supply_status(&mut self) -> Result<()> {
        for (_, obj) in self.persisted.objectives.iter_mut_cow() {
            let current_supply = obj.supply;
            let current_fuel = obj.fuel;
            let mut n = 0;
            let mut sum: u32 = 0;
            for (_, inv) in &obj.warehouse.equipment {
                if inv.capacity == 0 {
                    continue;
                }
                if let Some(pct) = inv.percent() {
                    sum += pct as u32;
                    n += 1;
                }
            }
            obj.supply = if n == 0 { 0 } else { (sum / n) as u8 };
            let mut fuel_stored: u64 = 0;
            let mut fuel_capacity: u64 = 0;
            for (_, inv) in &obj.warehouse.liquids {
                if inv.capacity == 0 {
                    continue;
                }
                fuel_stored += u64::from(inv.stored);
                fuel_capacity += u64::from(inv.capacity);
            }
            obj.fuel = if fuel_capacity == 0 {
                0
            } else {
                min(100, (fuel_stored * 100 / fuel_capacity) as u32) as u8
            };
            if current_supply != obj.supply || current_fuel != obj.fuel {
                self.ephemeral.stat(Stat::ObjectiveSupply {
                    id: obj.id,
                    supply: obj.supply,
                    fuel: obj.fuel,
                });
            }
        }
        self.ephemeral.dirty();
        Ok(())
    }

    pub fn sync_warehouse_to_objective<'lua>(
        &mut self,
        lua: MizLua<'lua>,
        oid: ObjectiveId,
    ) -> Result<(&mut Objective, warehouse::Warehouse<'lua>)> {
        if matches!(objective!(self, oid)?.kind, ObjectiveKind::Production) {
            debug!(
                "warehouse sync skipped for production objective {}",
                objective!(self, oid)?.name
            );
            return Err(WarehouseSyncSkipped.into());
        }
        let obj_name = objective!(self, oid)?.name.clone();
        let owner = objective!(self, oid)?.owner;
        let on_water = objective_airbase_on_water(lua, objective!(self, oid)?)?;
        let airbase_oid = self
            .ephemeral
            .airbase_by_oid
            .get(&oid)
            .ok_or_else(|| anyhow!("no logistics for objective {}", obj_name))?
            .clone();
        let warehouse = Airbase::get_instance(lua, &airbase_oid)
            .context("getting airbase")?
            .get_warehouse()
            .context("getting warehouse")?;
        let resource_meta = self
            .warehouse_resource_meta_cache(lua)
            .context("warehouse resource meta for DCS hydrate")?;
        let export = Arc::clone(&self.ephemeral.fowl_miz_export);
        let whcfg = self.ephemeral.cfg.warehouse.as_ref();
        let production = self
            .ephemeral
            .production_by_side
            .get(&owner)
            .map(Arc::clone);
        let skip_dep_hydrate = self.dep_farp_skip_dcs_hydrate(oid);
        let obj = objective_mut!(self, oid)?;
        if objective_is_ground_dep_farp_export(export.as_ref(), obj) {
            let keep_virtual = skip_dep_hydrate || dep_farp_has_persisted_virtual_stock(obj);
            reconcile_dcs_warehouse_to_virtual(obj, &warehouse)
                .context("pruning ground DEP FARP DCS warehouse to virtual stock")?;
            if !keep_virtual {
                self.ephemeral.dep_farp_authoritative_until.remove(&oid);
                sync_warehouse_to_obj(obj, &warehouse, true)
                    .context("syncing ground DEP FARP warehouse from DCS (tracked rows only)")?;
            }
            return Ok((obj, warehouse));
        }
        sync_warehouse_to_obj(obj, &warehouse, false)
            .context("syncing warehouse to objective")?;
        discover_dcs_warehouse_into_obj(
            obj,
            &warehouse,
            export.as_ref(),
            resource_meta.as_ref(),
            whcfg,
            production.as_deref(),
            on_water,
        )
        .context("hydrating virtual warehouse from DCS inventory")?;
        Ok((obj, warehouse))
    }

    pub fn sync_objective_to_warehouse<'lua>(
        &mut self,
        lua: MizLua<'lua>,
        oid: ObjectiveId,
    ) -> Result<(&mut Objective, warehouse::Warehouse<'lua>)> {
        let resource_meta = self
            .warehouse_resource_meta_cache(lua)
            .context("warehouse resource meta for prune")?;
        let obj = objective_mut!(self, oid)?;
        if matches!(obj.kind, ObjectiveKind::Production) {
            debug!(
                "warehouse sync skipped for production objective {}",
                obj.name
            );
            return Err(WarehouseSyncSkipped.into());
        }
        let airbase = self
            .ephemeral
            .airbase_by_oid
            .get(&oid)
            .ok_or_else(|| anyhow!("no logistics for objective {}", obj.name))?;
        let warehouse = Airbase::get_instance(lua, &airbase)
            .context("getting airbase")?
            .get_warehouse()
            .context("getting warehouse")?;
        let owner = obj.owner;
        let export = self.ephemeral.fowl_miz_export.as_ref();
        prune_disallowed_dcs_weapon_stock(&warehouse, export, owner, resource_meta.as_ref())?;
        let dep_farp_export = objective_is_ground_dep_farp_export(export, obj);
        sync_obj_to_warehouse(obj, &warehouse, dep_farp_export)
            .context("syncing warehouse to objective")?;
        if dep_farp_export {
            reconcile_dcs_warehouse_to_virtual(obj, &warehouse)
                .context("pruning ground DEP FARP DCS warehouse after virtual push")?;
        }
        Ok((obj, warehouse))
    }

    pub fn transfer_supplies(
        &mut self,
        lua: MizLua,
        from: ObjectiveId,
        to: ObjectiveId,
    ) -> Result<()> {
        if from == to {
            bail!("you can't transfer supplies to the same objective")
        }
        let whcfg = match self.ephemeral.cfg.warehouse.as_ref() {
            Some(whcfg) => whcfg,
            None => return Ok(()),
        };
        let size = whcfg.supply_transfer_size as f32 / 100.;
        let side = objective!(self, from)?.owner;
        if side != objective!(self, to)?.owner {
            bail!("can't transfer supply from an enemy objective")
        }
        let mut transfers: SmallVec<[Transfer; 128]> = smallvec![];
        let (_, from_wh) = self
            .sync_warehouse_to_objective(lua, from)
            .context("syncing from objective")?;
        let (_, to_wh) = self
            .sync_warehouse_to_objective(lua, to)
            .context("syncing to objective")?;
        let from_obj = objective!(self, from)?;
        let to_obj = objective!(self, to)?;
        macro_rules! compute {
            ($src:ident, $typ:ident) => {
                for (name, inv) in &from_obj.warehouse.$src {
                    if inv.stored > 0 {
                        let needed = match to_obj.warehouse.$src.get(name) {
                            None => 0,
                            Some(inv) => {
                                if inv.capacity >= inv.stored {
                                    inv.capacity - inv.stored
                                } else {
                                    0
                                }
                            }
                        };
                        let amount = min(needed, max(1, (inv.stored as f32 * size) as u32));
                        transfers.push(Transfer {
                            amount,
                            source: from,
                            target: to,
                            item: TransferItem::$typ(name.clone()),
                        });
                    }
                }
            };
        }
        compute!(equipment, Equipment);
        compute!(liquids, Liquid);
        for tr in transfers {
            tr.execute(&mut self.persisted, &self.ephemeral.to_bg)?
        }
        let export = self.ephemeral.fowl_miz_export.as_ref();
        let from_dep = objective_is_ground_dep_farp_export(export, objective!(self, from)?);
        let to_dep = objective_is_ground_dep_farp_export(export, objective!(self, to)?);
        sync_obj_to_warehouse(objective!(self, from)?, &from_wh, from_dep)?;
        sync_obj_to_warehouse(objective!(self, to)?, &to_wh, to_dep)?;
        self.update_supply_status()
            .context("updating supply status")?;
        Ok(())
    }

    pub fn admin_reduce_inventory(
        &mut self,
        lua: MizLua,
        oid: ObjectiveId,
        amount: u8,
    ) -> Result<()> {
        if amount > 100 {
            bail!("enter a percentage")
        }
        let percent = amount as f32 / 100.;
        let production = match self
            .ephemeral
            .production_by_side
            .get(&objective!(self, oid)?.owner)
        {
            Some(p) => Arc::clone(p),
            None => return Ok(()),
        };
        let dep_farp_export = objective_is_ground_dep_farp_export(
            self.ephemeral.fowl_miz_export.as_ref(),
            objective!(self, oid)?,
        );
        let (obj, warehouse) = self
            .sync_warehouse_to_objective(lua, oid)
            .with_context(|| format_compact!("syncing warehouses to {oid}"))?;
        for name in production.equipment.keys() {
            if let Some(inv) = obj.warehouse.equipment.get_mut_cow(name) {
                inv.reduce(percent);
            }
        }
        for liq in production.liquids.keys() {
            if let Some(inv) = obj.warehouse.liquids.get_mut_cow(&liq) {
                inv.reduce(percent);
            }
        }
        sync_obj_to_warehouse(obj, &warehouse, dep_farp_export).context("syncing from warehouse")?;
        self.update_supply_status()
            .context("updating supply status")?;
        self.ephemeral.dirty();
        Ok(())
    }

    pub fn admin_log_inventory(
        &mut self,
        lua: MizLua,
        kind: WarehouseKind,
        oid: ObjectiveId,
    ) -> Result<()> {
        use std::fmt::Write;
        match kind {
            WarehouseKind::DCS => {
                let abid = self
                    .ephemeral
                    .airbase_by_oid
                    .get(&oid)
                    .ok_or_else(|| anyhow!("no airbase for {oid}"))?;
                let wh = Airbase::get_instance(lua, &abid)
                    .context("getting airbase")?
                    .get_warehouse()
                    .context("getting warehouse")?;
                let map =
                    warehouse::Warehouse::get_resource_map(lua).context("getting resource map")?;
                let mut msg = CompactString::new("");
                map.for_each(|name, _| {
                    let qty = wh
                        .get_item_count(name.clone())
                        .with_context(|| format_compact!("getting {name} count from warehouse"))?;
                    if qty > 0 {
                        write!(msg, "{name}, {qty}\n")?
                    }
                    Ok(())
                })?;
                for name in LiquidType::ALL {
                    let qty = wh.get_liquid_amount(name).with_context(|| {
                        format_compact!("getting liquid {:?} from warehouse", name)
                    })?;
                    if qty > 0 {
                        write!(msg, "{:?}, {qty}\n", name)?
                    }
                }
                warn!("{msg}")
            }
            WarehouseKind::Objective => {
                let obj = objective!(self, oid)?;
                let mut msg = CompactString::new("");
                for (name, inv) in &obj.warehouse.equipment {
                    write!(msg, "{name}, {}/{}\n", inv.stored, inv.capacity)?
                }
                for (name, inv) in &obj.warehouse.liquids {
                    write!(msg, "{:?}, {}/{}\n", name, inv.stored, inv.capacity)?
                }
                warn!("{msg}")
            }
        }
        Ok(())
    }
}
