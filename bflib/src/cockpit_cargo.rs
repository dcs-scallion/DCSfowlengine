//! Attrition cockpit: Cargo / Troops catalogue + F10-parity actions (bfdb RPC).

use crate::{
    db::{
        cargo::{Cargo, SlotStats},
        group::DeployKind,
    },
    jtac::JtId,
    menu::jtac::init_jtac_menu_for_slot,
    Context,
};
use anyhow::{anyhow, bail, Context as _, Result};
use bfprotocols::cfg::LimitEnforceTyp;
use compact_str::{format_compact, CompactString};
use dcso3::{coalition::Side, net::SlotId, net::Ucid, MizLua};
use serde::{Deserialize, Serialize};

type StdString = std::string::String;

#[derive(Debug, Serialize)]
struct CargoCapacityDto {
    troop_slots: u8,
    crate_slots: u8,
    total_slots: u16,
}

#[derive(Debug, Serialize)]
struct PlayerCrateEntry {
    id: StdString,
    name: StdString,
}

#[derive(Debug, Serialize)]
struct CargoPanelResponse {
    /// Player slotted in a CFG `cargo` airframe with troop and/or crate capacity.
    available: bool,
    airframe: Option<StdString>,
    capacity: Option<CargoCapacityDto>,
    can_troops: bool,
    can_crates: bool,
    crates: Vec<CrateEntry>,
    troops: Vec<TroopEntry>,
    categories: Vec<StdString>,
    ed_bay: bool,
    csar: bool,
    supplies: bool,
    /// Calling player's campaign points (crate/troop affordability).
    points: i32,
    /// CFG `max_crates` (per-player placed crates).
    max_crates: Option<u32>,
    /// Player-owned crates in FIFO order (oldest first — spawn overflow deletes these).
    player_crates: Vec<PlayerCrateEntry>,
}

fn empty_cargo_panel(airframe: Option<StdString>) -> CargoPanelResponse {
    CargoPanelResponse {
        available: false,
        airframe,
        capacity: None,
        can_troops: false,
        can_crates: false,
        crates: Vec::new(),
        troops: Vec::new(),
        categories: Vec::new(),
        ed_bay: false,
        csar: false,
        supplies: false,
        points: 0,
        max_crates: None,
        player_crates: Vec::new(),
    }
}

fn player_points_and_crates(ctx: &Context, ucid: &Ucid) -> (i32, Vec<PlayerCrateEntry>) {
    let Some(player) = ctx.db.player(ucid) else {
        return (0, Vec::new());
    };
    let points = player.points;
    let mut player_crates = Vec::new();
    for gid in &player.crates {
        let name = match ctx.db.group(gid) {
            Ok(g) => match &g.origin {
                DeployKind::Crate { spec, .. } => s(&spec.name),
                _ => format!("{gid}"),
            },
            Err(_) => format!("{gid}"),
        };
        player_crates.push(PlayerCrateEntry {
            id: format!("{gid}"),
            name,
        });
    }
    (points, player_crates)
}

/// Same gate as F10 Cargo/Troops menus (`CarryCap` + rules).
fn carry_for_slot(
    ctx: &Context,
    ucid: &Ucid,
    slot: &SlotId,
) -> Result<(StdString, bool, bool, Option<CargoCapacityDto>, bool)> {
    let typ = ctx
        .db
        .ephemeral
        .get_slot_info(slot)
        .map(|s| s.typ.as_str().to_string())
        .unwrap_or_default();
    let cfg = &ctx.db.ephemeral.cfg;
    let Some(cap) = cfg.cargo.get(typ.as_str()).copied() else {
        return Ok((typ, false, false, None, false));
    };
    let dto = CargoCapacityDto {
        troop_slots: cap.troop_slots,
        crate_slots: cap.crate_slots,
        total_slots: cap.total_slots,
    };
    let can_troops = cap.troop_slots > 0
        && cap.total_slots > 0
        && cfg.rules.troops.check(ucid);
    let can_crates = cap.crate_slots > 0
        && cap.total_slots > 0
        && cfg.rules.cargo.check(ucid);
    let ed_bay = crate::db::dynamic_cargo::uses_shared_ed_cargo_bay(
        &cfg.dynamic_cargo_delivery,
        &typ,
    );
    Ok((typ, can_troops, can_crates, Some(dto), ed_bay))
}

