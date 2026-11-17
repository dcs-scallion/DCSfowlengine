//! Attrition cockpit: INFO panel — live CFG campaign rules + DEP/troop tables (player side).

use crate::Context;
use anyhow::{anyhow, Result};
use bfprotocols::cfg::{ActionKind, Cfg, Deployable, Troop};
use dcso3::{coalition::Side, net::Ucid};
use serde::Serialize;

type StdString = std::string::String;

#[derive(Debug, Serialize)]
struct KvEntry {
    label: StdString,
    value: StdString,
}

#[derive(Debug, Serialize)]
struct ActionEntry {
    name: StdString,
    limit: Option<u32>,
    cost: u32,
    penalty: Option<u32>,
    detail: Option<StdString>,
}

#[derive(Debug, Serialize)]
struct TroopInfoEntry {
    name: StdString,
    cost: u32,
    limit: u32,
    weight: u32,
    can_capture: bool,
    can_capture_csar: bool,
    jtac_range_m: Option<u32>,
    fire_min_m: Option<u32>,
    fire_max_m: Option<u32>,
}

#[derive(Debug, Serialize)]
struct DeployableInfoEntry {
    name: StdString,
    category: StdString,
    crates: u32,
    cost: u32,
    repair_cost: Option<u32>,
    limit: u32,
    jtac_range_m: Option<u32>,
    ewr_range_m: Option<u32>,
    fire_min_m: Option<u32>,
    fire_max_m: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ArtilleryRangeEntry {
    name: StdString,
    kind: StdString,
    min_m: Option<u32>,
    max_m: u32,
}

#[derive(Debug, Serialize)]
struct InfoPanelResponse {
    side: StdString,
    campaign_rules: Vec<KvEntry>,
    actions: Vec<ActionEntry>,
    points: Vec<KvEntry>,
    troops: Vec<TroopInfoEntry>,
    deployables: Vec<DeployableInfoEntry>,
    ships: Vec<DeployableInfoEntry>,
    artillery: Vec<ArtilleryRangeEntry>,
    misc: Vec<KvEntry>,
}

fn format_duration_seconds(secs: u32) -> StdString {
    if secs == 0 {
        return "—".into();
    }
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    if h > 0 && m == 0 {
        format!("{h} h")
    } else if h > 0 {
        format!("{h} h {m} min")
    } else {
        format!("{m} min")
    }
}

fn format_logistics_interval_minutes(minutes: u32) -> StdString {
    if minutes == 0 {
        return "—".into();
    }
    let h = minutes / 60;
    let m = minutes % 60;
    if h > 0 && m == 0 {
        format!("{h} h 0 min")
    } else if h > 0 {
        format!("{h} h {m} min")
    } else {
        format!("{minutes} min")
    }
}

fn format_deslot_penalty(cfg: &Cfg) -> StdString {
    let secs = cfg.airborne_deslot_penalty_secs;
    let points = cfg.airborne_deslot_penalty_points;
    let time = if secs == 0 {
        "0 min".into()
    } else {
        format_duration_seconds(secs)
    };
    format!("{time} / -{points} p.")
}

fn warehouse_logistics_intervals(cfg: &Cfg) -> (StdString, StdString) {
    let Some(wh) = cfg.warehouse.as_ref() else {
        return ("—".into(), "—".into());
    };
    (
        format_logistics_interval_minutes(wh.tick),
        format_logistics_interval_minutes(wh.tick.saturating_mul(wh.ticks_per_delivery)),
    )
}

fn campaign_rules(cfg: &Cfg) -> Vec<KvEntry> {
    let (supply_to_bases, delivery_to_hubs) = warehouse_logistics_intervals(cfg);
    let player_crates = cfg
        .max_crates
        .map(|n| n.to_string())
        .unwrap_or_else(|| "—".into());
    let mut out = vec![
        KvEntry {
            label: "Air-deslot penalty".into(),
            value: format_deslot_penalty(cfg),
        },
        KvEntry {
            label: "Player crates".into(),
            value: player_crates,
        },
        KvEntry {
            label: "Threatened".into(),
            value: format_duration_seconds(cfg.threatened_cooldown),
        },
        KvEntry {
            label: "Logi repair".into(),
            value: format_duration_seconds(cfg.logi_repair_rate_seconds),
        },
        KvEntry {
            label: "Health repair".into(),
            value: format_duration_seconds(cfg.repair_time),
        },
        KvEntry {
            label: "Factory repair".into(),
            value: format_duration_seconds(cfg.production_repair_rate_seconds),
        },
        KvEntry {
            label: "Static repair".into(),
            value: format_duration_seconds(cfg.static_repair_rate_seconds),
        },
    ];
    // All ME static types from CFG (`objective_static_units`); fewest crates first.
    let mut static_types: Vec<_> = cfg.objective_static_units.iter().collect();
    static_types.sort_by(|a, b| {
        a.1.repair_crates
            .cmp(&b.1.repair_crates)
            .then_with(|| a.0.cmp(b.0))
    });
    for (unit_type, scfg) in static_types {
        let n = scfg.repair_crates.max(1);
        let display = unit_type.trim_start_matches('.');
        out.push(KvEntry {
            label: format!("Static repair crates ({display})"),
            value: if n == 1 {
                "1 crate".into()
            } else {
                format!("{n} crates")
            },
        });
    }
    out.push(KvEntry {
        label: "Supply to bases".into(),
        value: supply_to_bases,
    });
    out.push(KvEntry {
        label: "Delivery to HUBs".into(),
        value: delivery_to_hubs,
    });
    out
}

fn action_detail(kind: &ActionKind) -> Option<StdString> {
    // Never expose ME template names in INFO — only player-facing ranges / payloads.
    match kind {
        ActionKind::Tanker(_)
        | ActionKind::Fighters(_)
        | ActionKind::Attackers(_)
        | ActionKind::Sead(_)
        | ActionKind::CruiseMissileSpawn(_)
        | ActionKind::LogisticsRepair(_)
        | ActionKind::LogisticsTransfer(_)
        | ActionKind::Bomber(_) => None,
        ActionKind::Awacs(a) => Some(format!("EWR {} km", a.ewr.range / 1000)),
        ActionKind::Drone(d) => Some(format!("JTAC {} km", d.jtac.range / 1000)),
        ActionKind::Paratrooper(d) => Some(d.name.to_string()),
        ActionKind::Deployable(d) => Some(d.name.to_string()),
        ActionKind::Nuke(n) => Some(format!("{} kt", n.power)),
        ActionKind::Move(m) => Some(format!(
            "troops {} m/cr · vehicles {} m/cr · ships {} m/cr",
            m.troop,
            m.deployable,
            m.deployable_naval()
        )),
        ActionKind::CruiseMissileWaypoint
        | ActionKind::FighersWaypoint
        | ActionKind::AttackersWaypoint
        | ActionKind::SeadWaypoint
        | ActionKind::DroneWaypoint
        | ActionKind::TankerWaypoint
        | ActionKind::AwacsWaypoint
        | ActionKind::Rtb
        | ActionKind::Start
        | ActionKind::Status
        | ActionKind::Rearm => None,
    }
}

fn points_entries(cfg: &Cfg) -> Vec<KvEntry> {
    let Some(p) = cfg.points.as_ref() else {
        return Vec::new();
    };
    let mut out = vec![
        KvEntry {
            label: "New player join".into(),
            value: p.new_player_join.to_string(),
        },
        KvEntry {
            label: "Carrier kill".into(),
            value: p.carrier_kill.to_string(),
        },
        KvEntry {
            label: "Ship (heliport) kill".into(),
            value: p.ships_with_heliport_kill.to_string(),
        },
        KvEntry {
            label: "Ship (no heliport) kill".into(),
            value: p.ships_no_heliport_kill.to_string(),
        },
        KvEntry {
            label: "Capture HUB".into(),
            value: p.capture_hub.to_string(),
        },
        KvEntry {
            label: "AWACS kill".into(),
            value: p.awacs_kill.to_string(),
        },
        KvEntry {
            label: "Capture airbase".into(),
            value: p.capture_airbase.to_string(),
        },
        KvEntry {
            label: "Capture FOB / FARP".into(),
            value: p.capture_fob.to_string(),
        },
        KvEntry {
            label: "EWR kill".into(),
            value: p.ewr_kill.to_string(),
        },
        KvEntry {
            label: "Air kill".into(),
            value: p.air_kill.to_string(),
        },
        KvEntry {
            label: "CSAR enemy pilot".into(),
            value: p.csar_delivery_enemy_pilot.to_string(),
        },
        KvEntry {
            label: "Factory kill".into(),
            value: p.production_kill.to_string(),
        },
        KvEntry {
            label: "Logistics transfer".into(),
            value: p.logistics_transfer.to_string(),
        },
        KvEntry {
            label: "CSAR coalition pilot".into(),
            value: p.csar_delivery_coalition_pilot.to_string(),
        },
        KvEntry {
            label: "LR SAM bonus".into(),
            value: p.lr_sam_bonus.to_string(),
        },
        KvEntry {
            label: "Ground kill".into(),
            value: p.ground_kill.to_string(),
        },
        KvEntry {
            label: "Logistics repair".into(),
            value: p.logistics_repair.to_string(),
        },
        KvEntry {
            label: "Production repair crate".into(),
            value: cfg.production_repair_crate_cost.to_string(),
        },
        KvEntry {
            label: "Static repair crate".into(),
            value: cfg.static_repair_crate_cost.to_string(),
        },
    ];
    let (gain, period) = p.periodic_point_gain;
    if period > 0 && gain != 0 {
        out.push(KvEntry {
            label: "Periodic gain".into(),
            value: format!(
                "{gain} pts / {}{}",
                format_duration_seconds(period),
                if p.balancing_point_gain {
                    " (balanced)"
                } else {
                    ""
                }
            ),
        });
    }
    out.push(KvEntry {
        label: "Provisional points".into(),
        value: if p.provisional { "yes".into() } else { "no".into() },
    });
    out.push(KvEntry {
        label: "Strict loadout".into(),
        value: if p.strict { "yes".into() } else { "no".into() },
    });
    out
}

fn is_naval_deployable(dep: &Deployable) -> bool {
    dep.path
        .first()
        .map(|p| p.as_str().starts_with("Naval"))
        .unwrap_or(false)
}

fn deployable_display_name(dep: &Deployable) -> StdString {
    dep.path
        .last()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "—".into())
}

