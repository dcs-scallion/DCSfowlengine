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
    env::miz::Miz,
    land::{Land, SurfaceType},
    object::DcsObject,
    perf::record_perf,
    warehouse::{self, LiquidType, WSAircraftCategory, WSCategory},
    world::World,
    LuaEnv, LuaVec2, MizLua, String, Vector2,
};
use fxhash::{FxHashMap, FxHashSet};
use log::{debug, error, info, warn};
use mlua::{prelude::*, Value};
use serde_derive::{Deserialize, Serialize};
use smallvec::{smallvec, SmallVec};
use std::{
    cmp::{max, min},
    collections::hash_map::Entry,
    mem,
    ops::{AddAssign, SubAssign},
    sync::Arc,
};
use tokio::sync::mpsc::UnboundedSender;

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

const DYNAMIC_TEMPLATE_GROUP_PREFIX: &str = "zzDT-";
const LEGACY_DYNAMIC_TEMPLATE_PREFIX: &str = "DT-";

fn scan_dyn_spawn_template_links(lua: MizLua) -> Result<FxHashMap<(Side, String), i64>> {
    let miz = Miz::singleton(lua)?;
    let mut out: FxHashMap<(Side, String), i64> = FxHashMap::default();
    for side in [Side::Blue, Side::Red] {
        let coa = miz.coalition(side)?;
        for country in coa.countries()? {
            let country = country?;
            for groups in [country.planes()?, country.helicopters()?] {
                for group in groups {
                    let group = group?;
                    let name = group.name()?;
                    let is_dt = group
                        .raw_get::<_, bool>("dynSpawnTemplate")
                        .unwrap_or(false)
                        || name.starts_with(DYNAMIC_TEMPLATE_GROUP_PREFIX)
                        || name.starts_with(LEGACY_DYNAMIC_TEMPLATE_PREFIX);
                    if !is_dt {
                        continue;
                    }
                    let gid = group.id()?.inner();
                    let mut unit_type: Option<String> = None;
                    for unit in group.units()? {
                        unit_type = Some(unit?.typ()?);
                        break;
                    }
                    let Some(unit_type) = unit_type else {
                        continue;
                    };
                    out.insert((side, unit_type), gid);
                }
            }
        }
    }
    Ok(out)
}

fn env_warehouses_table(lua: MizLua) -> Result<Option<LuaTable>> {
    let env: LuaTable = lua.inner().globals().raw_get("env")?;
    match env.raw_get::<_, Value>("warehouses")? {
        Value::Table(t) => Ok(Some(t)),
        _ => Ok(None),
    }
}

fn me_warehouse_table_for_airbase_id<'lua>(
    warehouses: &LuaTable<'lua>,
    ab_id: i64,
) -> Result<Option<LuaTable<'lua>>> {
    for section in ["airports", "warehouses"] {
        let Ok(section_tbl) = warehouses.raw_get::<_, LuaTable>(section) else {
            continue;
        };
        // ME keys may be integer or stringified id.
        if let Ok(t) = section_tbl.raw_get::<_, LuaTable>(ab_id) {
            return Ok(Some(t));
        }
        let key = format!("{ab_id}");
        if let Ok(t) = section_tbl.raw_get::<_, LuaTable>(key.as_str()) {
            return Ok(Some(t));
        }
    }
    Ok(None)
}

fn apply_me_warehouse_link_dyn_templ(
    lua: MizLua,
    ab_id: i64,
    new_owner: Side,
    links: &FxHashMap<(Side, String), i64>,
) -> Result<usize> {
    let Some(warehouses) = env_warehouses_table(lua)? else {
        warn!("capture linkDynTempl: env.warehouses missing (cannot set ME linkDynTempl)");
        return Ok(0);
    };
    let Some(wh) = me_warehouse_table_for_airbase_id(&warehouses, ab_id)? else {
        warn!("capture linkDynTempl: no ME warehouse row for airbase id {ab_id}");
        return Ok(0);
    };
    let aircrafts: LuaTable = match wh.raw_get("aircrafts") {
        Ok(t) => t,
        Err(_) => {
            let t = lua.inner().create_table()?;
            wh.raw_set("aircrafts", t.clone())?;
            t
        }
    };
    let mut updated = 0usize;
    let mut seen: FxHashSet<String> = FxHashSet::default();
    for cat in ["helicopters", "planes"] {
        let cat_tbl: LuaTable = match aircrafts.raw_get(cat) {
            Ok(t) => t,
            Err(_) => {
                let t = lua.inner().create_table()?;
                aircrafts.raw_set(cat, t.clone())?;
                t
            }
        };
        let mut keys: SmallVec<[String; 32]> = smallvec![];
        cat_tbl.for_each(|k: Value, _: Value| {
            if let Value::String(s) = k {
                keys.push(String::from(s.to_str()?));
            }
            Ok(())
        })?;
        for unit_type in keys {
            seen.insert(unit_type.clone());
            let row: LuaTable = cat_tbl.raw_get(unit_type.as_str())?;
            let link = links
                .get(&(new_owner, unit_type.clone()))
                .copied()
                .unwrap_or(0);
            row.raw_set("linkDynTempl", link)?;
            updated += 1;
        }
    }
    // Ensure new-owner DT types exist as ME aircraft rows (stock may be added via setItem).
    let resource_map = warehouse::Warehouse::get_resource_map(lua).ok();
    for ((side, unit_type), &gid) in links {
        if *side != new_owner || seen.contains(unit_type) {
            continue;
        }
        let cat = aircraft_me_category(resource_map.as_ref(), unit_type.as_str());
        let cat_tbl: LuaTable = aircrafts.raw_get(cat)?;
        let row = lua.inner().create_table()?;
        row.raw_set("initialAmount", 0u32)?;
        row.raw_set("linkDynTempl", gid)?;
        if let Some(rm) = resource_map.as_ref() {
            if let Some(quad) = resource_map_ws_quad(rm, unit_type.as_str()) {
                let ws = lua.inner().create_table()?;
                ws.raw_set(1, quad[0])?;
                ws.raw_set(2, quad[1])?;
                ws.raw_set(3, quad[2])?;
                ws.raw_set(4, quad[3])?;
                row.raw_set("wsType", ws)?;
            }
        }
        cat_tbl.raw_set(unit_type.as_str(), row)?;
        updated += 1;
    }
    Ok(updated)
}

/// Opposite/capture: Invisible FARP / ROAD FOB ME weapon tables are incomplete vs airports.
/// Rebuild `weapons` from virtual stock (drop opponent leftovers) before `Warehouse.setItem`.
fn rebuild_me_warehouse_weapons_from_virtual(
    lua: MizLua,
    ab_id: i64,
    obj: &Objective,
    resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
) -> Result<usize> {
    let Some(warehouses) = env_warehouses_table(lua)? else {
        return Ok(0);
    };
    let Some(wh) = me_warehouse_table_for_airbase_id(&warehouses, ab_id)? else {
        return Ok(0);
    };
    let weapons = lua.inner().create_table()?;
    let mut written: FxHashSet<[i32; 4]> = FxHashSet::default();
    let mut added = 0usize;
    for (name, inv) in &obj.warehouse.equipment {
        if inv.capacity == 0 {
            continue;
        }
        if resource_meta
            .get(name.as_str())
            .map(|m| m.is_aircraft)
            .unwrap_or(false)
        {
            continue;
        }
        let Some(quad) = equipment_ws_quad(name.as_str(), resource_meta) else {
            continue;
        };
        let need_me_row = (quad[0] == 1 && quad[1] == 3)
            || (quad[0] == 4 && ((4..=8).contains(&quad[1]) || quad[1] == 15));
        if !need_me_row {
            continue;
        }
        if !written.insert(quad) {
            continue;
        }
        let label = preferred_resource_meta_name_for_quad(quad, resource_meta)
            .unwrap_or_else(|| name.clone());
        let row = lua.inner().create_table()?;
        let ws = lua.inner().create_table()?;
        ws.raw_set(1, quad[0])?;
        ws.raw_set(2, quad[1])?;
        ws.raw_set(3, quad[2])?;
        ws.raw_set(4, quad[3])?;
        row.raw_set("wsType", ws)?;
        row.raw_set("initialAmount", 0u32)?;
        row.raw_set("name", label.as_str())?;
        row.raw_set("displayName", label.as_str())?;
        added += 1;
        weapons.raw_set(added, row)?;
    }
    wh.raw_set("weapons", weapons)?;
    Ok(added)
}

fn resource_map_ws_quad(rm: &warehouse::ResourceMap, unit_type: &str) -> Option<[i32; 4]> {
    let mut out = None;
    let _ = rm.for_each(|name, ws| {
        if name.as_str() == unit_type {
            out = ws.quad().ok();
        }
        Ok(())
    });
    out
}

fn aircraft_me_category(resource_map: Option<&warehouse::ResourceMap>, unit_type: &str) -> &'static str {
    let Some(rm) = resource_map else {
        return "planes";
    };
    let mut cat = "planes";
    let _ = rm.for_each(|name, ws| {
        if name.as_str() == unit_type {
            if matches!(
                ws.category()?,
                WSCategory::Aircraft(WSAircraftCategory::Helicopters)
            ) {
                cat = "helicopters";
            }
        }
        Ok(())
    });
    cat
}

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

fn wh_diag(msg: impl AsRef<str>) {
    info!("warehouse diag: {}", msg.as_ref());
}

fn transfer_item_label(item: &TransferItem) -> CompactString {
    match item {
        TransferItem::Equipment(name) => format_compact!("eq:{name}"),
        TransferItem::Liquid(liq) => format_compact!("liq:{liq:?}"),
    }
}

impl Transfer {
    pub(super) fn target(&self) -> ObjectiveId {
        self.target
    }

