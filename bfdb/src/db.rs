use crate::db_id;
use crate::instance::{InstanceCfg, InstanceId, Registry, DEFAULT_INSTANCE};
use anyhow::{anyhow, bail, Context, Result};
use arrayvec::ArrayVec;
use bfprotocols::{
    cfg::{migrate_legacy_setmission_in_stat_json, Cfg, LifeType, UnitTag, UnitTags, Vehicle},
    db::{
        group::{GroupId, UnitId},
        objective::{ObjectiveId, ObjectiveKind},
    },
    perf::PerfInner,
    shots::{Dead, Shot, Who},
    stats::{DetectionSource, EnId, Pos, Stat, StaticKillKind},
};
use chrono::prelude::*;
use dcso3::{
    coalition::Side,
    coord::LLPos,
    net::{SlotId, Ucid},
    perf::{HistogramSer, PerfInner as ApiPerfInner},
    warehouse::LiquidType,
    String,
};
use enumflags2::BitFlags;
use log::{debug, error, info, warn};
use netidx::{path::Path as NetidxPath, subscriber::Subscriber};
use netidx_archive::{
    config::file::Config as ArchiveFileCfg,
    logfile_collection::{ArchiveCollectionReader, ArchiveIndex},
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sled::{transaction::TransactionError, Db};
use smallvec::SmallVec;
use std::{
    collections::{Bound, HashMap, HashSet, VecDeque},
    io::{Read as IoRead, Write as IoWrite},
    ops::Deref,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StdMutex, RwLock,
    },
    time::Duration,
};
use tokio::{sync::broadcast, task};
use uuid::Uuid;
use yats::Tree;

db_id!(KillId);
db_id!(RoundId);
db_id!(SortieId);
db_id!(CaptureId);
db_id!(DeployId);

/// A recorded capture event -- who took an objective and for which side,
/// as opposed to objective_captures which only tracks a running count with
/// no attribution or timeline. Lets API consumers (e.g. the Discord live
/// capture alert) show who actually did it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CaptureRecord {
    pub(crate) time: DateTime<Utc>,
    pub(crate) objective_name: std::string::String,
    pub(crate) side: Side,
    pub(crate) by: SmallVec<[Ucid; 1]>,
}

/// A recorded deploy event -- who deployed what, from which aircraft (if
/// known), and by which method (air drop vs. manual unpack). Distinct from
/// the plain `deploys` counter on Aggregates, which has no attribution or
/// timeline; backs the pilot profile's deploy log.
/// Player destruction of an ME objective static / OPR factory (`Stat::StaticKill`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct StaticKillRecord {
    pub(crate) time: DateTime<Utc>,
    pub(crate) by: Ucid,
    pub(crate) side: Side,
    pub(crate) shooter_typ: Option<std::string::String>,
    pub(crate) weapon_name: Option<std::string::String>,
    pub(crate) target_typ: std::string::String,
    pub(crate) objective: std::string::String,
    pub(crate) objective_id: ObjectiveId,
    pub(crate) kind: StaticKillKind,
    pub(crate) points: i32,
    pub(crate) owner: Side,
    pub(crate) unit_id: UnitId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DeployRecord {
    pub(crate) time: DateTime<Utc>,
    pub(crate) by: Ucid,
    pub(crate) deployable: std::string::String,
    pub(crate) aircraft: Option<std::string::String>,
    pub(crate) method: Option<std::string::String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BanRecord {
    pub(crate) name:      std::string::String,
    pub(crate) banned_at: DateTime<Utc>,
    pub(crate) until:     Option<DateTime<Utc>>,
    pub(crate) reason:    std::string::String,
}

// ── Wiki (bfwiki) types ───────────────────────────────────────────────

/// A single wiki page, keyed by slug (e.g. "gameplay/objectives") in the
/// `wiki_pages` tree. `section`/`order` drive the sidebar grouping in
/// bfwiki -- there's no separate "page tree" structure, just these two
/// fields sorted client-side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WikiPage {
    pub(crate) title:      std::string::String,
    pub(crate) section:    std::string::String,
    pub(crate) order:      i32,
    pub(crate) content:    std::string::String,
    pub(crate) updated_at: DateTime<Utc>,
    pub(crate) updated_by: std::string::String,
}

/// An uploaded image (screenshot etc.), keyed by a generated Uuid in the
/// `wiki_images` tree and referenced from page Markdown as
/// `/api/wiki/images/<uuid>`. Content-addressed by nothing in particular --
/// just an opaque id -- since these are inserted once and never edited.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WikiImage {
    pub(crate) content_type: std::string::String,
    pub(crate) data:         Vec<u8>,
    pub(crate) uploaded_at:  DateTime<Utc>,
    pub(crate) uploaded_by:  std::string::String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WeatherSnapshot {
    pub(crate) temp_c: f64,
    pub(crate) wind_speed_kts: f64,
    pub(crate) wind_from_deg: f64,
    pub(crate) cloud_base_m: f64,
    pub(crate) qnh_hpa: f64,
    pub(crate) cloud_density: Option<u8>,
    pub(crate) visibility_m: Option<f64>,
}

// ── Auth / session types ─────────────────────────────────────────────