fn deployable_category(dep: &Deployable) -> StdString {
    if dep.path.len() >= 2 {
        dep.path[0].to_string()
    } else {
        "Other".into()
    }
}

fn crates_required(dep: &Deployable) -> u32 {
    dep.crates.iter().map(|c| c.required).sum()
}

fn deployable_entry(dep: &Deployable) -> DeployableInfoEntry {
    let repair_cost = if dep.repair_crate.is_some() || dep.repair_cost > 0 {
        Some(dep.repair_cost)
    } else {
        None
    };
    DeployableInfoEntry {
        name: deployable_display_name(dep),
        category: deployable_category(dep),
        crates: crates_required(dep),
        cost: dep.cost,
        repair_cost,
        limit: dep.limit,
        jtac_range_m: dep.jtac.map(|j| j.range),
        ewr_range_m: dep.ewr.as_ref().map(|e| e.range),
        fire_min_m: dep.fire_range.map(|r| r.min_m).filter(|&m| m > 0),
        fire_max_m: dep.fire_range.map(|r| r.max_m),
    }
}

fn troop_entry(t: &Troop) -> TroopInfoEntry {
    TroopInfoEntry {
        name: t.name.to_string(),
        cost: t.cost,
        limit: t.limit,
        weight: t.weight,
        can_capture: t.can_capture,
        can_capture_csar: t.can_capture_csar,
        jtac_range_m: t.jtac.map(|j| j.range),
        fire_min_m: t.fire_range.map(|r| r.min_m).filter(|&m| m > 0),
        fire_max_m: t.fire_range.map(|r| r.max_m),
    }
}

