//! Attrition cockpit: MAIN panel (identity, lives, time, points transfer).

use crate::{
    admin,
    db::csar::LIFE_TYPE_DISPLAY_ORDER,
    Context,
};
use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{Duration, Utc};
use compact_str::format_compact;
use dcso3::{net::Ucid, MizLua};
use netidx::utils::Either;
use serde::{Deserialize, Serialize};

type StdString = std::string::String;

#[derive(Debug, Serialize)]
struct LifeEntry {
    label: StdString,
    remaining: u32,
    max: u32,
    /// Slot-reserve display (1+cur) while occupying that life type.
    reserved: bool,
    /// Seconds until life bank reset; null when at max / no timer.
    reset_after_secs: Option<i64>,
    /// DCS internal types for this life class on the player's side (SPA aliases).
    airframes: Vec<StdString>,
}

#[derive(Debug, Serialize)]
struct TransferPlayerTarget {
    name: StdString,
}

#[derive(Debug, Serialize)]
struct TransferObjectiveTarget {
    name: StdString,
}

#[derive(Debug, Serialize)]
struct MainPanelResponse {
    name: StdString,
    side: StdString,
    points: i32,
    lives: Vec<LifeEntry>,
    /// Engine auto-shutdown (RFC3339). bfdb may overlay DCSServerBot restart.
    restart_at: Option<StdString>,
    /// CFG fallback text when no live countdown (same as `-time`).
    chat_time_command: Option<StdString>,
    players: Vec<TransferPlayerTarget>,
    objectives: Vec<TransferObjectiveTarget>,
}

#[derive(Debug, Deserialize)]
struct TransferReq {
    /// `"player"` or `"objective"`.
    kind: StdString,
    target: StdString,
    amount: u32,
}

