//! Append-only UCID alias log next to stats.jsonl.
//!
//! Lines are JSON objects:
//! `{"ts":"...","op":"merge","from":"...","to":"...","note":"..."}`
//! `{"ts":"...","op":"revoke","from":"...","to":"...","note":"..."}`
//!
//! For an event at time T: apply the latest merge for `from` whose start <= T
//! and whose until is null or > T. Revoke sets until = revoke.ts.

use anyhow::{bail, Context, Result};
use bfprotocols::{
    shots::{Dead, Who},
    stats::{EnId, Stat},
};
use chrono::{DateTime, Utc};
use dcso3::net::Ucid;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::{BufRead, Write},
    path::{Path, PathBuf},
    str::FromStr,
};

pub const ALIASES_FILE_NAME: &str = "stats_ucid_aliases.jsonl";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AliasOp {
    Merge,
    Revoke,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AliasRecord {
    pub ts: DateTime<Utc>,
    pub op: AliasOp,
    pub from: std::string::String,
    pub to: std::string::String,
    #[serde(default)]
    pub note: std::string::String,
}

#[derive(Debug, Clone)]
struct AliasWindow {
    to: Ucid,
    start: DateTime<Utc>,
    until: Option<DateTime<Utc>>,
}

#[derive(Debug, Default, Clone)]
pub struct UcidAliasTable {
    /// All windows per source UCID (chronological).
    by_from: HashMap<Ucid, Vec<AliasWindow>>,
    records: Vec<AliasRecord>,
}

impl UcidAliasTable {
    pub fn aliases_path_for_jsonl(jsonl: &Path) -> PathBuf {
        jsonl.with_file_name(ALIASES_FILE_NAME)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let mut table = Self::default();
        if !path.exists() {
            return Ok(table);
        }
        let file = std::fs::File::open(path)
            .with_context(|| format!("open UCID aliases {path:?}"))?;
        let reader = std::io::BufReader::new(file);
        for (lineno, line) in reader.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    error!("UCID aliases read error line {}: {e}", lineno + 1);
                    continue;
                }
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<AliasRecord>(trimmed) {
                Ok(rec) => table.apply_record(rec),
                Err(e) => {
                    warn!(
                        "UCID aliases skip bad line {}: {e}; raw: {}",
                        lineno + 1,
                        trimmed.chars().take(120).collect::<std::string::String>()
                    );
                }
            }
        }
        Ok(table)
    }

    fn apply_record(&mut self, rec: AliasRecord) {
        let from = match Ucid::from_str(&rec.from) {
            Ok(u) => u,
            Err(_) => {
                warn!("UCID aliases invalid from={}", rec.from);
                return;
            }
        };
        let to = match Ucid::from_str(&rec.to) {
            Ok(u) => u,
            Err(_) => {
                warn!("UCID aliases invalid to={}", rec.to);
                return;
            }
        };
        match rec.op {
            AliasOp::Merge => {
                self.by_from.entry(from).or_default().push(AliasWindow {
                    to,
                    start: rec.ts,
                    until: None,
                });
            }
            AliasOp::Revoke => {
                if let Some(windows) = self.by_from.get_mut(&from) {
                    if let Some(w) = windows.iter_mut().rev().find(|w| w.to == to && w.until.is_none())
                    {
                        w.until = Some(rec.ts);
                    } else {
                        warn!(
                            "UCID aliases revoke without open merge from={} to={}",
                            rec.from, rec.to
                        );
                    }
                } else {
                    warn!(
                        "UCID aliases revoke unknown from={} to={}",
                        rec.from, rec.to
                    );
                }
            }
        }
        self.records.push(rec);
    }

    pub fn records(&self) -> &[AliasRecord] {
        &self.records
    }

    pub fn resolve(&self, ucid: Ucid, event_ts: DateTime<Utc>) -> Ucid {
        let mut cur = ucid;
        // Bound chain length to avoid cycles from bad admin input.
        for _ in 0..8 {
            let Some(windows) = self.by_from.get(&cur) else {
                break;
            };
            let Some(w) = windows.iter().rev().find(|w| {
                w.start <= event_ts && w.until.map(|u| event_ts < u).unwrap_or(true)
            }) else {
                break;
            };
            if w.to == cur {
                break;
            }
            cur = w.to;
        }
        cur
    }

    pub fn append(path: &Path, rec: &AliasRecord) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create aliases dir {parent:?}"))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("append UCID aliases {path:?}"))?;
        serde_json::to_writer(&mut file, rec)?;
        file.write_all(b"\n")?;
        file.flush()?;
        info!(
            "UCID aliases appended op={:?} from={} to={}",
            rec.op, rec.from, rec.to
        );
        Ok(())
    }

    pub fn active_target(&self, from: &Ucid) -> Option<Ucid> {
        self.by_from.get(from).and_then(|windows| {
            windows
                .iter()
                .rev()
                .find(|w| w.until.is_none())
                .map(|w| w.to)
        })
    }
}

