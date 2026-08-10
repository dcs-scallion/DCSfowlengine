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

use super::{ArgTuple, player_name, slot_for_group};
use crate::{
    Context,
    db::cargo::{Cargo, Oldest, SlotStats, Unpakistan},
};
use anyhow::{Context as ErrContext, Result, anyhow};
use bfprotocols::cfg::{Cfg, LimitEnforceTyp};
use compact_str::{CompactString, ToCompactString, format_compact};
use dcso3::{
    MizLua, String,
    coalition::Side,
    env::miz::GroupId,
    mission_commands::{GroupSubMenu, MissionCommands},
    net::SlotId,
};
use fxhash::FxHashMap;
use std::collections::hash_map::Entry;

fn unpakistan(lua: MizLua, gid: GroupId) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    let (side, slot) = slot_for_group(lua, ctx, &gid).context("getting slot for group")?;
    match ctx.db.unpakistan(lua, &ctx.idx, &slot) {
        Ok(unpakistan) => {
            if let Some(ucid) = ctx.db.ephemeral.player_in_slot(&slot).copied() {
                ctx.db.campaign_top10_on_logistics(ucid);
            }
            let sound_key = match &unpakistan {
                Unpakistan::Unpacked(_, _)
                | Unpakistan::UnpackedFarp(_, _)
                | Unpakistan::TransferedSupplies(_, _) => "crate_unpack",
                Unpakistan::Repaired(_)
                | Unpakistan::RepairedBase(_, _)
                | Unpakistan::RepairedProduction(_, _)
                | Unpakistan::RepairedStaticQueue(_, _, _) => "repair",
            };
            ctx.db.play_sound_player(lua, sound_key, &slot);
            let player = player_name(&ctx.db, &slot);
            let msg = format_compact!("{player} {unpakistan}");
            ctx.db.ephemeral.msgs().panel_to_side(10, false, side, msg);
        }
        Err(e) => {
            let msg = format_compact!("{}", e);
            ctx.db.ephemeral.msgs().panel_to_group(10, false, gid, msg)
        }
    }
    Ok(())
}

fn load_crate(lua: MizLua, gid: GroupId) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    let (side, slot) = slot_for_group(lua, ctx, &gid).context("getting slot for group")?;
    match ctx.db.load_nearby_crate(lua, &slot) {
        Ok(cr) => {
            let (dep_name, limit_enforce, limit) = match ctx.db.deployable_by_crate(&side, &cr.name)
            {
                Some((dep_name, dep)) => (dep_name, &dep.limit_enforce, Some(dep.limit)),
                None => (&cr.name, &LimitEnforceTyp::DenyCrate, None),
            };
            let (n, oldest) = ctx
                .db
                .number_deployed(side, dep_name.as_str())
                .with_context(|| format_compact!("getting number of {} deployed", dep_name))?;
            let enforce = match limit_enforce {
                LimitEnforceTyp::DenyCrate => {
                    format_compact!("unpacking will be denied when the limit is exceeded")
                }
                LimitEnforceTyp::DeleteOldest => match oldest {
                    Some(Oldest::Group(gid)) => {
                        format_compact!(
                            "unpacking will delete oldest, {}, when the limit is exceeded",
                            gid
                        )
                    }
                    Some(Oldest::Objective(oid)) => {
                        format_compact!(
                            "unpacking will delete oldest, {}, when the limit is exceeded",
                            oid
                        )
                    }
                    None => {
                        format_compact!("unpacking will delete oldest when the limit is exceeded")
                    }
                },
            };
            let limit = limit
                .map(|i| i.to_compact_string())
                .unwrap_or_else(|| format_compact!("unlimited"));
            let msg = format_compact!(
                "{} crate loaded\n{n} of {} {} deployed, {}",
                cr.name,
                limit,
                dep_name,
                enforce
            );
            ctx.db.ephemeral.msgs().panel_to_group(10, false, gid, msg);
            ctx.db.play_sound_player(lua, "crate_load", &slot);
        }
        Err(e) => {
            let err = format_compact!("{e}");
            let msg = if err.as_str() == "CARGO BAY IS NOT READY, CHECK THE DOOR" {
                err
            } else {
                format_compact!("crate could not be loaded: {err}")
            };
            ctx.db.ephemeral.msgs().panel_to_group(10, false, gid, msg)
        }
    }
    Ok(())
}