    fn execute(
        &self,
        db: &mut Persisted,
        to_bg: &Option<UnboundedSender<Task>>,
        export: &FowlMizExport,
        resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
    ) -> Result<()> {
        let src_name = db
            .objectives
            .get(&self.source)
            .map(|o| o.name.clone())
            .unwrap_or_else(|| format_compact!("{:?}", self.source).into());
        let dst_name = db
            .objectives
            .get(&self.target)
            .map(|o| o.name.clone())
            .unwrap_or_else(|| format_compact!("{:?}", self.target).into());
        let item = transfer_item_label(&self.item);
        let src_cap = match &self.item {
            TransferItem::Equipment(name) => db
                .objectives
                .get(&self.source)
                .and_then(|o| o.warehouse.equipment.get(name.as_str()))
                .map(|i| i.capacity)
                .unwrap_or(0),
            TransferItem::Liquid(name) => db
                .objectives
                .get(&self.source)
                .and_then(|o| o.warehouse.liquids.get(name))
                .map(|i| i.capacity)
                .unwrap_or(0),
        };
        let src = db
            .objectives
            .get_mut_cow(&self.source)
            .ok_or_else(|| anyhow!("no such objective {:?}", self.source))?;
        let src_before = match &self.item {
            TransferItem::Equipment(name) => src.warehouse.equipment[name].stored,
            TransferItem::Liquid(name) => src.warehouse.liquids[name].stored,
        };
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
        let src_after = match &self.item {
            TransferItem::Equipment(name) => src.warehouse.equipment[name].stored,
            TransferItem::Liquid(name) => src.warehouse.liquids[name].stored,
        };
        let dst = db
            .objectives
            .get_mut_cow(&self.target)
            .ok_or_else(|| anyhow!("no such objective {:?}", self.target))?;
        let dst_before = match &self.item {
            TransferItem::Equipment(name) => dst
                .warehouse
                .equipment
                .get(name.as_str())
                .map(|i| i.stored)
                .unwrap_or(0),
            TransferItem::Liquid(name) => dst
                .warehouse
                .liquids
                .get(name)
                .map(|i| i.stored)
                .unwrap_or(0),
        };
        match &self.item {
            TransferItem::Equipment(name) => {
                let stored_after = dst_before.saturating_add(self.amount);
                let export_cap = equipment_capacity_for_discovered_row(
                    dst,
                    name.as_str(),
                    stored_after,
                    export,
                    resource_meta,
                    None,
                    None,
                    false,
                );
                let inv = dst
                    .warehouse
                    .equipment
                    .get_or_default_cow(name.clone());
                inv.stored = stored_after;
                if inv.capacity == 0 {
                    inv.capacity = export_cap.max(src_cap).max(inv.stored);
                }
                if let Some(to_bg) = to_bg.as_ref() {
                    let _ = to_bg.send(Task::Stat(Stat::EquipmentInventory {
                        id: dst.id,
                        item: name.clone(),
                        amount: inv.stored,
                    }));
                }
            }
            TransferItem::Liquid(name) => {
                let stored_after = dst_before.saturating_add(self.amount);
                let export_cap = liquid_capacity_for_discovered_row(
                    dst,
                    *name,
                    stored_after,
                    export,
                    None,
                    None,
                    false,
                );
                let inv = dst.warehouse.liquids.get_or_default_cow(*name);
                inv.stored = stored_after;
                if inv.capacity == 0 {
                    inv.capacity = export_cap.max(src_cap).max(inv.stored);
                }
                if let Some(to_bg) = to_bg.as_ref() {
                    let _ = to_bg.send(Task::Stat(Stat::LiquidInventory {
                        id: dst.id,
                        item: *name,
                        amount: inv.stored,
                    }));
                }
            }
        }
        let dst_after = match &self.item {
            TransferItem::Equipment(name) => dst.warehouse.equipment[name].stored,
            TransferItem::Liquid(name) => dst.warehouse.liquids[name].stored,
        };
        wh_diag(format_compact!(
            "transfer {item} x{amount}: {src_name} {src_before}->{src_after} => {dst_name} {dst_before}->{dst_after}",
            amount = self.amount,
        ));
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
    let mut eq_deltas = 0u32;
    let mut liq_deltas = 0u32;
    for (item, inv) in &obj.warehouse.equipment {
        perf.logistics_items.insert((item.clone(), obj.id));
        let current = warehouse
            .get_item_count(item.clone())
            .with_context(|| format_compact!("getting item count for {item}"))?;
        if current < inv.stored {
            let delta = inv.stored - current;
            warehouse
                .add_item(item.clone(), delta)
                .with_context(|| format_compact!("adding item {item}"))?;
            if eq_deltas < 12 {
                wh_diag(format_compact!(
                    "sync-to {} eq:{item} DCS {current}->{} (+{delta})",
                    obj.name,
                    inv.stored
                ));
            }
            eq_deltas += 1;
        } else if current > inv.stored {
            let delta = current - inv.stored;
            warehouse
                .remove_item(item.clone(), delta)
                .with_context(|| format_compact!("removing item {item}"))?;
            if eq_deltas < 12 {
                wh_diag(format_compact!(
                    "sync-to {} eq:{item} DCS {current}->{} (-{delta})",
                    obj.name,
                    inv.stored
                ));
            }
            eq_deltas += 1;
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
            let delta = target_kg - current;
            warehouse
                .add_liquid(*name, delta)
                .with_context(|| format_compact!("adding liquid {name:?}"))?;
            if liq_deltas < 8 {
                wh_diag(format_compact!(
                    "sync-to {} liq:{name:?} DCS {current}->{target_kg} (+{delta})",
                    obj.name
                ));
            }
            liq_deltas += 1;
        } else if current > target_kg {
            let delta = current - target_kg;
            warehouse
                .remove_liquid(*name, delta)
                .with_context(|| format_compact!("removing liquid {name:?}"))?;
            if liq_deltas < 8 {
                wh_diag(format_compact!(
                    "sync-to {} liq:{name:?} DCS {current}->{target_kg} (-{delta})",
                    obj.name
                ));
            }
            liq_deltas += 1;
        }
    }
    if eq_deltas > 0 || liq_deltas > 0 {
        wh_diag(format_compact!(
            "sync-to {} summary: {eq_deltas} equipment deltas, {liq_deltas} liquid deltas",
            obj.name
        ));
    }
    Ok(())
}

fn sync_warehouse_to_obj(
    obj: &mut Objective,
    warehouse: &warehouse::Warehouse,
    export_farp_liquids_tons: bool,
) -> Result<()> {
    let mut eq_deltas = 0u32;
    let mut liq_deltas = 0u32;
    for (name, inv) in obj.warehouse.equipment.iter_mut_cow() {
        let prev = inv.stored;
        inv.stored = warehouse.get_item_count(name.clone())?;
        if prev != inv.stored {
            if eq_deltas < 12 {
                wh_diag(format_compact!(
                    "sync-from {} eq:{name} virtual {prev}->{}",
                    obj.name,
                    inv.stored
                ));
            }
            eq_deltas += 1;
        }
    }
    for (name, inv) in obj.warehouse.liquids.iter_mut_cow() {
        let prev = inv.stored;
        let kg = warehouse.get_liquid_amount(*name)?;
        inv.stored = if export_farp_liquids_tons {
            dcs_liquid_kg_to_fowl_tons(kg)
        } else {
            kg
        };
        if prev != inv.stored {
            if liq_deltas < 8 {
                wh_diag(format_compact!(
                    "sync-from {} liq:{name:?} virtual {prev}->{}",
                    obj.name,
                    inv.stored
                ));
            }
            liq_deltas += 1;
        }
    }
    if eq_deltas > 0 || liq_deltas > 0 {
        wh_diag(format_compact!(
            "sync-from {} summary: {eq_deltas} equipment deltas, {liq_deltas} liquid deltas",
            obj.name
        ));
    }
    Ok(())
}

/// ME build may leave `initialAmount=1` on opposite DT rows so DCS registers `linkDynTempl`.
/// After SyncFrom with preserve_fill, zero those aircraft if they are not in the owner export profile.
/// Do not wipe ferried airframes already tracked in virtual stock (`stored > 0`).
fn prune_registration_aircraft_outside_export_profile(
    obj: &mut Objective,
    warehouse: &warehouse::Warehouse<'_>,
    export: &FowlMizExport,
    resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
) -> Result<usize> {
    let Some(profile) = objective_coalition_stock_for_objective(export, obj) else {
        return Ok(0);
    };
    let mut allowed: FxHashSet<String> = FxHashSet::default();
    for (name, item) in &profile.equipment {
        if item.baseline == 0 {
            continue;
        }
        let dcs_name = resolve_export_equipment_dcs_name(name.as_str(), resource_meta);
        let is_ac = resource_meta
            .get(dcs_name.as_str())
            .map(|m| m.is_aircraft)
            .unwrap_or(false);
        if is_ac {
            allowed.insert(dcs_name);
        }
    }
    let inv = warehouse
        .get_inventory(None)
        .context("warehouse inventory for DT registration prune")?;
    let mut pruned = 0usize;
    inv.aircraft()
        .context("aircraft inventory for DT registration prune")?
        .for_each(|name, qty| {
            if qty == 0 || allowed.contains(name.as_str()) {
                return Ok(());
            }
            let key = String::from(name.as_str());
            if obj
                .warehouse
                .equipment
                .get(&key)
                .map(|i| i.stored > 0)
                .unwrap_or(false)
            {
                return Ok(());
            }
            warehouse
                .set_item(name.clone(), 0)
                .with_context(|| format_compact!("zero registration A/C {name}"))?;
            obj.warehouse.equipment.remove_cow(&key);
            pruned += 1;
            Ok(())
        })?;
    Ok(pruned)
}

fn set_warehouse_equipment_count<'lua>(
    lua: MizLua<'lua>,
    warehouse: &warehouse::Warehouse<'lua>,
    name: &String,
    count: u32,
    resource_meta: Option<&FxHashMap<String, WarehouseResourceMeta>>,
) -> Result<()> {
    // Hoggit: setItem accepts name or wsType table — quads avoid alias/name mismatches (KMGU/BKF).
    if let Some(meta) = resource_meta.and_then(|m| m.get(name.as_str())) {
        if !meta.is_aircraft {
            if let Some(quad) = meta.quad {
                let wst = warehouse::WSType::from_quad(lua, quad)
                    .with_context(|| format_compact!("wsType from quad for {name}"))?;
                return warehouse
                    .set_item(warehouse::WarehouseItem::Typ(wst), count)
                    .with_context(|| {
                        format_compact!("set_item wsType {quad:?} ({name}) to {count}")
                    });
            }
        }
    }
    if let Some(quad) = parse_export_ws_type_key(name.as_str()) {
        let wst = warehouse::WSType::from_quad(lua, quad)
            .with_context(|| format_compact!("wsType from export key {name}"))?;
        return warehouse
            .set_item(warehouse::WarehouseItem::Typ(wst), count)
            .with_context(|| format_compact!("set_item wsType {quad:?} to {count}"));
    }
    warehouse
        .set_item(name.clone(), count)
        .with_context(|| format_compact!("set_item {name} to {count}"))
}

/// Drop DCS rows not in virtual; optional full profile row registration (capture reseed).
fn apply_virtual_warehouse_to_dcs<'lua>(
    lua: MizLua<'lua>,
    obj: &Objective,
    warehouse: &warehouse::Warehouse<'lua>,
    export_farp_liquids_tons: bool,
    establish_profile_rows: bool,
    resource_meta: Option<&FxHashMap<String, WarehouseResourceMeta>>,
) -> Result<()> {
    let mut virtual_quads: FxHashSet<[i32; 4]> = FxHashSet::default();
    if let Some(meta) = resource_meta {
        for (name, _) in &obj.warehouse.equipment {
            if let Some(q) = equipment_ws_quad(name.as_str(), meta) {
                virtual_quads.insert(q);
            }
        }
    }
    let inv = warehouse
        .get_inventory(None)
        .context("warehouse getInventory for virtual apply")?;
    let trim_equipment = |items: warehouse::ItemInventory<'_>| -> Result<()> {
        items.for_each(|name, qty| {
            if qty == 0 {
                return Ok(());
            }
            let in_virtual_by_name = obj.warehouse.equipment.get(name.as_str()).is_some();
            let in_virtual_by_quad = resource_meta
                .and_then(|m| m.get(name.as_str()))
                .and_then(|meta| meta.quad)
                .map(|q| virtual_quads.contains(&remap_legacy_kmgu_ws(q)))
                .unwrap_or(false);
            if !in_virtual_by_name && !in_virtual_by_quad {
                if let Some(quad) = resource_meta
                    .and_then(|m| m.get(name.as_str()))
                    .and_then(|meta| meta.quad)
                {
                    clear_dcs_warehouse_equipment_quad(lua, warehouse, quad, name.as_str())?;
                } else {
                    warehouse
                        .remove_item(name.clone(), qty)
                        .with_context(|| format_compact!("remove_item orphan {name}"))?;
                }
                return Ok(());
            }
            let Some(inv) = obj.warehouse.equipment.get(name.as_str()) else {
                // Kept by wsType quad under a different ResourceMap alias; establish sets amount.
                return Ok(());
            };
            let target = inv.stored;
            if qty != target {
                let key = String::from(name.as_str());
                set_warehouse_equipment_count(
                    lua,
                    warehouse,
                    &key,
                    target,
                    resource_meta,
                )?;
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
            set_warehouse_equipment_count(lua, warehouse, name, inv.stored, resource_meta)?;
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
    lua: MizLua<'lua>,
    obj: &Objective,
    warehouse: &warehouse::Warehouse<'lua>,
    resource_meta: Option<&FxHashMap<String, WarehouseResourceMeta>>,
) -> Result<()> {
    apply_virtual_warehouse_to_dcs(lua, obj, warehouse, true, false, resource_meta)
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
            // Keep template ceiling; stored may exceed after dynamic cargo To stock.
            return inv.capacity;
        }
    }
    if let (Some(whcfg), Some(prod)) = (whcfg, production) {
        if let Some(eq) = prod.equipment.get(name) {
            return whcfg.capacity(&obj.kind, on_water, eq.production);
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
            return inv.capacity;
        }
    }
    if let (Some(whcfg), Some(prod)) = (whcfg, production) {
        if let Some(qty) = prod.liquids.get(&typ) {
            return whcfg.capacity(&obj.kind, on_water, *qty);
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
                if capacity > 0 {
                    row.capacity = capacity;
                } else if row.capacity == 0 {
                    row.capacity = stored.max(1);
                }
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
            let stored_virtual = if objective_liquids_stored_as_tons(export, obj) {
                dcs_liquid_kg_to_fowl_tons(stored)
            } else {
                stored
            };
            let capacity = liquid_capacity_for_discovered_row(
                obj,
                typ,
                stored_virtual,
                export,
                whcfg,
                production,
                on_water,
            );
            let row = obj.warehouse.liquids.get_or_default_cow(typ);
            row.stored = stored_virtual;
            if capacity > 0 {
                row.capacity = capacity;
            } else if row.capacity == 0 {
                row.capacity = stored_virtual.max(1);
            }
            Ok(())
        })?;
    Ok(())
}

/// FARP/helipad `getInventory` can omit SKUs that `getItemCount` still returns.
/// Ensure side production keys exist in virtual for logistics/production.
fn hydrate_production_keys_from_dcs(
    obj: &mut Objective,
    warehouse: &warehouse::Warehouse,
    export: &FowlMizExport,
    resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
    whcfg: Option<&bfprotocols::cfg::WarehouseConfig>,
    production: &Production,
    on_water: bool,
) -> Result<u32> {
    let mut new_eq = 0u32;
    for name in production.equipment.keys() {
        let meta = resource_meta.get(name).copied();
        if !equipment_allowed_for_objective(export, obj, obj.owner, name.as_str(), meta) {
            continue;
        }
        let stored = warehouse
            .get_item_count(name.clone())
            .with_context(|| format_compact!("get_item_count production hydrate {name}"))?;
        let capacity = equipment_capacity_for_discovered_row(
            obj,
            name.as_str(),
            stored,
            export,
            resource_meta,
            whcfg,
            Some(production),
            on_water,
        );
        let existed = obj.warehouse.equipment.get(name).is_some();
        let row = obj.warehouse.equipment.get_or_default_cow(name.clone());
        row.stored = stored;
        if capacity > 0 {
            row.capacity = capacity;
        } else if row.capacity == 0 {
            row.capacity = stored.max(1);
        }
        if !existed {
            new_eq = new_eq.saturating_add(1);
        }
    }
    for typ in production.liquids.keys() {
        let stored_kg = warehouse
            .get_liquid_amount(*typ)
            .with_context(|| format_compact!("get_liquid_amount production hydrate {typ:?}"))?;
        let stored = if objective_liquids_stored_as_tons(export, obj) {
            dcs_liquid_kg_to_fowl_tons(stored_kg)
        } else {
            stored_kg
        };
        let capacity = liquid_capacity_for_discovered_row(
            obj,
            *typ,
            stored,
            export,
            whcfg,
            Some(production),
            on_water,
        );
        let row = obj.warehouse.liquids.get_or_default_cow(*typ);
        row.stored = stored;
        if capacity > 0 {
            row.capacity = capacity;
        } else if row.capacity == 0 {
            row.capacity = stored.max(1);
        }
    }
    if new_eq > 0 {
        wh_diag(format_compact!(
            "production-hydrate {}: +{new_eq} equipment keys via get_item_count",
            obj.name
        ));
    }
    Ok(new_eq)
}

fn mark_production_equipment_dcs_tracked(
    ephemeral: &mut super::ephemeral::Ephemeral,
    oid: ObjectiveId,
    production: &Production,
) {
    let Some(tracked) = ephemeral.warehouse_dcs_equipment_names.get_mut(&oid) else {
        return;
    };
    for name in production.equipment.keys() {
        tracked.insert(name.clone());
    }
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

/// Fowl export / bftools `InitFuel` baselines: metric tons; DCS `getLiquidAmount` / `setLiquidAmount`: kg.
const FOWL_LIQUID_TONS_TO_DCS_KG: u32 = 1000;

/// Export `InitFuel` / `objective_stock` liquid baselines are metric tons; DCS API uses kg.
pub(crate) fn objective_liquids_stored_as_tons(export: &FowlMizExport, obj: &Objective) -> bool {
    if matches!(obj.kind, ObjectiveKind::Production) {
        return false;
    }
    objective_coalition_stock_for_objective(export, obj).is_some()
        || objective_is_ground_dep_farp_export(export, obj)
}

pub(crate) fn fowl_liquid_tons_to_dcs_kg(tons: u32) -> u32 {
    tons.saturating_mul(FOWL_LIQUID_TONS_TO_DCS_KG)
}

pub(crate) fn dcs_liquid_kg_to_fowl_tons(kg: u32) -> u32 {
    kg / FOWL_LIQUID_TONS_TO_DCS_KG
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

fn objective_coalition_stock_for_objective_side<'a>(
    export: &'a FowlMizExport,
    obj: &Objective,
    side: Side,
) -> Option<&'a ObjectiveCoalitionStock> {
    if objective_is_ground_dep_farp(obj) {
        let ObjectiveKind::Farp { pad_template, .. } = &obj.kind else {
            return None;
        };
        return objective_coalition_stock_for_side(export, pad_template.as_str(), side).or_else(
            || objective_coalition_stock_for_side(export, obj.name.as_str(), side),
        );
    }
    if let Some(stock) = objective_coalition_stock_for_side(export, obj.name.as_str(), side) {
        return Some(stock);
    }
    let keys = farp_export_lookup_keys(obj)?;
    for key in keys {
        if let Some(stock) = objective_coalition_stock_for_side(export, key.as_str(), side) {
            return Some(stock);
        }
    }
    None
}

fn objective_coalition_stock_for_objective<'a>(
    export: &'a FowlMizExport,
    obj: &Objective,
) -> Option<&'a ObjectiveCoalitionStock> {
    objective_coalition_stock_for_objective_side(export, obj, obj.owner)
}