fn map_ucid(u: &mut Ucid, table: &UcidAliasTable, ts: DateTime<Utc>) {
    *u = table.resolve(*u, ts);
}

fn map_who(who: &mut Who, table: &UcidAliasTable, ts: DateTime<Utc>) {
    match who {
        Who::AI { ucid: Some(u), .. } => map_ucid(u, table, ts),
        Who::Player { ucid, .. } => map_ucid(ucid, table, ts),
        Who::AI { ucid: None, .. } => {}
    }
}

fn map_enid(id: &mut EnId, table: &UcidAliasTable, ts: DateTime<Utc>) {
    if let EnId::Player(u) = id {
        map_ucid(u, table, ts);
    }
}

fn map_dead(dead: &mut Dead, table: &UcidAliasTable, ts: DateTime<Utc>) {
    map_who(&mut dead.victim, table, ts);
    for shot in &mut dead.shots {
        map_who(&mut shot.shooter, table, ts);
        map_who(&mut shot.target, table, ts);
    }
}

/// Rewrite player UCIDs in a Stat according to active aliases at `event_ts`.
pub fn apply_to_stat(stat: &mut Stat, table: &UcidAliasTable, event_ts: DateTime<Utc>) {
    if table.by_from.is_empty() {
        return;
    }
    match stat {
        Stat::Capture { by, .. } => {
            for u in by.iter_mut() {
                map_ucid(u, table, event_ts);
            }
        }
        Stat::Repair { by, .. }
        | Stat::SupplyTransfer { by, .. }
        | Stat::DynamicCargoDelivery { by, .. }
        | Stat::CsarRescue { by, .. }
        | Stat::Action { by, .. }
        | Stat::DeployTroop { by, .. }
        | Stat::DeployGroup { by, .. }
        | Stat::DeployFarp { by, .. }
        | Stat::StaticKill { by, .. } => map_ucid(by, table, event_ts),
        Stat::Register { id, .. }
        | Stat::Sideswitch { id, .. }
        | Stat::Connect { id, .. }
        | Stat::Disconnect { id, .. }
        | Stat::Slot { id, .. }
        | Stat::Deslot { id, .. }
        | Stat::Takeoff { id, .. }
        | Stat::Land { id, .. }
        | Stat::Life { id, .. }
        | Stat::Points { id, .. }
        | Stat::Bind { id, .. }
        | Stat::PilotXp { id, .. } => map_ucid(id, table, event_ts),
        Stat::PointsTransfer { from, to, .. } => {
            map_ucid(from, table, event_ts);
            map_ucid(to, table, event_ts);
        }
        Stat::PointsTransferToObjective { from, .. } => map_ucid(from, table, event_ts),
        Stat::ConvoyDestroyed { killer: Some(u), .. } => map_ucid(u, table, event_ts),
        Stat::Unit { id, .. } | Stat::Position { id, .. } | Stat::Detected { id, .. } => {
            map_enid(id, table, event_ts)
        }
        Stat::Kill(dead) => map_dead(dead, table, event_ts),
        Stat::NewRound { .. }
        | Stat::RoundEnd { .. }
        | Stat::SessionStart { .. }
        | Stat::SessionEnd { .. }
        | Stat::Objective { .. }
        | Stat::EquipmentInventory { .. }
        | Stat::LiquidInventory { .. }
        | Stat::ObjectiveHealth { .. }
        | Stat::ObjectiveSupply { .. }
        | Stat::ObjectiveDestroyed { .. }
        | Stat::GroupDeleted { .. }
        | Stat::AirRouteDelivered { .. }
        | Stat::AirRouteDestroyed { .. }
        | Stat::SeaRouteDelivered { .. }
        | Stat::SeaRouteDestroyed { .. }
        | Stat::CampaignEvent { .. }
        | Stat::Weather { .. }
        | Stat::GciPicture(_)
        | Stat::ConvoyDestroyed { killer: None, .. } => {}
    }
}

pub fn validate_distinct_ucids(from: &str, to: &str) -> Result<(Ucid, Ucid)> {
    let from_u = Ucid::from_str(from).map_err(|e| anyhow::anyhow!("invalid from ucid: {e}"))?;
    let to_u = Ucid::from_str(to).map_err(|e| anyhow::anyhow!("invalid to ucid: {e}"))?;
    if from_u == to_u {
        bail!("from and to UCID must differ");
    }
    Ok((from_u, to_u))
}