fn unload_crate(lua: MizLua, gid: GroupId) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    let (_side, slot) = slot_for_group(lua, ctx, &gid).context("getting slot for group")?;
    match ctx.db.unload_crate(lua, &ctx.idx, &slot) {
        Ok(cr) => {
            let msg = format_compact!("{} crate unloaded", cr.name);
            ctx.db.ephemeral.msgs().panel_to_group(10, false, gid, msg);
            ctx.db.play_sound_player(lua, "crate_unload", &slot);
        }
        Err(e) => {
            let msg = format_compact!("{}", e);
            ctx.db.ephemeral.msgs().panel_to_group(10, false, gid, msg)
        }
    }
    Ok(())
}

pub(crate) fn list_cargo_for_slot(ctx: &mut Context, lua: MizLua, slot: &SlotId) -> Result<()> {
    let cargo = Cargo::default();
    let cargo = ctx.db.list_cargo(&slot).unwrap_or(&cargo);
    let sifo = ctx
        .db
        .ephemeral
        .get_slot_info(slot)
        .ok_or_else(|| anyhow!("invalid slot"))?;
    let capacity = ctx
        .db
        .cargo_capacity(&sifo.typ)
        .context("getting unit cargo capacity")?;
    let mut msg = CompactString::new("Current Cargo\n--------------------------------------------\n");
    let ucid = ctx.db.ephemeral.player_in_slot(slot);
    let ed_bay_crates = ctx.db.fowl_crates_on_ed_bay(lua, slot);
    let (crate_n, troop_n) = match ucid {
        Some(_) => (
            cargo.num_crates().saturating_add(ed_bay_crates.len()),
            cargo.num_troops(),
        ),
        None => (cargo.num_crates(), cargo.num_troops()),
    };
    msg.push_str(&format_compact!(
        "troops: {} of {}\n",
        troop_n,
        capacity.troop_slots
    ));
    msg.push_str(&format_compact!(
        "crates: {} of {}\n",
        crate_n,
        capacity.crate_slots
    ));
    msg.push_str(&format_compact!(
        "total : {} of {}\n",
        crate_n.saturating_add(troop_n),
        capacity.total_slots
    ));
    let mut detail = CompactString::new("");
    let mut fowl_hybrid_kg = 0u32;
    for (_, cr) in &cargo.crates {
        detail.push_str(&format_compact!(
            "{} crate weighing: {} kg\n",
            cr.name,
            cr.weight
        ));
        fowl_hybrid_kg = fowl_hybrid_kg.saturating_add(cr.weight);
    }
    // Fowl ED bay crates: names above Dynamic line; mass is included in Dynamic total.
    for (name, _) in &ed_bay_crates {
        detail.push_str(&format_compact!("{name} crate\n"));
    }
    for it in &cargo.troops {
        detail.push_str(&format_compact!(
            "{} troop weighing: {} kg\n",
            it.troop.name,
            it.troop.weight
        ));
        fowl_hybrid_kg = fowl_hybrid_kg.saturating_add(it.troop.weight);
    }
    if !detail.is_empty() {
        msg.push_str("--------------------------------------------\n");
        msg.push_str(&detail);
    }
    // All ED bay cargo (Fowl + warehouse); Fowl names are itemized above.
    let dynamic_kg = ctx.db.loaded_dynamic_cargo_weight_kg(lua, slot);
    msg.push_str("--------------------------------------------\n");
    msg.push_str(&format_compact!(
        "Dynamic cargo weight: {} kg\n",
        dynamic_kg
    ));
    let total = fowl_hybrid_kg.saturating_add(dynamic_kg);
    msg.push_str("======================\n");
    msg.push_str(&format_compact!("Total cargo weight: {} kg", total));
    ctx.db
        .ephemeral
        .msgs()
        .panel_to_unit(15, false, slot.as_unit_id().unwrap(), msg);
    Ok(())
}

pub(crate) fn list_cargo_for_slot_with_sound(
    ctx: &mut Context,
    lua: MizLua,
    slot: &dcso3::net::SlotId,
    sound_key: &str,
) -> Result<()> {
    list_cargo_for_slot(ctx, lua, slot)?;
    ctx.db.play_sound_player(lua, sound_key, slot);
    Ok(())
}

pub fn list_current_cargo(lua: MizLua, gid: GroupId) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    let (_side, slot) = slot_for_group(lua, ctx, &gid).context("getting slot for group")?;
    list_cargo_for_slot_with_sound(ctx, lua, &slot, "cargo_list")
}