pub(crate) fn list_main_panel_for_ucid(ctx: &mut Context, ucid: &Ucid) -> Result<StdString> {
    let _ = ctx.db.maybe_reset_lives(ucid, Utc::now());
    let player = ctx
        .db
        .player(ucid)
        .ok_or_else(|| anyhow!("player not registered"))?;
    let name = player.name.to_string();
    let side = player.side;
    let points = player.points;
    let side_str = format!("{side:?}");

    let cfg = &ctx.db.ephemeral.cfg;
    let active_life_type = player
        .current_slot
        .as_ref()
        .filter(|(slot, _)| {
            ctx.db
                .ephemeral
                .player_in_slot(slot)
                .is_some_and(|u| u == ucid)
        })
        .and_then(|(slot, _)| ctx.db.ephemeral.get_slot_info(slot))
        .and_then(|sifo| cfg.life_types.get(&sifo.typ))
        .copied()
        .or_else(|| {
            ctx.db
                .ephemeral
                .ca_oid_by_controller
                .contains_key(ucid)
                .then_some(bfprotocols::cfg::LifeType::CombinedArms)
        });

    // Side airframes: Fowl export allowlists (full mission set) ∪ live slots.
    // Slots alone only show currently placed/warehouse-visible types.
    let mut side_types: std::collections::BTreeSet<StdString> =
        std::collections::BTreeSet::new();
    {
        let export = ctx.db.ephemeral.fowl_miz_export.as_ref();
        for defs in export.objective_defaults.values() {
            let list = match side {
                dcso3::coalition::Side::Blue => &defs.blue_aircraft,
                dcso3::coalition::Side::Red => &defs.red_aircraft,
                _ => continue,
            };
            for t in list {
                side_types.insert(t.clone());
            }
        }
        let inv = match side {
            dcso3::coalition::Side::Blue => Some(&export.blue_inventory_zone_module_ws),
            dcso3::coalition::Side::Red => Some(&export.red_inventory_zone_module_ws),
            _ => None,
        };
        if let Some(inv) = inv {
            for typ in inv.keys() {
                side_types.insert(typ.clone());
            }
        }
        for si in ctx.db.ephemeral.iter_slot_info() {
            if si.side == side {
                side_types.insert(si.typ.to_string());
            }
        }
    }
    let mut airframes_by_life: std::collections::BTreeMap<
        bfprotocols::cfg::LifeType,
        std::collections::BTreeSet<StdString>,
    > = std::collections::BTreeMap::new();
    for typ_name in &side_types {
        let vehicle = bfprotocols::cfg::Vehicle::from(typ_name.as_str());
        let Some(lt) = cfg.life_types.get(&vehicle).copied() else {
            continue;
        };
        airframes_by_life
            .entry(lt)
            .or_default()
            .insert(typ_name.clone());
    }

    let now = Utc::now();
    let mut lives = Vec::new();
    for typ in LIFE_TYPE_DISPLAY_ORDER {
        let Some(&(n, reset_after)) = cfg.default_lives.get(&typ) else {
            continue;
        };
        let label = ctx.db.life_type_panel_label(typ);
        let (remaining, reserved, reset_after_secs) = match player.lives.get(&typ) {
            None => (n as u32, false, None),
            Some((reset, cur)) => {
                let since = now - *reset;
                let left = Duration::seconds(reset_after as i64) - since;
                let secs = if left > Duration::zero() {
                    Some(left.num_seconds())
                } else {
                    Some(0)
                };
                let reserved = cfg.limited_lives
                    && (cfg.lives_birth || typ == bfprotocols::cfg::LifeType::CombinedArms)
                    && active_life_type == Some(typ);
                (*cur as u32, reserved, secs)
            }
        };
        let airframes = airframes_by_life
            .remove(&typ)
            .map(|s| s.into_iter().collect())
            .unwrap_or_default();
        lives.push(LifeEntry {
            label,
            remaining,
            max: n as u32,
            reserved,
            reset_after_secs,
            airframes,
        });
    }

    let restart_at = ctx
        .shutdown
        .as_ref()
        .map(|asd| asd.when.to_rfc3339());
    let chat_time_command = cfg
        .chat_time_command
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let mut players = Vec::new();
    for (_id, ifo) in ctx.connected.info_by_player_id.iter() {
        if &ifo.ucid == ucid {
            continue;
        }
        let Some(tp) = ctx.db.player(&ifo.ucid) else {
            continue;
        };
        if tp.side != side {
            continue;
        }
        players.push(TransferPlayerTarget {
            name: ifo.name.to_string(),
        });
    }
    players.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    let mut objectives = Vec::new();
    for (_oid, obj) in ctx.db.persisted.objectives.into_iter() {
        if obj.owner != side {
            continue;
        }
        objectives.push(TransferObjectiveTarget {
            name: obj.name.to_string(),
        });
    }
    objectives.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));

    let resp = MainPanelResponse {
        name,
        side: side_str,
        points,
        lives,
        restart_at,
        chat_time_command,
        players,
        objectives,
    };
    serde_json::to_string(&resp).context("serialize main panel")
}

pub(crate) fn run_main_transfer(
    ctx: &mut Context,
    _lua: MizLua,
    ucid: Ucid,
    body: &str,
) -> Result<StdString> {
    let req: TransferReq = serde_json::from_str(body).context("parse transfer")?;
    if req.amount == 0 {
        bail!("amount must be > 0");
    }
    let target_trim = req.target.trim();
    if target_trim.is_empty() {
        bail!("target required");
    }
    match req.kind.as_str() {
        "player" => {
            let target_ucid = admin::get_player_ucid(ctx, target_trim)?;
            if target_ucid == ucid {
                bail!("can't transfer points to yourself");
            }
            ctx.db
                .transfer_points(&ucid, Either::Left(&target_ucid), req.amount)?;
            Ok(format_compact!(
                "Transferred {} points to {}",
                req.amount,
                target_trim
            )
            .into())
        }
        "objective" => {
            let oid = admin::get_airbase(&ctx.db, target_trim)?;
            ctx.db
                .transfer_points(&ucid, Either::Right(oid), req.amount)?;
            Ok(format_compact!(
                "Transferred {} points to {}",
                req.amount,
                target_trim
            )
            .into())
        }
        other => bail!("unknown transfer kind {other}"),
    }
}