fn artillery_from_cfg(deps: &[Deployable], troops: &[Troop]) -> Vec<ArtilleryRangeEntry> {
    let mut out = Vec::new();
    for t in troops {
        if let Some(fr) = t.fire_range {
            out.push(ArtilleryRangeEntry {
                name: t.name.to_string(),
                kind: "Troop".into(),
                min_m: (fr.min_m > 0).then_some(fr.min_m),
                max_m: fr.max_m,
            });
        }
    }
    for d in deps {
        if is_naval_deployable(d) {
            continue;
        }
        if let Some(fr) = d.fire_range {
            out.push(ArtilleryRangeEntry {
                name: deployable_display_name(d),
                kind: "Deployable".into(),
                min_m: (fr.min_m > 0).then_some(fr.min_m),
                max_m: fr.max_m,
            });
        }
    }
    out
}

fn misc_entries(cfg: &Cfg, side: Side) -> Vec<KvEntry> {
    let jtac_code = match side {
        Side::Blue => cfg.jtac_default_code_blue,
        Side::Red => cfg.jtac_default_code_red,
        _ => 0,
    };
    let mut out = vec![
        KvEntry {
            label: "JTAC default laser code".into(),
            value: jtac_code.to_string(),
        },
        KvEntry {
            label: "Logistics exclusion".into(),
            value: format!("{} m", cfg.logistics_exclusion),
        },
        KvEntry {
            label: "Crate load distance".into(),
            value: format!("{} m", cfg.crate_load_distance),
        },
        KvEntry {
            label: "Crate spread".into(),
            value: format!("{} m", cfg.crate_spread),
        },
        KvEntry {
            label: "Artillery mission range".into(),
            value: format!("{} m", cfg.artillery_mission_range),
        },
        KvEntry {
            label: "CALCM missions".into(),
            value: if cfg.calcm_mission {
                "enabled".into()
            } else {
                "disabled".into()
            },
        },
        KvEntry {
            label: "Lock sides".into(),
            value: if cfg.lock_sides {
                "yes".into()
            } else {
                "no".into()
            },
        },
        KvEntry {
            label: "Side switches".into(),
            value: cfg
                .side_switches
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unlimited".into()),
        },
        KvEntry {
            label: "Virtual resupply".into(),
            value: if cfg.virtual_resupply {
                "on".into()
            } else {
                "off".into()
            },
        },
    ];
    if let Some(actions) = cfg.actions.get(&side) {
        for (_name, act) in actions {
            if !act.enabled {
                continue;
            }
            if let ActionKind::Move(m) = &act.kind {
                out.push(KvEntry {
                    label: "Move".into(),
                    value: format!("cost {}", act.cost),
                });
                out.push(KvEntry {
                    label: "Move troops".into(),
                    value: format!("{} m/cr", m.troop),
                });
                out.push(KvEntry {
                    label: "Move vehicles".into(),
                    value: format!("{} m/cr", m.deployable),
                });
                out.push(KvEntry {
                    label: "Move ships".into(),
                    value: format!("{} m/cr", m.deployable_naval()),
                });
                break;
            }
        }
    }
    out
}