pub fn list_troop_cargo(lua: MizLua, gid: GroupId) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    let (_side, slot) = slot_for_group(lua, ctx, &gid).context("getting slot for group")?;
    list_cargo_for_slot_with_sound(ctx, lua, &slot, "troop_load_list")
}

fn list_nearby_crates(lua: MizLua, gid: GroupId) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    let (_side, slot) = slot_for_group(lua, ctx, &gid).context("getting slot for group")?;
    let st = SlotStats::get(&ctx.db, lua, &slot).context("getting slot stats")?;
    let nearby = ctx
        .db
        .list_nearby_crates(lua, &st)
        .context("listing nearby crates")?;
    let nearby_dyn = ctx
        .db
        .list_nearby_dynamic_cargo(lua, &st)
        .context("listing nearby dynamic cargo")?;
    if nearby.is_empty() && nearby_dyn.is_empty() {
        drop(nearby);
        drop(nearby_dyn);
        ctx.db
            .ephemeral
            .msgs()
            .panel_to_group(10, false, gid, "No nearby crates");
        return Ok(());
    }
    let mut msg = CompactString::new("");
    for nc in &nearby {
        if nc.loaded {
            msg.push_str(&format_compact!(
                "{} crate, loaded aboard\n",
                nc.crate_def.name
            ));
        } else {
            msg.push_str(&format_compact!(
                "{} crate, bearing {}, {} meters away\n",
                nc.crate_def.name,
                nc.heading as u32,
                nc.distance as u32
            ));
        }
    }
    for dc in &nearby_dyn {
        let damaged = if dc.damaged { " DAMAGED" } else { "" };
        if dc.loaded {
            msg.push_str(&format_compact!(
                "dynamic {}{}, loaded aboard\n",
                dc.type_name,
                damaged
            ));
        } else {
            msg.push_str(&format_compact!(
                "dynamic {}{}, bearing {}, {} meters away\n",
                dc.type_name,
                damaged,
                dc.heading as u32,
                dc.distance as u32
            ));
        }
    }
    drop(nearby);
    drop(nearby_dyn);
    ctx.db.ephemeral.msgs().panel_to_group(10, false, gid, msg);
    Ok(())
}

fn destroy_nearby_crate(lua: MizLua, gid: GroupId) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    let (_side, slot) = slot_for_group(lua, ctx, &gid).context("getting slot for group")?;
    if let Err(e) = ctx.db.destroy_nearby_crate(lua, &slot) {
        ctx.db
            .ephemeral
            .msgs()
            .panel_to_group(10, false, gid, format_compact!("{}", e))
    } else {
        ctx.db.play_sound_player(lua, "crate_destroy", &slot);
    }
    Ok(())
}

fn to_stock_dynamic_cargo(lua: MizLua, gid: GroupId) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    let (side, slot) = slot_for_group(lua, ctx, &gid).context("getting slot for group")?;
    match ctx.db.to_stock_dynamic_cargo(lua, &slot) {
        Ok(res) => {
            let player = player_name(&ctx.db, &slot);
            let msg = format_compact!(
                "{player} To stock: {} crate(s) into {}",
                res.crates,
                res.destination
            );
            ctx.db.ephemeral.msgs().panel_to_side(10, false, side, msg);
        }
        Err(e) => {
            ctx.db
                .ephemeral
                .msgs()
                .panel_to_group(10, false, gid, format_compact!("{e}"))
        }
    }
    Ok(())
}

fn spawn_crate(lua: MizLua, arg: ArgTuple<GroupId, String>) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    let (_side, slot) = slot_for_group(lua, ctx, &arg.fst).context("getting slot for group")?;
    match ctx.db.spawn_crate(lua, &ctx.idx, &slot, &arg.snd) {
        Err(e) => {
            ctx.db
                .ephemeral
                .msgs()
                .panel_to_group(10, false, arg.fst, format_compact!("{e}"))
        }
        Ok(st) => {
            ctx.db.play_sound_player(lua, "crate_drop", &slot);
            if let Some(max_crates) = ctx.db.ephemeral.cfg.max_crates {
                let (n, oldest) = ctx
                    .db
                    .number_crates_deployed(&st)
                    .context("getting number of deployed crates")?;
                let msg = match oldest {
                    None => format_compact!("{n} of {max_crates} crates spawned"),
                    Some(gid) => format_compact!(
                        "{n} of {max_crates} crates spawned, {gid} will be deleted if the limit is exceeded"
                    ),
                };
                ctx.db
                    .ephemeral
                    .msgs()
                    .panel_to_group(10, false, arg.fst, msg)
            }
        }
    }
    Ok(())
}