#[derive(Debug, Serialize)]
struct CrateEntry {
    name: StdString,
    category: StdString,
    required: u32,
    deployable: StdString,
    cost: u32,
    deployed: u32,
    limit: u32,
}

#[derive(Debug, Serialize)]
struct TroopEntry {
    name: StdString,
    cost: u32,
    deployed: u32,
    limit: u32,
}

#[derive(Debug, Deserialize)]
struct CargoActionReq {
    action: StdString,
    #[serde(default)]
    crate_name: Option<StdString>,
    #[serde(default)]
    troop_name: Option<StdString>,
    #[serde(default)]
    qty: Option<u32>,
}

fn require_slot(ctx: &Context, ucid: &Ucid) -> Result<(Side, SlotId)> {
    let player = ctx
        .db
        .player(ucid)
        .ok_or_else(|| anyhow!("player not registered"))?;
    let side = player.side;
    if !matches!(side, Side::Blue | Side::Red) {
        bail!("join a coalition first");
    }
    let slot = player
        .current_slot
        .as_ref()
        .map(|(s, _)| *s)
        .ok_or_else(|| anyhow!("occupy a slot first"))?;
    Ok((side, slot))
}

fn player_name(ctx: &Context, slot: &SlotId) -> StdString {
    ctx.db
        .ephemeral
        .player_in_slot(slot)
        .and_then(|ucid| ctx.db.player(ucid).map(|p| p.name.to_string()))
        .unwrap_or_default()
}

fn notify_player(ctx: &mut Context, ucid: &Ucid, msg: &str) {
    ctx.db
        .ephemeral
        .panel_to_player(&ctx.db.persisted, 10, ucid, msg);
}

fn s(v: impl std::fmt::Display) -> StdString {
    v.to_string()
}