pub(crate) fn list_info_panel_for_ucid(ctx: &Context, ucid: &Ucid) -> Result<StdString> {
    let player = ctx
        .db
        .player(ucid)
        .ok_or_else(|| anyhow!("player not registered"))?;
    let side = player.side;
    let cfg = &ctx.db.ephemeral.cfg;

    let actions = cfg
        .actions
        .get(&side)
        .map(|map| {
            map.iter()
                .filter(|(_, act)| act.enabled)
                .map(|(name, act)| ActionEntry {
                    name: name.to_string(),
                    limit: act.limit,
                    cost: act.cost,
                    penalty: act.penalty,
                    detail: action_detail(&act.kind),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let troops_src = cfg.troops.get(&side).map(|v| v.as_slice()).unwrap_or(&[]);
    let deps_src = cfg
        .deployables
        .get(&side)
        .map(|v| v.as_slice())
        .unwrap_or(&[]);

    let mut deployables = Vec::new();
    let mut ships = Vec::new();
    for dep in deps_src {
        let entry = deployable_entry(dep);
        if is_naval_deployable(dep) {
            ships.push(entry);
        } else {
            deployables.push(entry);
        }
    }

    let troops: Vec<_> = troops_src.iter().map(troop_entry).collect();
    let artillery = artillery_from_cfg(deps_src, troops_src);

    let resp = InfoPanelResponse {
        side: format!("{side:?}"),
        campaign_rules: campaign_rules(cfg),
        actions,
        points: points_entries(cfg),
        troops,
        deployables,
        ships,
        artillery,
        misc: misc_entries(cfg, side),
    };
    Ok(serde_json::to_string(&resp)?)
}