pub(super) fn add_cargo_menu_for_group(
    cfg: &Cfg,
    mc: &MissionCommands,
    side: &Side,
    group: GroupId,
    unit_typ: &str,
) -> Result<()> {
    let root = mc.add_submenu_for_group(group, "Cargo".into(), None)?;
    mc.add_command_for_group(
        group,
        "Unpack Nearby Crate(s)".into(),
        Some(root.clone()),
        unpakistan,
        group,
    )?;
    let ed_bay = crate::db::dynamic_cargo::uses_shared_ed_cargo_bay(
        &cfg.dynamic_cargo_delivery,
        unit_typ,
    );
    if !ed_bay {
        mc.add_command_for_group(
            group,
            "Load Nearby Crate".into(),
            Some(root.clone()),
            load_crate,
            group,
        )?;
        mc.add_command_for_group(
            group,
            "Unload Crate".into(),
            Some(root.clone()),
            unload_crate,
            group,
        )?;
    }
    mc.add_command_for_group(
        group,
        "List Nearby Crates".into(),
        Some(root.clone()),
        list_nearby_crates,
        group,
    )?;
    mc.add_command_for_group(
        group,
        "List Cargo".into(),
        Some(root.clone()),
        list_current_cargo,
        group,
    )?;
    mc.add_command_for_group(
        group,
        "Destroy Nearby Crate".into(),
        Some(root.clone()),
        destroy_nearby_crate,
        group,
    )?;
    if cfg.dynamic_cargo_delivery.enabled {
        let supplies = mc.add_submenu_for_group(group, "Supplies".into(), Some(root.clone()))?;
        mc.add_command_for_group(
            group,
            "To stock".into(),
            Some(supplies),
            to_stock_dynamic_cargo,
            group,
        )?;
    }
    let root = mc.add_submenu_for_group(group, "Crates".into(), Some(root.clone()))?;
    let rep = &cfg.repair_crate[side];
    let logi = mc.add_submenu_for_group(group, "Logistics".into(), Some(root.clone()))?;
    mc.add_command_for_group(
        group,
        rep.name.clone(),
        Some(logi.clone()),
        spawn_crate,
        ArgTuple {
            fst: group,
            snd: rep.name.clone(),
        },
    )?;
    if let Some(whcfg) = &cfg.warehouse {
        if cfg.supply_transfer_players {
            let cr = &whcfg.supply_transfer_crate[&side];
            mc.add_command_for_group(
                group,
                cr.name.clone(),
                Some(logi.clone()),
                spawn_crate,
                ArgTuple {
                    fst: group,
                    snd: cr.name.clone(),
                },
            )?;
        }
    }
    let mut created_menus: FxHashMap<String, GroupSubMenu> = FxHashMap::default();
    for dep in cfg.deployables.get(side).unwrap_or(&vec![]) {
        if dep.crates.is_empty() && dep.repair_crate.is_none() {
            continue;
        }
        let name = dep.path.last().unwrap();
        let root = dep
            .path
            .iter()
            .fold(Ok(root.clone()), |root: Result<_>, p| {
                let root = root?;
                match created_menus.entry(p.clone()) {
                    Entry::Occupied(e) => Ok(e.get().clone()),
                    Entry::Vacant(e) => {
                        let item = if p == name && dep.cost > 0 {
                            String::from(format_compact!("{p}({} pts)", dep.cost))
                        } else {
                            p.clone()
                        };
                        let menu = mc.add_submenu_for_group(group, item, Some(root))?;
                        Ok(e.insert(menu).clone())
                    }
                }
            })?;
        for cr in dep.crates.iter().chain(dep.repair_crate.iter()) {
            let title = if cr.required > 1 {
                String::from(format_compact!("{}({})", cr.name, cr.required))
            } else {
                cr.name.clone()
            };
            mc.add_command_for_group(
                group,
                title,
                Some(root.clone()),
                spawn_crate,
                ArgTuple {
                    fst: group,
                    snd: cr.name.clone(),
                },
            )?;
        }
    }
    Ok(())
}