/// New clean campaign only: list objectives missing Blue and/or Red `objective_stock` rows.
fn missing_objective_stock_profile_labels(
    export: &FowlMizExport,
    objectives: &super::MapM<ObjectiveId, Objective>,
) -> Vec<CompactString> {
    let mut missing: Vec<CompactString> = Vec::new();
    for (_, obj) in objectives {
        if matches!(obj.kind, ObjectiveKind::Production) {
            continue;
        }
        let mut sides: SmallVec<[&'static str; 2]> = smallvec![];
        if objective_coalition_stock_for_objective_side(export, obj, Side::Blue).is_none() {
            sides.push("Blue");
        }
        if objective_coalition_stock_for_objective_side(export, obj, Side::Red).is_none() {
            sides.push("Red");
        }
        if !sides.is_empty() {
            missing.push(format_compact!("{} ({})", obj.name, sides.join(", ")));
        }
    }
    missing.sort();
    missing
}

/// Capture spoils: old warehouse stock that is also in the new owner's export template.
/// Amounts are exact (may exceed new template baseline); Neutral→capture needs no old profile.
fn capture_spoils_intersection(
    export: &FowlMizExport,
    obj: &Objective,
    _previous_owner: Side,
    new_owner: Side,
    resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
    warehouse_eq: &FxHashMap<String, u32>,
    warehouse_liq: &FxHashMap<LiquidType, u32>,
) -> (FxHashMap<String, u32>, FxHashMap<LiquidType, u32>) {
    let mut spoils_eq: FxHashMap<String, u32> = FxHashMap::default();
    let mut spoils_liq: FxHashMap<LiquidType, u32> = FxHashMap::default();
    let Some(new_prof) = objective_coalition_stock_for_objective_side(export, obj, new_owner)
    else {
        return (spoils_eq, spoils_liq);
    };
    let new_eq: FxHashSet<String> = new_prof
        .equipment
        .iter()
        .filter(|(_, item)| item.baseline > 0)
        .map(|(k, _)| resolve_export_equipment_dcs_name(k.as_str(), resource_meta))
        .collect();
    for (name, &stored) in warehouse_eq {
        if stored > 0 && new_eq.contains(name) {
            spoils_eq.insert(name.clone(), stored);
        }
    }
    let new_liq: FxHashSet<LiquidType> = new_prof
        .liquids
        .iter()
        .filter(|(_, item)| item.baseline > 0)
        .filter_map(|(k, _)| liquid_type_from_export_key(k.as_str()).ok())
        .collect();
    for (typ, &stored) in warehouse_liq {
        if stored > 0 && new_liq.contains(typ) {
            spoils_liq.insert(*typ, stored);
        }
    }
    (spoils_eq, spoils_liq)
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
    let stored_in_tons = objective_liquids_stored_as_tons(export, obj);
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
        [a, b, c, d] => Some(remap_legacy_kmgu_ws([*a, *b, *c, *d])),
        _ => None,
    }
}

/// Zone/INV legacy KMGU markers → armable BKF/KMGU-2 dispenser wsTypes.
fn remap_legacy_kmgu_ws(ws: [i32; 4]) -> [i32; 4] {
    match ws {
        [4, 5, 32, 94] => [4, 5, 38, 361],
        [4, 5, 32, 95] => [4, 5, 38, 362],
        other => other,
    }
}

/// ME may keep both legacy `[4,5,32,94|95]` and armable `[4,5,38,361|362]` rows; DCS
/// Inventory treats them as distinct — clear every alias when zeroing orphans.
fn kmgu_ws_aliases(ws: [i32; 4]) -> SmallVec<[[i32; 4]; 2]> {
    match remap_legacy_kmgu_ws(ws) {
        [4, 5, 38, 361] => smallvec![[4, 5, 38, 361], [4, 5, 32, 94]],
        [4, 5, 38, 362] => smallvec![[4, 5, 38, 362], [4, 5, 32, 95]],
        other => smallvec![other],
    }
}

/// Zero DCS stock by wsType (KMGU/BKF name aliases are unreliable for removeItem).
fn clear_dcs_warehouse_equipment_quad<'lua>(
    lua: MizLua<'lua>,
    warehouse: &warehouse::Warehouse<'lua>,
    quad: [i32; 4],
    label: &str,
) -> Result<()> {
    for q in kmgu_ws_aliases(quad) {
        let wst = warehouse::WSType::from_quad(lua, q).with_context(|| {
            format_compact!("wsType from quad {q:?} clearing {label}")
        })?;
        warehouse
            .set_item(warehouse::WarehouseItem::Typ(wst), 0)
            .with_context(|| format_compact!("set_item wsType {q:?} (clear {label}) to 0"))?;
    }
    Ok(())
}

fn equipment_ws_quad(
    name: &str,
    resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
) -> Option<[i32; 4]> {
    if let Some(meta) = resource_meta.get(name) {
        if let Some(q) = meta.quad {
            return Some(remap_legacy_kmgu_ws(q));
        }
    }
    parse_export_ws_type_key(name)
}

fn preferred_resource_meta_name_for_quad(
    quad: [i32; 4],
    resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
) -> Option<String> {
    let mut best: Option<&String> = None;
    for (name, meta) in resource_meta {
        if meta.quad == Some(quad) {
            best = Some(match best {
                None => name,
                Some(cur) => {
                    let pick = preferred_dcs_equipment_name(cur.as_str(), name.as_str());
                    if pick == name.as_str() {
                        name
                    } else {
                        cur
                    }
                }
            });
        }
    }
    best.cloned()
}

fn resolve_export_equipment_dcs_name(
    export_key: &str,
    resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
) -> String {
    if let Some(quad) = parse_export_ws_type_key(export_key) {
        if let Some(name) = preferred_resource_meta_name_for_quad(quad, resource_meta) {
            return name;
        }
    }
    if let Some(meta) = resource_meta.get(export_key) {
        if let Some(quad) = meta.quad {
            let quad = remap_legacy_kmgu_ws(quad);
            if let Some(name) = preferred_resource_meta_name_for_quad(quad, resource_meta) {
                return name;
            }
        }
    }
    String::from(export_key)
}

/// Prefer Warehouse.setItem-friendly labels (KMGU/BKF/UPK…), not numeric launcher ids.
fn dcs_equipment_name_score(s: &str) -> i32 {
    let u = s.to_ascii_uppercase();
    if u.contains("EMPTY") {
        return 2000;
    }
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
        return 1500;
    }
    if u.contains("BYCLSID") || u.contains("CATEGORIES/") || u.starts_with("DB/") {
        return 900;
    }
    if (u.starts_with('{') && u.ends_with('}')) || u.contains("GUID") {
        return 700;
    }
    // Composite dispenser / gun-pod family: human labels beat short CLSID stubs.
    if u.contains("KMGU") && (u.contains(" - ") || u.contains("DISPENSER")) {
        return -50;
    }
    if u.contains("BKF") && u.contains(" - ") {
        return -45;
    }
    if u.contains("KMGU") || u.contains("BKF") || u.contains("UPK") || u.contains("SPPU") || u.contains("PKT")
    {
        if u.contains(" - ") || u.contains("GUN POD") || u.contains("MMG") {
            return -40;
        }
        return -10;
    }
    if u.contains("GIAT") || u.contains("M621") {
        return -20;
    }
    s.len() as i32
}

fn preferred_dcs_equipment_name<'a>(a: &'a str, b: &'a str) -> &'a str {
    let sa = dcs_equipment_name_score(a);
    let sb = dcs_equipment_name_score(b);
    if sa != sb {
        return if sa < sb { a } else { b };
    }
    if a.len() != b.len() {
        return if a.len() < b.len() { a } else { b };
    }
    if a < b {
        a
    } else {
        b
    }
}

/// Merge export `wsType [...]` virtual rows onto resolved DCS resource names.
fn canonicalize_virtual_equipment_keys(
    obj: &mut Objective,
    resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
) {
    let names: SmallVec<[String; 64]> = obj
        .warehouse
        .equipment
        .iter_mut_cow()
        .map(|(name, _)| name.clone())
        .collect();
    for old in names {
        let canonical = resolve_export_equipment_dcs_name(old.as_str(), resource_meta);
        if canonical == old {
            continue;
        }
        let Some(old_inv) = obj.warehouse.equipment.remove_cow(&old) else {
            continue;
        };
        let row = obj.warehouse.equipment.get_or_default_cow(canonical);
        if old_inv.stored > row.stored {
            row.stored = old_inv.stored;
        }
        if old_inv.capacity > row.capacity {
            row.capacity = old_inv.capacity;
        }
        if row.capacity == 0 && row.stored > 0 {
            row.capacity = row.stored;
        }
    }
}

