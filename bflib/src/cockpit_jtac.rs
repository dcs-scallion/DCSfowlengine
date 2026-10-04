//! Attrition in-game cockpit: JTAC list + F10-parity actions (bfdb RPC).

use crate::{
    db::group::DeployKind,
    jtac::JtId,
    menu::{
        jtac::{
            call_bomber, init_jtac_menu_for_slot, jtac_artillery_combo_mission,
            jtac_artillery_fire_all, jtac_artillery_mission, jtac_calcm_mission, jtac_clear_filter,
            jtac_filter, jtac_get_artillery_ammo, jtac_relay_target, jtac_set_code,
            jtac_set_full_code, jtac_shift, jtac_smoke_target, jtac_status, jtac_toggle_auto_shift,
            jtac_toggle_ir_pointer,
        },
        ArgQuad, ArgTriple, ArgTuple,
    },
    Context,
};
use anyhow::{anyhow, bail, Context as _, Result};
use bfprotocols::{
    cfg::{ActionKind, UnitTag},
    db::group::GroupId,
};
use dcso3::{coalition::Side, net::Ucid, MizLua};
use enumflags2::{BitFlag, BitFlags};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

type StdString = std::string::String;

#[derive(Debug, Serialize)]
struct JtacListResponse {
    jtacs: Vec<JtacListEntry>,
    filter_tags: Vec<StdString>,
    calcm_enabled: bool,
    bomber_missions: Vec<StdString>,
}

#[derive(Debug, Serialize)]
struct JtacListEntry {
    id: StdString,
    name: StdString,
    objective: StdString,
    code: u16,
    autoshift: bool,
    ir_pointer: bool,
    pinned: bool,
    filter: Vec<StdString>,
    status: StdString,
    nearby_artillery: Vec<StdString>,
    nearby_calcm: Vec<CalcmEntry>,
}

#[derive(Debug, Serialize)]
struct CalcmEntry {
    id: StdString,
    ammo: i32,
}

#[derive(Debug, Deserialize)]
struct JtacActionReq {
    jtac_id: StdString,
    action: StdString,
    #[serde(default)]
    arty_id: Option<StdString>,
    #[serde(default)]
    rounds: Option<u8>,
    #[serde(default)]
    rounds_per_target: Option<u8>,
    #[serde(default)]
    num_targets: Option<u8>,
    #[serde(default)]
    code: Option<u16>,
    #[serde(default)]
    code_part: Option<u16>,
    #[serde(default)]
    filter_tag: Option<StdString>,
    #[serde(default)]
    bomber: Option<StdString>,
    #[serde(default)]
    calcm_n: Option<u8>,
    #[serde(default)]
    calcm_per: Option<u8>,
}

fn jtac_display_name(ctx: &Context, jtac_id: JtId) -> StdString {
    match jtac_id {
        JtId::Group(gid) => match ctx.db.group(&gid) {
            Err(_) => format!("{gid}"),
            Ok(group) => match &group.origin {
                DeployKind::Action { name, .. } => format!("{gid}({name})"),
                DeployKind::Deployed { player, spec, .. } => match ctx.db.player(player) {
                    Some(player) => {
                        format!("{gid}({} {})", spec.path.last().unwrap(), player.name)
                    }
                    None => format!("{gid}({})", spec.path.last().unwrap()),
                },
                DeployKind::Troop { player, spec, .. } => match ctx.db.player(player) {
                    Some(player) => format!("{gid}({} {})", spec.name, player.name),
                    None => format!("{gid}({})", spec.name),
                },
                DeployKind::Objective { .. }
                | DeployKind::ObjectiveDeprecated
                | DeployKind::Crate { .. }
                | DeployKind::CsarPilot { .. } => format!("{gid}"),
            },
        },
        JtId::Slot(sl) => {
            let name = match ctx.db.ephemeral.player_in_slot(&sl) {
                None => StdString::new(),
                Some(ucid) => match ctx.db.player(ucid) {
                    None => StdString::new(),
                    Some(p) => p.name.to_string(),
                },
            };
            let typ = ctx
                .db
                .ephemeral
                .get_slot_info(&sl)
                .map(|ifo| ifo.typ.clone())
                .unwrap_or_else(|| bfprotocols::cfg::Vehicle::from(""));
            format!("sl{sl}({typ} {name})")
        }
    }
}

fn filter_tag_names(bits: BitFlags<UnitTag>) -> Vec<StdString> {
    bits.iter().map(|t| format!("{:?}", t)).collect()
}

fn parse_filter_tag(name: &str) -> Result<BitFlags<UnitTag>> {
    for tag in UnitTag::all().iter() {
        if format!("{:?}", tag).eq_ignore_ascii_case(name) {
            return Ok(BitFlags::from(tag));
        }
    }
    bail!("unknown filter tag {name}")
}