pub(crate) fn list_cargo_panel_for_ucid(ctx: &Context, ucid: &Ucid) -> Result<StdString> {
    let (points, player_crates) = player_points_and_crates(ctx, ucid);
    let max_crates = ctx.db.ephemeral.cfg.max_crates;

    let player = match ctx.db.player(ucid) {
        Some(p) => p,
        None => {
            let mut resp = empty_cargo_panel(None);
            resp.points = points;
            resp.max_crates = max_crates;
            resp.player_crates = player_crates;
            return Ok(serde_json::to_string(&resp)?);
        }
    };
    let side = player.side;
    if !matches!(side, Side::Blue | Side::Red) {
        let mut resp = empty_cargo_panel(None);
        resp.points = points;
        resp.max_crates = max_crates;
        resp.player_crates = player_crates;
        return Ok(serde_json::to_string(&resp)?);
    }
    let slot = match player.current_slot.as_ref().map(|(s, _)| *s) {
        Some(s) => s,
        None => {
            let mut resp = empty_cargo_panel(None);
            resp.points = points;
            resp.max_crates = max_crates;
            resp.player_crates = player_crates;
            return Ok(serde_json::to_string(&resp)?);
        }
    };

    let (typ, can_troops, can_crates, capacity, ed_bay) = carry_for_slot(ctx, ucid, &slot)?;
    let available = can_troops || can_crates;
    if !available {
        let mut resp = empty_cargo_panel(if typ.is_empty() {
            None
        } else {
            Some(typ)
        });
        resp.capacity = capacity;
        resp.points = points;
        resp.max_crates = max_crates;
        resp.player_crates = player_crates;
        return Ok(serde_json::to_string(&resp)?);
    }

    let cfg = &ctx.db.ephemeral.cfg;
    let mut crates = Vec::new();
    let mut categories: Vec<StdString> = Vec::new();

    if can_crates {
        if let Some(rep) = cfg.repair_crate.get(&side) {
            crates.push(CrateEntry {
                name: s(&rep.name),
                category: "Logistics".into(),
                required: rep.required.max(1),
                deployable: "Logistics".into(),
                cost: 0,
                deployed: 0,
                limit: 0,
            });
            categories.push("Logistics".into());
        }

        if let Some(whcfg) = &cfg.warehouse {
            if cfg.supply_transfer_players {
                if let Some(cr) = whcfg.supply_transfer_crate.get(&side) {
                    if !categories.iter().any(|c| c == "Logistics") {
                        categories.push("Logistics".into());
                    }
                    crates.push(CrateEntry {
                        name: s(&cr.name),
                        category: "Logistics".into(),
                        required: cr.required.max(1),
                        deployable: "Logistics".into(),
                        cost: 0,
                        deployed: 0,
                        limit: 0,
                    });
                }
            }
        }

        for dep in cfg.deployables.get(&side).unwrap_or(&vec![]) {
            if dep.crates.is_empty() && dep.repair_crate.is_none() {
                continue;
            }
            let deployable = dep
                .path
                .last()
                .map(s)
                .unwrap_or_else(|| "Deployable".into());
            let category = if dep.path.len() >= 2 {
                dep.path[..dep.path.len() - 1]
                    .iter()
                    .map(s)
                    .collect::<Vec<_>>()
                    .join(" / ")
            } else {
                deployable.clone()
            };
            if !categories.iter().any(|c| c == &category) {
                categories.push(category.clone());
            }
            let (deployed, _) = ctx
                .db
                .number_deployed(side, deployable.as_str())
                .unwrap_or((0, None));
            for cr in dep.crates.iter().chain(dep.repair_crate.iter()) {
                crates.push(CrateEntry {
                    name: s(&cr.name),
                    category: category.clone(),
                    required: cr.required.max(1),
                    deployable: deployable.clone(),
                    cost: dep.cost,
                    deployed: deployed as u32,
                    limit: dep.limit,
                });
            }
        }
    }

    let mut troops = Vec::new();
    if can_troops {
        for sq in cfg.troops.get(&side).unwrap_or(&vec![]) {
            let (deployed, _) = ctx
                .db
                .number_troops_deployed(side, sq.name.as_str())
                .unwrap_or((0, None));
            troops.push(TroopEntry {
                name: s(&sq.name),
                cost: sq.cost,
                deployed: deployed as u32,
                limit: sq.limit,
            });
        }
    }

    // Keep CFG / F10 radio order (Logistics first, then deployables path order). Do not sort A–Z.
    let resp = CargoPanelResponse {
        available: true,
        airframe: Some(typ),
        capacity,
        can_troops,
        can_crates,
        crates,
        troops,
        categories,
        ed_bay,
        csar: can_troops && cfg.csar.enabled,
        supplies: can_crates && cfg.dynamic_cargo_delivery.enabled,
        points,
        max_crates,
        player_crates,
    };
    Ok(serde_json::to_string(&resp)?)
}