/// After DCS setItem, align virtual keys to getInventory aliases (FARP ME name ≠ ResourceMap preferred).
fn rematch_virtual_equipment_to_dcs_names(
    obj: &mut Objective,
    dcs_names: &FxHashSet<String>,
    resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
) {
    let mut quad_to_dcs: FxHashMap<[i32; 4], String> = FxHashMap::default();
    for name in dcs_names {
        let Some(meta) = resource_meta.get(name.as_str()) else {
            continue;
        };
        let Some(q) = meta.quad.map(remap_legacy_kmgu_ws) else {
            continue;
        };
        match quad_to_dcs.get(&q) {
            None => {
                quad_to_dcs.insert(q, name.clone());
            }
            Some(cur) => {
                let pick = preferred_dcs_equipment_name(cur.as_str(), name.as_str());
                if pick == name.as_str() {
                    quad_to_dcs.insert(q, name.clone());
                }
            }
        }
    }
    let names: SmallVec<[String; 64]> = obj
        .warehouse
        .equipment
        .iter_mut_cow()
        .map(|(name, _)| name.clone())
        .collect();
    for old in names {
        let Some(quad) = equipment_ws_quad(old.as_str(), resource_meta) else {
            continue;
        };
        let Some(canonical) = quad_to_dcs.get(&quad).cloned() else {
            continue;
        };
        if canonical == old {
            continue;
        }
        let Some(old_inv) = obj.warehouse.equipment.remove_cow(&old) else {
            continue;
        };
        let row = obj.warehouse.equipment.get_or_default_cow(canonical);
        if old_inv.stored > row.stored {
            row.stored = old_inv.stored;
        }
        if old_inv.capacity > row.capacity {
            row.capacity = old_inv.capacity;
        }
        if row.capacity == 0 && row.stored > 0 {
            row.capacity = row.stored;
        }
    }
}

fn collect_dcs_warehouse_equipment_names(
    warehouse: &warehouse::Warehouse,
) -> Result<FxHashSet<String>> {
    let inv = warehouse
        .get_inventory(None)
        .context("warehouse getInventory for supply tracking")?;
    let mut out: FxHashSet<String> = FxHashSet::default();
    let mut ingest = |items: warehouse::ItemInventory<'_>| -> Result<()> {
        items.for_each(|name, _qty| {
            out.insert(name);
            Ok(())
        })
    };
    ingest(inv.weapons().context("warehouse weapon inventory for supply tracking")?)?;
    ingest(inv.aircraft().context("warehouse aircraft inventory for supply tracking")?)?;
    Ok(out)
}

fn record_objective_dcs_equipment_names(
    ephemeral: &mut super::ephemeral::Ephemeral,
    oid: ObjectiveId,
    warehouse: &warehouse::Warehouse,
) -> Result<()> {
    ephemeral
        .warehouse_dcs_equipment_names
        .insert(oid, collect_dcs_warehouse_equipment_names(warehouse)?);
    Ok(())
}

/// Non-OLO: drop airframe demand for types not in the objective export template (baseline 0 / missing).
/// Keep ferried stock (`stored > 0`) like delivered weapons outside the local template.
fn clamp_non_olo_airframe_rows_to_export(
    obj: &mut Objective,
    export: &FowlMizExport,
    resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
) {
    if matches!(obj.kind, ObjectiveKind::Logistics | ObjectiveKind::Production) {
        return;
    }
    let profile = objective_coalition_stock_for_objective(export, obj);
    for (name, inv) in obj.warehouse.equipment.iter_mut_cow() {
        let Some(meta) = resource_meta.get(name) else {
            continue;
        };
        if !meta.is_aircraft {
            continue;
        }
        let baseline = profile
            .and_then(|p| profile_export_equipment_item(p, name.as_str(), resource_meta))
            .map(|item| item.baseline)
            .unwrap_or(0);
        if baseline == 0 {
            if inv.stored > 0 {
                inv.capacity = inv.capacity.max(inv.stored);
                continue;
            }
            inv.capacity = 0;
            inv.stored = 0;
        }
    }
}

/// DT registration / filler pattern: empty 0/1 (or capacity 1) rows must not skew Supply %.
/// Warehouse rows are unchanged — this is only for the Supply % average.
fn equipment_counts_toward_supply_pct(inv: &Inventory) -> bool {
    !(inv.stored == 0 && inv.capacity <= 1)
}

/// SETTINGS-Ai supplement airframes (`production == 0`): align virtual capacity to DCS stock.
/// Legacy exports doubled baseline via `merge_ai_template_stock_export`; skip when already matched.
fn reconcile_objective_stock_aircraft_capacity_to_dcs(
    obj: &mut Objective,
    export: &FowlMizExport,
    resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
) {
    let Some(profile) = objective_coalition_stock_for_objective(export, obj) else {
        return;
    };
    if matches!(obj.kind, ObjectiveKind::Production) {
        return;
    }
    for (name, inv) in obj.warehouse.equipment.iter_mut_cow() {
        let Some(meta) = resource_meta.get(name) else {
            continue;
        };
        if !meta.is_aircraft {
            continue;
        }
        let Some(item) = profile_export_equipment_item(profile, name.as_str(), resource_meta) else {
            continue;
        };
        if item.production != 0 {
            continue;
        }
        if inv.stored == 0 || inv.capacity <= inv.stored {
            continue;
        }
        if inv.capacity == inv.stored.saturating_mul(2) {
            inv.capacity = inv.stored.max(1);
        }
    }
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
    // When bftools wrote a non-empty profile, never invent SKUs outside it (OAB/OFO/DEP/OLO).
    if profile.equipment.values().any(|i| i.baseline > 0) {
        return profile_export_equipment_has(profile, dcs_name, resource_meta);
    }
    let _ = obj;
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
    // OLO: `objective_stock` is full INV+DEFAULT synthesize; `objective_defaults` weapon
    // lists are thin TTD/policy (e.g. Zestafoni red_weapon_ws=3) and must not prune hubs.
    if matches!(obj.kind, ObjectiveKind::Logistics)
        && objective_coalition_stock_for_side(export, obj.name.as_str(), side).is_some()
    {
        return true;
    }
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
    lua: MizLua<'lua>,
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
            let quad_n = remap_legacy_kmgu_ws(quad);
            if !row_subject_to_weapon_allowlist(export, &quad)
                && !row_subject_to_weapon_allowlist(export, &quad_n)
            {
                return Ok(());
            }
            if allowed.contains(&quad) || allowed.contains(&quad_n) {
                return Ok(());
            }
            clear_dcs_warehouse_equipment_quad(lua, warehouse, quad, name.as_str())?;
            Ok(())
        })
        .context("pruning disallowed weapon stock from DCS")?;
    Ok(())
}

pub(super) fn hub_to_objective_distance_km(hub: &Objective, dest: &Objective) -> f64 {
    na::distance(&hub.zone.pos().into(), &dest.zone.pos().into()) / 1000.
}

pub(super) fn virtual_resupply_threatened_without_deliveries(cfg: &bfprotocols::cfg::Cfg) -> bool {
    cfg.virtual_resupply && cfg.virtual_resupply_threatened_without_deliveries
}

pub(super) fn virtual_resupply_threatened_blocks(
    cfg: &bfprotocols::cfg::Cfg,
    obj: &Objective,
) -> bool {
    virtual_resupply_threatened_without_deliveries(cfg) && obj.threatened
}

/// Snapshot before DCS sync-from (threatened: block DCS/ME stock refill).
fn snapshot_liquid_stored(obj: &Objective) -> SmallVec<[(LiquidType, u32); 4]> {
    obj.warehouse
        .liquids
        .into_iter()
        .map(|(typ, inv)| (*typ, inv.stored))
        .collect()
}

fn snapshot_equipment_stored(obj: &Objective) -> FxHashMap<String, u32> {
    obj.warehouse
        .equipment
        .into_iter()
        .map(|(name, inv)| (name.clone(), inv.stored))
        .collect()
}

fn clamp_liquid_stored_no_increase(obj: &mut Objective, previous: &[(LiquidType, u32)]) {
    for &(typ, prev_stored) in previous {
        if let Some(inv) = obj.warehouse.liquids.get_mut_cow(&typ) {
            if inv.stored > prev_stored {
                inv.stored = prev_stored;
            }
        }
    }
}

fn clamp_equipment_stored_no_increase(obj: &mut Objective, previous: &FxHashMap<String, u32>) {
    for (name, inv) in obj.warehouse.equipment.iter_mut_cow() {
        let prev = previous.get(name.as_str()).copied().unwrap_or(0);
        if inv.stored > prev {
            inv.stored = prev;
        }
    }
}

fn clamp_threatened_sync_from_refill(
    obj: &mut Objective,
    prev_equipment: &FxHashMap<String, u32>,
    prev_liquids: &[(LiquidType, u32)],
) {
    clamp_equipment_stored_no_increase(obj, prev_equipment);
    clamp_liquid_stored_no_increase(obj, prev_liquids);
}

pub(super) fn virtual_resupply_link_active(
    cfg: &bfprotocols::cfg::Cfg,
    hub: &Objective,
    dest: &Objective,
) -> bool {
    !virtual_resupply_threatened_blocks(cfg, hub)
        && !virtual_resupply_threatened_blocks(cfg, dest)
}

pub(super) fn virtual_resupply_dest_receives(
    cfg: &bfprotocols::cfg::Cfg,
    hub: &Objective,
    dest: &Objective,
    mobile_underway: &FxHashSet<ObjectiveId>,
) -> bool {
    virtual_resupply_link_active(cfg, hub, dest) && !mobile_underway.contains(&dest.id)
}

pub(super) fn opr_feed_hub(persisted: &Persisted, opr: &Objective) -> Option<ObjectiveId> {
    if !matches!(opr.kind, ObjectiveKind::Production) {
        return None;
    }
    opr.feed_hub
        .or_else(|| nearest_logistics_hub(persisted, opr.owner, opr.zone.pos()))
}

fn hub_production_from_active_oprs(
    persisted: &Persisted,
    cfg: &bfprotocols::cfg::Cfg,
    hub_id: ObjectiveId,
) -> u8 {
    let Some(hub) = persisted.objectives.get(&hub_id) else {
        return 0;
    };
    let mut weighted: u32 = 0;
    let mut cap_sum: u32 = 0;
    for (_, opr) in persisted.objectives.into_iter() {
        if !matches!(opr.kind, ObjectiveKind::Production) {
            continue;
        }
        if opr_feed_hub(persisted, opr) != Some(hub_id) {
            continue;
        }
        if !production_feed_line_active(cfg, opr, hub) {
            continue;
        }
        let capacity = opr.production_capacity.max(1);
        weighted = weighted.saturating_add(u32::from(opr.production) * u32::from(capacity));
        cap_sum = cap_sum.saturating_add(u32::from(capacity));
    }
    if cap_sum == 0 {
        return 0;
    }
    let denom = cap_sum.max(1) as u64;
    let numer = weighted as u64;
    ((numer + denom - 1) / denom).min(100) as u8
}

/// F10 OLO Production: factory damage (OPR `production` %) and threatened feed cuts vs full assigned capacity.
fn hub_production_display_from_opr_feeds(
    persisted: &Persisted,
    cfg: &bfprotocols::cfg::Cfg,
    hub_id: ObjectiveId,
) -> u8 {
    let (active, potential) = hub_opr_feed_weighted_sums(persisted, cfg, hub_id);
    weighted_production_pct(active, potential)
}

fn hub_opr_feed_weighted_sums(
    persisted: &Persisted,
    cfg: &bfprotocols::cfg::Cfg,
    hub_id: ObjectiveId,
) -> (u32, u32) {
    let Some(hub) = persisted.objectives.get(&hub_id) else {
        return (0, 0);
    };
    let mut active_weighted: u32 = 0;
    let mut potential_weighted: u32 = 0;
    for (_, opr) in persisted.objectives.into_iter() {
        if !matches!(opr.kind, ObjectiveKind::Production) {
            continue;
        }
        if opr_feed_hub(persisted, opr) != Some(hub_id) {
            continue;
        }
        let capacity = opr.production_capacity.max(1);
        potential_weighted = potential_weighted.saturating_add(
            100u32.saturating_mul(u32::from(capacity)),
        );
        if production_feed_line_active(cfg, opr, hub) {
            active_weighted = active_weighted.saturating_add(
                u32::from(opr.production).saturating_mul(u32::from(capacity)),
            );
        }
    }
    (active_weighted, potential_weighted)
}