pub(crate) fn list_jtacs_for_ucid(ctx: &Context, ucid: &Ucid) -> Result<StdString> {
    let player = ctx
        .db
        .player(ucid)
        .ok_or_else(|| anyhow!("player not registered"))?;
    let side = player.side;
    if !matches!(side, Side::Blue | Side::Red) {
        bail!("join a coalition first");
    }
    let slot = player.current_slot.as_ref().map(|(s, _)| *s);
    let pinned = slot
        .and_then(|s| ctx.subscribed_jtac_menus.get(&s))
        .map(|sub| sub.pinned.clone())
        .unwrap_or_default();

    let mut jtacs = Vec::new();
    for jtac in ctx.jtac.jtacs() {
        if jtac.side() != side {
            continue;
        }
        let id = jtac.gid();
        let objective = ctx
            .db
            .objective(&jtac.location().oid)
            .map(|o| o.name.to_string())
            .unwrap_or_else(|_| "unknown".into());
        let status = jtac
            .status(&ctx.db, ctx.jtac.location_by_code())
            .map(|s| s.to_string())
            .unwrap_or_else(|e| format!("status error: {e:?}"));
        jtacs.push(JtacListEntry {
            id: id.to_string(),
            name: jtac_display_name(ctx, id),
            objective,
            code: jtac.code(),
            autoshift: jtac.autoshift(),
            ir_pointer: jtac.ir_pointer(),
            pinned: pinned.contains(&id),
            filter: filter_tag_names(jtac.filter()),
            status,
            nearby_artillery: jtac
                .nearby_artillery()
                .iter()
                .map(|g| g.to_string())
                .collect(),
            nearby_calcm: jtac
                .nearby_calcm()
                .iter()
                .map(|(g, ammo)| CalcmEntry {
                    id: g.to_string(),
                    ammo: *ammo,
                })
                .collect(),
        });
    }
    jtacs.sort_by(|a, b| b.pinned.cmp(&a.pinned).then(a.objective.cmp(&b.objective)));

    let bomber_missions: Vec<StdString> = ctx
        .db
        .ephemeral
        .cfg
        .actions
        .get(&side)
        .into_iter()
        .flat_map(|acts| {
            acts.iter().filter_map(|(n, a)| match a.kind {
                ActionKind::Bomber(_) => Some(n.to_string()),
                _ => None,
            })
        })
        .collect();

    let filter_tags: Vec<StdString> = UnitTag::all()
        .iter()
        .map(|t| format!("{:?}", t))
        .collect();

    let resp = JtacListResponse {
        jtacs,
        filter_tags,
        calcm_enabled: ctx.db.ephemeral.cfg.calcm_mission,
        bomber_missions,
    };
    Ok(serde_json::to_string(&resp)?)
}

fn ensure_side(ctx: &Context, ucid: &Ucid, jtid: JtId) -> Result<()> {
    let side = ctx
        .db
        .player(ucid)
        .ok_or_else(|| anyhow!("player not registered"))?
        .side;
    let jtac = ctx.jtac.get(&jtid)?;
    if jtac.side() != side {
        bail!("you can't give orders to enemy jtacs");
    }
    Ok(())
}