/// CSRF state for one in-flight Discord OAuth login, plus which frontend
/// origin initiated it (so the callback can send the browser back to the
/// right site -- bfweb/bfsite/bfwiki are all separate origins now, not
/// embedded in bfdb, so a bare "/" redirect only ever lands on bfdb itself).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct OAuthState {
    pub(crate) expires:   DateTime<Utc>,
    pub(crate) return_to: Option<std::string::String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionData {
    pub(crate) discord_id: std::string::String,
    pub(crate) username:   std::string::String,
    pub(crate) avatar:     Option<std::string::String>,
    pub(crate) is_admin:   bool,
    pub(crate) expires:    DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TrailPoint {
    pub(crate) unit_id: std::string::String,
    pub(crate) lat:     f64,
    pub(crate) lon:     f64,
    pub(crate) alt:     f64,
    pub(crate) hdg:     f64,
    pub(crate) ts:      i64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub(crate) struct Aggregates {
    pub(crate) air_kills: u32,
    pub(crate) ground_kills: u32,
    pub(crate) captures: u32,
    pub(crate) repairs: u32,
    pub(crate) supply_transfers: u32,
    pub(crate) troops: u32,
    pub(crate) farps: u32,
    pub(crate) deploys: u32,
    pub(crate) actions: u32,
    pub(crate) deaths: u32,
    pub(crate) hours: f32,
    pub(crate) donated_points: u32,
    /// Ship (A2S) kills. Skipped in bincode so existing `aggregates` / `Pilot.total`
    /// rows stay readable; persisted in `agg_ship_kills` / `pilot_ship_kills`.
    #[serde(skip)]
    pub(crate) ship_kills: u32,
    /// Ground-to-air kills (G2A). Side tables `agg_ground_air_kills` / `pilot_ground_air_kills`.
    /// `air_kills` is air-to-air only (shooter airframe).
    #[serde(skip)]
    pub(crate) ground_air_kills: u32,
    /// Ground-to-ground kills (G2G). Side tables `agg_ground_ground_kills` / `pilot_ground_ground_kills`.
    /// `ground_kills` is air-to-ground only (shooter airframe).
    #[serde(skip)]
    pub(crate) ground_ground_kills: u32,
    /// Ground-to-ship kills (G2S, e.g. Silkworm). Side tables
    /// `agg_ground_ship_kills` / `pilot_ground_ship_kills`.
    #[serde(skip)]
    pub(crate) ground_ship_kills: u32,
    /// CSAR rescues (pilots delivered). Side tables `agg_csar` / `pilot_csar`.
    #[serde(skip)]
    pub(crate) csar: u32,
}

fn total_kills(a: &Aggregates) -> u32 {
    a.air_kills
        .saturating_add(a.ground_kills)
        .saturating_add(a.ship_kills)
        .saturating_add(a.ground_air_kills)
        .saturating_add(a.ground_ground_kills)
        .saturating_add(a.ground_ship_kills)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillTarget {
    Air,
    Ground,
    Ship,
}

fn kill_target_from_tags(tags: &UnitTags) -> KillTarget {
    if tags.contains(UnitTag::Aircraft) || tags.contains(UnitTag::Helicopter) {
        KillTarget::Air
    } else if tags.contains(UnitTag::ShipCarrier)
        || tags.contains(UnitTag::ShipWithHeliport)
        || tags.contains(UnitTag::ShipNoHeliport)
        || tags.contains(UnitTag::Boat)
    {
        KillTarget::Ship
    } else {
        KillTarget::Ground
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Pilot {
    pub(crate) name: ArrayVec<String, 8>,
    pub(crate) total: Aggregates,
    pub(crate) token: ArrayVec<Uuid, 4>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PilotRoundInfo {
    pub(crate) points: i32,
    pub(crate) side: (DateTime<Utc>, Side),
    pub(crate) slot: Option<Slot>,
    pub(crate) lives: ArrayVec<(LifeType, DateTime<Utc>, u8), 5>,
    pub(crate) connected: Option<(DateTime<Utc>, String)>,
}

impl Default for PilotRoundInfo {
    fn default() -> Self {
        Self {
            points: 0,
            side: (Utc::now(), Side::Neutral),
            slot: None,
            lives: ArrayVec::new(),
            connected: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Sortie {
    pub(crate) vehicle: Vehicle,
    pub(crate) takeoff: DateTime<Utc>,
    /// End of sortie (landing or death / mid-air deslot).
    pub(crate) land: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Slot {
    pub(crate) id: SlotId,
    pub(crate) time: DateTime<Utc>,
    pub(crate) vehicle: Option<Vehicle>,
    pub(crate) sortie: Option<SortieId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Unit {
    pub(crate) group: Option<GroupId>,
    pub(crate) owner: Side,
    pub(crate) typ: Vehicle,
    pub(crate) tags: UnitTags,
    pub(crate) pos: Pos,
    pub(crate) dead: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Objective {
    pub(crate) name: String,
    pub(crate) pos: LLPos,
    pub(crate) kind: ObjectiveKind,
    pub(crate) by: Option<Ucid>,
    pub(crate) owner: Side,
    pub(crate) last_change: DateTime<Utc>,
    pub(crate) health: u8,
    pub(crate) logi: u8,
    pub(crate) supply: u8,
    pub(crate) fuel: u8,
    #[serde(default = "default_production_pct")]
    pub(crate) production: u8,
    #[serde(default)]
    pub(crate) threatened: bool,
}

fn default_production_pct() -> u8 {
    100
}

#[derive(Clone)]
struct Pilots {
    pilots: Tree<Ucid, Pilot>,
    aggregates: Tree<(Ucid, Vehicle, RoundId), Aggregates>,
    by_name: Tree<String, ArrayVec<Ucid, 8>>,
    by_token: Tree<Uuid, Ucid>,
    sortie: Tree<(Ucid, RoundId, SortieId), Sortie>,
    /// Side table: true = closed as Lost (death / mid-air deslot). Kept separate so
    /// existing bincode `sortie` rows stay readable (no schema change on Sortie).
    sortie_crashed: Tree<(Ucid, RoundId, SortieId), bool>,
    /// Per-vehicle/round ship kills (parallel to `aggregates.ground_kills` path).
    agg_ship_kills: Tree<(Ucid, Vehicle, RoundId), u32>,
    /// Career ship kills (parallel to `Pilot.total`).
    pilot_ship_kills: Tree<Ucid, u32>,
    /// Per-vehicle/round ground-to-air kills (G2A).
    agg_ground_air_kills: Tree<(Ucid, Vehicle, RoundId), u32>,
    /// Career G2A kills.
    pilot_ground_air_kills: Tree<Ucid, u32>,
    /// Per-vehicle/round ground-to-ground kills (G2G).
    agg_ground_ground_kills: Tree<(Ucid, Vehicle, RoundId), u32>,
    /// Career G2G kills.
    pilot_ground_ground_kills: Tree<Ucid, u32>,
    /// Per-vehicle/round ground-to-ship kills (G2S).
    agg_ground_ship_kills: Tree<(Ucid, Vehicle, RoundId), u32>,
    /// Career G2S kills.
    pilot_ground_ship_kills: Tree<Ucid, u32>,
    /// Per-vehicle/round CSAR rescues.
    agg_csar: Tree<(Ucid, Vehicle, RoundId), u32>,
    /// Career CSAR rescues.
    pilot_csar: Tree<Ucid, u32>,
    round_info: Tree<(Ucid, RoundId), PilotRoundInfo>,
}

impl Pilots {
    fn new(db: &Db) -> Result<Self> {
        Ok(Self {
            pilots: Tree::open(db, "pilots")?,
            aggregates: Tree::open(db, "aggregates")?,
            by_name: Tree::open(db, "by_name")?,
            by_token: Tree::open(db, "by_token")?,
            sortie: Tree::open(db, "sortie")?,
            sortie_crashed: Tree::open(db, "sortie_crashed")?,
            agg_ship_kills: Tree::open(db, "agg_ship_kills")?,
            pilot_ship_kills: Tree::open(db, "pilot_ship_kills")?,
            agg_ground_air_kills: Tree::open(db, "agg_ground_air_kills")?,
            pilot_ground_air_kills: Tree::open(db, "pilot_ground_air_kills")?,
            agg_ground_ground_kills: Tree::open(db, "agg_ground_ground_kills")?,
            pilot_ground_ground_kills: Tree::open(db, "pilot_ground_ground_kills")?,
            agg_ground_ship_kills: Tree::open(db, "agg_ground_ship_kills")?,
            pilot_ground_ship_kills: Tree::open(db, "pilot_ground_ship_kills")?,
            agg_csar: Tree::open(db, "agg_csar")?,
            pilot_csar: Tree::open(db, "pilot_csar")?,
            round_info: Tree::open(db, "pilot_round_info")?,
        })
    }

    fn bump_ship_kill(&self, ucid: Ucid, round: RoundId) -> Result<()> {
        self.pilot_ship_kills
            .fetch_and_update(&ucid, |n| Some(n.unwrap_or(0).saturating_add(1)))?;
        let vehicle = self
            .round_info
            .get(&(ucid, round))?
            .and_then(|ri| ri.slot.and_then(|s| s.vehicle));
        if let Some(vehicle) = vehicle {
            self.agg_ship_kills
                .fetch_and_update(&(ucid, vehicle, round), |n| {
                    Some(n.unwrap_or(0).saturating_add(1))
                })?;
        }
        Ok(())
    }

    fn bump_ground_ship_kill(
        &self,
        ucid: Ucid,
        round: RoundId,
        shooter_typ: Option<&str>,
    ) -> Result<()> {
        self.pilot_ground_ship_kills
            .fetch_and_update(&ucid, |n| Some(n.unwrap_or(0).saturating_add(1)))?;
        let vehicle = shooter_typ
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .map(Vehicle::from)
            .or_else(|| {
                self.round_info
                    .get(&(ucid, round))
                    .ok()
                    .flatten()
                    .and_then(|ri| ri.slot.and_then(|s| s.vehicle))
            });
        if let Some(vehicle) = vehicle {
            self.agg_ground_ship_kills
                .fetch_and_update(&(ucid, vehicle, round), |n| {
                    Some(n.unwrap_or(0).saturating_add(1))
                })?;
        }
        Ok(())
    }

    fn bump_ground_air_kill(
        &self,
        ucid: Ucid,
        round: RoundId,
        shooter_typ: Option<&str>,
    ) -> Result<()> {
        self.pilot_ground_air_kills
            .fetch_and_update(&ucid, |n| Some(n.unwrap_or(0).saturating_add(1)))?;
        let vehicle = shooter_typ
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .map(Vehicle::from)
            .or_else(|| {
                self.round_info
                    .get(&(ucid, round))
                    .ok()
                    .flatten()
                    .and_then(|ri| ri.slot.and_then(|s| s.vehicle))
            });
        if let Some(vehicle) = vehicle {
            self.agg_ground_air_kills
                .fetch_and_update(&(ucid, vehicle, round), |n| {
                    Some(n.unwrap_or(0).saturating_add(1))
                })?;
        }
        Ok(())
    }

    fn bump_ground_ground_kill(
        &self,
        ucid: Ucid,
        round: RoundId,
        shooter_typ: Option<&str>,
    ) -> Result<()> {
        self.pilot_ground_ground_kills
            .fetch_and_update(&ucid, |n| Some(n.unwrap_or(0).saturating_add(1)))?;
        let vehicle = shooter_typ
            .map(|t| t.trim())
            .filter(|t| !t.is_empty())
            .map(Vehicle::from)
            .or_else(|| {
                self.round_info
                    .get(&(ucid, round))
                    .ok()
                    .flatten()
                    .and_then(|ri| ri.slot.and_then(|s| s.vehicle))
            });
        if let Some(vehicle) = vehicle {
            self.agg_ground_ground_kills
                .fetch_and_update(&(ucid, vehicle, round), |n| {
                    Some(n.unwrap_or(0).saturating_add(1))
                })?;
        }
        Ok(())
    }

    fn bump_csar(&self, ucid: Ucid, round: RoundId) -> Result<()> {
        self.pilot_csar
            .fetch_and_update(&ucid, |n| Some(n.unwrap_or(0).saturating_add(1)))?;
        let vehicle = self
            .round_info
            .get(&(ucid, round))?
            .and_then(|ri| ri.slot.and_then(|s| s.vehicle));
        if let Some(vehicle) = vehicle {
            self.agg_csar
                .fetch_and_update(&(ucid, vehicle, round), |n| {
                    Some(n.unwrap_or(0).saturating_add(1))
                })?;
        }
        Ok(())
    }

    fn career_ship_kills(&self, ucid: &Ucid) -> Result<u32> {
        Ok(self.pilot_ship_kills.get(ucid)?.unwrap_or(0))
    }

    fn career_ground_air_kills(&self, ucid: &Ucid) -> Result<u32> {
        Ok(self.pilot_ground_air_kills.get(ucid)?.unwrap_or(0))
    }

    fn career_ground_ground_kills(&self, ucid: &Ucid) -> Result<u32> {
        Ok(self.pilot_ground_ground_kills.get(ucid)?.unwrap_or(0))
    }

    fn career_ground_ship_kills(&self, ucid: &Ucid) -> Result<u32> {
        Ok(self.pilot_ground_ship_kills.get(ucid)?.unwrap_or(0))
    }

    fn career_csar(&self, ucid: &Ucid) -> Result<u32> {
        Ok(self.pilot_csar.get(ucid)?.unwrap_or(0))
    }

    fn with_pilot<F: FnMut(&mut Pilot)>(&self, k: Ucid, mut f: F) -> Result<()> {
        self.pilots
            .fetch_and_update(&k, |o| match o {
                None => None,
                Some(mut p) => {
                    f(&mut p);
                    Some(p)
                }
            })?
            .ok_or_else(|| anyhow!("pilot {k:?} is missing"))?;
        Ok(())
    }

    fn with_aggregates<F: FnMut(&mut Aggregates)>(
        &self,
        k: (Ucid, Vehicle, RoundId),
        mut f: F,
    ) -> Result<()> {
        self.aggregates
            .fetch_and_update(&k, |a| {
                let mut a = a.unwrap_or_default();
                f(&mut a);
                Some(a)
            })?;
        Ok(())
    }

    fn with_pilot_and_aggregates<F, G>(&self, ucid: Ucid, round: RoundId, f: F, g: G) -> Result<()>
    where
        F: FnMut(&mut Pilot),
        G: FnMut(&mut Aggregates),
    {
        let vehicle = self
            .round_info
            .get(&(ucid, round))?
            .and_then(|ri| ri.slot.and_then(|s| s.vehicle));
        self.with_pilot(ucid, f)?;
        if let Some(vehicle) = vehicle {
            self.with_aggregates((ucid, vehicle, round), g)?
        }
        Ok(())
    }

    fn with_pilot_round_info<F>(&self, ucid: Ucid, round: RoundId, mut f: F) -> Result<()>
    where
        F: FnMut(&mut PilotRoundInfo),
    {
        self.round_info.fetch_and_update(&(ucid, round), |ri| {
            let mut ri = ri.unwrap_or_default();
            f(&mut ri);
            Some(ri)
        })?;
        Ok(())
    }

    fn with_sortie<F>(&self, k: (Ucid, RoundId, SortieId), mut f: F) -> Result<()>
    where
        F: FnMut(&mut Sortie),
    {
        self.sortie
            .fetch_and_update(&k, |s| match s {
                None => None,
                Some(mut s) => {
                    f(&mut s);
                    Some(s)
                }
            })?
            .ok_or_else(|| anyhow!("sortie {k:?} is missing"))?;
        Ok(())
    }

    fn saw_pilot(&self, id: Ucid, name: String) -> Result<()> {
        self.pilots.fetch_and_update(&id, |pilot| match pilot {
            None => Some(Pilot {
                name: ArrayVec::from_iter([name.clone()]),
                total: Aggregates::default(),
                token: ArrayVec::new(),
            }),
            Some(mut pilot) => match pilot.name.iter().enumerate().find(|(_, n)| name == **n) {
                Some((i, _)) => {
                    let last = pilot.name.len() - 1;
                    pilot.name.swap(i, last);
                    Some(pilot)
                }
                None => {
                    if pilot.name.is_full() {
                        let _ = pilot.name.pop_at(0);
                    }
                    pilot.name.push(name.clone());
                    Some(pilot)
                }
            },
        })?;
        self.by_name.update_and_fetch(&name, |ids| match ids {
            None => Some(ArrayVec::from_iter([id])),
            Some(mut ids) if !ids.contains(&id) => {
                if ids.is_full() {
                    ids.pop_at(0);
                }
                ids.push(id);
                Some(ids)
            }
            Some(ids) => Some(ids),
        })?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct Round {
    pub(crate) start: DateTime<Utc>,
    pub(crate) end: Option<DateTime<Utc>>,
    pub(crate) winner: Option<Side>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessionEnd {
    pub(crate) time: DateTime<Utc>,
    pub(crate) frame: HistogramSer,
    pub(crate) api: ApiPerfInner,
    pub(crate) engine: PerfInner,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Session {
    pub(crate) stop_time: Option<DateTime<Utc>>,
    pub(crate) end: Option<SessionEnd>,
    pub(crate) cfg: Cfg,
}

pub(crate) type Scenario = String;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum GroupKind {
    Deployed { name: String, by: Ucid },
    Troop { name: String, by: Ucid },
    Action { name: String, by: Ucid },
    Objective,
}

impl Default for GroupKind {
    fn default() -> Self {
        GroupKind::Objective
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Group {
    pub(crate) owner: Side,
    pub(crate) units: SmallVec<[EnId; 16]>,
    pub(crate) kind: GroupKind,
}

#[derive(Debug, Clone)]
struct StatCtxInner {
    sortie: Scenario,
    round: RoundId,
    seq: DateTime<Utc>
}

#[derive(Debug, Clone, Default)]
struct StatCtx(Option<StatCtxInner>);

impl StatCtx {
    #[allow(dead_code)]
    fn get(&self) -> Result<&StatCtxInner> {
        match &self.0 {
            Some(t) => Ok(t),
            None => bail!("expected to see NewSession before stats"),
        }
    }

    fn get_mut(&mut self) -> Result<&mut StatCtxInner> {
        match &mut self.0 {
            Some(t) => Ok(t),
            None => bail!("expected to see NewSession before stats"),
        }
    }
}

/// Per-DCS-server runtime. One per `--instances` entry (or one `default`).
pub(crate) struct InstanceState {
    pub(crate) cfg: Arc<InstanceCfg>,
    pub(crate) id: InstanceId,
    #[allow(dead_code)]
    subscriber: Option<Subscriber>,
    base: Option<NetidxPath>,
    stats_dir: Option<PathBuf>,
    stats_jsonl: Option<PathBuf>,
    current_sortie: StdMutex<Option<Scenario>>,
    latest_weather: RwLock<Option<WeatherSnapshot>>,
    engine_log_tx: broadcast::Sender<std::string::String>,
    engine_log_history: StdMutex<VecDeque<std::string::String>>,
    engine_error_history: StdMutex<VecDeque<std::string::String>>,
    jsonl_reset: AtomicBool,
    pub(crate) health_cache:
        StdMutex<Option<(std::time::Instant, bool, Option<std::string::String>)>>,
}

impl InstanceState {
    fn new(cfg: Arc<InstanceCfg>, subscriber: Option<Subscriber>) -> Self {
        let id: InstanceId = Arc::from(cfg.id.as_str());
        Self {
            id,
            subscriber,
            base: cfg.base.clone(),
            stats_dir: cfg.stats_dir.clone(),
            stats_jsonl: cfg.stats_jsonl.clone(),
            current_sortie: StdMutex::new(None),
            latest_weather: RwLock::new(None),
            engine_log_tx: broadcast::channel(1024).0,
            engine_log_history: StdMutex::new(VecDeque::new()),
            engine_error_history: StdMutex::new(VecDeque::new()),
            jsonl_reset: AtomicBool::new(false),
            health_cache: StdMutex::new(None),
            cfg,
        }
    }

    pub(crate) fn live_sortie_public(&self) -> Option<std::string::String> {
        self.current_sortie
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.to_string())
    }
}

#[derive(Clone)]
pub(crate) struct StatsDbInner {
    instances: Registry,
    states: Arc<HashMap<InstanceId, Arc<InstanceState>>>,
    #[allow(dead_code)]
    include: Option<Regex>,
    #[allow(dead_code)]
    exclude: Option<Regex>,
    db: Db,
    pilots: Pilots,
    seq: Tree<(Scenario, RoundId), DateTime<Utc>>,
    round: Tree<(Scenario, RoundId), Round>,
    session: Tree<(RoundId, DateTime<Utc>), Session>,
    kills: Tree<(EnId, RoundId, KillId), Dead>,
    shared_kills: Tree<KillId, SmallVec<[EnId; 2]>>,
    /// (round, victim, death time millis) — skip redelivered Stat::Kill
    kill_seen: Tree<(RoundId, EnId, i64), KillId>,
    sortie_seen: Tree<(RoundId, Ucid, i64), SortieId>,
    deploy_seen: Tree<(RoundId, GroupId), DeployId>,
    units: Tree<(RoundId, EnId), Unit>,
    groups: Tree<(RoundId, GroupId), Group>,
    detected: Tree<(RoundId, EnId), BitFlags<DetectionSource, u8>>,
    objectives: Tree<(RoundId, ObjectiveId), Objective>,
    equipment: Tree<(RoundId, ObjectiveId, String), u32>,
    liquids: Tree<(RoundId, ObjectiveId, LiquidType), u32>,
    /// Round → instance id. Untagged rounds belong to the registry default.
    round_instance: Tree<RoundId, std::string::String>,
    auth_sessions: Tree<Uuid, SessionData>,
    auth_states: Tree<Uuid, OAuthState>,
    trail_points: Tree<(RoundId, std::string::String, i64), (f64, f64, f64, f64)>,
    objective_captures: Tree<(RoundId, ObjectiveId), u32>,
    captures: Tree<(RoundId, CaptureId), CaptureRecord>,
    deploys: Tree<(Ucid, RoundId, DeployId), DeployRecord>,
    static_kills: Tree<(Ucid, RoundId, KillId), StaticKillRecord>,
    static_kill_seen: Tree<(RoundId, UnitId, i64), KillId>,
    aircraft_sorties: Tree<(RoundId, std::string::String), (u32, f32)>,
    pilot_last_activity: Tree<(Ucid, RoundId), DateTime<Utc>>,
    admin_bans: Tree<Ucid, BanRecord>,
    wiki_pages: Tree<std::string::String, WikiPage>,
    wiki_images: Tree<Uuid, WikiImage>,
    /// Per-instance archive replay cursor (instance id → last batch ts).
    replay_cursor: Tree<std::string::String, DateTime<Utc>>,
    legacy_replay_cursor: Tree<u8, DateTime<Utc>>,
    /// Per-instance JSONL byte offset (instance id → offset).
    jsonl_cursor: Tree<std::string::String, u64>,
    legacy_jsonl_cursor: Tree<u8, u64>,
    /// Sealed segment keys, prefixed `{instance_id}:{stem}.jsonl`.
    jsonl_sealed: Tree<std::string::String, u8>,
    rebuild_status: Arc<StdMutex<RebuildStatusInner>>,
}

#[derive(Debug, Clone)]
pub(crate) struct RebuildStatusInner {
    pub phase: std::string::String,
    pub active: bool,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl Default for RebuildStatusInner {
    fn default() -> Self {
        Self {
            phase: "idle".into(),
            active: false,
            started_at: None,
            finished_at: None,
        }
    }
}

const ENGINE_LOG_HISTORY_CAP: usize = 500;
const ENGINE_ERROR_HISTORY_CAP: usize = 200;

/// Matches the `[ERROR]`/`[WARN]`/`[WARNING]` level tags bflib's engine log
/// lines carry -- mirrors ENGINE_LOG_LEVEL_RE in the fowlengine Discord plugin
/// so the dashboard's error feed and the Discord alert relay agree on what
/// counts as noteworthy.
fn is_engine_error_line(line: &str) -> bool {
    let upper = line.to_ascii_uppercase();
    upper.contains("[ERROR]") || upper.contains("[WARN]") || upper.contains("[WARNING]")
}

pub(crate) struct StatsDb(Arc<StatsDbInner>);

impl Clone for StatsDb {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

/// Copy a file that may be locked by another process (e.g., DCS holding an exclusive lock).
/// On Windows, uses CreateFileW with FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE.
fn copy_locked_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::io::FromRawHandle;
        use std::os::windows::ffi::OsStrExt;
        extern "system" {
            fn CreateFileW(
                lpFileName: *const u16,
                dwDesiredAccess: u32,
                dwShareMode: u32,
                lpSecurityAttributes: *mut u8,
                dwCreationDisposition: u32,
                dwFlagsAndAttributes: u32,
                hTemplateFile: *mut u8,
            ) -> isize;
        }
        const GENERIC_READ: u32 = 0x80000000;
        const FILE_SHARE_READ: u32 = 1;
        const FILE_SHARE_WRITE: u32 = 2;
        const FILE_SHARE_DELETE: u32 = 4;
        const OPEN_EXISTING: u32 = 3;
        const INVALID_HANDLE_VALUE: isize = -1;

        let wide_path: Vec<u16> = src.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null_mut(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let mut src_file = unsafe { std::fs::File::from_raw_handle(handle as *mut std::ffi::c_void) };
        let mut buf = Vec::new();
        src_file.read_to_end(&mut buf)?;
        let mut dst_file = std::fs::File::create(dst)?;
        dst_file.write_all(&buf)?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::copy(src, dst)?;
        Ok(())
    }
}

fn stat_variant_name(s: &Stat) -> &'static str {
    match s {
        Stat::NewRound { .. } => "NewRound",
        Stat::RoundEnd { .. } => "RoundEnd",
        Stat::SessionStart { .. } => "SessionStart",
        Stat::SessionEnd { .. } => "SessionEnd",
        Stat::Objective { .. } => "Objective",
        Stat::ObjectiveDestroyed { .. } => "ObjectiveDestroyed",
        Stat::ObjectiveHealth { .. } => "ObjectiveHealth",
        Stat::ObjectiveSupply { .. } => "ObjectiveSupply",
        Stat::Capture { .. } => "Capture",
        Stat::Repair { .. } => "Repair",
        Stat::SupplyTransfer { .. } => "SupplyTransfer",
        Stat::DynamicCargoDelivery { .. } => "DynamicCargoDelivery",
        Stat::CsarRescue { .. } => "CsarRescue",
        Stat::Kill(_) => "Kill",
        Stat::StaticKill { .. } => "StaticKill",
        Stat::Unit { .. } => "Unit",
        Stat::Position { .. } => "Position",
        Stat::Detected { .. } => "Detected",
        Stat::EquipmentInventory { .. } => "EquipmentInventory",
        Stat::LiquidInventory { .. } => "LiquidInventory",
        Stat::Action { .. } => "Action",
        Stat::DeployTroop { .. } => "DeployTroop",
        Stat::DeployGroup { .. } => "DeployGroup",
        Stat::DeployFarp { .. } => "DeployFarp",
        Stat::Register { .. } => "Register",
        Stat::Sideswitch { .. } => "Sideswitch",
        Stat::Connect { .. } => "Connect",
        Stat::Disconnect { .. } => "Disconnect",
        Stat::Slot { .. } => "Slot",
        Stat::Deslot { .. } => "Deslot",
        Stat::GroupDeleted { .. } => "GroupDeleted",
        Stat::Takeoff { .. } => "Takeoff",
        Stat::Land { .. } => "Land",
        Stat::Life { .. } => "Life",
        Stat::Points { .. } => "Points",
        Stat::PointsTransfer { .. } => "PointsTransfer",
        Stat::PointsTransferToObjective { .. } => "PointsTransferToObjective",
        Stat::Bind { .. } => "Bind",
        Stat::ConvoyDestroyed { .. } => "ConvoyDestroyed",
        Stat::CampaignEvent { .. } => "CampaignEvent",
        Stat::PilotXp { .. } => "PilotXp",
        Stat::AirRouteDelivered { .. } => "AirRouteDelivered",
        Stat::AirRouteDestroyed { .. } => "AirRouteDestroyed",
        Stat::SeaRouteDelivered { .. } => "SeaRouteDelivered",
        Stat::SeaRouteDestroyed { .. } => "SeaRouteDestroyed",
        Stat::Weather { .. } => "Weather",
        Stat::GciPicture(_) => "GciPicture",
    }
}

impl Deref for StatsDb {
    type Target = StatsDbInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}


#[allow(dead_code)]
fn txn_err(e: TransactionError<anyhow::Error>) -> anyhow::Error {
    match e {
        TransactionError::Abort(e) => e,
        TransactionError::Storage(e) => e.into(),
    }
}

impl StatsDb {
    /// Open the DB and start one ingest + engine-log pipeline per instance.
    pub(crate) fn new<P: AsRef<Path>>(
        subscribers: &HashMap<Option<PathBuf>, Subscriber>,
        db: P,
        instances: Registry,
        include: Option<Regex>,
        exclude: Option<Regex>,
    ) -> Result<Self> {
        let db = sled::open(db.as_ref())?;
        let states: HashMap<InstanceId, Arc<InstanceState>> = instances
            .all()
            .iter()
            .map(|cfg| {
                let st = Arc::new(InstanceState::new(
                    cfg.clone(),
                    cfg.base
                        .as_ref()
                        .and_then(|_| subscribers.get(&cfg.netidx_config).cloned()),
                ));
                (st.id.clone(), st)
            })
            .collect();
        for cfg in instances.all() {
            info!(
                "instance {:?} ({}): base={} sortie={} jsonl={} archive={} export_port={}",
                cfg.id,
                cfg.label(),
                cfg.base
                    .as_ref()
                    .map(|b| format!("{b}"))
                    .unwrap_or_else(|| "-".into()),
                cfg.sortie.clone().unwrap_or_else(|| "auto".into()),
                cfg.stats_jsonl
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "-".into()),
                cfg.stats_dir
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "-".into()),
                cfg.export_port
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "-".into()),
            );
        }
        let t = Self(Arc::new(StatsDbInner {
            instances: instances.clone(),
            states: Arc::new(states),
            include,
            exclude,
            db: db.clone(),
            pilots: Pilots::new(&db)?,
            seq: Tree::open(&db, "seq")?,
            round: Tree::open(&db, "round")?,
            session: Tree::open(&db, "session")?,
            kills: Tree::open(&db, "kills")?,
            shared_kills: Tree::open(&db, "shared_kills")?,
            kill_seen: Tree::open(&db, "kill_seen")?,
            sortie_seen: Tree::open(&db, "sortie_seen")?,
            deploy_seen: Tree::open(&db, "deploy_seen")?,
            units: Tree::open(&db, "units")?,
            groups: Tree::open(&db, "groups")?,
            detected: Tree::open(&db, "detected")?,
            objectives: Tree::open(&db, "objectives")?,
            equipment: Tree::open(&db, "equipment")?,
            liquids: Tree::open(&db, "liquids")?,
            round_instance: Tree::open(&db, "round_instance")?,
            auth_sessions: Tree::open(&db, "auth_sessions")?,
            auth_states: Tree::open(&db, "auth_states")?,
            trail_points: Tree::open(&db, "trail_points")?,
            objective_captures: Tree::open(&db, "objective_captures")?,
            captures: Tree::open(&db, "captures")?,
            deploys: Tree::open(&db, "deploys")?,
            static_kills: Tree::open(&db, "static_kills")?,
            static_kill_seen: Tree::open(&db, "static_kill_seen")?,
            aircraft_sorties: Tree::open(&db, "aircraft_sorties")?,
            pilot_last_activity: Tree::open(&db, "pilot_last_activity")?,
            admin_bans: Tree::open(&db, "admin_bans")?,
            wiki_pages: Tree::open(&db, "wiki_pages")?,
            wiki_images: Tree::open(&db, "wiki_images")?,
            replay_cursor: Tree::open(&db, "replay_cursor_v2")?,
            legacy_replay_cursor: Tree::open(&db, "replay_cursor")?,
            jsonl_cursor: Tree::open(&db, "jsonl_cursor_v2")?,
            legacy_jsonl_cursor: Tree::open(&db, "jsonl_cursor")?,
            jsonl_sealed: Tree::open(&db, "jsonl_sealed")?,
            rebuild_status: Arc::new(StdMutex::new(RebuildStatusInner::default())),
        }));
        t.migrate_legacy_cursors()?;
        t.migrate_legacy_sealed_keys()?;
        t.seed_wiki_if_empty()?;
        t.seed_wiki_images_if_empty()?;
        t.reconcile_flight_hours_from_sorties_once()?;
        t.reconcile_a2a_g2a_once()?;
        t.reconcile_a2g_g2g_once()?;
        t.close_stale_open_rounds()?;
        for st in t.0.states.values() {
            let _t = t.clone();
            let _st = st.clone();
            task::spawn(async move {
                if let Err(e) = _t.background_loop(_st.clone()).await {
                    error!("[{}] background task failed {e:?}", _st.id)
                }
            });
            if st.base.is_some() {
                let _t = t.clone();
                let _st = st.clone();
                task::spawn(async move {
                    if let Err(e) = _t.engine_log_loop(_st.clone()).await {
                        error!("[{}] engine log subscription failed {e:?}", _st.id)
                    }
                });
            }
        }
        if t.0.states.values().all(|s| s.base.is_none()) {
            info!("running in offline mode (no instance has a netidx base)");
        }
        Ok(t)
    }

    /// Offline open (no netidx). Single default instance from paths.
    pub(crate) fn new_offline<P: AsRef<Path>>(
        db: P,
        stats_dir: Option<PathBuf>,
        stats_jsonl: Option<PathBuf>,
    ) -> Result<Self> {
        let reg = Registry::single(InstanceCfg {
            id: DEFAULT_INSTANCE.to_string(),
            label: None,
            base: None,
            netidx_config: None,
            sortie: None,
            stats_jsonl,
            stats_dir,
            export_port: None,
            engine_config: None,
            srs_url: None,
            dcs_server_name: None,
            server_ip: None,
            public: true,
        });
        Self::new(&HashMap::new(), db, reg, None, None)
    }

    fn migrate_legacy_cursors(&self) -> Result<()> {
        let default = self.0.instances.default_id().to_string();
        if let Some(pos) = self.0.legacy_jsonl_cursor.get(&0u8)? {
            if self.0.jsonl_cursor.get(&default)?.is_none() {
                info!("migrating legacy JSONL cursor (offset {pos}) to instance {default:?}");
                self.0.jsonl_cursor.insert(&default, &pos)?;
            }
            self.0.legacy_jsonl_cursor.remove(&0u8)?;
        }
        if let Some(ts) = self.0.legacy_replay_cursor.get(&0u8)? {
            if self.0.replay_cursor.get(&default)?.is_none() {
                info!("migrating legacy archive replay cursor ({ts}) to instance {default:?}");
                self.0.replay_cursor.insert(&default, &ts)?;
            }
            self.0.legacy_replay_cursor.remove(&0u8)?;
        }
        Ok(())
    }

    fn migrate_legacy_sealed_keys(&self) -> Result<()> {
        let default = self.0.instances.default_id().to_string();
        let prefix = format!("{default}:");
        let keys: Vec<std::string::String> = self
            .jsonl_sealed
            .iter()
            .keys()
            .filter_map(|k| k.ok())
            .filter(|k| !k.contains(':'))
            .collect();
        for key in &keys {
            let new_key = format!("{prefix}{key}");
            if self.jsonl_sealed.get(&new_key)?.is_none() {
                if let Some(v) = self.jsonl_sealed.get(key)? {
                    self.jsonl_sealed.insert(&new_key, &v)?;
                }
            }
            self.jsonl_sealed.remove(key)?;
        }
        if !keys.is_empty() {
            info!(
                "migrated {} sealed JSONL key(s) under instance {default:?}",
                keys.len()
            );
        }
        Ok(())
    }

    fn close_stale_open_rounds(&self) -> Result<()> {
        let mut open_by_instance: HashMap<InstanceId, Vec<(Scenario, RoundId, Round)>> =
            HashMap::new();
        for r in self.round.iter() {
            let ((s, rid), rd) = r?;
            if rd.end.is_some() {
                continue;
            }
            let inst = self.round_instance_of(rid);
            open_by_instance.entry(inst).or_default().push((s, rid, rd));
        }
        for (inst, mut open) in open_by_instance {
            open.sort_by(|a, b| a.2.start.cmp(&b.2.start).then(a.1.cmp(&b.1)));
            let keep = open.pop();
            for (s, rid, mut rd) in open {
                warn!("[{inst}] closing stale open round {rid:?} (scenario {s:?})");
                rd.end = Some(chrono::Utc::now());
                let _ = self.round.insert(&(s, rid), &rd)?;
            }
            if let (Some((sortie, _, _)), Some(st)) = (keep, self.0.states.get(&inst)) {
                info!("[{inst}] resuming with active round sortie={sortie:?}");
                *st.current_sortie.lock().unwrap() = Some(sortie);
            }
        }
        Ok(())
    }

    pub(crate) fn instances(&self) -> &Registry {
        &self.0.instances
    }

    pub(crate) fn state(&self, id: &InstanceId) -> Arc<InstanceState> {
        self.0
            .states
            .get(id)
            .or_else(|| self.0.states.get(self.0.instances.default_id()))
            .expect("registry always has its default instance")
            .clone()
    }

    pub(crate) fn default_state(&self) -> Arc<InstanceState> {
        self.state(self.0.instances.default_id())
    }

    pub(crate) fn resolve_state(&self, requested: Option<&str>) -> Result<Arc<InstanceState>> {
        let cfg = self.0.instances.resolve(requested)?;
        let id: InstanceId = Arc::from(cfg.id.as_str());
        Ok(self.state(&id))
    }

    pub(crate) fn round_instance_of(&self, round: RoundId) -> InstanceId {
        match self.round_instance.get(&round) {
            Ok(Some(id)) => Arc::from(id.as_str()),
            _ => self.0.instances.default_id().clone(),
        }
    }

    pub(crate) fn rounds_of(&self, inst: &InstanceId) -> Result<HashSet<RoundId>> {
        let mut out = HashSet::new();
        for r in self.round.iter() {
            let ((_, rid), _) = r?;
            if &self.round_instance_of(rid) == inst {
                out.insert(rid);
            }
        }
        Ok(out)
    }

    pub(crate) fn latest_rounds_for(
        &self,
        inst: &InstanceId,
    ) -> Result<Vec<(Scenario, RoundId, Round)>> {
        Ok(self
            .latest_rounds()?
            .into_iter()
            .filter(|(_, rid, _)| &self.round_instance_of(*rid) == inst)
            .collect())
    }

    /// Round for a dashboard request scoped to `inst`.
    /// Explicit id must belong to that instance; otherwise `None` (empty view).
    /// With no id: open round for `inst`, else latest closed for `inst`.
    pub(crate) fn resolve_round_id(
        &self,
        inst: &InstanceId,
        round_id: Option<u64>,
    ) -> Result<Option<RoundId>> {
        if let Some(id) = round_id {
            let rid = RoundId(id);
            if &self.round_instance_of(rid) != inst {
                return Ok(None);
            }
            return Ok(Some(rid));
        }
        let rounds = self.latest_rounds_for(inst)?;
        Ok(rounds
            .iter()
            .find(|(_, _, r)| r.end.is_none())
            .or_else(|| rounds.first())
            .map(|(_, rid, _)| *rid))
    }

    async fn engine_log_loop(self, inst: Arc<InstanceState>) -> Result<()> {
        use futures::{channel::mpsc, StreamExt};
        use netidx::publisher::Value;
        use netidx::subscriber::{Event, UpdatesFlags};

        let (subscriber, base) = match (&inst.subscriber, &inst.base) {
            (Some(s), Some(b)) => (s.clone(), b.clone()),
            _ => return Ok(()),
        };
        loop {
            let sortie = loop {
                if let Some(s) = inst.current_sortie.lock().unwrap().clone() {
                    break s;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            };
            let dval = subscriber.subscribe(base.append(&sortie).append("log"));
            let (tx, mut rx) = mpsc::channel(10);
            dval.updates(UpdatesFlags::empty(), tx);
            let mut seen_len = 0usize;
            while let Some(batch) = rx.next().await {
                if inst.current_sortie.lock().unwrap().as_ref() != Some(&sortie) {
                    break;
                }
                for (_id, ev) in batch.iter() {
                    let Event::Update(Value::String(chars)) = ev else {
                        continue;
                    };
                    let full: &str = chars.as_ref();
                    let start = if full.len() >= seen_len { seen_len } else { 0 };
                    let new_part = &full[start..];
                    seen_len = full.len();
                    for line in new_part.lines().filter(|l| !l.is_empty()) {
                        let line = std::string::String::from(line);
                        let mut hist = inst.engine_log_history.lock().unwrap();
                        if hist.len() >= ENGINE_LOG_HISTORY_CAP {
                            hist.pop_front();
                        }
                        hist.push_back(line.clone());
                        drop(hist);
                        if is_engine_error_line(&line) {
                            let mut errs = inst.engine_error_history.lock().unwrap();
                            if errs.len() >= ENGINE_ERROR_HISTORY_CAP {
                                errs.pop_front();
                            }
                            errs.push_back(line.clone());
                        }
                        let _ = inst.engine_log_tx.send(line);
                    }
                }
            }
            if inst.current_sortie.lock().unwrap().as_ref() == Some(&sortie) {
                return Ok(());
            }
        }
    }

    pub(crate) fn engine_log_subscribe(
        &self,
        inst: &InstanceState,
    ) -> (
        broadcast::Receiver<std::string::String>,
        Vec<std::string::String>,
    ) {
        let rx = inst.engine_log_tx.subscribe();
        let hist = inst.engine_log_history.lock().unwrap().iter().cloned().collect();
        (rx, hist)
    }

    pub(crate) fn engine_error_snapshot(&self, inst: &InstanceState) -> Vec<std::string::String> {
        inst.engine_error_history
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect()
    }

    pub(crate) async fn call_engine_rpc(
        &self,
        inst: &InstanceState,
        proc_name: &str,
        args: Vec<(&str, netidx::publisher::Value)>,
    ) -> Result<netidx::publisher::Value> {
        use netidx_protocols::rpc::client::Proc;
        let (subscriber, base) = match (&inst.subscriber, &inst.base) {
            (Some(s), Some(b)) => (s, b),
            _ => bail!("netidx is disabled (bfdb started without --base)"),
        };
        let sortie = inst
            .current_sortie
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("no active sortie yet (mission hasn't reported in)"))?;
        let path = base.append(&sortie).append("api").append(proc_name);
        let proc = Proc::new(subscriber, path)?;
        proc.call(args).await
    }

    async fn background_loop(self, inst: Arc<InstanceState>) -> Result<()> {
        if let Some(jsonl_path) = inst.stats_jsonl.clone() {
            return self.jsonl_loop(inst, jsonl_path).await;
        }

        use arcstr::ArcStr;
        use netidx::subscriber::Event;
        use netidx_archive::logfile::BatchItem;
        use tokio::time;

        let stats_dir = match &inst.stats_dir {
            Some(d) => d.clone(),
            None => return Ok(()),
        };
        let inst_key = inst.id.to_string();

        let shard: ArcStr = "0".into();
        let mut archive_cfg = ArchiveFileCfg::default();
        archive_cfg.archive_directory = stats_dir;
        archive_cfg.archive_cmds = None;
        let archive_cfg = Arc::new(netidx_archive::config::Config::try_from(archive_cfg)?);

        let head_path = archive_cfg.archive_directory().join(shard.as_str()).join("current");
        let head_copy_path = archive_cfg
            .archive_directory()
            .join(shard.as_str())
            .join("current_copy");
        let resume_from = self.0.replay_cursor.get(&inst_key)?;
        if let Some(ts) = resume_from {
            info!("[{}] resuming stats archive replay after {ts}", inst.id);
        }

        let mut ctx = StatCtx::default();
        let mut timer = time::interval(Duration::from_secs(5));
        let mut total_batches = 0u64;
        let mut total_items = 0u64;
        let mut last_seen_ts: Option<DateTime<Utc>> = resume_from;

        loop {
            timer.tick().await;
            let new_index = task::block_in_place(|| ArchiveIndex::new(&archive_cfg, &shard)).ok();
            let new_head = task::block_in_place(|| {
                match copy_locked_file(&head_path, &head_copy_path) {
                    Ok(()) => netidx_archive::logfile::ArchiveReader::open(&head_copy_path).ok(),
                    Err(_) => netidx_archive::logfile::ArchiveReader::open(&head_path).ok(),
                }
            });
            let Some(new_index) = new_index else {
                continue;
            };
            let start_bound = match last_seen_ts {
                Some(ts) => Bound::Excluded(ts),
                None => Bound::Unbounded,
            };
            let mut reader = ArchiveCollectionReader::new(
                new_index,
                archive_cfg.clone(),
                shard.clone(),
                new_head,
                start_bound,
                Bound::Unbounded,
            );
            const MAX_BATCHES_PER_TICK: u32 = 2_000;
            let mut batches_this_tick = 0u32;
            loop {
                if batches_this_tick >= MAX_BATCHES_PER_TICK {
                    break;
                }
                batches_this_tick += 1;
                let batch = task::block_in_place(|| reader.read_next(None));
                match batch {
                    Err(e) => {
                        if e.to_string().contains("no data source available") {
                            debug!("[{}] archive read: nothing available this tick ({e})", inst.id);
                        } else {
                            error!("[{}] archive read error: {e:?}", inst.id);
                        }
                        break;
                    }
                    Ok(None) => break,
                    Ok(Some((ts, items))) => {
                        total_batches += 1;
                        total_items += items.len() as u64;
                        let log_every = if total_batches <= 100_000 { 100 } else { 50_000 };
                        if total_batches <= 5 || total_batches % log_every == 0 {
                            info!(
                                "[{}] batch #{total_batches} ts={ts} items={} (total_items={total_items})",
                                inst.id,
                                items.len()
                            );
                        }
                        last_seen_ts = Some(ts);
                        reader.position_mut().set_current(ts);
                        for BatchItem(path_id, ev) in items.iter() {
                            if let Event::Update(v) = ev {
                                let s = match v {
                                    netidx::publisher::Value::String(s) => s.clone(),
                                    other => {
                                        if total_batches <= 5 {
                                            info!(
                                                "[{}]   non-string value type for path_id={path_id:?}: {other:?}",
                                                inst.id
                                            );
                                        }
                                        continue;
                                    }
                                };
                                let st: Stat = match serde_json::from_str::<Stat>(&s) {
                                    Ok(s) => s,
                                    Err(e) => {
                                        let preview: std::string::String =
                                            s.chars().take(200).collect();
                                        error!(
                                            "[{}] failed to deserialize stat: {e}, raw: {preview}",
                                            inst.id
                                        );
                                        continue;
                                    }
                                };
                                if let Err(e) =
                                    task::block_in_place(|| self.add_stat(&inst, &mut ctx, ts, st))
                                {
                                    error!("[{}] failed to add stat {e:?}", inst.id)
                                }
                            }
                        }
                    }
                }
            }
            if let Some(ts) = last_seen_ts {
                if let Err(e) = self.0.replay_cursor.insert(&inst_key, &ts) {
                    error!("[{}] failed to save replay cursor: {e:?}", inst.id);
                }
            }
        }
    }

    async fn jsonl_loop(self, inst: Arc<InstanceState>, jsonl_path: PathBuf) -> Result<()> {
        use tokio::time;

        let inst_key = inst.id.to_string();
        let mut ctx = StatCtx::default();
        let mut timer = time::interval(Duration::from_secs(5));
        let mut last_pos: u64 = self.0.jsonl_cursor.get(&inst_key)?.unwrap_or(0);

        if last_pos > 0 {
            if let Ok(rounds) = self.latest_rounds_for(&inst.id) {
                if let Some((sortie, round, _)) =
                    rounds.into_iter().find(|(_, _, r)| r.end.is_none())
                {
                    if let Ok(Some(seq)) = self.seq.get(&(sortie.clone(), round)) {
                        ctx.0 = Some(StatCtxInner {
                            sortie,
                            round,
                            seq,
                        });
                    }
                }
            }
        }

        info!(
            "[{}] starting JSONL reader from {jsonl_path:?} at offset {last_pos}",
            inst.id
        );

        loop {
            timer.tick().await;

            if inst.jsonl_reset.swap(false, Ordering::SeqCst) {
                warn!(
                    "[{}] jsonl rebuild requested -- wiping derived stats and re-ingesting {jsonl_path:?} from offset 0",
                    inst.id
                );
                {
                    let mut st = self.0.rebuild_status.lock().unwrap();
                    st.phase = "rebuilding".into();
                    st.active = true;
                    if st.started_at.is_none() {
                        st.started_at = Some(Utc::now());
                    }
                }
                if let Err(e) = task::block_in_place(|| self.wipe_stats_derived_trees()) {
                    error!(
                        "[{}] jsonl rebuild: wipe failed ({e:?}) -- aborting, keeping current data",
                        inst.id
                    );
                    let mut st = self.0.rebuild_status.lock().unwrap();
                    st.phase = "error".into();
                    st.active = false;
                    st.finished_at = Some(Utc::now());
                } else {
                    let _ = self.0.jsonl_cursor.insert(&inst_key, &0u64);
                    *inst.current_sortie.lock().unwrap() = None;
                    last_pos = 0;
                    ctx = StatCtx::default();
                }
            }

            let aliases_path =
                crate::ucid_alias::UcidAliasTable::aliases_path_for_jsonl(&jsonl_path);
            let aliases = match crate::ucid_alias::UcidAliasTable::load(&aliases_path) {
                Ok(t) => t,
                Err(e) => {
                    error!("[{}] failed to load UCID aliases: {e:?}", inst.id);
                    crate::ucid_alias::UcidAliasTable::default()
                }
            };

            if let Err(e) = task::block_in_place(|| {
                self.ingest_pending_sealed_segments(&inst, &jsonl_path, &aliases, &mut ctx)
            }) {
                error!("[{}] sealed JSONL ingest error: {e:?}", inst.id);
            }

            let read_result =
                task::block_in_place(|| read_live_jsonl_stats(&jsonl_path, last_pos, &aliases));
            match read_result {
                Ok((pos, stats, unparsed, first_unparsed, undecodable, first_undecodable)) => {
                    if unparsed > 0 {
                        error!(
                            "[{}] skipped {unparsed} unparsable JSONL line(s); first: {}",
                            inst.id,
                            first_unparsed.as_deref().unwrap_or("?")
                        );
                    }
                    if undecodable > 0 {
                        error!(
                            "[{}] skipped {undecodable} undecodable JSONL stat(s); first: {}",
                            inst.id,
                            first_undecodable.as_deref().unwrap_or("?")
                        );
                    }
                    if !stats.is_empty() {
                        let count = stats.len();
                        for (ts, st) in stats {
                            if let Err(e) =
                                task::block_in_place(|| self.add_stat(&inst, &mut ctx, ts, st))
                            {
                                warn!("[{}] failed to add stat from JSONL: {e:?}", inst.id);
                            }
                        }
                        info!(
                            "[{}] processed {count} stats from JSONL (pos {last_pos} -> {pos})",
                            inst.id
                        );
                    }
                    last_pos = pos;
                    if let Err(e) = self.0.jsonl_cursor.insert(&inst_key, &pos) {
                        error!("[{}] failed to save JSONL cursor: {e:?}", inst.id);
                    }
                    {
                        let mut st = self.0.rebuild_status.lock().unwrap();
                        if st.active && st.phase == "rebuilding" {
                            let live_len =
                                std::fs::metadata(&jsonl_path).map(|m| m.len()).unwrap_or(0);
                            if pos >= live_len {
                                st.phase = "complete".into();
                                st.active = false;
                                st.finished_at = Some(Utc::now());
                                info!("[{}] jsonl rebuild complete at offset {pos}", inst.id);
                            }
                        }
                    }
                }
                Err(e) => error!("[{}] JSONL read error: {e:?}", inst.id),
            }
        }
    }

    fn ingest_pending_sealed_segments(
        &self,
        inst: &InstanceState,
        live_path: &Path,
        aliases: &crate::ucid_alias::UcidAliasTable,
        ctx: &mut StatCtx,
    ) -> Result<()> {
        let prefix = format!("{}:", inst.id);
        for (key, path) in list_sealed_stats_segments(live_path)? {
            let sealed_key = format!("{prefix}{key}");
            if self.jsonl_sealed.get(&sealed_key)?.is_some() {
                continue;
            }
            info!("[{}] ingesting sealed stats segment {path:?}", inst.id);
            let (stats, unparsed, first_unparsed, undecodable, first_undecodable) =
                read_stats_lines_from_path(&path, aliases)?;
            if unparsed > 0 {
                error!(
                    "[{}] sealed {key}: skipped {unparsed} unparsable line(s); first: {}",
                    inst.id,
                    first_unparsed.as_deref().unwrap_or("?")
                );
            }
            if undecodable > 0 {
                error!(
                    "[{}] sealed {key}: skipped {undecodable} undecodable stat(s); first: {}",
                    inst.id,
                    first_undecodable.as_deref().unwrap_or("?")
                );
            }
            let count = stats.len();
            for (ts, st) in stats {
                if let Err(e) = self.add_stat(inst, ctx, ts, st) {
                    warn!("[{}] failed to add stat from sealed segment: {e:?}", inst.id);
                }
            }
            self.jsonl_sealed.insert(&sealed_key, &1u8)?;
            info!("[{}] sealed segment {key}: ingested {count} stats", inst.id);
        }
        Ok(())
    }
}


/// Canonical sealed key is the `.jsonl` name so plain and `.zst` share one cursor entry.
fn sealed_stats_key(file_name: &str) -> Option<std::string::String> {
    if !file_name.starts_with("stats-") {
        return None;
    }
    if let Some(stem) = file_name.strip_suffix(".jsonl.zst") {
        return Some(format!("{stem}.jsonl"));
    }
    if file_name.ends_with(".jsonl") {
        return Some(file_name.to_string());
    }
    None
}

fn list_sealed_stats_segments(live_path: &Path) -> Result<Vec<(std::string::String, PathBuf)>> {
    let Some(dir) = live_path.parent() else {
        return Ok(Vec::new());
    };
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    // Prefer .zst over mid-rotate plain for the same stem.
    let mut by_key: std::collections::BTreeMap<std::string::String, PathBuf> =
        std::collections::BTreeMap::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(key) = sealed_stats_key(&name) else {
            continue;
        };
        let path = entry.path();
        let is_zst = name.ends_with(".zst");
        match by_key.get(&key) {
            Some(_) if !is_zst => {}
            _ => {
                by_key.insert(key, path);
            }
        }
    }
    Ok(by_key.into_iter().collect())
}

fn open_stats_line_reader(path: &Path) -> Result<Box<dyn std::io::BufRead>> {
    let file = std::fs::File::open(path).with_context(|| format!("open stats {path:?}"))?;
    let is_zst = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.ends_with(".zst"))
        .unwrap_or(false);
    if is_zst {
        let decoder = zstd::stream::read::Decoder::new(file)
            .with_context(|| format!("zstd decoder {path:?}"))?;
        Ok(Box::new(std::io::BufReader::new(decoder)))
    } else {
        Ok(Box::new(std::io::BufReader::new(file)))
    }
}

fn parse_stats_jsonl_line(
    trimmed: &str,
    aliases: &crate::ucid_alias::UcidAliasTable,
    stats: &mut Vec<(DateTime<Utc>, Stat)>,
    unparsed: &mut u64,
    first_unparsed: &mut Option<std::string::String>,
    undecodable: &mut u64,
    first_undecodable: &mut Option<std::string::String>,
) {
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(val) => {
            let ts_str = val.get("ts").and_then(|v| v.as_str()).unwrap_or("");
            let ts = ts_str
                .parse::<DateTime<Utc>>()
                .unwrap_or_else(|_| Utc::now());
            if let Some(stat_val) = val.get("stat") {
                let mut stat_val = stat_val.clone();
                migrate_legacy_setmission_in_stat_json(&mut stat_val);
                match serde_json::from_value::<Stat>(stat_val) {
                    Ok(mut st) => {
                        crate::ucid_alias::apply_to_stat(&mut st, aliases, ts);
                        stats.push((ts, st));
                    }
                    Err(e) => {
                        *undecodable += 1;
                        if first_undecodable.is_none() {
                            let preview: std::string::String =
                                trimmed.chars().take(120).collect();
                            *first_undecodable = Some(format!("{e}; raw: {preview}"));
                        }
                    }
                }
            }
        }
        Err(e) => {
            *unparsed += 1;
            if first_unparsed.is_none() {
                *first_unparsed = Some(e.to_string());
            }
        }
    }
}

fn read_stats_lines_from_path(
    path: &Path,
    aliases: &crate::ucid_alias::UcidAliasTable,
) -> Result<(
    Vec<(DateTime<Utc>, Stat)>,
    u64,
    Option<std::string::String>,
    u64,
    Option<std::string::String>,
)> {
    use std::io::BufRead;
    let mut reader = open_stats_line_reader(path)?;
    let mut line = std::string::String::new();
    let mut stats = Vec::new();
    let mut unparsed = 0u64;
    let mut first_unparsed: Option<std::string::String> = None;
    let mut undecodable = 0u64;
    let mut first_undecodable: Option<std::string::String> = None;
    while reader.read_line(&mut line)? > 0 {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            parse_stats_jsonl_line(
                trimmed,
                aliases,
                &mut stats,
                &mut unparsed,
                &mut first_unparsed,
                &mut undecodable,
                &mut first_undecodable,
            );
        }
        line.clear();
    }
    Ok((
        stats,
        unparsed,
        first_unparsed,
        undecodable,
        first_undecodable,
    ))
}

fn read_live_jsonl_stats(
    jsonl_path: &Path,
    last_pos: u64,
    aliases: &crate::ucid_alias::UcidAliasTable,
) -> Result<(
    u64,
    Vec<(DateTime<Utc>, Stat)>,
    u64,
    Option<std::string::String>,
    u64,
    Option<std::string::String>,
)> {
    use std::io::{BufRead, Seek};
    let file = match std::fs::File::open(jsonl_path) {
        Ok(f) => f,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                error!("failed to open JSONL file: {e:?}");
            }
            return Ok((last_pos, vec![], 0, None, 0, None));
        }
    };
    let metadata = file.metadata()?;
    let file_len = metadata.len();
    if file_len < last_pos {
        // Live file rotated: sealed ingest handles the old bytes; resume new file.
        warn!("JSONL file shrank ({file_len} < {last_pos}) — resetting live cursor to 0");
        return Ok((0, vec![], 0, None, 0, None));
    }
    if file_len <= last_pos {
        return Ok((last_pos, vec![], 0, None, 0, None));
    }
    let mut reader = std::io::BufReader::new(file);
    reader.seek(std::io::SeekFrom::Start(last_pos))?;
    let mut line = std::string::String::new();
    let mut new_pos = last_pos;
    let mut stats = Vec::new();
    let mut unparsed = 0u64;
    let mut first_unparsed: Option<std::string::String> = None;
    let mut undecodable = 0u64;
    let mut first_undecodable: Option<std::string::String> = None;
    while reader.read_line(&mut line)? > 0 {
        new_pos = reader.stream_position()?;
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            parse_stats_jsonl_line(
                trimmed,
                aliases,
                &mut stats,
                &mut unparsed,
                &mut first_unparsed,
                &mut undecodable,
                &mut first_undecodable,
            );
        }
        line.clear();
    }
    Ok((
        new_pos,
        stats,
        unparsed,
        first_unparsed,
        undecodable,
        first_undecodable,
    ))
}

impl StatsDb {
    fn new_round(
        &self,
        inst: &InstanceState,
        ctx: &mut StatCtx,
        start: DateTime<Utc>,
        sortie: String,
        seqnum: DateTime<Utc>,
    ) -> Result<()> {
        let id = RoundId::new(&self.db)?;
        let key = (sortie.clone(), id);
        let r = Round {
            start,
            end: None,
            winner: None,
        };
        info!(
            "[{}] new_round: inserting round id={id:?} sortie={sortie:?}",
            inst.id
        );
        self.seq.insert(&key, &seqnum)?;
        self.round.insert(&key, &r)?;
        self.round_instance.insert(&id, &inst.id.to_string())?;
        info!("new_round: round inserted successfully");
        *inst.current_sortie.lock().unwrap() = Some(sortie.clone());
        ctx.0 = Some(StatCtxInner {
            sortie,
            round: id,
            seq: seqnum,
        });
        Ok(())
    }

    fn round_end(
        &self,
        ctx: &mut StatCtx,
        time: DateTime<Utc>,
        winner: Option<Side>,
    ) -> Result<()> {
        let inner = ctx.get_mut()?;
        let key = (inner.sortie.clone(), inner.round);
        let mut round = self
            .round
            .get(&key)?
            .ok_or_else(|| anyhow!("round not found"))?;
        if self.round_is_roundend_blip(inner.round, round.start, time)? {
            info!(
                "RoundEnd: discarding restart blip {:?} (short, no sorties)",
                inner.round
            );
            let sortie = inner.sortie.clone();
            let rid = inner.round;
            ctx.0 = None;
            return self.delete_round_cascade(&sortie, rid);
        }
        round.end = Some(time);
        round.winner = winner;
        let _ = self.round.insert(&key, &round)?;
        ctx.0 = None;
        Ok(())
    }

    fn with_objective<F: FnMut(&mut Objective)>(
        &self,
        k: (RoundId, ObjectiveId),
        mut f: F,
    ) -> Result<()> {
        self.objectives
            .fetch_and_update(&k, |o| match o {
                None => None,
                Some(mut o) => {
                    f(&mut o);
                    Some(o)
                }
            })?
            .ok_or_else(|| anyhow!("objective {k:?} is missing"))?;
        Ok(())
    }

    fn with_group<F: FnMut(&mut Group)>(&self, k: (RoundId, GroupId), mut f: F) -> Result<()> {
        self.groups
            .fetch_and_update(&k, |g| match g {
                None => None,
                Some(mut g) => {
                    f(&mut g);
                    Some(g)
                }
            })?
            .ok_or_else(|| anyhow!("group {k:?} is missing"))?;
        Ok(())
    }

    fn with_unit<F: FnMut(&mut Unit)>(&self, k: (RoundId, EnId), mut f: F) -> Result<()> {
        self.units
            .fetch_and_update(&k, |g| match g {
                None => None,
                Some(mut u) => {
                    f(&mut u);
                    Some(u)
                }
            })?
            .ok_or_else(|| anyhow!("unit {k:?} is missing"))?;
        Ok(())
    }

    fn with_shared_kills<F: FnMut(&mut SmallVec<[EnId; 2]>)>(
        &self,
        k: KillId,
        mut f: F,
    ) -> Result<()> {
        self.shared_kills.update_and_fetch(&k, |sk| {
            let mut sk = sk.unwrap_or_default();
            f(&mut sk);
            Some(sk)
        })?;
        Ok(())
    }

    /// Close an open sortie at `end`, credit flight hours once. `crashed` marks
    /// death / mid-air deslot (Flight Log shows Lost, not Landed).
    fn finalize_sortie(
        &self,
        ucid: Ucid,
        round: RoundId,
        sid: SortieId,
        end: DateTime<Utc>,
        crashed: bool,
    ) -> Result<()> {
        let key = (ucid, round, sid);
        let Some(s) = self.pilots.sortie.get(&key)? else {
            return Ok(());
        };
        if s.land.is_some() {
            return Ok(());
        }
        let takeoff = s.takeoff;
        let vehicle = s.vehicle.clone();
        // Guard clock skew / bad event order — never stamp End before Takeoff.
        let end = if end < takeoff { takeoff } else { end };
        self.pilots.with_sortie(key, |s| {
            s.land = Some(end);
        })?;
        self.pilots.sortie_crashed.insert(&key, &crashed)?;
        let hours = ((end - takeoff).num_seconds().max(0) as f32) / 3600.0;
        if hours > 0.0 {
            let ac_key = (round, vehicle.to_string());
            let (cnt, prev_hrs) = self.aircraft_sorties.get(&ac_key)?.unwrap_or((0, 0.0));
            self.aircraft_sorties
                .insert(&ac_key, &(cnt, prev_hrs + hours))?;
            // Credit from Sortie.vehicle — not current slot (often cleared on
            // Deslot/kick/orphan Connect before finalize runs).
            self.pilots.with_pilot(ucid, |p| p.total.hours += hours)?;
            self.pilots
                .with_aggregates((ucid, vehicle, round), |a| a.hours += hours)?;
        }
        Ok(())
    }

    fn touch_pilot_activity(&self, ucid: Ucid, round: RoundId, time: DateTime<Utc>) -> Result<()> {
        self.pilot_last_activity
            .fetch_and_update(&(ucid, round), |prev| {
                let t = prev.map(|p| if time > p { time } else { p }).unwrap_or(time);
                Some(t)
            })?;
        Ok(())
    }

    /// Close the pilot's current open sortie (Lost / mid-air end).
    /// Only the active slot sortie — never sweep every historical `land: None`
    /// row in the round (that stamped death time onto old open legs and made
    /// End / Duration nonsense).
    fn finalize_open_sorties(
        &self,
        ucid: Ucid,
        round: RoundId,
        end: DateTime<Utc>,
        crashed: bool,
    ) -> Result<()> {
        let mut sid: Option<SortieId> = None;
        self.pilots.with_pilot_round_info(ucid, round, |ri| {
            if let Some(sl) = ri.slot.as_mut() {
                sid = sl.sortie.take();
            }
        })?;
        if sid.is_none() {
            // Slot already cleared (e.g. Kill after Deslot ordering); newest open only.
            let mut best: Option<(SortieId, DateTime<Utc>)> = None;
            for r in self.pilots.sortie.scan_prefix(&(ucid, round))? {
                let ((_, _, id), s) = r?;
                if s.land.is_none() {
                    let take = best.map(|(_, t)| t);
                    if take.map(|t| s.takeoff > t).unwrap_or(true) {
                        best = Some((id, s.takeoff));
                    }
                }
            }
            sid = best.map(|(id, _)| id);
        }
        if let Some(sid) = sid {
            self.finalize_sortie(ucid, round, sid, end, crashed)?;
        }
        Ok(())
    }

    /// Close every still-open Flight Log leg for this pilot in the round.
    /// Used when Slot arrives without Deslot (death → respawn) or Takeoff
    /// would orphan a prior `land: None` row.
    fn finalize_all_open_sorties(
        &self,
        ucid: Ucid,
        round: RoundId,
        end: DateTime<Utc>,
        crashed: bool,
    ) -> Result<()> {
        let mut open: Vec<SortieId> = Vec::new();
        for r in self.pilots.sortie.scan_prefix(&(ucid, round))? {
            let ((_, _, id), s) = r?;
            if s.land.is_none() {
                open.push(id);
            }
        }
        if open.is_empty() {
            return Ok(());
        }
        self.pilots.with_pilot_round_info(ucid, round, |ri| {
            if let Some(sl) = ri.slot.as_mut() {
                sl.sortie = None;
            }
        })?;
        for sid in open {
            self.finalize_sortie(ucid, round, sid, end, crashed)?;
        }
        Ok(())
    }

    /// Close every still-open Flight Log leg for this pilot (all rounds).
    /// End time = last activity in that round when known, else takeoff (0 h) —
    /// never Connect "now", so a late reconnect cannot credit days of air time.
    fn finalize_orphan_sorties_on_connect(
        &self,
        ucid: Ucid,
        connect_time: DateTime<Utc>,
    ) -> Result<()> {
        let mut open: Vec<(RoundId, SortieId, DateTime<Utc>)> = Vec::new();
        for r in self.pilots.sortie.scan_prefix(&ucid)? {
            let ((_, round, sid), s) = r?;
            if s.land.is_none() {
                open.push((round, sid, s.takeoff));
            }
        }
        if open.is_empty() {
            return Ok(());
        }
        for (round, sid, takeoff) in open {
            let activity = self.pilot_last_activity.get(&(ucid, round))?;
            let end = activity
                .filter(|t| *t >= takeoff)
                .unwrap_or(takeoff)
                .min(connect_time);
            // Clear slot pointer if it still names this orphan.
            self.pilots.with_pilot_round_info(ucid, round, |ri| {
                if let Some(sl) = ri.slot.as_mut() {
                    if sl.sortie == Some(sid) {
                        sl.sortie = None;
                    }
                }
            })?;
            self.finalize_sortie(ucid, round, sid, end, true)?;
            debug!(
                "Connect: closed orphan sortie {sid:?} ucid={ucid:?} round={round:?} takeoff={takeoff} end={end}"
            );
        }
        Ok(())
    }

    fn record_kill(&self, ctx: &mut StatCtxInner, dead: Dead) -> Result<()> {
        // Idempotency: one real kill = one (round, victim, death-time) triple.
        // Redelivery (JSONL re-read after restart, archive replay) would otherwise
        // mint a second KillId and inflate A/A / A/G / A/S victories.
        let victim_enid = match &dead.victim {
            Who::Player { ucid, .. } => EnId::Player(*ucid),
            Who::AI { uid, .. } => EnId::Unit(*uid),
        };
        let dedup_key = (ctx.round, victim_enid, dead.time.timestamp_millis());
        if self.kill_seen.get(&dedup_key)?.is_some() {
            return Ok(());
        }
        let kid = KillId::new(&self.db)?;
        self.kill_seen.insert(&dedup_key, &kid)?;
        let kind = self.classify_kill_target(ctx.round, &dead)?;
        match &dead.victim {
            Who::Player { ucid, .. } => {
                self.pilots.with_pilot_and_aggregates(
                    *ucid,
                    ctx.round,
                    |p| p.total.deaths += 1,
                    |a| a.deaths += 1,
                )?;
                // End open flight on death: End time + duration + hours (Lost, not Landed).
                self.finalize_open_sorties(*ucid, ctx.round, dead.time, true)?;
            }
            Who::AI { .. } => {}
        }
        // Dashboard / Kill Feed: one kill → one credit = finishing blow (latest hit).
        let Some(shot) = self.finishing_shot(&dead) else {
            return Ok(());
        };
        let enid = match &shot.shooter {
            Who::AI {
                ucid: None, uid, ..
            } => EnId::Unit(*uid),
            Who::Player { ucid, .. }
            | Who::AI {
                ucid: Some(ucid), ..
            } => {
                match kind {
                    KillTarget::Ship => {
                        // A2S = airframe shooter; G2S = ground / CA / Silkworm deploy.
                        let typ = Self::resolve_shooter_typ(&dead, shot);
                        if self.shooter_is_airframe(ctx.round, &shot.shooter, typ)? {
                            self.pilots.bump_ship_kill(*ucid, ctx.round)?;
                        } else {
                            self.pilots.bump_ground_ship_kill(*ucid, ctx.round, typ)?;
                        }
                    }
                    KillTarget::Ground => {
                        // A2G = airframe shooter; G2G = ground / CA / deploy.
                        let typ = Self::resolve_shooter_typ(&dead, shot);
                        if self.shooter_is_airframe(ctx.round, &shot.shooter, typ)? {
                            self.pilots.with_pilot_and_aggregates(
                                *ucid,
                                ctx.round,
                                |p| p.total.ground_kills += 1,
                                |a| a.ground_kills += 1,
                            )?;
                        } else {
                            self.pilots.bump_ground_ground_kill(*ucid, ctx.round, typ)?;
                        }
                    }
                    KillTarget::Air => {
                        // A2A = airframe shooter; G2A = ground / CA / deploy.
                        let typ = Self::resolve_shooter_typ(&dead, shot);
                        if self.shooter_is_airframe(ctx.round, &shot.shooter, typ)? {
                            self.pilots.with_pilot_and_aggregates(
                                *ucid,
                                ctx.round,
                                |p| p.total.air_kills += 1,
                                |a| a.air_kills += 1,
                            )?;
                        } else {
                            self.pilots.bump_ground_air_kill(*ucid, ctx.round, typ)?;
                        }
                    }
                }
                EnId::Player(*ucid)
            }
        };
        self.kills.insert(&(enid, ctx.round, kid), &dead)?;
        self.with_shared_kills(kid, |sk| {
            if !sk.contains(&enid) {
                sk.push(enid)
            }
        })?;
        Ok(())
    }

    fn record_static_kill(&self, ctx: &mut StatCtxInner, rec: StaticKillRecord) -> Result<()> {
        let dedup_key = (ctx.round, rec.unit_id, rec.time.timestamp_millis());
        if self.static_kill_seen.get(&dedup_key)?.is_some() {
            return Ok(());
        }
        let kid = KillId::new(&self.db)?;
        self.static_kill_seen.insert(&dedup_key, &kid)?;
        let typ = rec.shooter_typ.as_deref();
        if self.ucid_shooter_is_airframe(ctx.round, rec.by, typ)? {
            self.pilots.with_pilot_and_aggregates(
                rec.by,
                ctx.round,
                |p| p.total.ground_kills += 1,
                |a| a.ground_kills += 1,
            )?;
        } else {
            self.pilots
                .bump_ground_ground_kill(rec.by, ctx.round, typ)?;
        }
        self.static_kills
            .insert(&(rec.by, ctx.round, kid), &rec)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn pilots(&self) -> impl Iterator<Item = Result<(Ucid, String)>> {
        self.pilots.pilots.iter().map(|r| {
            let (ucid, pilot) = r?;
            let name = pilot
                .name
                .last()
                .map(|s| s.clone())
                .unwrap_or(String::default());
            Ok((ucid, name))
        })
    }

    /// Get all pilots with their aggregate stats, sorted by total kills descending.
    /// If `round` is Some, only stats from that round are included; otherwise all-time.
    /// If `inst` is Some (and `round` is None), only rounds owned by that DCS
    /// server instance are summed — multi-instance dashboards must pass this.
    /// All-time sums the same `aggregates` (+ side kill tables) as PILOTS theater
    /// breakdown — not `pilot.total`, which can drift from the per-round trees.
    /// UCIDs with an open merge (source side) are omitted — their history lives on the target.
    pub(crate) fn pilot_leaderboard(
        &self,
        round: Option<RoundId>,
        inst: Option<&InstanceId>,
    ) -> Result<Vec<(Ucid, String, Aggregates)>> {
        let merged_away = self.merged_away_ucids();
        let inst_rounds = match (round, inst) {
            (Some(_), _) => None,
            (None, Some(id)) => Some(self.rounds_of(id)?),
            (None, None) => None,
        };
        let allow = |round_id: RoundId| -> bool {
            if let Some(rid) = round {
                return round_id == rid;
            }
            if let Some(ref allowed) = inst_rounds {
                return allowed.contains(&round_id);
            }
            true
        };
        let mut map: std::collections::HashMap<Ucid, Aggregates> =
            std::collections::HashMap::new();
        for r in self.pilots.aggregates.iter() {
            let ((ucid, _vehicle, round_id), agg) = r?;
            if !allow(round_id) {
                continue;
            }
            if merged_away.contains(&ucid) {
                continue;
            }
            let e = map.entry(ucid).or_insert_with(Aggregates::default);
            e.air_kills += agg.air_kills;
            e.ground_kills += agg.ground_kills;
            e.captures += agg.captures;
            e.repairs += agg.repairs;
            e.supply_transfers += agg.supply_transfers;
            e.troops += agg.troops;
            e.farps += agg.farps;
            e.deploys += agg.deploys;
            e.actions += agg.actions;
            e.deaths += agg.deaths;
            e.hours += agg.hours;
            e.donated_points += agg.donated_points;
        }
        for r in self.pilots.agg_ship_kills.iter() {
            let ((ucid, _vehicle, round_id), n) = r?;
            if !allow(round_id) {
                continue;
            }
            if merged_away.contains(&ucid) {
                continue;
            }
            let e = map.entry(ucid).or_insert_with(Aggregates::default);
            e.ship_kills = e.ship_kills.saturating_add(n);
        }
        for r in self.pilots.agg_ground_air_kills.iter() {
            let ((ucid, _vehicle, round_id), n) = r?;
            if !allow(round_id) {
                continue;
            }
            if merged_away.contains(&ucid) {
                continue;
            }
            let e = map.entry(ucid).or_insert_with(Aggregates::default);
            e.ground_air_kills = e.ground_air_kills.saturating_add(n);
        }
        for r in self.pilots.agg_ground_ground_kills.iter() {
            let ((ucid, _vehicle, round_id), n) = r?;
            if !allow(round_id) {
                continue;
            }
            if merged_away.contains(&ucid) {
                continue;
            }
            let e = map.entry(ucid).or_insert_with(Aggregates::default);
            e.ground_ground_kills = e.ground_ground_kills.saturating_add(n);
        }
        for r in self.pilots.agg_ground_ship_kills.iter() {
            let ((ucid, _vehicle, round_id), n) = r?;
            if !allow(round_id) {
                continue;
            }
            if merged_away.contains(&ucid) {
                continue;
            }
            let e = map.entry(ucid).or_insert_with(Aggregates::default);
            e.ground_ship_kills = e.ground_ship_kills.saturating_add(n);
        }
        for r in self.pilots.agg_csar.iter() {
            let ((ucid, _vehicle, round_id), n) = r?;
            if !allow(round_id) {
                continue;
            }
            if merged_away.contains(&ucid) {
                continue;
            }
            let e = map.entry(ucid).or_insert_with(Aggregates::default);
            e.csar = e.csar.saturating_add(n);
        }
        let mut entries: Vec<(Ucid, String, Aggregates)> = map
            .into_iter()
            .map(|(ucid, agg)| {
                let name = self
                    .pilots
                    .pilots
                    .get(&ucid)
                    .ok()
                    .flatten()
                    .and_then(|p| p.name.last().cloned())
                    .unwrap_or_default();
                (ucid, name, agg)
            })
            .collect();
        entries.sort_by(|a, b| total_kills(&b.2).cmp(&total_kills(&a.2)));
        Ok(entries)
    }

    /// Active-round side if registered, else most recent Blue/Red on record.
    pub(crate) fn pilot_current_side(&self, ucid: &Ucid) -> Result<Option<Side>> {
        let active = self
            .latest_rounds()?
            .into_iter()
            .find(|(_, _, r)| r.end.is_none())
            .map(|(_, rid, _)| rid);
        let mut best: Option<(DateTime<Utc>, Side)> = None;
        for r in self.pilots.round_info.scan_prefix(ucid)? {
            let ((_, rid), ri) = r?;
            if !matches!(ri.side.1, Side::Blue | Side::Red) {
                continue;
            }
            if Some(rid) == active {
                return Ok(Some(ri.side.1));
            }
            if best.map_or(true, |(t, _)| ri.side.0 > t) {
                best = Some(ri.side);
            }
        }
        Ok(best.map(|(_, s)| s))
    }

    /// Get all pilot UCIDs and their most recent names (all-time, for name resolution)
    /// Latest known display name for a pilot, if we've ever seen them.
    pub(crate) fn pilot_name(&self, ucid: &Ucid) -> Option<std::string::String> {
        self.pilots
            .pilots
            .get(ucid)
            .ok()
            .flatten()
            .and_then(|p| p.name.last().map(|s| s.to_string()))
    }

    pub(crate) fn all_pilot_names(&self) -> Result<Vec<(Ucid, String)>> {
        let merged_away = self.merged_away_ucids();
        let mut entries = Vec::new();
        for r in self.pilots.pilots.iter() {
            let (ucid, pilot) = r?;
            if merged_away.contains(&ucid) {
                continue;
            }
            let name = pilot.name.last().map(|s| s.clone()).unwrap_or_default();
            entries.push((ucid, name));
        }
        Ok(entries)
    }

    /// Get the latest round for each scenario
    /// Pilot points for active round, sorted descending
    pub(crate) fn pilot_points(&self, round: RoundId) -> Result<Vec<(std::string::String, i32, std::string::String)>> {
        // Returns Vec<(name, points, side)>
        let mut result = Vec::new();
        for r in self.pilots.round_info.iter() {
            let ((ucid, rid), ri) = r?;
            if rid != round { continue; }
            if ri.points == 0 { continue; }
            let name = self.pilots.pilots.get(&ucid)?
                .and_then(|p| p.name.last().map(|s| s.to_string()))
                .unwrap_or_default();
            let side = format!("{:?}", ri.side.1);
            result.push((name, ri.points, side));
        }
        result.sort_by(|a, b| b.1.cmp(&a.1));
        Ok(result)
    }

    /// Most captured objectives for a round, sorted by capture count desc
    pub(crate) fn most_captured(&self, round: RoundId) -> Result<Vec<(std::string::String, u32)>> {
        // Returns Vec<(objective_name, capture_count)>
        let mut result = Vec::new();
        for r in self.objective_captures.scan_prefix(&round)? {
            let ((_, oid), count) = r?;
            // Look up objective name
            let name = self.objectives.get(&(round, oid))?
                .map(|o| o.name.to_string())
                .unwrap_or_else(|| format!("{:?}", oid));
            result.push((name, count));
        }
        result.sort_by(|a, b| b.1.cmp(&a.1));
        Ok(result)
    }

    /// Recent capture events for a round, newest first, with pilot
    /// attribution -- distinct from most_captured, which is just a count.
    pub(crate) fn recent_captures(&self, round: RoundId, limit: usize) -> Result<Vec<CaptureRecord>> {
        let mut result = Vec::new();
        for r in self.captures.scan_prefix(&round)?.rev() {
            let (_, rec) = r?;
            result.push(rec);
            if result.len() >= limit {
                break;
            }
        }
        Ok(result)
    }

    /// Aircraft usage for a round, optionally filtered by Blue/Red.
    /// Built from pilot sorties + round side (not the unscoped aircraft_sorties tree).
    pub(crate) fn aircraft_usage(
        &self,
        round: RoundId,
        side_filter: Option<Side>,
    ) -> Result<Vec<(std::string::String, u32, f32)>> {
        use std::collections::HashMap;
        let mut map: HashMap<std::string::String, (u32, f32)> = HashMap::new();
        for r in self.pilots.round_info.iter() {
            let ((ucid, rid), ri) = r?;
            if rid != round {
                continue;
            }
            let side = match ri.side.1 {
                Side::Blue | Side::Red => ri.side.1,
                _ => self.pilot_current_side(&ucid)?.unwrap_or(Side::Neutral),
            };
            if let Some(want) = side_filter {
                if side != want {
                    continue;
                }
            }
            for sr in self.pilots.sortie.scan_prefix(&(ucid, round))? {
                let ((_, _, _), s) = sr?;
                let hours = match s.land {
                    Some(land) => {
                        let end = if land < s.takeoff { s.takeoff } else { land };
                        ((end - s.takeoff).num_seconds().max(0) as f32) / 3600.0
                    }
                    None => 0.0,
                };
                let e = map.entry(s.vehicle.to_string()).or_insert((0, 0.0));
                e.0 += 1;
                e.1 += hours;
            }
        }
        let mut result: Vec<_> = map
            .into_iter()
            .map(|(vehicle, (count, hours))| (vehicle, count, hours))
            .collect();
        result.sort_by(|a, b| {
            b.2.partial_cmp(&a.2)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.1.cmp(&a.1))
        });
        Ok(result)
    }

    /// Get connected pilots for a round with name, side, and current aircraft type
    pub(crate) fn connected_pilots(&self, round: RoundId) -> Result<Vec<(std::string::String, std::string::String, Side, Option<std::string::String>)>> {
        // Returns Vec<(ucid, name, side, aircraft_type)> for currently connected pilots
        let mut result = Vec::new();
        let mut heal: SmallVec<[(Ucid, Side); 8]> = SmallVec::new();
        for r in self.pilots.round_info.iter() {
            let ((ucid, rid), ri) = r?;
            if rid != round { continue; }
            if ri.connected.is_none() { continue; }
            let name = self.pilots.pilots.get(&ucid)?
                .and_then(|p| p.name.last().map(|s| s.to_string()))
                .unwrap_or_default();
            let aircraft = ri.slot.and_then(|s| s.vehicle).map(|v| format!("{}", v));
            // Register fires only once per campaign; later rounds often have Neutral
            // until Sideswitch — fall back to last known Blue/Red and persist it.
            let side = match ri.side.1 {
                Side::Blue | Side::Red => ri.side.1,
                _ => {
                    let s = self.pilot_current_side(&ucid)?.unwrap_or(Side::Neutral);
                    if matches!(s, Side::Blue | Side::Red) {
                        heal.push((ucid, s));
                    }
                    s
                }
            };
            result.push((ucid.to_string(), name, side, aircraft));
        }
        for (ucid, side) in heal {
            let _ = self.pilots.with_pilot_round_info(ucid, round, |ri| {
                if matches!(ri.side.1, Side::Neutral) {
                    ri.side = (Utc::now(), side);
                }
            });
        }
        result.sort_by(|a, b| a.2.cmp(&b.2).then(a.1.cmp(&b.1)));
        Ok(result)
    }

    /// Count registered pilots per side and online pilots for a round
    pub(crate) fn pilot_side_counts(&self, round: RoundId) -> Result<(u32, u32, u32, u32)> {
        // Returns (blue_registered, red_registered, blue_online, red_online)
        let mut blue_reg = 0u32;
        let mut red_reg  = 0u32;
        let mut blue_online = 0u32;
        let mut red_online  = 0u32;
        for r in self.pilots.round_info.iter() {
            let ((ucid, rid), ri) = r?;
            if rid != round { continue; }
            let side = match ri.side.1 {
                Side::Blue | Side::Red => ri.side.1,
                _ => self.pilot_current_side(&ucid)?.unwrap_or(Side::Neutral),
            };
            match side {
                Side::Blue => {
                    blue_reg += 1;
                    if ri.connected.is_some() { blue_online += 1; }
                }
                Side::Red => {
                    red_reg += 1;
                    if ri.connected.is_some() { red_online += 1; }
                }
                _ => {}
            }
        }
        Ok((blue_reg, red_reg, blue_online, red_online))
    }

    pub(crate) fn latest_weather(&self, inst: &InstanceState) -> Option<WeatherSnapshot> {
        inst.latest_weather.read().ok()?.clone()
    }

    pub(crate) fn latest_session_end(&self) -> Result<Option<SessionEnd>> {
        // Walk all sessions, newest last, return the most recent one that has a SessionEnd.
        // Skip individual records that fail to deserialize (e.g. written by an
        // older/incompatible build, or left partially-written by an unclean
        // shutdown) instead of letting one bad entry permanently break
        // /api/admin/perf for every session that comes after it.
        let mut latest: Option<SessionEnd> = None;
        for r in self.session.iter() {
            let session = match r {
                Ok((_, session)) => session,
                Err(e) => {
                    log::warn!("latest_session_end: skipping unreadable session record: {e:?}");
                    continue;
                }
            };
            if let Some(end) = session.end {
                latest = Some(end);
            }
        }
        Ok(latest)
    }

    // ── Admin ban management ─────────────────────────────────────────────────

    pub(crate) fn ban_player(&self, ucid: Ucid, record: BanRecord) -> Result<()> {
        self.admin_bans.insert(&ucid, &record)?;
        Ok(())
    }

    pub(crate) fn unban_player(&self, ucid: &Ucid) -> Result<bool> {
        let had = self.admin_bans.remove(ucid)?.is_some();
        Ok(had)
    }

    pub(crate) fn list_admin_bans(&self) -> Result<Vec<(Ucid, BanRecord)>> {
        let mut out = Vec::new();
        for r in self.admin_bans.iter() {
            let (ucid, rec) = r?;
            out.push((ucid, rec));
        }
        Ok(out)
    }

    /// Bans recorded by bflib in the latest session's Cfg (read-only mirror)
    pub(crate) fn session_bans_from_cfg(&self) -> Result<Vec<(Ucid, std::string::String, Option<DateTime<Utc>>)>> {
        let mut latest_cfg: Option<Cfg> = None;
        for r in self.session.iter() {
            let (_, s) = r?;
            latest_cfg = Some(s.cfg);
        }
        let mut out = Vec::new();
        if let Some(cfg) = latest_cfg {
            for (ucid, (until, name)) in &cfg.banned {
                out.push((*ucid, name.to_string(), *until));
            }
        }
        Ok(out)
    }

    // ── bfwiki content management ────────────────────────────────────────────

    pub(crate) fn wiki_get_page(&self, slug: &str) -> Result<Option<WikiPage>> {
        self.wiki_pages.get(&slug.to_string())
    }

    /// All pages, sorted by (section, order) -- the order bfwiki's sidebar
    /// renders them in.
    pub(crate) fn wiki_list_pages(&self) -> Result<Vec<(std::string::String, WikiPage)>> {
        let mut out = Vec::new();
        for r in self.wiki_pages.iter() {
            let (slug, page) = r?;
            out.push((slug, page));
        }
        // Sections read top-to-bottom in a deliberate order, not alphabetically
        // ("Advanced Topics" would otherwise sort before "Introduction"). Any
        // section an admin types that isn't in this built-in list just falls
        // in after the known ones, alphabetically among themselves.
        fn section_rank(section: &str) -> i32 {
            match section {
                "Introduction" => 0,
                "Getting Started" => 1,
                "Core Gameplay" => 2,
                "F10 Menu Systems" => 3,
                "Advanced Topics" => 4,
                "Reference" => 5,
                _ => 100,
            }
        }
        out.sort_by(|(_, a), (_, b)| {
            section_rank(&a.section).cmp(&section_rank(&b.section))
                .then(a.section.cmp(&b.section))
                .then(a.order.cmp(&b.order))
        });
        Ok(out)
    }

    pub(crate) fn wiki_save_page(&self, slug: &str, page: WikiPage) -> Result<()> {
        self.wiki_pages.insert(&slug.to_string(), &page)?;
        Ok(())
    }

    pub(crate) fn wiki_delete_page(&self, slug: &str) -> Result<bool> {
        Ok(self.wiki_pages.remove(&slug.to_string())?.is_some())
    }

    pub(crate) fn wiki_save_image(&self, id: Uuid, image: WikiImage) -> Result<()> {
        self.wiki_images.insert(&id, &image)?;
        Ok(())
    }

    pub(crate) fn wiki_get_image(&self, id: &Uuid) -> Result<Option<WikiImage>> {
        self.wiki_images.get(id)
    }

    /// Vector seed_wiki content was removed for Fowl 2.0 (wrong campaign).
    /// Wiki API trees stay empty; later wire UI link to Attrition website guide.
    fn seed_wiki_if_empty(&self) -> Result<()> {
        Ok(())
    }

    fn seed_wiki_images_if_empty(&self) -> Result<()> {
        Ok(())
    }

    /// One-shot: set Career / Theater / aircraft_sorties hours from closed Flight Log
    /// legs. Fixes drift when finalize credited `Pilot.total` but skipped round
    /// aggregates (no slot vehicle after kick / Deslot race).
    fn reconcile_flight_hours_from_sorties_once(&self) -> Result<()> {
        const FLAG: &[u8] = b"reconcile_flight_hours_v1";
        if self.db.get(FLAG)?.is_some() {
            return Ok(());
        }
        info!("reconciling flight hours from closed sorties (once)");

        use std::collections::HashMap;
        let mut by_agg: HashMap<(Ucid, Vehicle, RoundId), f32> = HashMap::new();
        let mut by_pilot: HashMap<Ucid, f32> = HashMap::new();
        let mut by_ac: HashMap<(RoundId, std::string::String), f32> = HashMap::new();

        for r in self.pilots.sortie.iter() {
            let ((ucid, round, _), s) = r?;
            let Some(land) = s.land else {
                continue;
            };
            let end = if land < s.takeoff { s.takeoff } else { land };
            let hours = ((end - s.takeoff).num_seconds().max(0) as f32) / 3600.0;
            if hours <= 0.0 {
                continue;
            }
            *by_agg
                .entry((ucid, s.vehicle.clone(), round))
                .or_default() += hours;
            *by_pilot.entry(ucid).or_default() += hours;
            *by_ac
                .entry((round, s.vehicle.to_string()))
                .or_default() += hours;
        }

        let agg_keys: Vec<_> = self
            .pilots
            .aggregates
            .iter()
            .filter_map(|r| r.ok().map(|(k, _)| k))
            .collect();
        for k in agg_keys {
            self.pilots.with_aggregates(k, |a| a.hours = 0.0)?;
        }
        for (k, hours) in by_agg {
            self.pilots.with_aggregates(k, |a| a.hours = hours)?;
        }

        let pilot_ids: Vec<_> = self
            .pilots
            .pilots
            .iter()
            .filter_map(|r| r.ok().map(|(u, _)| u))
            .collect();
        for ucid in &pilot_ids {
            self.pilots.with_pilot(*ucid, |p| p.total.hours = 0.0)?;
        }
        for (ucid, hours) in by_pilot {
            self.pilots.with_pilot(ucid, |p| p.total.hours = hours)?;
        }

        let ac_rows: Vec<_> = self
            .aircraft_sorties
            .iter()
            .filter_map(|r| r.ok().map(|(k, (cnt, _))| (k, cnt)))
            .collect();
        for (k, cnt) in ac_rows {
            let hrs = by_ac.remove(&k).unwrap_or(0.0);
            self.aircraft_sorties.insert(&k, &(cnt, hrs))?;
        }
        for (k, hrs) in by_ac {
            let (cnt, _) = self.aircraft_sorties.get(&k)?.unwrap_or((0, 0.0));
            self.aircraft_sorties.insert(&k, &(cnt, hrs))?;
        }

        self.db.insert(FLAG, b"1")?;
        info!("flight hours reconciliation complete");
        Ok(())
    }

    /// One-shot: split historical air_kills into A2A (airframe shooter) vs G2A
    /// (ground / CA / deploy) from the kill log. Pure AI (ucid None) stays out.
    /// v2: do not use stale Player slot/units (v1 mis-labeled almost all A2A as G2A).
    fn reconcile_a2a_g2a_once(&self) -> Result<()> {
        const FLAG: &[u8] = b"reconcile_a2a_g2a_v2";
        if self.db.get(FLAG)?.is_some() {
            return Ok(());
        }
        info!("reconciling A2A vs G2A from kill log (once, v2)");

        use std::collections::HashMap;
        let mut by_kid: HashMap<KillId, (RoundId, Dead)> = HashMap::new();
        for r in self.kills.iter() {
            let ((_enid, rid, kid), dead) = r?;
            by_kid.entry(kid).or_insert((rid, dead));
        }

        let mut career_a2a: HashMap<Ucid, u32> = HashMap::new();
        let mut career_g2a: HashMap<Ucid, u32> = HashMap::new();
        let mut agg_a2a: HashMap<(Ucid, Vehicle, RoundId), u32> = HashMap::new();
        let mut agg_g2a: HashMap<(Ucid, Vehicle, RoundId), u32> = HashMap::new();

        for (rid, dead) in by_kid.into_values() {
            if self.classify_kill_target(rid, &dead)? != KillTarget::Air {
                continue;
            }
            let Some(shot) = self.finishing_shot(&dead) else {
                continue;
            };
            let ucid = match &shot.shooter {
                Who::Player { ucid, .. }
                | Who::AI {
                    ucid: Some(ucid), ..
                } => *ucid,
                Who::AI { ucid: None, .. } => continue,
            };
            let typ = Self::resolve_shooter_typ(&dead, shot);
            let is_a2a = self.shooter_is_airframe(rid, &shot.shooter, typ)?;
            let vehicle = typ
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(Vehicle::from)
                .or_else(|| {
                    self.pilots
                        .round_info
                        .get(&(ucid, rid))
                        .ok()
                        .flatten()
                        .and_then(|ri| ri.slot.and_then(|s| s.vehicle))
                });
            if is_a2a {
                *career_a2a.entry(ucid).or_default() += 1;
                if let Some(v) = vehicle {
                    *agg_a2a.entry((ucid, v, rid)).or_default() += 1;
                }
            } else {
                *career_g2a.entry(ucid).or_default() += 1;
                if let Some(v) = vehicle {
                    *agg_g2a.entry((ucid, v, rid)).or_default() += 1;
                }
            }
        }

        let agg_keys: Vec<_> = self
            .pilots
            .aggregates
            .iter()
            .filter_map(|r| r.ok().map(|(k, _)| k))
            .collect();
        for k in &agg_keys {
            self.pilots.with_aggregates(k.clone(), |a| a.air_kills = 0)?;
        }
        let pilot_ids: Vec<_> = self
            .pilots
            .pilots
            .iter()
            .filter_map(|r| r.ok().map(|(u, _)| u))
            .collect();
        for ucid in &pilot_ids {
            self.pilots.with_pilot(*ucid, |p| p.total.air_kills = 0)?;
        }
        self.pilots.agg_ground_air_kills.clear()?;
        self.pilots.pilot_ground_air_kills.clear()?;

        for (ucid, n) in career_a2a {
            self.pilots.with_pilot(ucid, |p| p.total.air_kills = n)?;
        }
        for (ucid, n) in career_g2a {
            self.pilots.pilot_ground_air_kills.insert(&ucid, &n)?;
        }
        for (k, n) in agg_a2a {
            self.pilots.with_aggregates(k, |a| a.air_kills = n)?;
        }
        for (k, n) in agg_g2a {
            self.pilots.agg_ground_air_kills.insert(&k, &n)?;
        }

        self.db.insert(FLAG, b"1")?;
        info!("A2A/G2A reconciliation complete (v2)");
        Ok(())
    }

    /// One-shot: split historical ground_kills into A2G (airframe) vs G2G
    /// (ground / CA / deploy) from unit + static kill logs.
    /// v2: same stale-slot fix as A2A/G2A v2.
    fn reconcile_a2g_g2g_once(&self) -> Result<()> {
        const FLAG: &[u8] = b"reconcile_a2g_g2g_v2";
        if self.db.get(FLAG)?.is_some() {
            return Ok(());
        }
        info!("reconciling A2G vs G2G from kill log (once, v2)");

        use std::collections::HashMap;
        let mut career_a2g: HashMap<Ucid, u32> = HashMap::new();
        let mut career_g2g: HashMap<Ucid, u32> = HashMap::new();
        let mut agg_a2g: HashMap<(Ucid, Vehicle, RoundId), u32> = HashMap::new();
        let mut agg_g2g: HashMap<(Ucid, Vehicle, RoundId), u32> = HashMap::new();

        {
            let mut credit = |ucid: Ucid,
                              rid: RoundId,
                              is_airframe: bool,
                              vehicle: Option<Vehicle>| {
                if is_airframe {
                    *career_a2g.entry(ucid).or_default() += 1;
                    if let Some(v) = vehicle {
                        *agg_a2g.entry((ucid, v, rid)).or_default() += 1;
                    }
                } else {
                    *career_g2g.entry(ucid).or_default() += 1;
                    if let Some(v) = vehicle {
                        *agg_g2g.entry((ucid, v, rid)).or_default() += 1;
                    }
                }
            };

            let mut by_kid: HashMap<KillId, (RoundId, Dead)> = HashMap::new();
            for r in self.kills.iter() {
                let ((_enid, rid, kid), dead) = r?;
                by_kid.entry(kid).or_insert((rid, dead));
            }
            for (rid, dead) in by_kid.into_values() {
                if self.classify_kill_target(rid, &dead)? != KillTarget::Ground {
                    continue;
                }
                let Some(shot) = self.finishing_shot(&dead) else {
                    continue;
                };
                let ucid = match &shot.shooter {
                    Who::Player { ucid, .. }
                    | Who::AI {
                        ucid: Some(ucid), ..
                    } => *ucid,
                    Who::AI { ucid: None, .. } => continue,
                };
                let typ = Self::resolve_shooter_typ(&dead, shot);
                let is_a2g = self.shooter_is_airframe(rid, &shot.shooter, typ)?;
                let vehicle = typ
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(Vehicle::from)
                    .or_else(|| {
                        self.pilots
                            .round_info
                            .get(&(ucid, rid))
                            .ok()
                            .flatten()
                            .and_then(|ri| ri.slot.and_then(|s| s.vehicle))
                    });
                credit(ucid, rid, is_a2g, vehicle);
            }

            for r in self.static_kills.iter() {
                let ((ucid, rid, _), rec) = r?;
                let typ = rec.shooter_typ.as_deref();
                let is_a2g = self.ucid_shooter_is_airframe(rid, ucid, typ)?;
                let vehicle = typ
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(Vehicle::from)
                    .or_else(|| {
                        self.pilots
                            .round_info
                            .get(&(ucid, rid))
                            .ok()
                            .flatten()
                            .and_then(|ri| ri.slot.and_then(|s| s.vehicle))
                    });
                credit(ucid, rid, is_a2g, vehicle);
            }
        }

        let agg_keys: Vec<_> = self
            .pilots
            .aggregates
            .iter()
            .filter_map(|r| r.ok().map(|(k, _)| k))
            .collect();
        for k in &agg_keys {
            self.pilots
                .with_aggregates(k.clone(), |a| a.ground_kills = 0)?;
        }
        let pilot_ids: Vec<_> = self
            .pilots
            .pilots
            .iter()
            .filter_map(|r| r.ok().map(|(u, _)| u))
            .collect();
        for ucid in &pilot_ids {
            self.pilots.with_pilot(*ucid, |p| p.total.ground_kills = 0)?;
        }
        self.pilots.agg_ground_ground_kills.clear()?;
        self.pilots.pilot_ground_ground_kills.clear()?;

        for (ucid, n) in career_a2g {
            self.pilots.with_pilot(ucid, |p| p.total.ground_kills = n)?;
        }
        for (ucid, n) in career_g2g {
            self.pilots.pilot_ground_ground_kills.insert(&ucid, &n)?;
        }
        for (k, n) in agg_a2g {
            self.pilots.with_aggregates(k, |a| a.ground_kills = n)?;
        }
        for (k, n) in agg_g2g {
            self.pilots.agg_ground_ground_kills.insert(&k, &n)?;
        }

        self.db.insert(FLAG, b"1")?;
        info!("A2G/G2G reconciliation complete (v2)");
        Ok(())
    }

    // ── Perf history ─────────────────────────────────────────────────────────

    pub(crate) fn session_perf_history(&self, limit: usize) -> Result<Vec<SessionEnd>> {
        let mut ends: Vec<SessionEnd> = Vec::new();
        for r in self.session.iter() {
            let (_, s) = r?;
            if let Some(end) = s.end {
                ends.push(end);
            }
        }
        if ends.len() > limit {
            ends.drain(0..ends.len() - limit);
        }
        Ok(ends)
    }

    pub(crate) fn active_session_stop(&self, round: RoundId) -> Option<DateTime<Utc>> {
        self.session
            .scan_prefix(&round)
            .ok()?
            .next_back()
            .and_then(|r| r.ok())
            .and_then(|(_, s)| s.stop_time)
    }

    pub(crate) fn latest_rounds(&self) -> Result<Vec<(Scenario, RoundId, Round)>> {
        let mut rounds = Vec::new();
        let mut seen_scenarios = std::collections::HashSet::new();
        // Scan all rounds, keep the latest per scenario
        for r in self.round.iter() {
            let ((scenario, rid), round) = r?;
            if !seen_scenarios.contains(&scenario) || round.end.is_none() {
                seen_scenarios.insert(scenario.clone());
                // Remove previous entry for this scenario if exists
                rounds.retain(|(s, _, _): &(Scenario, RoundId, Round)| s != &scenario);
                rounds.push((scenario, rid, round));
            }
        }
        Ok(rounds)
    }

    /// Every round ever recorded, not just the latest per scenario. Used for
    /// the round-history selector -- `latest_rounds` intentionally discards
    /// history and can't serve that purpose.
    pub(crate) fn all_rounds(&self) -> Result<Vec<(Scenario, RoundId, Round)>> {
        let mut rounds = Vec::new();
        for r in self.round.iter() {
            let ((scenario, rid), round) = r?;
            rounds.push((scenario, rid, round));
        }
        rounds.sort_by(|a, b| b.2.start.cmp(&a.2.start));
        Ok(rounds)
    }

    /// Ended rounds with no sorties, kills, or per-round aggregates.
    pub(crate) fn list_empty_rounds(&self) -> Result<Vec<(Scenario, RoundId, Round)>> {
        let mut out = Vec::new();
        for (scenario, rid, round) in self.all_rounds()? {
            if round.end.is_none() {
                continue;
            }
            if self.round_is_empty(rid)? {
                out.push((scenario, rid, round));
            }
        }
        Ok(out)
    }

    /// Delete ended empty rounds (no sorties/kills/aggregates) and cascade data.
    pub(crate) fn purge_empty_rounds(&self) -> Result<Vec<(Scenario, RoundId)>> {
        let victims = self.list_empty_rounds()?;
        let mut deleted = Vec::with_capacity(victims.len());
        for (scenario, rid, _) in victims {
            self.delete_round_cascade(&scenario, rid)?;
            info!("purge_empty_rounds: deleted round id={rid:?} scenario={scenario:?}");
            deleted.push((scenario, rid));
        }
        Ok(deleted)
    }

    /// Admin force-delete of one ended round (and cascade), even if not empty.
    pub(crate) fn delete_round_by_id(&self, rid: RoundId) -> Result<(Scenario, RoundId)> {
        let found = self
            .all_rounds()?
            .into_iter()
            .find(|(_, id, _)| *id == rid)
            .ok_or_else(|| anyhow!("round {rid:?} not found"))?;
        let (scenario, _, round) = found;
        if round.end.is_none() {
            bail!("refusing to delete open/active round {rid:?}");
        }
        self.delete_round_cascade(&scenario, rid)?;
        info!("delete_round_by_id: deleted round id={rid:?} scenario={scenario:?}");
        Ok((scenario, rid))
    }

    fn round_is_empty(&self, rid: RoundId) -> Result<bool> {
        for r in self.pilots.sortie.iter() {
            let ((_u, round, _), _) = r?;
            if round == rid {
                return Ok(false);
            }
        }
        for r in self.kills.iter() {
            let ((_e, round, _), _) = r?;
            if round == rid {
                return Ok(false);
            }
        }
        for r in self.static_kills.iter() {
            let ((_u, round, _), _) = r?;
            if round == rid {
                return Ok(false);
            }
        }
        for r in self.pilots.aggregates.iter() {
            let ((_u, _v, round), _) = r?;
            if round == rid {
                return Ok(false);
            }
        }
        for r in self.pilots.agg_ship_kills.iter() {
            let ((_u, _v, round), _) = r?;
            if round == rid {
                return Ok(false);
            }
        }
        for r in self.pilots.agg_ground_air_kills.iter() {
            let ((_u, _v, round), _) = r?;
            if round == rid {
                return Ok(false);
            }
        }
        for r in self.pilots.agg_ground_ground_kills.iter() {
            let ((_u, _v, round), _) = r?;
            if round == rid {
                return Ok(false);
            }
        }
        for r in self.pilots.agg_ground_ship_kills.iter() {
            let ((_u, _v, round), _) = r?;
            if round == rid {
                return Ok(false);
            }
        }
        for r in self.pilots.agg_csar.iter() {
            let ((_u, _v, round), _) = r?;
            if round == rid {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Abort/restart window: second NewRound within this age collapses the prior segment.
    const ROUND_BLIP_MAX_SECS: i64 = 2 * 3600;

    fn round_age_is_blip_window(start: DateTime<Utc>, end_or_now: DateTime<Utc>) -> bool {
        let age = end_or_now.signed_duration_since(start);
        age >= chrono::Duration::zero()
            && age <= chrono::Duration::seconds(Self::ROUND_BLIP_MAX_SECS)
    }

    fn round_has_sortie(&self, rid: RoundId) -> Result<bool> {
        for r in self.pilots.sortie.iter() {
            let ((_u, round, _), _) = r?;
            if round == rid {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// NewRound superseding a short prior segment (kills/FARPs from a failed start OK).
    fn round_is_newround_blip(start: DateTime<Utc>, end_or_now: DateTime<Utc>) -> bool {
        Self::round_age_is_blip_window(start, end_or_now)
    }

    /// RoundEnd discard: short and nobody flew (keep a real short war that ended).
    fn round_is_roundend_blip(
        &self,
        rid: RoundId,
        start: DateTime<Utc>,
        end_or_now: DateTime<Utc>,
    ) -> Result<bool> {
        if !Self::round_age_is_blip_window(start, end_or_now) {
            return Ok(false);
        }
        Ok(!self.round_has_sortie(rid)?)
    }

    fn delete_round_cascade(&self, scenario: &Scenario, rid: RoundId) -> Result<()> {
        self.round.remove(&(scenario.clone(), rid))?;
        self.seq.remove(&(scenario.clone(), rid))?;
        let _ = self.round_instance.remove(&rid)?;

        macro_rules! purge_prefix {
            ($tree:expr) => {{
                let keys: Vec<_> = $tree.scan_prefix(&rid)?.keys().collect::<Result<Vec<_>>>()?;
                for k in keys {
                    $tree.remove(&k)?;
                }
            }};
        }
        purge_prefix!(self.session);
        purge_prefix!(self.objectives);
        purge_prefix!(self.equipment);
        purge_prefix!(self.liquids);
        purge_prefix!(self.units);
        purge_prefix!(self.groups);
        purge_prefix!(self.detected);
        purge_prefix!(self.objective_captures);
        purge_prefix!(self.captures);
        purge_prefix!(self.kill_seen);
        purge_prefix!(self.sortie_seen);
        purge_prefix!(self.deploy_seen);
        purge_prefix!(self.static_kill_seen);
        purge_prefix!(self.aircraft_sorties);
        purge_prefix!(self.trail_points);

        let mut kill_ids = Vec::new();
        for r in self.kills.iter() {
            let ((e, round, kid), _) = r?;
            if round == rid {
                kill_ids.push((e, kid));
            }
        }
        for (e, kid) in kill_ids {
            self.kills.remove(&(e, rid, kid))?;
            let _ = self.shared_kills.remove(&kid)?;
        }

        let mut keys: Vec<(Ucid, RoundId, SortieId)> = Vec::new();
        for r in self.pilots.sortie.iter() {
            let ((u, round, sid), _) = r?;
            if round == rid {
                keys.push((u, round, sid));
            }
        }
        for k in &keys {
            self.pilots.sortie.remove(k)?;
            let _ = self.pilots.sortie_crashed.remove(k)?;
        }

        let mut sk: Vec<(Ucid, RoundId, KillId)> = Vec::new();
        for r in self.static_kills.iter() {
            let ((u, round, kid), _) = r?;
            if round == rid {
                sk.push((u, round, kid));
            }
        }
        for k in &sk {
            self.static_kills.remove(k)?;
        }

        let mut dep: Vec<(Ucid, RoundId, DeployId)> = Vec::new();
        for r in self.deploys.iter() {
            let ((u, round, did), _) = r?;
            if round == rid {
                dep.push((u, round, did));
            }
        }
        for k in &dep {
            self.deploys.remove(k)?;
        }

        let mut agg: Vec<(Ucid, Vehicle, RoundId)> = Vec::new();
        for r in self.pilots.aggregates.iter() {
            let ((u, v, round), _) = r?;
            if round == rid {
                agg.push((u, v, round));
            }
        }
        for k in &agg {
            self.pilots.aggregates.remove(k)?;
            let _ = self.pilots.agg_ship_kills.remove(k)?;
            let _ = self.pilots.agg_ground_air_kills.remove(k)?;
            let _ = self.pilots.agg_ground_ground_kills.remove(k)?;
            let _ = self.pilots.agg_ground_ship_kills.remove(k)?;
            let _ = self.pilots.agg_csar.remove(k)?;
        }

        let mut ri: Vec<(Ucid, RoundId)> = Vec::new();
        for r in self.pilots.round_info.iter() {
            let ((u, round), _) = r?;
            if round == rid {
                ri.push((u, round));
            }
        }
        for k in &ri {
            self.pilots.round_info.remove(k)?;
        }

        let mut act: Vec<(Ucid, RoundId)> = Vec::new();
        for r in self.pilot_last_activity.iter() {
            let ((u, round), _) = r?;
            if round == rid {
                act.push((u, round));
            }
        }
        for k in &act {
            self.pilot_last_activity.remove(k)?;
        }

        Ok(())
    }

    /// Get objectives for a given round
    pub(crate) fn objectives_for_round(&self, round: RoundId) -> Result<Vec<(ObjectiveId, Objective)>> {
        let mut objs = Vec::new();
        for r in self.objectives.scan_prefix(&round)? {
            let ((_, oid), obj) = r?;
            objs.push((oid, obj));
        }
        Ok(objs)
    }

    /// Get all detected, alive units for a given round
    pub(crate) fn detected_units_for_round(
        &self,
        round: RoundId,
    ) -> Result<Vec<(EnId, Unit, BitFlags<DetectionSource, u8>)>> {
        let mut results = Vec::new();
        for r in self.detected.scan_prefix(&round)? {
            let ((_, eid), flags) = r?;
            if flags.is_empty() {
                continue;
            }
            if let Some(unit) = self.units.get(&(round, eid))? {
                if !unit.dead {
                    results.push((eid, unit, flags));
                }
            }
        }
        Ok(results)
    }

    /// Career aggregates for one pilot — sum of per-round trees (same as leaderboard).
    pub(crate) fn pilot_detail(&self, ucid: &Ucid) -> Result<Option<(String, Aggregates)>> {
        let name = match self.pilots.pilots.get(ucid)? {
            None => return Ok(None),
            Some(pilot) => pilot.name.last().cloned().unwrap_or_default(),
        };
        let mut total = Aggregates::default();
        let mut any = false;
        for r in self.pilots.aggregates.iter() {
            let ((u, _vehicle, _), agg) = r?;
            if u != *ucid {
                continue;
            }
            any = true;
            total.air_kills += agg.air_kills;
            total.ground_kills += agg.ground_kills;
            total.captures += agg.captures;
            total.repairs += agg.repairs;
            total.supply_transfers += agg.supply_transfers;
            total.troops += agg.troops;
            total.farps += agg.farps;
            total.deploys += agg.deploys;
            total.actions += agg.actions;
            total.deaths += agg.deaths;
            total.hours += agg.hours;
            total.donated_points += agg.donated_points;
        }
        for r in self.pilots.agg_ship_kills.iter() {
            let ((u, _, _), n) = r?;
            if u == *ucid {
                any = true;
                total.ship_kills = total.ship_kills.saturating_add(n);
            }
        }
        for r in self.pilots.agg_ground_air_kills.iter() {
            let ((u, _, _), n) = r?;
            if u == *ucid {
                any = true;
                total.ground_air_kills = total.ground_air_kills.saturating_add(n);
            }
        }
        for r in self.pilots.agg_ground_ground_kills.iter() {
            let ((u, _, _), n) = r?;
            if u == *ucid {
                any = true;
                total.ground_ground_kills = total.ground_ground_kills.saturating_add(n);
            }
        }
        for r in self.pilots.agg_ground_ship_kills.iter() {
            let ((u, _, _), n) = r?;
            if u == *ucid {
                any = true;
                total.ground_ship_kills = total.ground_ship_kills.saturating_add(n);
            }
        }
        for r in self.pilots.agg_csar.iter() {
            let ((u, _, _), n) = r?;
            if u == *ucid {
                any = true;
                total.csar = total.csar.saturating_add(n);
            }
        }
        if !any && name.is_empty() {
            return Ok(None);
        }
        Ok(Some((name, total)))
    }

    /// All sorties for a pilot across all rounds, sorted chronologically.
    /// Last bool is crashed/Lost (death or mid-air deslot).
    pub(crate) fn pilot_sorties(&self, ucid: &Ucid) -> Result<Vec<(RoundId, SortieId, Sortie, bool)>> {
        let mut result = Vec::new();
        for r in self.pilots.sortie.scan_prefix(ucid)? {
            let ((_, round_id, sortie_id), sortie) = r?;
            let crashed = self
                .pilots
                .sortie_crashed
                .get(&(*ucid, round_id, sortie_id))?
                .unwrap_or(false);
            result.push((round_id, sortie_id, sortie, crashed));
        }
        // Sort chronologically
        result.sort_by(|a, b| a.2.takeoff.cmp(&b.2.takeoff));
        Ok(result)
    }

    /// Per-round aggregates for a pilot, enriched with scenario name
    pub(crate) fn pilot_round_breakdown(&self, ucid: &Ucid) -> Result<Vec<(Scenario, RoundId, Aggregates)>> {
        // Build a round_id → scenario lookup
        let mut rid_to_scenario: std::collections::HashMap<RoundId, Scenario> = std::collections::HashMap::new();
        for r in self.round.iter() {
            let ((scenario, rid), _) = r?;
            rid_to_scenario.insert(rid, scenario);
        }
        // Sum aggregates per round for this pilot
        let mut map: std::collections::HashMap<RoundId, Aggregates> = std::collections::HashMap::new();
        for r in self.pilots.aggregates.iter() {
            let ((u, _vehicle, round_id), agg) = r?;
            if u != *ucid { continue; }
            let e = map.entry(round_id).or_insert_with(Aggregates::default);
            e.air_kills        += agg.air_kills;
            e.ground_kills     += agg.ground_kills;
            e.captures         += agg.captures;
            e.repairs          += agg.repairs;
            e.supply_transfers += agg.supply_transfers;
            e.troops           += agg.troops;
            e.farps            += agg.farps;
            e.deploys          += agg.deploys;
            e.actions          += agg.actions;
            e.deaths           += agg.deaths;
            e.hours            += agg.hours;
            e.donated_points   += agg.donated_points;
        }
        for r in self.pilots.agg_ship_kills.iter() {
            let ((u, _vehicle, round_id), n) = r?;
            if u != *ucid { continue; }
            let e = map.entry(round_id).or_insert_with(Aggregates::default);
            e.ship_kills = e.ship_kills.saturating_add(n);
        }
        for r in self.pilots.agg_ground_air_kills.iter() {
            let ((u, _vehicle, round_id), n) = r?;
            if u != *ucid { continue; }
            let e = map.entry(round_id).or_insert_with(Aggregates::default);
            e.ground_air_kills = e.ground_air_kills.saturating_add(n);
        }
        for r in self.pilots.agg_ground_ground_kills.iter() {
            let ((u, _vehicle, round_id), n) = r?;
            if u != *ucid { continue; }
            let e = map.entry(round_id).or_insert_with(Aggregates::default);
            e.ground_ground_kills = e.ground_ground_kills.saturating_add(n);
        }
        for r in self.pilots.agg_ground_ship_kills.iter() {
            let ((u, _vehicle, round_id), n) = r?;
            if u != *ucid { continue; }
            let e = map.entry(round_id).or_insert_with(Aggregates::default);
            e.ground_ship_kills = e.ground_ship_kills.saturating_add(n);
        }
        for r in self.pilots.agg_csar.iter() {
            let ((u, _vehicle, round_id), n) = r?;
            if u != *ucid { continue; }
            let e = map.entry(round_id).or_insert_with(Aggregates::default);
            e.csar = e.csar.saturating_add(n);
        }
        let mut result: Vec<(Scenario, RoundId, Aggregates)> = map
            .into_iter()
            .map(|(rid, agg)| {
                let scenario = rid_to_scenario.get(&rid).cloned().unwrap_or_default();
                (scenario, rid, agg)
            })
            .collect();
        // Sort by round id ascending (oldest first)
        result.sort_by(|a, b| a.1.cmp(&b.1));
        Ok(result)
    }

    /// All kills made by a specific pilot (killer = Player(ucid)), all rounds
    pub(crate) fn pilot_kills_for(&self, ucid: &Ucid) -> Result<Vec<(RoundId, Dead)>> {
        let prefix_key = EnId::Player(*ucid);
        let mut result = Vec::new();
        for r in self.kills.scan_prefix(&prefix_key)? {
            let ((_, round_id, _), dead) = r?;
            result.push((round_id, dead));
        }
        // Sort newest first
        result.sort_by(|a, b| b.1.time.cmp(&a.1.time));
        Ok(result)
    }

    /// ME static / OPR factory kills by pilot (newest first).
    pub(crate) fn pilot_static_kills_for(
        &self,
        ucid: &Ucid,
    ) -> Result<Vec<(RoundId, StaticKillRecord)>> {
        let mut result = Vec::new();
        for r in self.static_kills.scan_prefix(ucid)? {
            let ((_, round_id, _), rec) = r?;
            result.push((round_id, rec));
        }
        result.sort_by(|a, b| b.1.time.cmp(&a.1.time));
        Ok(result)
    }

    /// All deploys done by a specific pilot, all rounds, newest first.
    pub(crate) fn pilot_deploys_for(&self, ucid: &Ucid) -> Result<Vec<(RoundId, DeployRecord)>> {
        let mut result = Vec::new();
        for r in self.deploys.scan_prefix(ucid)? {
            let ((_, round_id, _), rec) = r?;
            result.push((round_id, rec));
        }
        result.sort_by(|a, b| b.1.time.cmp(&a.1.time));
        Ok(result)
    }

    pub(crate) fn recent_kills(&self, round: RoundId, limit: usize) -> Result<Vec<Dead>> {
        // The kills tree is keyed (killer EnId, round, KillId), so iterating it
        // (even reversed) orders by *killer*, not time -- a naive `.rev().take(n)`
        // returns only AI-made kills (EnId::Unit sorts after EnId::Player) and
        // never reaches player kills once the cap is hit. Collect the whole
        // round, dedupe multi-shooter kills by KillId, then sort by time.
        let mut seen: std::collections::HashSet<KillId> = std::collections::HashSet::new();
        let mut kills = Vec::new();
        for r in self.kills.iter() {
            let ((_, rid, kid), dead) = r?;
            if rid == round && seen.insert(kid) {
                kills.push(dead);
            }
        }
        kills.sort_by(|a, b| b.time.cmp(&a.time));
        kills.truncate(limit);
        Ok(kills)
    }

    /// Recent ME static / OPR factory kills in a round (newest first).
    pub(crate) fn recent_static_kills(
        &self,
        round: RoundId,
        limit: usize,
    ) -> Result<Vec<StaticKillRecord>> {
        let mut kills = Vec::new();
        for r in self.static_kills.iter() {
            let ((_, rid, _), rec) = r?;
            if rid == round {
                kills.push(rec);
            }
        }
        kills.sort_by(|a, b| b.time.cmp(&a.time));
        kills.truncate(limit);
        Ok(kills)
    }

    /// Unique kill events (one Dead / KillId) in a round — not per-hit and not
    /// summed pilot credits (shared kills would otherwise inflate the total).
    pub(crate) fn round_kill_count(&self, round: RoundId) -> Result<u32> {
        let mut seen: std::collections::HashSet<KillId> = std::collections::HashSet::new();
        for r in self.kills.iter() {
            let ((_, rid, kid), _) = r?;
            if rid == round {
                seen.insert(kid);
            }
        }
        for r in self.static_kills.iter() {
            let ((_, rid, kid), _) = r?;
            if rid == round {
                seen.insert(kid);
            }
        }
        Ok(seen.len() as u32)
    }

    /// Same classification record_kill uses for air_kills vs ground/ship:
    /// a player death always counts as air (players are always in aircraft),
    /// an AI death counts as air only if the unit is tagged Aircraft or
    /// Helicopter. Exposed separately so API consumers (e.g. the Discord
    /// kill-streak/achievement poller) can filter on the same definition
    /// instead of guessing from the raw DCS unit-type string.
    pub(crate) fn victim_is_air(&self, round: RoundId, victim: &Who) -> Result<bool> {
        Ok(self.classify_victim_tags(round, victim, None)?.0 == KillTarget::Air)
    }

    pub(crate) fn victim_is_ship(&self, round: RoundId, victim: &Who, dead: Option<&Dead>) -> Result<bool> {
        Ok(self.classify_victim_tags(round, victim, dead)?.0 == KillTarget::Ship)
    }

    fn classify_kill_target(&self, round: RoundId, dead: &Dead) -> Result<KillTarget> {
        Ok(self.classify_victim_tags(round, &dead.victim, Some(dead))?.0)
    }

    /// Returns (kind, tags used). Same ship tags as Discord live-map Top10 A2S.
    fn classify_victim_tags(
        &self,
        round: RoundId,
        victim: &Who,
        dead: Option<&Dead>,
    ) -> Result<(KillTarget, UnitTags)> {
        match victim {
            Who::Player { .. } => Ok((KillTarget::Air, UnitTags::default())),
            Who::AI { uid, .. } => {
                let mut tags = self
                    .units
                    .get(&(round, EnId::Unit(*uid)))?
                    .map(|u| u.tags)
                    .unwrap_or_default();
                if tags.is_empty() {
                    if let Some(dead) = dead {
                        if let Some(typ) = dead
                            .shots
                            .iter()
                            .find(|s| !s.target_typ.trim().is_empty())
                            .map(|s| s.target_typ.as_str())
                        {
                            if let Some(cfg_tags) = self.unit_tags_from_session_cfg(round, typ)? {
                                tags = cfg_tags;
                            }
                        }
                    }
                }
                Ok((kill_target_from_tags(&tags), tags))
            }
        }
    }

    fn unit_tags_from_session_cfg(&self, round: RoundId, typ: &str) -> Result<Option<UnitTags>> {
        let mut best: Option<(DateTime<Utc>, UnitTags)> = None;
        for r in self.session.scan_prefix(&round)? {
            let ((_, ts), session) = r?;
            if let Some(tags) = session.cfg.unit_classification.get(&Vehicle::from(typ)) {
                if best.map_or(true, |(t, _)| ts >= t) {
                    best = Some((ts, *tags));
                }
            }
        }
        Ok(best.map(|(_, t)| t))
    }

    /// Prefer this shot's type; else another shot by the same UCID on the Dead
    /// (Hit events often omit shooter_typ).
    fn resolve_shooter_typ<'a>(dead: &'a Dead, shot: &'a Shot) -> Option<&'a str> {
        if let Some(t) = shot
            .shooter_typ
            .as_ref()
            .map(|s| s.as_str().trim())
            .filter(|t| !t.is_empty())
        {
            return Some(t);
        }
        let ucid = shot.shooter.ucid()?;
        dead.shots.iter().rev().find_map(|s| {
            if s.shooter.ucid() == Some(ucid) {
                s.shooter_typ
                    .as_ref()
                    .map(|t| t.as_str().trim())
                    .filter(|t| !t.is_empty())
            } else {
                None
            }
        })
    }

    fn tags_are_airframe(tags: &UnitTags) -> bool {
        tags.contains(UnitTag::Aircraft) || tags.contains(UnitTag::Helicopter)
    }

    /// A2A/A2G shooter = Aircraft/Helicopter. Else G2A/G2G (ground / CA / deploy).
    ///
    /// Do **not** use `EnId::Player` units or current `round_info.slot` when typ is
    /// missing — those reflect the latest slot after full ingest and turn
    /// historical air kills into false G2A/G2G on reconcile (CA / deslot).
    fn shooter_is_airframe(
        &self,
        round: RoundId,
        shooter: &Who,
        shooter_typ: Option<&str>,
    ) -> Result<bool> {
        if let Some(typ) = shooter_typ.map(str::trim).filter(|t| !t.is_empty()) {
            if let Some(tags) = self.unit_tags_from_session_cfg(round, typ)? {
                return Ok(Self::tags_are_airframe(&tags));
            }
        }
        match shooter {
            Who::AI { uid, .. } => {
                if let Some(unit) = self.units.get(&(round, EnId::Unit(*uid)))? {
                    if !unit.tags.is_empty() {
                        return Ok(Self::tags_are_airframe(&unit.tags));
                    }
                    if let Some(tags) =
                        self.unit_tags_from_session_cfg(round, &unit.typ.to_string())?
                    {
                        return Ok(Self::tags_are_airframe(&tags));
                    }
                }
                // Deployed / AI without classifiable typ → ground.
                Ok(false)
            }
            Who::Player { .. } => {
                // Unknown typ on a player slot → airframe (A2A / A2G).
                Ok(true)
            }
        }
    }

    /// Static kills: typ from record, else airframe default (no stale slot).
    fn ucid_shooter_is_airframe(
        &self,
        round: RoundId,
        _ucid: Ucid,
        shooter_typ: Option<&str>,
    ) -> Result<bool> {
        if let Some(typ) = shooter_typ.map(str::trim).filter(|t| !t.is_empty()) {
            if let Some(tags) = self.unit_tags_from_session_cfg(round, typ)? {
                return Ok(Self::tags_are_airframe(&tags));
            }
        }
        Ok(true)
    }

    fn finishing_shot<'a>(&self, dead: &'a Dead) -> Option<&'a Shot> {
        let any_hit = dead.shots.iter().any(|s| s.hit);
        if any_hit {
            dead.shots
                .iter()
                .filter(|s| s.hit && s.shooter.unit() != dead.victim.unit())
                .max_by_key(|s| s.time)
                .or_else(|| dead.shots.iter().filter(|s| s.hit).max_by_key(|s| s.time))
        } else {
            dead.shots.iter().max_by_key(|s| s.time)
        }
    }

    fn last_seq_for(
        &self,
        inst: &InstanceState,
        sortie: &Scenario,
    ) -> Result<Option<(RoundId, DateTime<Utc>)>> {
        for r in self.seq.scan_prefix(sortie)?.rev() {
            let ((_, round), seq) = r?;
            if self.round_instance_of(round) == inst.id {
                return Ok(Some((round, seq)));
            }
        }
        Ok(None)
    }

    fn add_stat(
        &self,
        inst: &InstanceState,
        ctx: &mut StatCtx,
        time: DateTime<Utc>,
        stat: Stat,
    ) -> Result<()> {
        if let Some(ctx) = &ctx.0 {
            if time <= ctx.seq {
                return Ok(());
            }
        }
        if let Stat::NewRound { sortie } = &stat {
            ctx.0 = None;
            info!("[{}] processing NewRound sortie={sortie:?}", inst.id);
            match self.last_seq_for(inst, sortie)? {
                None => {
                    info!("NewRound: no existing seq, creating new round");
                    return self.new_round(inst, ctx, time, sortie.clone(), time);
                }
                Some((round, _seq)) => match self.round.get(&(sortie.clone(), round))? {
                    Some(r) if r.end.is_none() => {
                        if Self::round_is_newround_blip(r.start, time) {
                            info!(
                                "NewRound: collapsing restart blip {round:?} before new round (age <= {}h)",
                                Self::ROUND_BLIP_MAX_SECS / 3600
                            );
                            self.delete_round_cascade(sortie, round)?;
                        } else {
                            info!("NewRound: ending stale open round {round:?}, creating new round");
                            let key = (sortie.clone(), round);
                            let mut stale = r;
                            stale.end = Some(time);
                            let _ = self.round.insert(&key, &stale)?;
                        }
                        return self.new_round(inst, ctx, time, sortie.clone(), time);
                    }
                    Some(r) => {
                        let blip_end = r.end.unwrap_or(time);
                        if Self::round_is_newround_blip(r.start, blip_end) {
                            info!(
                                "NewRound: discarding prior restart blip {round:?} (age <= {}h)",
                                Self::ROUND_BLIP_MAX_SECS / 3600
                            );
                            self.delete_round_cascade(sortie, round)?;
                        }
                        info!("NewRound: existing round is ended, creating new round");
                        return self.new_round(inst, ctx, time, sortie.clone(), time);
                    }
                    None => {
                        info!("NewRound: seq entry exists but round missing, creating new round");
                        return self.new_round(inst, ctx, time, sortie.clone(), time);
                    }
                },
            }
        }
        if let Stat::SessionStart {
            cfg,
            sortie: start_sortie,
            ..
        } = &stat
        {
            if ctx.0.is_none() {
                let sortie = inst
                    .current_sortie
                    .lock()
                    .unwrap()
                    .clone()
                    .or_else(|| {
                        let s = start_sortie.trim();
                        if s.is_empty() {
                            None
                        } else {
                            Some(String::from(s))
                        }
                    })
                    .or_else(|| {
                        cfg.netidx_base.as_ref().map(|p| {
                            let s = format!("{p}");
                            String::from(s.rsplit('/').next().unwrap_or("unknown"))
                        })
                    });
                match sortie {
                    Some(sortie) => {
                        let open = self.last_seq_for(inst, &sortie)?.and_then(|(round, seq)| {
                            match self.round.get(&(sortie.clone(), round)) {
                                Ok(Some(r)) if r.end.is_none() => Some((round, seq)),
                                _ => None,
                            }
                        });
                        match open {
                            Some((round, seq)) => {
                                info!(
                                    "[{}] SessionStart: reattaching to open round {round:?} for sortie {sortie:?}",
                                    inst.id
                                );
                                *inst.current_sortie.lock().unwrap() = Some(sortie.clone());
                                ctx.0 = Some(StatCtxInner {
                                    sortie,
                                    round,
                                    seq,
                                });
                            }
                            None => {
                                info!(
                                    "[{}] auto-creating round from SessionStart, sortie={sortie:?}",
                                    inst.id
                                );
                                self.new_round(inst, ctx, time, sortie, time)?;
                            }
                        }
                    }
                    None => {
                        warn!("[{}] SessionStart with no round context and no sortie to derive -- skipping", inst.id);
                    }
                }
            }
        }
        if let Stat::RoundEnd { winner } = &stat {
            return self.round_end(ctx, time, *winner);
        }
        let ctx = match ctx.get_mut() {
            Ok(c) => c,
            Err(_) => return Ok(()), // no NewRound seen yet, skip
        };
        match stat {
            Stat::NewRound { .. } | Stat::RoundEnd { .. } => unreachable!(),
            Stat::SessionStart { stop, cfg, .. } => {
                self.session.insert(
                    &(ctx.round, time),
                    &Session {
                        cfg: (*cfg).clone(),
                        stop_time: stop,
                        end: None,
                    },
                )?;
                // Fresh DCS session — clear connected left from abrupt shutdowns
                // (no Disconnect). Bootstrap Connect stats follow SessionStart.
                let stale: Vec<Ucid> = self
                    .pilots
                    .round_info
                    .iter()
                    .filter_map(|r| r.ok())
                    .filter(|((_, rid), ri)| *rid == ctx.round && ri.connected.is_some())
                    .map(|((ucid, _), _)| ucid)
                    .collect();
                if !stale.is_empty() {
                    info!(
                        "SessionStart: clearing {} stale connected flag(s) in round {:?}",
                        stale.len(),
                        ctx.round
                    );
                }
                for ucid in stale {
                    self.pilots
                        .with_pilot_round_info(ucid, ctx.round, |ri| ri.connected = None)?;
                }
            }
            Stat::SessionEnd {
                api_perf,
                perf,
                frame,
            } => {
                match self
                    .session
                    .scan_prefix(&ctx.round)?
                    .next_back()
                    .transpose()?
                {
                    None => bail!("no session for {} is in progress", &ctx.sortie),
                    Some((k, mut session)) => {
                        session.end = Some(SessionEnd {
                            api: api_perf,
                            engine: perf,
                            frame,
                            time,
                        });
                        self.session.insert(&k, &session)?;
                    }
                }
            }
            Stat::Objective {
                name,
                id,
                pos,
                owner,
                kind,
            } => {
                // bflib re-emits Stat::Objective for every objective on a mission
                // reload. If we already have this objective in the current round,
                // keep its health/logi/supply/fuel rather than resetting to 100 --
                // a fresh ObjectiveHealth only follows when those values change,
                // so clobbering here left the tactical map stuck at 100%.
                let prev = self.objectives.get(&(ctx.round, id))?;
                let (health, logi, supply, fuel, last_change, production, threatened) = match &prev {
                    Some(o) => (
                        o.health,
                        o.logi,
                        o.supply,
                        o.fuel,
                        o.last_change,
                        o.production,
                        o.threatened,
                    ),
                    None => (100, 100, 100, 100, time, 100, false),
                };
                self.objectives.insert(
                    &(ctx.round, id),
                    &Objective {
                        name,
                        pos,
                        kind,
                        owner,
                        by: None,
                        last_change,
                        health,
                        logi,
                        supply,
                        fuel,
                        production,
                        threatened,
                    },
                )?;
            }
            Stat::ObjectiveDestroyed { id } => {
                self.objectives.remove(&(ctx.round, id))?;
            }
            Stat::ObjectiveHealth {
                id,
                last_change,
                health,
                logi,
                production,
                threatened,
            } => {
                self.with_objective((ctx.round, id), |o| {
                    o.last_change = last_change;
                    o.health = health;
                    o.logi = logi;
                    if let Some(p) = production {
                        o.production = p;
                    }
                    if let Some(t) = threatened {
                        o.threatened = t;
                    }
                })?;
            }
            Stat::ObjectiveSupply { id, supply, fuel } => {
                self.with_objective((ctx.round, id), |o| {
                    o.supply = supply;
                    o.fuel = fuel
                })?;
            }
            Stat::Capture { id, by, side } => {
                let objective_name = self
                    .objectives
                    .get(&(ctx.round, id))?
                    .map(|o| o.name.to_string())
                    .unwrap_or_else(|| format!("{:?}", id));
                self.with_objective((ctx.round, id), |o| o.owner = side)?;
                // Track capture count per objective
                let cap_key = (ctx.round, id);
                let prev = self.objective_captures.get(&cap_key)?.unwrap_or(0);
                self.objective_captures.insert(&cap_key, &(prev + 1))?;
                // Record the event itself (who/what/when) -- objective_captures
                // above is just a running total with no attribution or timeline.
                let cid = CaptureId::new(&self.db)?;
                self.captures.insert(
                    &(ctx.round, cid),
                    &CaptureRecord { time, objective_name, side, by: by.clone() },
                )?;
                for ucid in by {
                    self.pilots.with_pilot_and_aggregates(
                        ucid,
                        ctx.round,
                        |pilot| pilot.total.captures += 1,
                        |agg| agg.captures += 1,
                    )?
                }
            }
            Stat::Repair { id: _, by } => {
                self.pilots.with_pilot_and_aggregates(
                    by,
                    ctx.round,
                    |pilot| pilot.total.repairs += 1,
                    |agg| agg.repairs += 1,
                )?;
            }
            Stat::SupplyTransfer { from: _, to: _, by } => {
                self.pilots.with_pilot_and_aggregates(
                    by,
                    ctx.round,
                    |pilot| pilot.total.supply_transfers += 1,
                    |agg| agg.supply_transfers += 1,
                )?;
            }
            Stat::DynamicCargoDelivery { .. } => {
                // Reserved for a future dashboard counter (Supply Runs uses SupplyTransfer).
            }
            Stat::CsarRescue { by, enemy: _ } => {
                self.pilots.bump_csar(by, ctx.round)?;
            }
            Stat::EquipmentInventory { id, item, amount } => {
                self.equipment
                    .fetch_and_update(&(ctx.round, id, item), |_| Some(amount))?;
            }
            Stat::LiquidInventory { id, item, amount } => {
                self.liquids
                    .fetch_and_update(&(ctx.round, id, item), |_| Some(amount))?;
            }
            Stat::Action { by, gid, action } => {
                self.pilots.with_pilot_and_aggregates(
                    by,
                    ctx.round,
                    |p| p.total.actions += 1,
                    |a| a.actions += 1,
                )?;
                if let Some(gid) = gid {
                    self.with_group((ctx.round, gid), |group| {
                        group.kind = GroupKind::Action {
                            by,
                            name: action.clone(),
                        }
                    })?;
                }
            }
            Stat::DeployTroop { by, troop, gid } => {
                self.pilots.with_pilot_and_aggregates(
                    by,
                    ctx.round,
                    |p| p.total.troops += 1,
                    |a| a.troops += 1,
                )?;
                // The group row is created later (from Stat::Unit once the units
                // actually spawn in DCS -- the deploy is queued), so tag it if
                // present but never fail the whole stat over a missing row.
                if let Err(e) = self.with_group((ctx.round, gid), |group| {
                    group.kind = GroupKind::Troop {
                        by,
                        name: troop.clone(),
                    }
                }) {
                    debug!("DeployTroop: group {gid:?} not tracked yet ({e})");
                }
            }
            Stat::DeployGroup {
                by,
                gid,
                deployable,
                aircraft,
                method,
            } => {
                if self.deploy_seen.get(&(ctx.round, gid))?.is_some() {
                    return Ok(());
                }
                self.pilots.with_pilot_and_aggregates(
                    by,
                    ctx.round,
                    |p| p.total.deploys += 1,
                    |a| a.deploys += 1,
                )?;
                // See DeployTroop above -- the group row may not exist yet.
                // Recording the deploy (counter + log) must not depend on it.
                if let Err(e) = self.with_group((ctx.round, gid), |group| {
                    group.kind = GroupKind::Deployed {
                        by,
                        name: deployable.clone(),
                    }
                }) {
                    debug!("DeployGroup: group {gid:?} not tracked yet ({e})");
                }
                let did = DeployId::new(&self.db)?;
                self.deploy_seen.insert(&(ctx.round, gid), &did)?;
                self.deploys.insert(
                    &(by, ctx.round, did),
                    &DeployRecord {
                        time,
                        by,
                        deployable: deployable.to_string(),
                        aircraft: aircraft.map(|a| a.to_string()),
                        method: method.map(|m| m.to_string()),
                    },
                )?;
            }
            Stat::DeployFarp {
                by,
                oid,
                deployable: _,
            } => {
                self.pilots.with_pilot_and_aggregates(
                    by,
                    ctx.round,
                    |p| p.total.farps += 1,
                    |a| a.farps += 1,
                )?;
                self.with_objective((ctx.round, oid), |o| o.by = Some(by))?;
            }
            Stat::Register {
                name,
                id,
                side,
                initial_points,
            } => {
                self.pilots.saw_pilot(id, name)?;
                self.pilots.with_pilot_round_info(id, ctx.round, |ri| {
                    ri.side = (time, side);
                    ri.points = initial_points;
                })?;
            }
            Stat::Sideswitch { id, side } => {
                self.pilots
                    .with_pilot_round_info(id, ctx.round, |ri| ri.side = (time, side))?;
            }
            Stat::Connect { id, addr, name, side } => {
                self.pilots.saw_pilot(id, name)?;
                // Orphan In-flight legs (missed Disconnect/Deslot after client kick).
                self.finalize_orphan_sorties_on_connect(id, time)?;
                // Prefer side from the engine (registered player). Fall back to
                // last known Blue/Red — Register is once-per-campaign so new
                // rounds often start Neutral until Sideswitch/Slot.
                let inherit = side
                    .filter(|s| matches!(s, Side::Blue | Side::Red))
                    .or_else(|| self.pilot_current_side(&id).ok().flatten());
                self.pilots.with_pilot_round_info(id, ctx.round, |ri| {
                    ri.connected = Some((time, addr.clone()));
                    if let Some(side) = inherit.filter(|s| matches!(s, Side::Blue | Side::Red)) {
                        ri.side = (time, side);
                    }
                })?;
            }
            Stat::Disconnect { id } => {
                self.touch_pilot_activity(id, ctx.round, time)?;
                // Same as Deslot: close open Flight Log (belt+suspenders if Deslot is lost).
                self.finalize_open_sorties(id, ctx.round, time, true)?;
                self.pilots
                    .with_pilot_round_info(id, ctx.round, |ri| ri.connected = None)?;
            }
            Stat::Slot { id, slot, typ, side } => {
                // Death/respawn often emits Slot without Deslot — close open legs
                // before wiping the slot.sortie pointer.
                self.finalize_all_open_sorties(id, ctx.round, time, true)?;
                self.touch_pilot_activity(id, ctx.round, time)?;
                // Mirror Deslot: drop previous Player unit row (ghost tracks).
                self.units.remove(&(ctx.round, EnId::Player(id)))?;
                self.pilots.with_pilot_round_info(id, ctx.round, |ri| {
                    ri.slot = Some(Slot {
                        time,
                        id: slot,
                        vehicle: typ.as_ref().map(|u| u.typ.clone()),
                        sortie: None,
                    });
                    if let Some(side) = side.filter(|s| matches!(s, Side::Blue | Side::Red)) {
                        ri.side = (time, side);
                    }
                })?;
            }
            Stat::Deslot { id } => {
                self.touch_pilot_activity(id, ctx.round, time)?;
                // Mid-air quit / death deslot with no Land yet — still credit air time.
                self.finalize_open_sorties(id, ctx.round, time, true)?;
                self.pilots
                    .with_pilot_round_info(id, ctx.round, |ri| ri.slot = None)?;
                self.units.remove(&(ctx.round, EnId::Player(id)))?;
            }
            Stat::Unit {
                id,
                gid,
                owner,
                typ,
                pos,
            } => {
                self.units.fetch_and_update(&(ctx.round, id), |_| {
                    Some(Unit {
                        dead: false,
                        group: gid,
                        owner,
                        typ: typ.typ.clone(),
                        tags: typ.tags,
                        pos,
                    })
                })?;
                if let Some(gid) = gid {
                    self.groups.fetch_and_update(&(ctx.round, gid), |g| {
                        let mut g = g.unwrap_or_default();
                        g.owner = owner;
                        if !g.units.contains(&id) {
                            g.units.push(id);
                        }
                        Some(g)
                    })?;
                }
            }
            Stat::Position { id, pos } => {
                self.with_unit((ctx.round, id), |u| u.pos = pos)?;
                if let EnId::Player(ucid) = id {
                    self.touch_pilot_activity(ucid, ctx.round, time)?;
                }
            }
            Stat::GroupDeleted { id } => {
                if let Some(group) = self.groups.remove(&(ctx.round, id))? {
                    for uid in group.units {
                        self.units.remove(&(ctx.round, uid))?;
                    }
                }
            }
            Stat::Detected {
                id,
                detected,
                source,
            } => {
                self.detected.update_and_fetch(&(ctx.round, id), |d| {
                    let mut d = d.unwrap_or_default();
                    if detected {
                        d.insert(source);
                    } else {
                        d.remove(source);
                    }
                    if d.is_empty() {
                        None
                    } else {
                        Some(d)
                    }
                })?;
            }
            Stat::Takeoff { id } => {
                let dedup_key = (ctx.round, id, time.timestamp_millis());
                if self.sortie_seen.get(&dedup_key)?.is_some() {
                    return Ok(());
                }
                // Prior open legs (missed Kill/Deslot) must not stay In flight.
                self.finalize_all_open_sorties(id, ctx.round, time, true)?;
                let sid = SortieId::new(&self.db)?;
                let mut vehicle = None;
                self.pilots.with_pilot_round_info(id, ctx.round, |ri| {
                    if let Some(sl) = ri.slot.as_mut() {
                        sl.sortie = Some(sid);
                        vehicle = sl.vehicle.clone()
                    }
                })?;
                let vehicle = vehicle.ok_or_else(|| anyhow!("{id} takeoff without slotting"))?;
                // Commit only after slotting succeeds so a premature Takeoff
                // ahead of Stat::Slot stays retryable.
                self.sortie_seen.insert(&dedup_key, &sid)?;
                // Track sortie count per aircraft type
                let ac_key = (ctx.round, vehicle.to_string());
                let (prev_cnt, prev_hrs) = self.aircraft_sorties.get(&ac_key)?.unwrap_or((0, 0.0));
                self.aircraft_sorties.insert(&ac_key, &(prev_cnt + 1, prev_hrs))?;
                self.pilots.sortie.insert(
                    &(id, ctx.round, sid),
                    &Sortie {
                        takeoff: time,
                        land: None,
                        vehicle,
                    },
                )?;
                self.touch_pilot_activity(id, ctx.round, time)?;
            }
            Stat::Land { id } => {
                let mut sid: Option<SortieId> = None;
                self.pilots.with_pilot_round_info(id, ctx.round, |ri| {
                    if let Some(sl) = ri.slot.as_mut() {
                        sid = sl.sortie.take();
                    }
                })?;
                let Some(sid) = sid else {
                    debug!("{id} landed with no active sortie -- orphan or replay, ignoring");
                    return Ok(());
                };
                self.finalize_sortie(id, ctx.round, sid, time, false)?;
                self.touch_pilot_activity(id, ctx.round, time)?;
            }
            Stat::Life { id, lives } => {
                self.pilots.with_pilot_round_info(id, ctx.round, |ri| {
                    ri.lives.clear();
                    ri.lives
                        .extend(lives.into_iter().map(|(lt, (dt, n))| (*lt, *dt, *n)));
                })?;
            }
            Stat::Kill(dead) => self.record_kill(ctx, dead)?,
            Stat::StaticKill {
                by,
                side,
                shooter_typ,
                weapon_name,
                target_typ,
                objective,
                objective_id,
                kind,
                points,
                time,
                owner,
                unit_id,
            } => self.record_static_kill(
                ctx,
                StaticKillRecord {
                    time,
                    by,
                    side,
                    shooter_typ: shooter_typ.map(|s| s.to_string()),
                    weapon_name: weapon_name.map(|s| s.to_string()),
                    target_typ: target_typ.to_string(),
                    objective: objective.to_string(),
                    objective_id,
                    kind,
                    points,
                    owner,
                    unit_id,
                },
            )?,
            Stat::Points {
                id,
                points,
                reason: _,
            } => {
                self.pilots
                    .with_pilot_round_info(id, ctx.round, |ri| ri.points += points)?;
            }
            Stat::PointsTransfer { from, to, points } => {
                self.pilots
                    .with_pilot_round_info(from, ctx.round, |ri| ri.points -= points as i32)?;
                self.pilots.with_pilot_and_aggregates(
                    from,
                    ctx.round,
                    |p| p.total.donated_points += points,
                    |a| a.donated_points += points,
                )?;
                self.pilots
                    .with_pilot_round_info(to, ctx.round, |ri| ri.points += points as i32)?;
            }
            Stat::Bind { id, token } => {
                let token = Uuid::from_str(&token)?;
                let mut remove = None;
                self.pilots.with_pilot(id, |p| {
                    if p.token.is_full() {
                        remove = p.token.pop_at(0);
                    }
                    p.token.push(token)
                })?;
                self.pilots.by_token.insert(&token, &id)?;
                if let Some(token) = remove {
                    self.pilots.by_token.remove(&token)?;
                }
            }
            Stat::PointsTransferToObjective { from: _, to: _, points: _ } => {
                // Not currently tracked in database
            }
            Stat::Weather { temp_c, wind_speed_kts, wind_from_deg, cloud_base_m, qnh_hpa, cloud_density, visibility_m } => {
                if let Ok(mut w) = inst.latest_weather.write() {
                    *w = Some(WeatherSnapshot {
                        temp_c,
                        wind_speed_kts,
                        wind_from_deg,
                        cloud_base_m,
                        qnh_hpa,
                        cloud_density,
                        visibility_m,
                    });
                }
            }
            Stat::ConvoyDestroyed { .. }
            | Stat::CampaignEvent { .. }
            | Stat::PilotXp { .. }
            | Stat::AirRouteDelivered { .. }
            | Stat::AirRouteDestroyed { .. }
            | Stat::SeaRouteDelivered { .. }
            | Stat::SeaRouteDestroyed { .. }
            | Stat::GciPicture(_) => {
                // Future: track in dedicated tables
            }
        };
        self.seq
            .insert(&(ctx.sortie.clone(), ctx.round), &time)?;
        ctx.seq = time;
        Ok(())
    }

    // ── Auth session methods ─────────────────────────────────────────

    pub(crate) fn create_session(&self, id: Uuid, data: SessionData) -> Result<()> {
        self.auth_sessions.insert(&id, &data)?;
        Ok(())
    }

    pub(crate) fn get_session(&self, id: Uuid) -> Result<Option<SessionData>> {
        match self.auth_sessions.get(&id)? {
            None => Ok(None),
            Some(s) if s.expires < Utc::now() => {
                let _ = self.auth_sessions.remove(&id);
                Ok(None)
            }
            Some(s) => Ok(Some(s)),
        }
    }

    pub(crate) fn delete_session(&self, id: Uuid) -> Result<()> {
        self.auth_sessions.remove(&id)?;
        Ok(())
    }

    pub(crate) fn store_oauth_state(&self, state: Uuid, return_to: Option<std::string::String>) -> Result<()> {
        let expires = Utc::now() + chrono::Duration::minutes(10);
        self.auth_states.insert(&state, &OAuthState { expires, return_to })?;
        Ok(())
    }

    /// Consumes the one-time state, returning the stored `return_to` (which
    /// may itself be `None`, if the login started without one) if it was
    /// valid and unexpired -- outer `None` means reject the callback outright.
    pub(crate) fn take_oauth_state(&self, state: Uuid) -> Result<Option<Option<std::string::String>>> {
        match self.auth_states.remove(&state)? {
            None => Ok(None),
            Some(s) if s.expires > Utc::now() => Ok(Some(s.return_to)),
            Some(_) => Ok(None),
        }
    }

    pub(crate) fn list_sessions(&self) -> Result<Vec<(Uuid, SessionData)>> {
        let now = Utc::now();
        let mut out = Vec::new();
        for item in self.auth_sessions.iter() {
            let (id, data) = item?;
            if data.expires > now {
                out.push((id, data));
            }
        }
        Ok(out)
    }

    // ── Trail point methods ──────────────────────────────────────────

    pub(crate) fn append_trail_point(
        &self,
        round_id: RoundId,
        unit_id: &std::string::String,
        ts: i64,
        lat: f64,
        lon: f64,
        alt: f64,
        hdg: f64,
    ) -> Result<()> {
        self.trail_points.insert(&(round_id, unit_id.clone(), ts), &(lat, lon, alt, hdg))?;
        Ok(())
    }

    pub(crate) fn get_trail_points(&self, round_id: RoundId) -> Result<Vec<TrailPoint>> {
        // Keep last 30 minutes of trail history
        let cutoff = Utc::now().timestamp() - 1800;
        let mut points = Vec::new();
        for item in self.trail_points.range(
            (round_id, std::string::String::new(), cutoff)..,
        )? {
            let ((rid, unit_id, ts), (lat, lon, alt, hdg)) = item?;
            if rid != round_id {
                break;
            }
            if ts >= cutoff {
                points.push(TrailPoint { unit_id, lat, lon, alt, hdg, ts });
            }
        }
        Ok(points)
    }

    /// Clear only the `session` tree (per-round Cfg snapshot + perf history),
    /// leaving rounds/kills/objectives/pilots untouched. Use this to recover
    /// from old `Session` records that predate a bincode-incompatible change
    /// to `Cfg`/`Deployable` (mid-struct field insertions break positional
    /// decoding for anything serialized under the old layout, surfacing as
    /// "string is not valid utf8" errors from /api/admin/perf and
    /// /api/admin/banned, which both read the latest session's Cfg).
    pub(crate) fn clear_stale_sessions(&self) -> Result<()> {
        self.session.clear()?;
        Ok(())
    }

    /// Wipe all campaign data — rounds, kills, objectives, pilot stats, trails,
    /// captures, sorties, weather — while preserving auth sessions and Discord links
    /// so admins remain logged in and pilot linking is not lost.
    pub(crate) fn reset_campaign_data(&self) -> Result<()> {
        // Pilot stat trees
        self.pilots.pilots.clear()?;
        self.pilots.aggregates.clear()?;
        self.pilots.by_name.clear()?;
        self.pilots.sortie.clear()?;
        self.pilots.sortie_crashed.clear()?;
        self.pilots.agg_ship_kills.clear()?;
        self.pilots.pilot_ship_kills.clear()?;
        self.pilots.agg_ground_air_kills.clear()?;
        self.pilots.pilot_ground_air_kills.clear()?;
        self.pilots.agg_ground_ground_kills.clear()?;
        self.pilots.pilot_ground_ground_kills.clear()?;
        self.pilots.agg_ground_ship_kills.clear()?;
        self.pilots.pilot_ground_ship_kills.clear()?;
        self.pilots.agg_csar.clear()?;
        self.pilots.pilot_csar.clear()?;
        self.pilots.round_info.clear()?;
        // Round / mission trees
        self.seq.clear()?;
        self.round.clear()?;
        self.session.clear()?;
        // Combat trees
        self.kills.clear()?;
        self.shared_kills.clear()?;
        self.kill_seen.clear()?;
        self.static_kills.clear()?;
        self.static_kill_seen.clear()?;
        self.sortie_seen.clear()?;
        self.deploy_seen.clear()?;
        self.units.clear()?;
        self.groups.clear()?;
        self.detected.clear()?;
        // Objectives
        self.objectives.clear()?;
        self.equipment.clear()?;
        self.liquids.clear()?;
        // Captures & sorties
        self.objective_captures.clear()?;
        self.captures.clear()?;
        self.deploys.clear()?;
        self.aircraft_sorties.clear()?;
        self.pilot_last_activity.clear()?;
        // Trails & weather
        self.trail_points.clear()?;
        self.round_instance.clear()?;
        for st in self.0.states.values() {
            if let Ok(mut w) = st.latest_weather.write() {
                *w = None;
            }
            if let Some(path) = &st.stats_jsonl {
                let end = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                self.jsonl_cursor.insert(&st.id.to_string(), &end)?;
            }
        }
        // auth_sessions, auth_states → preserved
        Ok(())
    }

    /// Wipe derived stats trees and rewind cursors so the next start (or the
    /// live JSONL loop) re-ingests from the top with idempotency guards.
    pub(crate) fn rebuild_stats_from_archive(&self) -> Result<()> {
        self.wipe_stats_derived_trees()?;
        self.replay_cursor.clear()?;
        self.jsonl_cursor.clear()?;
        for st in self.0.states.values() {
            *st.current_sortie.lock().unwrap() = None;
        }
        Ok(())
    }

    fn wipe_stats_derived_trees(&self) -> Result<()> {
        self.pilots.pilots.clear()?;
        self.pilots.aggregates.clear()?;
        self.pilots.by_name.clear()?;
        self.pilots.sortie.clear()?;
        self.pilots.sortie_crashed.clear()?;
        self.pilots.agg_ship_kills.clear()?;
        self.pilots.pilot_ship_kills.clear()?;
        self.pilots.agg_ground_air_kills.clear()?;
        self.pilots.pilot_ground_air_kills.clear()?;
        self.pilots.agg_ground_ground_kills.clear()?;
        self.pilots.pilot_ground_ground_kills.clear()?;
        self.pilots.agg_ground_ship_kills.clear()?;
        self.pilots.pilot_ground_ship_kills.clear()?;
        self.pilots.agg_csar.clear()?;
        self.pilots.pilot_csar.clear()?;
        self.pilots.round_info.clear()?;
        self.seq.clear()?;
        self.round.clear()?;
        self.session.clear()?;
        self.kills.clear()?;
        self.shared_kills.clear()?;
        self.kill_seen.clear()?;
        self.static_kills.clear()?;
        self.static_kill_seen.clear()?;
        self.sortie_seen.clear()?;
        self.deploy_seen.clear()?;
        self.units.clear()?;
        self.groups.clear()?;
        self.detected.clear()?;
        self.objectives.clear()?;
        self.equipment.clear()?;
        self.liquids.clear()?;
        self.objective_captures.clear()?;
        self.captures.clear()?;
        self.deploys.clear()?;
        self.aircraft_sorties.clear()?;
        self.pilot_last_activity.clear()?;
        self.trail_points.clear()?;
        self.jsonl_sealed.clear()?;
        self.round_instance.clear()?;
        for st in self.0.states.values() {
            if let Ok(mut w) = st.latest_weather.write() {
                *w = None;
            }
        }
        Ok(())
    }

    /// Queue in-process JSONL rebuild (next jsonl_loop tick) for every instance.
    pub(crate) fn request_jsonl_rebuild(&self) -> Result<()> {
        let mut any = false;
        for st in self.0.states.values() {
            if st.stats_jsonl.is_some() {
                any = true;
                self.jsonl_cursor.insert(&st.id.to_string(), &0u64)?;
                st.jsonl_reset.store(true, Ordering::SeqCst);
            }
        }
        if !any {
            bail!("no stats.jsonl configured -- use --rebuild-stats with --stats-jsonl offline");
        }
        {
            let mut st = self.0.rebuild_status.lock().unwrap();
            st.phase = "queued".into();
            st.active = true;
            st.started_at = Some(Utc::now());
            st.finished_at = None;
        }
        Ok(())
    }

    pub(crate) fn rebuild_status_snapshot(&self) -> RebuildStatusInner {
        self.0.rebuild_status.lock().unwrap().clone()
    }

    fn aliases_path(&self) -> Option<PathBuf> {
        self.default_state()
            .stats_jsonl
            .as_ref()
            .map(|p| crate::ucid_alias::UcidAliasTable::aliases_path_for_jsonl(p))
    }

    pub(crate) fn aliases_path_public(&self) -> Option<PathBuf> {
        self.aliases_path()
    }

    /// UCIDs currently merged into another account (open alias, source side).
    fn merged_away_ucids(&self) -> std::collections::HashSet<Ucid> {
        let Some(path) = self.aliases_path() else {
            return std::collections::HashSet::new();
        };
        match crate::ucid_alias::UcidAliasTable::load(&path) {
            Ok(t) => t.active_sources().into_iter().collect(),
            Err(e) => {
                error!("failed to load UCID aliases for leaderboard filter: {e:?}");
                std::collections::HashSet::new()
            }
        }
    }

    pub(crate) fn list_ucid_aliases(&self) -> Result<Vec<crate::ucid_alias::AliasRecord>> {
        let Some(path) = self.aliases_path() else {
            return Ok(Vec::new());
        };
        Ok(crate::ucid_alias::UcidAliasTable::load(&path)?.records().to_vec())
    }

    /// Merge UCID `from` into `to`: alias file + ban `from` + queue JSONL rebuild.
    pub(crate) fn merge_ucid(
        &self,
        from: &str,
        to: &str,
        note: &str,
    ) -> Result<()> {
        let path = self
            .aliases_path()
            .ok_or_else(|| anyhow!("no stats.jsonl configured"))?;
        let (from_u, _to_u) = crate::ucid_alias::validate_distinct_ucids(from, to)?;
        let table = crate::ucid_alias::UcidAliasTable::load(&path)?;
        if table.active_target(&from_u).is_some() {
            bail!("UCID {from} already has an open merge; revoke it first");
        }
        let rec = crate::ucid_alias::AliasRecord {
            ts: Utc::now(),
            op: crate::ucid_alias::AliasOp::Merge,
            from: from.to_string(),
            to: to.to_string(),
            note: note.to_string(),
        };
        crate::ucid_alias::UcidAliasTable::append(&path, &rec)?;
        let name = self
            .pilot_name(&from_u)
            .unwrap_or_else(|| from.to_string());
        self.ban_player(
            from_u,
            BanRecord {
                name,
                banned_at: Utc::now(),
                until: None,
                reason: format!("merged into {to} / account migration."),
            },
        )?;
        self.request_jsonl_rebuild()?;
        Ok(())
    }

    /// Revoke open merge for `from`: alias until-cut + unban + queue rebuild.
    pub(crate) fn revoke_ucid_merge(&self, from: &str, note: &str) -> Result<()> {
        let path = self
            .aliases_path()
            .ok_or_else(|| anyhow!("no stats.jsonl configured"))?;
        let from_u = from
            .parse::<Ucid>()
            .map_err(|e| anyhow!("invalid from ucid: {e}"))?;
        let table = crate::ucid_alias::UcidAliasTable::load(&path)?;
        let to_u = table
            .active_target(&from_u)
            .ok_or_else(|| anyhow!("no open merge for UCID {from}"))?;
        let rec = crate::ucid_alias::AliasRecord {
            ts: Utc::now(),
            op: crate::ucid_alias::AliasOp::Revoke,
            from: from.to_string(),
            to: to_u.to_string(),
            note: note.to_string(),
        };
        crate::ucid_alias::UcidAliasTable::append(&path, &rec)?;
        let _ = self.unban_player(&from_u)?;
        self.request_jsonl_rebuild()?;
        Ok(())
    }
}