fn weighted_production_pct(active_weighted: u32, potential_weighted: u32) -> u8 {
    if potential_weighted == 0 {
        return 0;
    }
    let numer = u64::from(active_weighted).saturating_mul(100);
    let denom = u64::from(potential_weighted);
    ((numer + denom - 1) / denom).min(100) as u8
}

fn log_opr_threat_hub_production(
    db: &Db,
    opr_id: ObjectiveId,
    opr: &Objective,
) {
    if !virtual_resupply_threatened_without_deliveries(&db.ephemeral.cfg) {
        return;
    }
    if !matches!(opr.kind, ObjectiveKind::Production) {
        return;
    }
    let cfg = &db.ephemeral.cfg;
    let persisted = &db.persisted;
    let Some(hid) = opr_feed_hub(persisted, opr) else {
        info!(
            "OPR threat feed: opr {} id={:?} threatened={} has no OLO feed hub",
            db.objective_f10_map_label(opr),
            opr_id,
            opr.threatened,
        );
        return;
    };
    let Some(hub) = persisted.objectives.get(&hid) else {
        return;
    };
    let eff = effective_hub_production(cfg, persisted, hub);
    let (active, potential) = hub_opr_feed_weighted_sums(persisted, cfg, hid);
    let mut feeders = CompactString::default();
    for (_, o) in persisted.objectives.into_iter() {
        if !matches!(o.kind, ObjectiveKind::Production) {
            continue;
        }
        if opr_feed_hub(persisted, o) != Some(hid) {
            continue;
        }
        if !feeders.is_empty() {
            feeders.push_str("; ");
        }
        feeders.push_str(&format_compact!(
            "{} th={} prod={}% active={}",
            db.objective_f10_map_label(o),
            o.threatened,
            o.production,
            production_feed_line_active(cfg, o, hub),
        ));
    }
    info!(
        "OPR threat feed: opr {} threatened={} -> hub {} stored={}% display={}% active_w={} potential_w={} feeders=[{}]",
        db.objective_f10_map_label(opr),
        opr.threatened,
        db.objective_f10_map_label(hub),
        hub.production,
        eff,
        active,
        potential,
        feeders,
    );
}

pub(super) fn effective_hub_production(
    cfg: &bfprotocols::cfg::Cfg,
    persisted: &Persisted,
    hub: &Objective,
) -> u8 {
    if virtual_resupply_threatened_blocks(cfg, hub) {
        return 0;
    }
    if virtual_resupply_threatened_without_deliveries(cfg) {
        hub_production_display_from_opr_feeds(persisted, cfg, hub.id)
    } else {
        hub.production
    }
}

pub(super) fn production_feed_line_active(
    cfg: &bfprotocols::cfg::Cfg,
    opr: &Objective,
    hub: &Objective,
) -> bool {
    opr.production > 0
        && !virtual_resupply_threatened_blocks(cfg, opr)
        && !virtual_resupply_threatened_blocks(cfg, hub)
}

/// OPR feed line mark target; None when the line is hidden (threat, zero production, no hub).
pub(super) fn visible_production_feed_hub(
    cfg: &bfprotocols::cfg::Cfg,
    persisted: &Persisted,
    obj: &Objective,
) -> Option<ObjectiveId> {
    let hid = opr_feed_hub(persisted, obj)?;
    let hub = persisted.objectives.get(&hid)?;
    if production_feed_line_active(cfg, obj, hub) {
        Some(hid)
    } else {
        None
    }
}

/// Occupied-hub supply line anchor; None when the line is not drawn.
pub(super) fn visible_occupied_supply_anchor(
    cfg: &bfprotocols::cfg::Cfg,
    persisted: &Persisted,
    obj: &Objective,
) -> Option<ObjectiveId> {
    if !obj.is_occupied_logistics_hub() || virtual_resupply_threatened_blocks(cfg, obj) {
        return None;
    }
    let aid = nearest_normal_logistics_hub(persisted, obj.owner, obj.zone.pos())?;
    let anchor = persisted.objectives.get(&aid)?;
    if virtual_resupply_threatened_blocks(cfg, anchor) {
        None
    } else {
        Some(aid)
    }
}

pub(crate) fn refresh_virtual_resupply_threat_markups(
    persisted: &Persisted,
    ephemeral: &mut super::ephemeral::Ephemeral,
    changed: &[ObjectiveId],
) {
    if !virtual_resupply_threatened_without_deliveries(&ephemeral.cfg) {
        return;
    }
    for oid in changed {
        let Some(obj) = persisted.objectives.get(oid) else {
            continue;
        };
        ephemeral.update_objective_markup(persisted, obj, &[]);
        if let Some(sid) = obj.warehouse.supplier {
            if let Some(hub) = persisted.objectives.get(&sid) {
                ephemeral.update_objective_markup(persisted, hub, &[*oid]);
            }
        }
        if matches!(obj.kind, ObjectiveKind::Logistics) {
            for (pid, opr) in &persisted.objectives {
                if opr_feed_hub(persisted, opr) == Some(*oid) {
                    if let Some(opr_obj) = persisted.objectives.get(pid) {
                        ephemeral.update_objective_markup(persisted, opr_obj, &[*oid]);
                    }
                }
            }
        }
        if matches!(obj.kind, ObjectiveKind::Production) {
            if let Some(hid) = opr_feed_hub(persisted, obj) {
                if let Some(hub) = persisted.objectives.get(&hid) {
                    ephemeral.update_objective_markup(persisted, hub, &[*oid]);
                }
            }
        }
    }
}

impl Db {
    /// Recompute every OLO `production` from non-threatened OPR feeds (flagged virtual resupply).
    pub(crate) fn recompute_all_logistics_hub_production_from_opr_feeds(&mut self) -> Result<()> {
        if !virtual_resupply_threatened_without_deliveries(&self.ephemeral.cfg) {
            return Ok(());
        }
        let cfg = &self.ephemeral.cfg;
        for hid in self.persisted.logistics_hubs.clone().into_iter() {
            let new_prod = {
                let hub_ref = objective!(self, hid)?;
                if virtual_resupply_threatened_blocks(cfg, hub_ref) {
                    0
                } else {
                    hub_production_from_active_oprs(&self.persisted, cfg, *hid)
                }
            };
            objective_mut!(self, hid)?.production = new_prod;
        }
        Ok(())
    }

    /// Recompute OLO `production` from non-threatened OPR feeds and refresh F10 stats.
    pub(crate) fn sync_hub_production_for_opr_threat_feeds(
        &mut self,
        changed: &[ObjectiveId],
    ) -> Result<()> {
        if !virtual_resupply_threatened_without_deliveries(&self.ephemeral.cfg) {
            return Ok(());
        }
        let mut feed_updates: SmallVec<[(ObjectiveId, ObjectiveId); 8]> = smallvec![];
        for oid in changed {
            let Some(obj) = self.persisted.objectives.get(oid) else {
                continue;
            };
            if matches!(obj.kind, ObjectiveKind::Production) {
                if let Some(hid) = opr_feed_hub(&self.persisted, obj) {
                    if obj.feed_hub != Some(hid) {
                        feed_updates.push((*oid, hid));
                    }
                }
            }
        }
        for (oid, hid) in feed_updates {
            objective_mut!(self, oid)?.feed_hub = Some(hid);
        }
        self.recompute_all_logistics_hub_production_from_opr_feeds()?;
        for oid in changed {
            let Some(obj) = self.persisted.objectives.get(oid) else {
                continue;
            };
            if matches!(obj.kind, ObjectiveKind::Production) {
                log_opr_threat_hub_production(self, *oid, obj);
            }
            self.ephemeral
                .update_objective_markup(&self.persisted, obj, &[]);
            if matches!(obj.kind, ObjectiveKind::Production) {
                if let Some(hid) = opr_feed_hub(&self.persisted, obj) {
                    if let Some(hub) = self.persisted.objectives.get(&hid) {
                        self.ephemeral
                            .update_objective_markup(&self.persisted, hub, &[*oid]);
                    }
                }
            }
        }
        self.sync_logistics_hub_production_displays();
        Ok(())
    }