fn format_current_cargo(ctx: &Context, lua: MizLua, slot: &SlotId) -> Result<StdString> {
    let cargo = Cargo::default();
    let cargo = ctx.db.list_cargo(slot).unwrap_or(&cargo);
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
    let ed_bay_crates = ctx.db.fowl_crates_on_ed_bay(lua, slot);
    let crate_n = cargo.num_crates().saturating_add(ed_bay_crates.len());
    let troop_n = cargo.num_troops();
    let pilot_n = cargo.num_pilots();
    msg.push_str(&format_compact!(
        "troops: {} of {}\n",
        troop_n,
        capacity.troop_slots
    ));
    if pilot_n > 0 {
        msg.push_str(&format_compact!(
            "downed pilots: {} (2 = 1 troop squad)\n",
            pilot_n
        ));
    }
    msg.push_str(&format_compact!(
        "crates: {} of {}\n",
        crate_n,
        capacity.crate_slots
    ));
    msg.push_str(&format_compact!(
        "total : {} of {}\n",
        crate_n
            .saturating_add(troop_n)
            .saturating_add((pilot_n + 1) / 2),
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
    for p in &cargo.pilots {
        let kind = if p.enemy { "captured" } else { "rescued" };
        detail.push_str(&format_compact!(
            "{} {kind} pilot weighing: {} kg\n",
            p.name,
            p.weight_kg
        ));
        fowl_hybrid_kg = fowl_hybrid_kg.saturating_add(p.weight_kg);
    }
    if !detail.is_empty() {
        msg.push_str("--------------------------------------------\n");
        msg.push_str(&detail);
    }
    let dynamic_kg = ctx.db.loaded_dynamic_cargo_weight_kg(lua, slot);
    msg.push_str("--------------------------------------------\n");
    msg.push_str(&format_compact!(
        "Dynamic cargo weight: {} kg\n",
        dynamic_kg
    ));
    let total = fowl_hybrid_kg.saturating_add(dynamic_kg);
    msg.push_str("======================\n");
    msg.push_str(&format_compact!("Total cargo weight: {} kg", total));
    Ok(msg.into())
}

fn format_nearby_crates(ctx: &Context, lua: MizLua, slot: &SlotId) -> Result<StdString> {
    let st = SlotStats::get(&ctx.db, lua, slot).context("getting slot stats")?;
    let nearby = ctx
        .db
        .list_nearby_crates(lua, &st, slot)
        .context("listing nearby crates")?;
    let nearby_dyn = ctx
        .db
        .list_nearby_dynamic_cargo(lua, &st)
        .context("listing nearby dynamic cargo")?;
    if nearby.is_empty() && nearby_dyn.is_empty() {
        return Ok("No nearby crates".into());
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
    Ok(msg.into())
}

pub(crate) fn run_cargo_action(
    ctx: &mut Context,
    lua: MizLua,
    ucid: Ucid,
    body: &str,
) -> Result<StdString> {
    let req: CargoActionReq = serde_json::from_str(body).context("parse cargo action")?;
    let (side, slot) = require_slot(ctx, &ucid)?;
    let action = req.action.to_ascii_lowercase();
    let qty = req.qty.unwrap_or(1).max(1);

    let (_typ, can_troops, can_crates, _capacity, ed_bay) = carry_for_slot(ctx, &ucid, &slot)?;
    if !can_troops && !can_crates {
        bail!("this aircraft cannot carry cargo");
    }
    let need_crates = matches!(
        action.as_str(),
        "list_nearby"
            | "load_crate"
            | "unload_crate"
            | "unpack"
            | "destroy"
            | "to_stock"
            | "spawn"
            | "f8_load"
            | "f8_unload"
    );
    let need_troops = matches!(
        action.as_str(),
        "troop_load"
            | "troop_unload"
            | "troop_extract"
            | "troop_return"
            | "csar_list"
            | "csar_smoke"
            | "csar_extract"
            | "csar_extract_enemy"
            | "csar_deliver"
    );
    // list_cargo / troop_list: shared status readout when either capability exists
    if need_crates && !can_crates {
        bail!("this aircraft cannot carry crates");
    }
    if need_troops && !can_troops {
        bail!("this aircraft cannot carry troops");
    }

    match action.as_str() {
        "list_cargo" => {
            let msg = format_current_cargo(ctx, lua, &slot)?;
            notify_player(ctx, &ucid, &msg);
            Ok(msg)
        }
        "list_nearby" => {
            let msg = format_nearby_crates(ctx, lua, &slot)?;
            notify_player(ctx, &ucid, &msg);
            Ok(msg)
        }
        "f8_load" | "f8_unload" => {
            if !ed_bay {
                bail!("F8 cargo bay is not used by this aircraft");
            }
            let msg = if action == "f8_load" {
                "Press F8 (Ground Crew) → Cargo LOAD"
            } else {
                "Press F8 (Ground Crew) → Cargo UNLOAD"
            };
            notify_player(ctx, &ucid, msg);
            Ok(msg.into())
        }
        "load_crate" => {
            if ed_bay {
                bail!("this aircraft loads crates via the ED cargo bay (F8)");
            }
            match ctx.db.load_nearby_crate(lua, &slot) {
                Ok(cr) => {
                    ctx.db.play_sound_player(lua, "crate_load", &slot);
                    let msg = format!("{} crate loaded", cr.name);
                    notify_player(ctx, &ucid, &msg);
                    Ok(msg)
                }
                Err(e) => bail!("{e:#}"),
            }
        }
        "unload_crate" => {
            if ed_bay {
                match ctx.db.unload_ed_bay_crate(lua, &slot) {
                    Ok(name) => {
                        ctx.db.play_sound_player(lua, "crate_unload", &slot);
                        let msg = format!("{name} crate unloaded to the ground");
                        notify_player(ctx, &ucid, &msg);
                        Ok(msg)
                    }
                    Err(e) => bail!("{e:#}"),
                }
            } else {
                match ctx.db.unload_crate(lua, &ctx.idx, &slot) {
                    Ok(cr) => {
                        ctx.db.play_sound_player(lua, "crate_unload", &slot);
                        let msg = format!("{} crate unloaded", cr.name);
                        notify_player(ctx, &ucid, &msg);
                        Ok(msg)
                    }
                    Err(e) => bail!("{e:#}"),
                }
            }
        }
        "unpack" => match ctx.db.unpakistan(lua, &ctx.idx, &slot) {
            Ok(unpakistan) => {
                ctx.db.campaign_top10_on_logistics(ucid);
                ctx.db.play_sound_player(lua, "crate_unpack", &slot);
                let player = player_name(ctx, &slot);
                let msg = format!("{player} {unpakistan}");
                ctx.db
                    .ephemeral
                    .msgs()
                    .panel_to_side(10, false, side, msg.as_str());
                Ok(msg)
            }
            Err(e) => bail!("{e:#}"),
        },
        "destroy" => match ctx.db.destroy_nearby_crate(lua, &slot) {
            Ok(()) => {
                ctx.db.play_sound_player(lua, "crate_destroy", &slot);
                let msg = "nearby crate destroyed".to_string();
                notify_player(ctx, &ucid, &msg);
                Ok(msg)
            }
            Err(e) => bail!("{e:#}"),
        },
        "to_stock" => match ctx.db.to_stock_dynamic_cargo(lua, &slot) {
            Ok(res) => {
                let player = player_name(ctx, &slot);
                let msg = format!(
                    "{player} To stock: {} crate(s) into {}",
                    res.crates, res.destination
                );
                ctx.db
                    .ephemeral
                    .msgs()
                    .panel_to_side(10, false, side, msg.as_str());
                Ok(msg)
            }
            Err(e) => bail!("{e:#}"),
        },
        "spawn" => {
            let name = req
                .crate_name
                .as_deref()
                .ok_or_else(|| anyhow!("crate_name required"))?;
            let mut msgs = Vec::new();
            for i in 0..qty {
                match ctx.db.spawn_crate(lua, &ctx.idx, &slot, name) {
                    Ok(_) => {
                        ctx.db.play_sound_player(lua, "crate_drop", &slot);
                        msgs.push(format!("{name} spawned ({}/{qty})", i + 1));
                    }
                    Err(e) => {
                        if msgs.is_empty() {
                            bail!("{e:#}");
                        }
                        msgs.push(format!("stopped: {e:#}"));
                        break;
                    }
                }
            }
            let msg = msgs.join("\n");
            notify_player(ctx, &ucid, &msg);
            Ok(msg)
        }
        "troop_list" => {
            let msg = format_current_cargo(ctx, lua, &slot)?;
            notify_player(ctx, &ucid, &msg);
            Ok(msg)
        }
        "troop_load" => {
            let name = req
                .troop_name
                .as_deref()
                .ok_or_else(|| anyhow!("troop_name required"))?;
            match ctx.db.load_troops(lua, &slot, name) {
                Ok((tr, oid)) => {
                    let (n, oldest) = ctx
                        .db
                        .number_troops_deployed(side, &tr.name)
                        .context("getting number of deployed troops")?;
                    let player = player_name(ctx, &slot);
                    let sub = ctx.subscribed_jtac_menus.entry(slot).or_default();
                    sub.pinned.insert(JtId::Slot(slot));
                    sub.subscribed_objectives.insert(oid);
                    init_jtac_menu_for_slot(ctx, lua, &slot)?;
                    let enforce = match tr.limit_enforce {
                        LimitEnforceTyp::DenyCrate => {
                            format_compact!("unloading will be denied when the limit is exceeded")
                        }
                        LimitEnforceTyp::DeleteOldest => match oldest {
                            Some(gid) => format_compact!(
                                "unloading will delete oldest, {gid}, when the limit is exceeded"
                            ),
                            None => format_compact!(
                                "unloading will delete oldest when the limit is exceeded"
                            ),
                        },
                    };
                    let msg = format!(
                        "{player} loaded {}\n{n} of {} {} deployed, {}",
                        tr.name, tr.limit, tr.name, enforce
                    );
                    ctx.db
                        .ephemeral
                        .msgs()
                        .panel_to_side(10, false, side, msg.as_str());
                    ctx.db.play_sound_player(lua, "troop_load_unload", &slot);
                    Ok(msg)
                }
                Err(e) => bail!("{e:#}"),
            }
        }
        "troop_unload" => match ctx.db.unload_troops(lua, &ctx.idx, &slot) {
            Ok((tr, tgid, oid)) => {
                let player = player_name(ctx, &slot);
                let sub = ctx.subscribed_jtac_menus.entry(slot).or_default();
                sub.pinned.insert(JtId::Group(tgid));
                if let Some(oid) = oid {
                    sub.subscribed_objectives.insert(oid);
                }
                init_jtac_menu_for_slot(ctx, lua, &slot)?;
                let msg = format!("{player} dropped {} troops into the field", tr.name);
                ctx.db
                    .ephemeral
                    .msgs()
                    .panel_to_side(10, false, side, msg.as_str());
                ctx.db.play_sound_player(lua, "troop_load_unload", &slot);
                Ok(msg)
            }
            Err(e) => bail!("{e:#}"),
        },
        "troop_extract" => match ctx.db.extract_troops(lua, &slot) {
            Ok(tr) => {
                let player = player_name(ctx, &slot);
                let msg = format!("{player} extracted {} troops from the field", tr.name);
                ctx.db
                    .ephemeral
                    .msgs()
                    .panel_to_side(10, false, side, msg.as_str());
                ctx.db.play_sound_player(lua, "troop_load_extract", &slot);
                Ok(msg)
            }
            Err(e) => bail!("{e:#}"),
        },
        "troop_return" => match ctx.db.return_troops(lua, &slot) {
            Ok(tr) => {
                let player = player_name(ctx, &slot);
                let msg = format!("{player} returned {} troops", tr.name);
                ctx.db
                    .ephemeral
                    .msgs()
                    .panel_to_side(10, false, side, msg.as_str());
                ctx.db.play_sound_player(lua, "troop_load_return", &slot);
                Ok(msg)
            }
            Err(e) => bail!("{e:#}"),
        },
        "csar_list" => match ctx.db.csar_list_nearby(lua, &slot) {
            Ok(msg) => {
                let msg = s(msg);
                notify_player(ctx, &ucid, &msg);
                Ok(msg)
            }
            Err(e) => bail!("{e:#}"),
        },
        "csar_smoke" => match ctx.db.csar_request_smoke(lua, &slot) {
            Ok(msg) => {
                let msg = s(msg);
                notify_player(ctx, &ucid, &msg);
                Ok(msg)
            }
            Err(e) => bail!("{e:#}"),
        },
        "csar_extract" => match ctx.db.csar_extract_friendly(lua, &slot) {
            Ok(msg) => {
                let msg = s(msg);
                notify_player(ctx, &ucid, &msg);
                Ok(msg)
            }
            Err(e) => bail!("{e:#}"),
        },
        "csar_extract_enemy" => match ctx.db.csar_extract_captured_enemy(lua, &slot) {
            Ok(msg) => {
                let msg = s(msg);
                notify_player(ctx, &ucid, &msg);
                Ok(msg)
            }
            Err(e) => bail!("{e:#}"),
        },
        "csar_deliver" => match ctx.db.csar_deliver(lua, &slot) {
            Ok(msg) => {
                let msg = s(msg);
                notify_player(ctx, &ucid, &msg);
                Ok(msg)
            }
            Err(e) => bail!("{e:#}"),
        },
        other => bail!("unknown cargo action {other}"),
    }
}