pub(crate) fn run_jtac_action(
    ctx: &mut Context,
    lua: MizLua,
    ucid: Ucid,
    body: &str,
) -> Result<StdString> {
    let req: JtacActionReq = serde_json::from_str(body).context("parse jtac action")?;
    let jtid = JtId::from_str(&req.jtac_id).context("jtac_id")?;
    ensure_side(ctx, &ucid, jtid)?;

    let action = req.action.to_ascii_lowercase();
    match action.as_str() {
        "status" => {
            jtac_status(
                lua,
                ArgTuple {
                    fst: Some(ucid),
                    snd: jtid,
                },
            )?;
            Ok("status sent".into())
        }
        "shift" => {
            jtac_shift(
                lua,
                ArgTuple {
                    fst: ucid,
                    snd: jtid,
                },
            )?;
            Ok("shifted".into())
        }
        "toggle_auto_shift" | "autoshift" => {
            jtac_toggle_auto_shift(
                lua,
                ArgTuple {
                    fst: ucid,
                    snd: jtid,
                },
            )?;
            Ok("auto shift toggled".into())
        }
        "toggle_ir" | "pointer" => {
            jtac_toggle_ir_pointer(
                lua,
                ArgTuple {
                    fst: ucid,
                    snd: jtid,
                },
            )?;
            Ok("ir pointer toggled".into())
        }
        "smoke" => {
            jtac_smoke_target(
                lua,
                ArgTuple {
                    fst: ucid,
                    snd: jtid,
                },
            )?;
            Ok("smoke requested".into())
        }
        "pin" | "unpin" | "toggle_pin" => {
            let slot = ctx
                .db
                .player(&ucid)
                .and_then(|p| p.current_slot.as_ref().map(|(s, _)| *s))
                .ok_or_else(|| anyhow!("occupy a slot to pin JTACs"))?;
            let subd = ctx.subscribed_jtac_menus.entry(slot).or_default();
            if subd.pinned.contains(&jtid) {
                subd.pinned.remove(&jtid);
            } else {
                subd.pinned.insert(jtid);
            }
            init_jtac_menu_for_slot(ctx, lua, &slot)?;
            Ok("pin toggled".into())
        }
        "clear_filter" => {
            jtac_clear_filter(
                lua,
                ArgTuple {
                    fst: ucid,
                    snd: jtid,
                },
            )?;
            Ok("filter cleared".into())
        }
        "filter" => {
            let tag = req
                .filter_tag
                .as_deref()
                .ok_or_else(|| anyhow!("filter_tag required"))?;
            let bits = parse_filter_tag(tag)?;
            jtac_filter(
                lua,
                ArgTriple {
                    fst: jtid,
                    snd: bits.bits(),
                    trd: ucid,
                },
            )?;
            Ok(format!("filter {tag}"))
        }
        "set_code" => {
            let code = req.code.ok_or_else(|| anyhow!("code required"))?;
            jtac_set_full_code(
                lua,
                ArgTriple {
                    fst: jtid,
                    snd: code,
                    trd: ucid,
                },
            )?;
            Ok(format!("code {code}"))
        }
        "set_code_part" => {
            let part = req.code_part.ok_or_else(|| anyhow!("code_part required"))?;
            jtac_set_code(
                lua,
                ArgTriple {
                    fst: jtid,
                    snd: part,
                    trd: ucid,
                },
            )?;
            Ok(format!("code part {part}"))
        }
        "arty_relay" => {
            let aid = parse_arty(&req)?;
            jtac_relay_target(
                lua,
                ArgTriple {
                    fst: jtid,
                    snd: aid,
                    trd: ucid,
                },
            )?;
            Ok("target relayed".into())
        }
        "arty_ammo" => {
            let aid = parse_arty(&req)?;
            jtac_get_artillery_ammo(
                lua,
                ArgTriple {
                    fst: jtid,
                    snd: aid,
                    trd: ucid,
                },
            )?;
            Ok("ammo report sent".into())
        }
        "arty_fire" => {
            let aid = parse_arty(&req)?;
            let n = req.rounds.ok_or_else(|| anyhow!("rounds required"))?;
            jtac_artillery_mission(
                lua,
                ArgQuad {
                    fst: jtid,
                    snd: aid,
                    trd: n,
                    fth: ucid,
                },
            )?;
            Ok(format!("arty fire {n}"))
        }
        "arty_fire_all" => {
            let aid = parse_arty(&req)?;
            jtac_artillery_fire_all(
                lua,
                ArgTriple {
                    fst: jtid,
                    snd: aid,
                    trd: ucid,
                },
            )?;
            Ok("arty fire all".into())
        }
        "arty_combo" => {
            let aid = parse_arty(&req)?;
            let rpt = req
                .rounds_per_target
                .ok_or_else(|| anyhow!("rounds_per_target required"))?;
            let nt = req
                .num_targets
                .ok_or_else(|| anyhow!("num_targets required"))?;
            jtac_artillery_combo_mission(
                lua,
                ArgQuad {
                    fst: jtid,
                    snd: aid,
                    trd: vec![rpt, nt],
                    fth: ucid,
                },
            )?;
            Ok(format!("arty combo {rpt}x{nt}"))
        }
        "calcm" => {
            let aid = parse_arty(&req)?;
            let n = req.calcm_n.ok_or_else(|| anyhow!("calcm_n required"))?;
            let per = req.calcm_per.ok_or_else(|| anyhow!("calcm_per required"))?;
            jtac_calcm_mission(
                lua,
                ArgQuad {
                    fst: jtid,
                    snd: aid,
                    trd: vec![n, per],
                    fth: ucid,
                },
            )?;
            Ok(format!("calcm {n}/{per}"))
        }
        "bomber" => {
            let name = match req.bomber.clone() {
                Some(n) if !n.is_empty() => n,
                _ => {
                    let side = ctx.db.player(&ucid).map(|p| p.side).unwrap_or(Side::Neutral);
                    ctx.db
                        .ephemeral
                        .cfg
                        .actions
                        .get(&side)
                        .into_iter()
                        .flat_map(|acts| {
                            acts.iter().filter_map(|(n, a)| match a.kind {
                                ActionKind::Bomber(_) => Some(n.to_string()),
                                _ => None,
                            })
                        })
                        .next()
                        .ok_or_else(|| anyhow!("no bomber mission"))?
                }
            };
            call_bomber(
                lua,
                ArgTriple {
                    fst: jtid,
                    snd: ucid,
                    trd: name.clone().into(),
                },
            )?;
            Ok(format!("bomber {name}"))
        }
        other => bail!("unknown action {other}"),
    }
}

fn parse_arty(req: &JtacActionReq) -> Result<GroupId> {
    let s = req
        .arty_id
        .as_deref()
        .ok_or_else(|| anyhow!("arty_id required"))?;
    s.parse::<GroupId>()
        .map_err(|e| anyhow!("invalid arty_id: {e:?}"))
}