    pub(super) fn sync_logistics_hub_production_displays(&mut self) {
        self.ephemeral
            .sync_logistics_hub_production_displays(&self.persisted);
    }
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
        let cfg = &self.ephemeral.cfg;
        let threat_feed_cut = virtual_resupply_threatened_without_deliveries(cfg);
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
                if threat_feed_cut && virtual_resupply_threatened_blocks(cfg, obj) {
                    continue;
                }
                let e = sums.entry(hid).or_insert((0, 0));
                e.0 = e.0.saturating_add(u32::from(production) * u32::from(capacity));
                e.1 = e.1.saturating_add(u32::from(capacity));
            }
        }
        for hid in &self.persisted.logistics_hubs {
            let hid = *hid;
            let new_prod = if threat_feed_cut {
                let hub_ref = objective!(self, hid)?;
                if virtual_resupply_threatened_blocks(cfg, hub_ref) {
                    0
                } else {
                    hub_production_from_active_oprs(&self.persisted, cfg, hid)
                }
            } else if let Some((weighted, cap_sum)) = sums.get(&hid).copied() {
                let denom = cap_sum.max(1) as u64;
                let numer = weighted as u64;
                ((numer + denom - 1) / denom).min(100) as u8
            } else {
                0
            };
            objective_mut!(self, hid)?.production = new_prod;
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

    /// Drops DCS `initialAmount=1` opposite-DT registration filler A/C rows outside owner export.
    /// Shared by fresh (preserve_fill) and loaded (apply_persisted) load paths; capture flow
    /// rebuilds rows from the new-owner export separately.
    fn prune_dt_registration_aircraft_for_oid(
        &mut self,
        lua: MizLua,
        oid: ObjectiveId,
        context_tag: &'static str,
    ) -> Result<()> {
        let export = Arc::clone(&self.ephemeral.fowl_miz_export);
        let resource_meta = self
            .warehouse_resource_meta_cache(lua)
            .context("resource meta for DT registration prune")?;
        let airbase_oid = self
            .ephemeral
            .airbase_by_oid
            .get(&oid)
            .ok_or_else(|| anyhow!("no airbase for DT registration prune {oid:?}"))?
            .clone();
        let warehouse = Airbase::get_instance(lua, &airbase_oid)?
            .get_warehouse()
            .context("warehouse for DT registration prune")?;
        let obj = objective_mut!(self, oid)?;
        let n = prune_registration_aircraft_outside_export_profile(
            obj,
            &warehouse,
            export.as_ref(),
            resource_meta.as_ref(),
        )?;
        if n > 0 {
            info!(
                "{context_tag}: pruned {n} registration-only A/C row(s) outside export for {:?}",
                obj.name
            );
        }
        Ok(())
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
                // DEP FARP objective_stock is authoritative.
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
    /// Called only on a new clean campaign (`Db::init`).
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
        let missing = missing_objective_stock_profile_labels(export, &self.persisted.objectives);
        if !missing.is_empty() {
            let list = missing.join("\n");
            error!(
                "Fowl: missing objective_stock profiles for {} objective(s):\n{list}",
                missing.len()
            );
            bail!(
                "Fowl: missing warehouse profiles in _fowl_export.json for:\n{list}\n\
                 Rebuild the mission with bftools so every objective has Blue and Red stock."
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
                    // Validated Blue+Red above; Neutral owner has no side profile to seed yet.
                    continue;
                };
                for (name, item) in &profile.equipment {
                    if item.baseline == 0 {
                        continue;
                    }
                    let dcs_name =
                        resolve_export_equipment_dcs_name(name.as_str(), &resource_meta);
                    // objective_stock is authoritative (see apply_export_profile).
                    let row = obj.warehouse.equipment.get_or_default_cow(dcs_name);
                    if item.baseline > row.capacity {
                        row.capacity = item.baseline;
                    }
                }
                for (key, liq) in &profile.liquids {
                    if liq.baseline == 0 {
                        continue;
                    }
                    let typ = liquid_type_from_export_key(key)
                        .with_context(|| format_compact!("DEP FARP {:?}", obj.name))?;
                    let row = obj.warehouse.liquids.get_or_default_cow(typ);
                    if liq.baseline > row.capacity {
                        row.capacity = liq.baseline;
                    }
                }
                canonicalize_virtual_equipment_keys(obj, &resource_meta);
                continue;
            }
            let Some(profile) =
                objective_coalition_stock_for_side(export, obj.name.as_str(), obj.owner)
            else {
                // Neutral owner: Blue/Red rows exist (validated); seed waits until capture.
                continue;
            };
            for (name, item) in &profile.equipment {
                if item.baseline == 0 {
                    continue;
                }
                let dcs_name =
                    resolve_export_equipment_dcs_name(name.as_str(), &resource_meta);
                // objective_stock is authoritative (see apply_export_profile).
                let row = obj.warehouse.equipment.get_or_default_cow(dcs_name);
                if item.baseline > row.capacity {
                    row.capacity = item.baseline;
                }
            }
            for (key, liq) in &profile.liquids {
                if liq.baseline == 0 {
                    continue;
                }
                let typ = liquid_type_from_export_key(key)
                    .with_context(|| format_compact!("objective {:?}", obj.name))?;
                let row = obj.warehouse.liquids.get_or_default_cow(typ);
                if liq.baseline > row.capacity {
                    row.capacity = liq.baseline;
                }
            }
            canonicalize_virtual_equipment_keys(obj, &resource_meta);
        }
        self.ephemeral.dirty();
        Ok(())
    }

    /// Test-only: swap Red↔Blue owners from ME zone prefixes (OAB/OFO/OLO).
    pub(super) fn apply_debugging_objectives_coalition_switch(&mut self) {
        if !self.ephemeral.cfg.debugging_objectives_coalition_switch {
            return;
        }
        let mut flipped = 0usize;
        for (_oid, obj) in self.persisted.objectives.iter_mut_cow() {
            if !matches!(
                obj.kind,
                ObjectiveKind::Airbase | ObjectiveKind::Fob | ObjectiveKind::Logistics
            ) {
                continue;
            }
            let from = obj.owner;
            if !matches!(from, Side::Blue | Side::Red) {
                continue;
            }
            let to = from.opposite();
            obj.owner = to;
            obj.nominal_owner = Some(to);
            flipped += 1;
            info!(
                "debugging_objectives_coalition_switch: {} {:?} -> {:?}",
                obj.name, from, to
            );
        }
        info!(
            "debugging_objectives_coalition_switch: flipped {flipped} objective owner(s)"
        );
        self.ephemeral.dirty();
    }

    /// Test-only: after opposite export seed, fill stored=capacity for SyncTo into DCS.
    pub(super) fn fill_virtual_warehouses_to_capacity_for_debug(&mut self) {
        if !self.ephemeral.cfg.debugging_objectives_coalition_switch {
            return;
        }
        for (_oid, obj) in self.persisted.objectives.iter_mut_cow() {
            if !matches!(
                obj.kind,
                ObjectiveKind::Airbase | ObjectiveKind::Fob | ObjectiveKind::Logistics
            ) {
                continue;
            }
            if !matches!(obj.owner, Side::Blue | Side::Red) {
                continue;
            }
            for (_name, inv) in obj.warehouse.equipment.iter_mut_cow() {
                inv.stored = inv.capacity;
            }
            for (_typ, inv) in obj.warehouse.liquids.iter_mut_cow() {
                inv.stored = inv.capacity;
            }
        }
        info!(
            "debugging_objectives_coalition_switch: virtual warehouses filled to opposite export capacity"
        );
        self.ephemeral.dirty();
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
            // objective_stock is the build-time allowlist; do not re-filter with narrower
            // objective_defaults (policy-only, misses B/RDEFAULT+ / zone ordnance e.g. AGM-114K).
            let row = obj.warehouse.equipment.get_or_default_cow(dcs_name);
            if item.baseline > row.capacity {
                row.capacity = item.baseline;
            }
        }
        for (key, liq) in &profile.liquids {
            if liq.baseline == 0 {
                continue;
            }
            let typ = liquid_type_from_export_key(key)
                .with_context(|| format_compact!("capture warehouse {:?}", obj.name))?;
            let row = obj.warehouse.liquids.get_or_default_cow(typ);
            if liq.baseline > row.capacity {
                row.capacity = liq.baseline;
            }
        }
        canonicalize_virtual_equipment_keys(obj, resource_meta);
        Ok(true)
    }

    fn apply_capture_spoils_to_virtual(
        obj: &mut Objective,
        equipment: &FxHashMap<String, u32>,
        liquids: &FxHashMap<LiquidType, u32>,
    ) {
        // Exact transfer; over-template amounts are kept (capacity stays template baseline).
        for (name, inv) in obj.warehouse.equipment.iter_mut_cow() {
            if let Some(&stored) = equipment.get(name.as_str()) {
                inv.stored = stored;
            }
        }
        for (typ, inv) in obj.warehouse.liquids.iter_mut_cow() {
            if let Some(&stored) = liquids.get(typ) {
                inv.stored = stored;
            }
        }
    }

    /// Fill template rows still at `stored == 0` after spoils (CFG % of capacity). OFO/OAB/OLO.
    fn apply_capture_stock_percentage_to_virtual(obj: &mut Objective, pct: u8) -> usize {
        if pct == 0 {
            return 0;
        }
        let pct = pct.min(100);
        let mut filled = 0usize;
        for (_, inv) in obj.warehouse.equipment.iter_mut_cow() {
            if inv.capacity > 0 && inv.stored == 0 {
                inv.stored = scale_capacity_by_percent_floor(inv.capacity, pct);
                if inv.stored > 0 {
                    filled += 1;
                }
            }
        }
        for (_, inv) in obj.warehouse.liquids.iter_mut_cow() {
            if inv.capacity > 0 && inv.stored == 0 {
                inv.stored = scale_capacity_by_percent_floor(inv.capacity, pct);
                if inv.stored > 0 {
                    filled += 1;
                }
            }
        }
        filled
    }

    /// Re-assert export template maxima after spoils / capture % (and before DCS sync).
    fn reapply_export_capacities_to_virtual(
        obj: &mut Objective,
        export: &FowlMizExport,
        resource_meta: &FxHashMap<String, WarehouseResourceMeta>,
    ) {
        let Some(profile) = (if objective_is_ground_dep_farp(obj) {
            dep_farp_export_profile(export, obj)
        } else {
            objective_coalition_stock_for_objective(export, obj)
        }) else {
            return;
        };
        for (name, inv) in obj.warehouse.equipment.iter_mut_cow() {
            if let Some(item) = profile_export_equipment_item(profile, name.as_str(), resource_meta)
            {
                if item.baseline > 0 {
                    inv.capacity = item.baseline.max(inv.stored);
                }
            } else if inv.capacity == 0 && inv.stored > 0 {
                inv.capacity = inv.stored;
            }
        }
        for (typ, inv) in obj.warehouse.liquids.iter_mut_cow() {
            let baseline = profile.liquids.iter().find_map(|(key, liq)| {
                (liquid_type_from_export_key(key).ok() == Some(*typ) && liq.baseline > 0)
                    .then_some(liq.baseline)
            });
            if let Some(baseline) = baseline {
                inv.capacity = baseline.max(inv.stored);
            } else if inv.capacity == 0 && inv.stored > 0 {
                inv.capacity = inv.stored;
            }
        }
    }

    fn execute_transfer(&mut self, tr: &Transfer) -> Result<()> {
        let export = self.ephemeral.fowl_miz_export.as_ref();
        let empty_meta = FxHashMap::default();
        let resource_meta = self
            .ephemeral
            .warehouse_resource_meta
            .as_ref()
            .map(|m| m.as_ref())
            .unwrap_or(&empty_meta);
        tr.execute(
            &mut self.persisted,
            &self.ephemeral.to_bg,
            export,
            resource_meta,
        )
    }

    fn ensure_dyn_spawn_template_links(&mut self, lua: MizLua) -> Result<()> {
        if !self.ephemeral.dyn_spawn_template_links.is_empty() {
            return Ok(());
        }
        self.ephemeral.dyn_spawn_template_links = scan_dyn_spawn_template_links(lua)?;
        info!(
            "dyn spawn template links cached: {} (zzDT-*/dynSpawnTemplate)",
            self.ephemeral.dyn_spawn_template_links.len()
        );
        Ok(())
    }

    /// ME `linkDynTempl` swap for new owner (bftools `patch_warehouse_dynamic_spawn_links` shape).
    /// Uses `env.warehouses` (Hoggit: warehouses table is scripting-accessible). Official Warehouse
    /// class has no linkDynTempl API — live dyn-spawn may only pick this up if DCS re-reads ME rows.
    fn apply_capture_link_dyn_templ(
        &mut self,
        lua: MizLua,
        oid: ObjectiveId,
        new_owner: Side,
    ) -> Result<()> {
        if matches!(new_owner, Side::Neutral) {
            return Ok(());
        }
        self.ensure_dyn_spawn_template_links(lua)?;
        let links = self.ephemeral.dyn_spawn_template_links.clone();
        if links.is_empty() {
            warn!("capture linkDynTempl: no zzDT-* / dynSpawnTemplate groups in mission");
            return Ok(());
        }
        let airbase_oid = self
            .ephemeral
            .airbase_by_oid
            .get(&oid)
            .ok_or_else(|| anyhow!("no airbase for objective {oid:?}"))?
            .clone();
        let airbase = Airbase::get_instance(lua, &airbase_oid).context("airbase for linkDynTempl")?;
        let ab_id = airbase.get_id().context("airbase id for linkDynTempl")?.inner();
        let updated = apply_me_warehouse_link_dyn_templ(lua, ab_id, new_owner, &links)?;
        info!(
            "capture linkDynTempl oid={oid:?} owner={new_owner:?} airbase_id={ab_id}: updated {updated} aircraft rows"
        );
        Ok(())
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
        let apply_persisted = self.ephemeral.warehouses_apply_persisted;
        let debug_coalition_switch = self.ephemeral.cfg.debugging_objectives_coalition_switch;
        for oid in sync_oids {
            if matches!(objective!(self, oid)?.kind, ObjectiveKind::Production) {
                continue;
            }
            if apply_persisted && !debug_coalition_switch {
                self.sync_objective_to_warehouse(lua, oid).with_context(|| {
                    format_compact!(
                        "loaded campaign: apply persisted warehouse to DCS for {:?}",
                        oid
                    )
                })?;
                // ME `initialAmount=1` opposite DT rows resurrect after load; virtual has no
                // matching entry so SyncTo alone leaves DCS at 1. Reapply the same prune as
                // preserve_fill so UI shows 0/1 and capture path can still refill on ownership change.
                self.prune_dt_registration_aircraft_for_oid(lua, oid, "loaded campaign")?;
                continue;
            }
            if debug_coalition_switch {
                let (kind, owner) = {
                    let obj = objective!(self, oid)?;
                    (obj.kind.clone(), obj.owner)
                };
                if matches!(
                    kind,
                    ObjectiveKind::Airbase | ObjectiveKind::Fob | ObjectiveKind::Logistics
                ) && matches!(owner, Side::Blue | Side::Red)
                {
                    self.replace_dcs_warehouse_from_virtual(lua, oid)
                        .with_context(|| {
                            format_compact!(
                                "debugging_objectives_coalition_switch: replace DCS stock {:?}",
                                oid
                            )
                        })?;
                    // Same as capture: ME linkDynTempl must follow new owner zzDT-* (stock alone is not enough).
                    if let Err(e) = self.apply_capture_link_dyn_templ(lua, oid, owner) {
                        error!(
                            "debugging_objectives_coalition_switch linkDynTempl {:?}: {e:?}",
                            oid
                        );
                    }
                } else {
                    self.sync_warehouse_to_objective(lua, oid).with_context(|| {
                        format_compact!(
                            "debugging_objectives_coalition_switch: SyncFrom ME {:?}",
                            oid
                        )
                    })?;
                }
                continue;
            }
            self.sync_warehouse_to_objective(lua, oid)
                .with_context(|| format_compact!("seed virtual stock from DCS warehouse for {:?}", oid))?;
            if !preserve_fill {
                self.sync_objective_to_warehouse(lua, oid).with_context(|| {
                    format_compact!("Fowl export: prune/sync DCS warehouse for {:?}", oid)
                })?;
            } else {
                self.prune_dt_registration_aircraft_for_oid(lua, oid, "preserve_fill")?;
            }
        }
        if apply_persisted && !debug_coalition_switch {
            info!(
                "loaded campaign: applied persisted warehouses to DCS (skipped SyncFrom ME template)"
            );
        } else if debug_coalition_switch {
            info!(
                "debugging_objectives_coalition_switch: replaced DCS stock with opposite export (orphans removed)"
            );
        } else if preserve_fill {
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
        let mut farp_zone_resync = false;
        let farp_oids: Vec<ObjectiveId> =
            self.persisted.farps.into_iter().copied().collect();
        for oid in farp_oids {
            match self.sync_ground_dep_farp_zone_from_pad(lua, oid) {
                Ok(true) => farp_zone_resync = true,
                Ok(false) => (),
                Err(e) => warn!("ground DEP FARP zone sync {:?}: {e:?}", oid),
            }
            if let Err(e) = self.mark_farp_threatened_by_nearby_enemy_ground(oid) {
                warn!("ground DEP FARP threat scan {:?}: {e:?}", oid);
            }
        }
        if farp_zone_resync {
            self.setup_supply_lines()
                .context("supply lines after ground DEP FARP zone sync on load")?;
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
            self.sync_logistics_hub_production_displays();
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
                    let objectives = self.warehouse_sync_objective_ids();
                    wh_diag(format_compact!(
                        "tick start: sync-from {} objectives; ticks_since_delivery={}/{}",
                        objectives.len(),
                        self.persisted.logistics_ticks_since_delivery,
                        ticks_per_delivery
                    ));
                    self.ephemeral.logistics_stage =
                        LogiStage::SyncFromWarehouses { objectives: objectives.into() };
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
                            wh_diag("phase: deliver_production (+ hub distribute)");
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
                            wh_diag(format_compact!(
                                "phase: hub distribute (ticks_since_delivery now {})",
                                self.persisted.logistics_ticks_since_delivery
                            ));
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
                        wh_diag(format_compact!(
                            "planned {} transfers this tick",
                            transfers.len()
                        ));
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
                        wh_diag("no transfers; balance hubs then sync-to DCS");
                        self.balance_logistics_hubs()?;
                        let objectives = self.warehouse_sync_objective_ids().into();
                        self.ephemeral.logistics_stage =
                            LogiStage::SyncToWarehouses { objectives };
                    }
                    record_perf(&mut perf.logistics_transfer, st);
                }
                LogiStage::ExecuteTransfers { transfers } => {
                    let st = Utc::now();
                    let before = transfers.len();
                    let export = Arc::clone(&self.ephemeral.fowl_miz_export);
                    let resource_meta = self
                        .ephemeral
                        .warehouse_resource_meta
                        .clone()
                        .unwrap_or_else(|| Arc::new(FxHashMap::default()));
                    while let Some(tr) = transfers.pop() {
                        if let Err(e) = tr.execute(
                            &mut self.persisted,
                            &self.ephemeral.to_bg,
                            export.as_ref(),
                            resource_meta.as_ref(),
                        ) {
                            error!("executing transfer {:?} {e:?}", tr)
                        }
                        if Utc::now() - st > Duration::milliseconds(6) {
                            break;
                        }
                    }
                    wh_diag(format_compact!(
                        "executed {} transfers ({} remaining this stage)",
                        before.saturating_sub(transfers.len()),
                        transfers.len()
                    ));
                    record_perf(&mut perf.logistics_transfer, st);
                }
                LogiStage::SyncToWarehouses { objectives } => match objectives.pop() {
                    None => {
                        self.update_supply_status()
                            .context("supply status after logistics sync-to")?;
                        self.ephemeral.logistics_stage = LogiStage::Complete { last_tick: ts };
                    }
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

    pub(super) fn capture_warehouse(
        &mut self,
        lua: MizLua,
        oid: ObjectiveId,
        previous_owner: Side,
    ) -> Result<()> {
        if self.ephemeral.cfg.warehouse.is_none() {
            return Ok(());
        }
        let export = Arc::clone(&self.ephemeral.fowl_miz_export);
        if !export_has_objective_stock(export.as_ref()) {
            warn!(
                "capture warehouse {:?}: Fowl export has no objective_stock (mission build bug)",
                objective!(self, oid)?.name
            );
            return Ok(());
        }
        let resource_meta = self
            .warehouse_resource_meta_cache(lua)
            .context("resource meta for capture warehouse")?;
        let (spoils_eq, spoils_liq, new_owner, name) = {
            let obj = objective!(self, oid)?;
            let mut raw_eq: FxHashMap<String, u32> = FxHashMap::default();
            for (k, v) in &obj.warehouse.equipment {
                raw_eq.insert(k.clone(), v.stored);
            }
            let mut raw_liq: FxHashMap<LiquidType, u32> = FxHashMap::default();
            for (k, v) in &obj.warehouse.liquids {
                raw_liq.insert(*k, v.stored);
            }
            let (spoils_eq, spoils_liq) = capture_spoils_intersection(
                export.as_ref(),
                obj,
                previous_owner,
                obj.owner,
                resource_meta.as_ref(),
                &raw_eq,
                &raw_liq,
            );
            (spoils_eq, spoils_liq, obj.owner, obj.name.clone())
        };
        let reseeded = {
            let obj = objective_mut!(self, oid)?;
            Self::apply_export_profile_to_objective_virtual_warehouse(
                obj,
                export.as_ref(),
                resource_meta.as_ref(),
            )?
        };
        if !reseeded {
            warn!(
                "capture warehouse {name}: missing objective_stock profile for {new_owner:?} \
                 (previous {previous_owner:?}) — should have failed at new campaign start"
            );
            return Ok(());
        }
        let capture_stock_pct = self
            .ephemeral
            .cfg
            .warehouse
            .as_ref()
            .map(|w| {
                let kind = match objective!(self, oid) {
                    Ok(o) => &o.kind,
                    Err(_) => return 0,
                };
                match kind {
                    ObjectiveKind::Fob => w.captured_stock_percentage_ofo,
                    ObjectiveKind::Airbase => w.captured_stock_percentage_oab,
                    ObjectiveKind::Logistics => w.captured_stock_percentage_olo,
                    _ => 0,
                }
            })
            .unwrap_or(0);
        let (eq_cap_rows, liq_cap_rows, pct_filled, pct) = {
            let obj = objective_mut!(self, oid)?;
            Self::apply_capture_spoils_to_virtual(obj, &spoils_eq, &spoils_liq);
            let pct = capture_stock_pct;
            let pct_filled = Self::apply_capture_stock_percentage_to_virtual(obj, pct);
            Self::reapply_export_capacities_to_virtual(obj, export.as_ref(), resource_meta.as_ref());
            let mut eq_cap_rows = 0usize;
            for (_, inv) in &obj.warehouse.equipment {
                if inv.capacity > 0 {
                    eq_cap_rows += 1;
                }
            }
            let mut liq_cap_rows = 0usize;
            for (_, inv) in &obj.warehouse.liquids {
                if inv.capacity > 0 {
                    liq_cap_rows += 1;
                }
            }
            (eq_cap_rows, liq_cap_rows, pct_filled, pct)
        };
        if let Err(e) = self.apply_capture_link_dyn_templ(lua, oid, new_owner) {
            error!("capture linkDynTempl {name}: {e:?}");
        }
        // Ensure ME weapon rows for new-owner ordnance before Warehouse.setItem (AGM-114K etc.).
        if let Ok(Some(ab_id)) = (|| -> Result<Option<i64>> {
            let airbase_oid = match self.ephemeral.airbase_by_oid.get(&oid) {
                Some(id) => id,
                None => return Ok(None),
            };
            let ab = Airbase::get_instance(lua, airbase_oid)?;
            Ok(Some(ab.get_id()?.inner()))
        })() {
            let obj = objective!(self, oid)?;
            match rebuild_me_warehouse_weapons_from_virtual(
                lua,
                ab_id,
                obj,
                resource_meta.as_ref(),
            ) {
                Ok(n) if n > 0 => info!(
                    "capture {name}: rebuilt {n} ME weapon row(s) for {new_owner:?} export stock"
                ),
                Ok(_) => {}
                Err(e) => warn!("capture {name}: ME weapon rebuild {e:?}"),
            }
        }
        let capture_track = match self.sync_objective_to_warehouse(lua, oid) {
            Ok((obj, wh)) => {
                let liquids_tons = objective_liquids_stored_as_tons(export.as_ref(), obj);
                if let Err(e) = apply_virtual_warehouse_to_dcs(
                    lua,
                    obj,
                    &wh,
                    liquids_tons,
                    true,
                    Some(resource_meta.as_ref()),
                ) {
                    error!("apply DCS warehouse after capture {oid}: {e:?}");
                }
                if let Ok(dcs_names) = collect_dcs_warehouse_equipment_names(&wh) {
                    rematch_virtual_equipment_to_dcs_names(
                        obj,
                        &dcs_names,
                        resource_meta.as_ref(),
                    );
                    canonicalize_virtual_equipment_keys(obj, resource_meta.as_ref());
                }
                let mut track_names: Vec<String> = Vec::new();
                for (name, inv) in &obj.warehouse.equipment {
                    if inv.capacity > 0 {
                        track_names.push(name.clone());
                    }
                }
                match collect_dcs_warehouse_equipment_names(&wh) {
                    Ok(dcs_names) => Some((track_names, dcs_names)),
                    Err(e) => {
                        error!("record DCS warehouse equipment after capture {oid}: {e:?}");
                        Some((track_names, FxHashSet::default()))
                    }
                }
            }
            Err(e) if warehouse_sync_skip(&e) => None,
            Err(e) => {
                error!("sync warehouse after capture {oid}: {e:?}");
                None
            }
        };
        if let Some((track_names, mut dcs_names)) = capture_track {
            for name in track_names {
                dcs_names.insert(name);
            }
            self.ephemeral
                .warehouse_dcs_equipment_names
                .insert(oid, dcs_names);
        }
        self.update_supply_status()
            .context("supply status after capture warehouse reseed")?;
        info!(
            "capture warehouse {name}: reseeded to {new_owner:?} profile; \
             virtual eq_cap={eq_cap_rows} liq_cap={liq_cap_rows}; spoils eq={} liq={}; \
             capture_stock_pct={pct} filled_rows={pct_filled}",
            spoils_eq.len(),
            spoils_liq.len(),
        );
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
        let cfg = &self.ephemeral.cfg;
        let mut deliver_produced_supplies = || -> Result<()> {
            let hub_ids: Vec<ObjectiveId> =
                self.persisted.logistics_hubs.into_iter().copied().collect();
            for side in Side::ALL {
                let production = match self.ephemeral.production_by_side.get(&side) {
                    Some(e) => e,
                    None => continue,
                };
                for oid in &hub_ids {
                    let Some(logi_ref) = self.persisted.objectives.get(oid) else {
                        continue;
                    };
                    if logi_ref.owner != side || !logi_ref.is_normal_logistics_hub() {
                        continue;
                    }
                    if virtual_resupply_threatened_blocks(cfg, logi_ref) {
                        continue;
                    }
                    let hub_prod = effective_hub_production(cfg, &self.persisted, logi_ref);
                    let hub_name = logi_ref.name.clone();
                    let logi = objective_mut!(self, oid)?;
                    let mut added_eq = 0u32;
                    let mut added_liq = 0u32;
                    for (name, inv) in logi.warehouse.equipment.iter_mut_cow() {
                        if let Some(eq) = production.equipment.get(name) {
                            let add = Self::scale_production_amount(eq.production, hub_prod);
                            if add > 0 {
                                *inv += add;
                                added_eq = added_eq.saturating_add(add);
                            }
                        }
                    }
                    for (name, inv) in logi.warehouse.liquids.iter_mut_cow() {
                        if let Some(pr) = production.liquids.get(name) {
                            let add = Self::scale_production_amount(*pr, hub_prod);
                            if add > 0 {
                                *inv += add;
                                added_liq = added_liq.saturating_add(add);
                            }
                        }
                    }
                    if added_eq > 0 || added_liq > 0 {
                        wh_diag(format_compact!(
                            "production {hub_name}: hub_prod={hub_prod}% added equipment_units={added_eq} liquid_units={added_liq}"
                        ));
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
            wh_diag("hub distribute skipped (virtual_resupply=false)");
            return Ok(vec![]);
        }
        self.update_supply_status()
            .context("updating supply status")?;
        let cfg = &self.ephemeral.cfg;
        let mut transfers: Vec<Transfer> = vec![];
        for lid in &self.persisted.logistics_hubs {
            let logi = objective!(self, lid)?;
            if virtual_resupply_threatened_blocks(cfg, logi) {
                wh_diag(format_compact!(
                    "hub {} skipped (threatened blocks deliveries)",
                    logi.name
                ));
                continue;
            }
            let hub_destinations = || {
                logi.warehouse
                    .destination
                    .into_iter()
                    .filter_map(|oid| Some((oid, self.persisted.objectives.get(oid)?)))
                    .filter(|(_, obj)| logi.owner == obj.owner)
                    .filter(|(_, obj)| !virtual_resupply_threatened_blocks(cfg, obj))
                    .filter(|(oid, _)| !self.ephemeral.mobile_farp_underway.contains(oid))
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
            if needed_equipment.is_empty() && needed_liquid.is_empty() {
                wh_diag(format_compact!(
                    "hub {} supply/fuel: all destinations at 100% (no outbound demand)",
                    logi.name
                ));
            } else {
                let mut eq_names = CompactString::new("");
                for (i, n) in needed_equipment.iter().enumerate() {
                    if i > 0 {
                        eq_names.push_str(", ");
                    }
                    if i >= 8 {
                        eq_names.push_str("...");
                        break;
                    }
                    eq_names.push_str(&format_compact!("{} supply={}%", n.obj.name, n.obj.supply));
                }
                let mut liq_names = CompactString::new("");
                for (i, n) in needed_liquid.iter().enumerate() {
                    if i > 0 {
                        liq_names.push_str(", ");
                    }
                    if i >= 8 {
                        liq_names.push_str("...");
                        break;
                    }
                    liq_names.push_str(&format_compact!("{} fuel={}%", n.obj.name, n.obj.fuel));
                }
                wh_diag(format_compact!(
                    "hub {} demand: eq_dests={} [{}] liq_dests={} [{}]",
                    logi.name,
                    needed_equipment.len(),
                    eq_names,
                    needed_liquid.len(),
                    liq_names
                ));
            }
            let transfers_before = transfers.len();
            macro_rules! schedule_transfers {
                ($typ:expr, $from:ident, $get:ident, $needed:ident, $reserved_fn:ident) => {
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
                            let reserved = self.$reserved_fn(*n.oid, name);
                            let demanded =
                                Db::dynamic_cargo_demand_room(inv.stored, inv.capacity, reserved);
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
                needed_equipment,
                dynamic_cargo_equipment_reserved
            );
            schedule_transfers!(
                TransferItem::Liquid,
                liquids,
                get_liquids,
                needed_liquid,
                dynamic_cargo_liquid_reserved
            );
            let hub_planned = transfers.len() - transfers_before;
            if hub_planned > 0 {
                wh_diag(format_compact!(
                    "hub {} scheduled {} outbound transfers",
                    logi.name,
                    hub_planned
                ));
            }
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
                if virtual_resupply_threatened_blocks(cfg, occ) {
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
            if virtual_resupply_threatened_blocks(cfg, logi) {
                continue;
            }
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
                ($typ:expr, $from:ident, $get:ident, $needed:ident, $reserved_fn:ident) => {
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
                            let reserved = self.$reserved_fn(*n.oid, name);
                            let demanded =
                                Db::dynamic_cargo_demand_room(inv.stored, inv.capacity, reserved);
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
                needed_equipment,
                dynamic_cargo_equipment_reserved
            );
            schedule_occupied_transfers!(
                TransferItem::Liquid,
                liquids,
                get_liquids,
                needed_liquid,
                dynamic_cargo_liquid_reserved
            );
        }
        wh_diag(format_compact!(
            "hub distribute total planned transfers={}",
            transfers.len()
        ));
        Ok(transfers)
    }

    fn balance_logistics_hubs(&mut self) -> Result<()> {
        // Virtual resupply: no domestic OLO↔OLO equalization (only hub→base / hub→captured).
        if self.ephemeral.cfg.virtual_resupply {
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
            let cfg = &self.ephemeral.cfg;
            macro_rules! schedule_transfers {
                ($typ:expr, $from:ident, $get:ident) => {{
                    let mut needed: SmallVec<[Needed; 16]> = self
                        .persisted
                        .logistics_hubs
                        .into_iter()
                        .filter_map(|lid| {
                            let obj = &self.persisted.objectives[lid];
                            if obj.owner != side
                                || obj.is_occupied_logistics_hub()
                                || virtual_resupply_threatened_blocks(cfg, obj)
                            {
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
            if !transfers.is_empty() {
                wh_diag(format_compact!(
                    "balance hubs side={side:?}: executing {} transfers",
                    transfers.len()
                ));
            }
            for tr in transfers.drain(..) {
                self.execute_transfer(&tr)
                    .with_context(|| format_compact!("executing transfer {:?}", tr))?
            }
            self.ephemeral.dirty();
        }
        self.update_supply_status()?;
        Ok(())
    }

    pub(crate) fn update_supply_status(&mut self) -> Result<()> {
        for (id, obj) in self.persisted.objectives.iter_mut_cow() {
            let current_supply = obj.supply;
            let current_fuel = obj.fuel;
            let dcs_tracked = self.ephemeral.warehouse_dcs_equipment_names.get(id);
            let mut n = 0;
            let mut sum: u32 = 0;
            for (name, inv) in &obj.warehouse.equipment {
                if inv.capacity == 0 {
                    continue;
                }
                if let Some(tracked) = dcs_tracked {
                    if !tracked.contains(name.as_str()) {
                        continue;
                    }
                }
                if !equipment_counts_toward_supply_pct(inv) {
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
                wh_diag(format_compact!(
                    "status {} ({:?}): supply {}->{}% fuel {}->{}% (eq_rows={n})",
                    obj.name,
                    obj.kind,
                    current_supply,
                    obj.supply,
                    current_fuel,
                    obj.fuel
                ));
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
        let block_sync_from_refill = {
            let obj = objective!(self, oid)?;
            virtual_resupply_threatened_blocks(&self.ephemeral.cfg, obj)
        };
        let obj = objective_mut!(self, oid)?;
        let (prev_equipment, prev_liquids) = if block_sync_from_refill {
            (snapshot_equipment_stored(obj), snapshot_liquid_stored(obj))
        } else {
            (FxHashMap::default(), smallvec![])
        };
        canonicalize_virtual_equipment_keys(obj, resource_meta.as_ref());
        if objective_is_ground_dep_farp_export(export.as_ref(), obj) {
            let keep_virtual = skip_dep_hydrate || dep_farp_has_persisted_virtual_stock(obj);
            reconcile_dcs_warehouse_to_virtual(
                lua,
                obj,
                &warehouse,
                Some(resource_meta.as_ref()),
            )
                .context("pruning ground DEP FARP DCS warehouse to virtual stock")?;
            if !keep_virtual {
                self.ephemeral.dep_farp_authoritative_until.remove(&oid);
                sync_warehouse_to_obj(obj, &warehouse, true)
                    .context("syncing ground DEP FARP warehouse from DCS (tracked rows only)")?;
            }
            if block_sync_from_refill {
                clamp_threatened_sync_from_refill(obj, &prev_equipment, &prev_liquids);
            }
            canonicalize_virtual_equipment_keys(obj, resource_meta.as_ref());
            record_objective_dcs_equipment_names(&mut self.ephemeral, oid, &warehouse)
                .context("recording DCS warehouse equipment for supply")?;
            if self.ephemeral.cfg.dynamic_cargo_delivery.enabled
                && Db::clamp_dynamic_cargo_checkout_obj(
                    obj,
                    oid,
                    &self.persisted.dynamic_cargo_crates,
                    self.ephemeral.fowl_miz_export.as_ref(),
                )
            {
                self.ephemeral.dirty();
            }
            return Ok((obj, warehouse));
        }
        let is_logistics_hub = matches!(obj.kind, ObjectiveKind::Logistics);
        let liquids_tons = objective_liquids_stored_as_tons(export.as_ref(), obj);
        sync_warehouse_to_obj(obj, &warehouse, liquids_tons)
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
        // Production-key hydrate is OLO-only (FARP/helipad Inventory gaps). On OAB/OFO/FARP
        // it invented airframe demand and skewed Supply %.
        if is_logistics_hub {
            if let Some(prod) = production.as_deref() {
                hydrate_production_keys_from_dcs(
                    obj,
                    &warehouse,
                    export.as_ref(),
                    resource_meta.as_ref(),
                    whcfg,
                    prod,
                    on_water,
                )
                .context("hydrating production keys from DCS get_item_count")?;
            }
        }
        if block_sync_from_refill {
            clamp_threatened_sync_from_refill(obj, &prev_equipment, &prev_liquids);
        }
        reconcile_objective_stock_aircraft_capacity_to_dcs(
            obj,
            export.as_ref(),
            resource_meta.as_ref(),
        );
        if !is_logistics_hub {
            clamp_non_olo_airframe_rows_to_export(
                obj,
                export.as_ref(),
                resource_meta.as_ref(),
            );
        }
        canonicalize_virtual_equipment_keys(obj, resource_meta.as_ref());
        record_objective_dcs_equipment_names(&mut self.ephemeral, oid, &warehouse)
            .context("recording DCS warehouse equipment for supply")?;
        if is_logistics_hub {
            if let Some(prod) = production.as_deref() {
                mark_production_equipment_dcs_tracked(&mut self.ephemeral, oid, prod);
            }
        }
        if self.ephemeral.cfg.dynamic_cargo_delivery.enabled
            && Db::clamp_dynamic_cargo_checkout_obj(
                obj,
                oid,
                &self.persisted.dynamic_cargo_crates,
                self.ephemeral.fowl_miz_export.as_ref(),
            )
        {
            self.ephemeral.dirty();
        }
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
        prune_disallowed_dcs_weapon_stock(lua, &warehouse, export, owner, resource_meta.as_ref())?;
        let liquids_tons = objective_liquids_stored_as_tons(export, obj);
        sync_obj_to_warehouse(obj, &warehouse, liquids_tons)
            .context("syncing warehouse to objective")?;
        if objective_is_ground_dep_farp_export(export, obj) {
            reconcile_dcs_warehouse_to_virtual(
                lua,
                obj,
                &warehouse,
                Some(resource_meta.as_ref()),
            )
                .context("pruning ground DEP FARP DCS warehouse after virtual push")?;
        }
        Ok((obj, warehouse))
    }

    /// Drop DCS rows not in virtual, then set all virtual rows (opposite-owner debug / capture-style).
    fn replace_dcs_warehouse_from_virtual<'lua>(
        &mut self,
        lua: MizLua<'lua>,
        oid: ObjectiveId,
    ) -> Result<()> {
        let resource_meta = self
            .warehouse_resource_meta_cache(lua)
            .context("warehouse resource meta for opposite replace")?;
        let obj = objective_mut!(self, oid)?;
        if matches!(obj.kind, ObjectiveKind::Production) {
            return Ok(());
        }
        let airbase = self
            .ephemeral
            .airbase_by_oid
            .get(&oid)
            .ok_or_else(|| anyhow!("no logistics for objective {}", obj.name))?;
        let airbase_inst = Airbase::get_instance(lua, airbase).context("getting airbase")?;
        let ab_id = airbase_inst
            .get_id()
            .context("airbase id for ME weapon rows")?
            .inner();
        let warehouse = airbase_inst
            .get_warehouse()
            .context("getting warehouse")?;
        let owner = obj.owner;
        let export = self.ephemeral.fowl_miz_export.as_ref();
        // ROAD FOB / Invisible FARP: additive ensure_me leaves owner leftover weapons and
        // misses most opposite rows (eq_rows ~20–70). Rebuild ME weapons from virtual.
        let rebuilt = rebuild_me_warehouse_weapons_from_virtual(
            lua,
            ab_id,
            obj,
            resource_meta.as_ref(),
        )?;
        if rebuilt > 0 {
            info!(
                "ME warehouse weapons: rebuilt {rebuilt} row(s) for {:?} (opposite/export stock)",
                obj.name
            );
        }
        prune_disallowed_dcs_weapon_stock(lua, &warehouse, export, owner, resource_meta.as_ref())?;
        let liquids_tons = objective_liquids_stored_as_tons(export, obj);
        apply_virtual_warehouse_to_dcs(
            lua,
            obj,
            &warehouse,
            liquids_tons,
            true,
            Some(resource_meta.as_ref()),
        )
            .context("replace DCS warehouse from virtual")?;
        let dcs_names = collect_dcs_warehouse_equipment_names(&warehouse)
            .context("collect DCS equipment names after opposite replace")?;
        rematch_virtual_equipment_to_dcs_names(obj, &dcs_names, resource_meta.as_ref());
        canonicalize_virtual_equipment_keys(obj, resource_meta.as_ref());
        self.ephemeral
            .warehouse_dcs_equipment_names
            .insert(oid, dcs_names);
        Ok(())
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
            ($src:ident, $typ:ident, $reserved_fn:ident) => {
                for (name, inv) in &from_obj.warehouse.$src {
                    if inv.stored > 0 {
                        let needed = match to_obj.warehouse.$src.get(name) {
                            None => 0,
                            Some(inv) => {
                                let reserved = self.$reserved_fn(to, name);
                                Db::dynamic_cargo_demand_room(inv.stored, inv.capacity, reserved)
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
        compute!(equipment, Equipment, dynamic_cargo_equipment_reserved);
        compute!(liquids, Liquid, dynamic_cargo_liquid_reserved);
        for tr in transfers {
            self.execute_transfer(&tr)?
        }
        let export = self.ephemeral.fowl_miz_export.as_ref();
        let from_tons = objective_liquids_stored_as_tons(export, objective!(self, from)?);
        let to_tons = objective_liquids_stored_as_tons(export, objective!(self, to)?);
        sync_obj_to_warehouse(objective!(self, from)?, &from_wh, from_tons)?;
        sync_obj_to_warehouse(objective!(self, to)?, &to_wh, to_tons)?;
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
        let liquids_tons = objective_liquids_stored_as_tons(
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
        sync_obj_to_warehouse(obj, &warehouse, liquids_tons).context("syncing from warehouse")?;
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
