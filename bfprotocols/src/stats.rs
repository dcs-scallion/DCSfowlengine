use crate::{
    cfg::{Cfg, LifeType, UnitTags, Vehicle},
    db::{
        group::{GroupId, UnitId},
        objective::{ObjectiveId, ObjectiveKind},
    },
    perf::PerfInner,
    shots::Dead,
};
use chrono::prelude::*;
use dcso3::{
    coalition::Side,
    coord::LLPos,
    net::{SlotId, Ucid},
    perf::{HistogramSer, PerfInner as ApiPerfInner},
    warehouse::LiquidType,
    String, Vector3,
};
use enumflags2::bitflags;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;
use std::fmt;

/// the path in the archive of the stats
pub const PATH: &str = "/stats";

pub type MapS<K, V> = immutable_chunkmap::map::Map<K, V, 16>;

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum EnId {
    Player(Ucid),
    Unit(UnitId),
}

impl fmt::Display for EnId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unit {
    pub typ: Vehicle,
    pub tags: UnitTags,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Pos {
    pub pos: LLPos,
    pub velocity: Vector3,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[bitflags]
#[repr(u8)]
pub enum DetectionSource {
    EWR,
    Jtac,
}

/// Destroyed ME static credited via `static_dead` (not unit ShotDb / `Stat::Kill`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StaticKillKind {
    Production,
    ObjectiveStatic,
}

/// Discord live-map "War losses" row (victim-side). Dashboard aggregates `Stat::WarLoss`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarLossBucket {
    PlanesStandard,
    PlanesIntercept,
    PlanesAttack,
    PlanesRecon,
    PlanesBomber,
    PlanesTanker,
    PlanesAwacs,
    PlanesLog,
    Drones,
    HelicoptersAtk,
    HelicoptersLog,
    Armored,
    Artillery,
    Troops,
    Utility,
    Aaa,
    SamSr,
    SamMr,
    SamLr,
    EwrRadars,
    ShipsSmall,
    ShipsMedium,
    Carriers,
    BuildingLogi,
    BuildingProd,
    BuildingStatic,
}

impl WarLossBucket {
    pub const ALL: [WarLossBucket; 26] = [
        Self::PlanesStandard,
        Self::PlanesIntercept,
        Self::PlanesAttack,
        Self::PlanesRecon,
        Self::PlanesBomber,
        Self::PlanesTanker,
        Self::PlanesAwacs,
        Self::PlanesLog,
        Self::Drones,
        Self::HelicoptersAtk,
        Self::HelicoptersLog,
        Self::Armored,
        Self::Artillery,
        Self::Troops,
        Self::Utility,
        Self::Aaa,
        Self::SamSr,
        Self::SamMr,
        Self::SamLr,
        Self::EwrRadars,
        Self::ShipsSmall,
        Self::ShipsMedium,
        Self::Carriers,
        Self::BuildingLogi,
        Self::BuildingProd,
        Self::BuildingStatic,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::PlanesStandard => "planes_standard",
            Self::PlanesIntercept => "planes_intercept",
            Self::PlanesAttack => "planes_attack",
            Self::PlanesRecon => "planes_recon",
            Self::PlanesBomber => "planes_bomber",
            Self::PlanesTanker => "planes_tanker",
            Self::PlanesAwacs => "planes_awacs",
            Self::PlanesLog => "planes_log",
            Self::Drones => "drones",
            Self::HelicoptersAtk => "helicopters_atk",
            Self::HelicoptersLog => "helicopters_log",
            Self::Armored => "armored",
            Self::Artillery => "artillery",
            Self::Troops => "troops",
            Self::Utility => "utility",
            Self::Aaa => "aaa",
            Self::SamSr => "sam_sr",
            Self::SamMr => "sam_mr",
            Self::SamLr => "sam_lr",
            Self::EwrRadars => "ewr_radars",
            Self::ShipsSmall => "ships_small",
            Self::ShipsMedium => "ships_medium",
            Self::Carriers => "carriers",
            Self::BuildingLogi => "building_logi",
            Self::BuildingProd => "building_prod",
            Self::BuildingStatic => "building_static",
        }
    }

    /// Same labels as Discord interactive map War losses.
    pub fn label(self) -> &'static str {
        match self {
            Self::PlanesStandard => "Planes standard",
            Self::PlanesIntercept => "Planes intercept",
            Self::PlanesAttack => "Planes attack",
            Self::PlanesRecon => "Planes recon",
            Self::PlanesBomber => "Planes bomber",
            Self::PlanesTanker => "Planes tanker",
            Self::PlanesAwacs => "Planes awacs",
            Self::PlanesLog => "Planes log",
            Self::Drones => "Drones",
            Self::HelicoptersAtk => "Helicopters atk",
            Self::HelicoptersLog => "Helicopters log",
            Self::Armored => "Armored",
            Self::Artillery => "Artillery",
            Self::Troops => "Troops",
            Self::Utility => "Utility",
            Self::Aaa => "AAA",
            Self::SamSr => "SAM SR",
            Self::SamMr => "SAM MR",
            Self::SamLr => "SAM LR",
            Self::EwrRadars => "EWR radars",
            Self::ShipsSmall => "Ships small",
            Self::ShipsMedium => "Ships medium",
            Self::Carriers => "Carriers",
            Self::BuildingLogi => "Building logi",
            Self::BuildingProd => "Building prod",
            Self::BuildingStatic => "Building static",
        }
    }

    pub fn divider_before(self) -> bool {
        matches!(
            self,
            Self::HelicoptersAtk | Self::Armored | Self::ShipsSmall | Self::BuildingLogi
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Stat {
    NewRound {
        sortie: String,
    },
    RoundEnd {
        winner: Option<Side>,
    },
    SessionStart {
        stop: Option<DateTime<Utc>>,
        cfg: Box<Cfg>,
        /// Mission Sortie name (miz). Used by bfdb offline ingest when
        /// `netidx_base` is unset so SessionStart can open/reattach a round
        /// without a preceding NewRound (load-from-save path).
        #[serde(default)]
        sortie: String,
    },
    SessionEnd {
        api_perf: ApiPerfInner,
        perf: PerfInner,
        frame: HistogramSer,
    },
    Objective {
        name: String,
        id: ObjectiveId,
        pos: LLPos,
        owner: Side,
        kind: ObjectiveKind,
    },
    Capture {
        id: ObjectiveId,
        by: SmallVec<[Ucid; 1]>,
        side: Side,
    },
    Repair {
        id: ObjectiveId,
        by: Ucid,
    },
    SupplyTransfer {
        from: ObjectiveId,
        to: ObjectiveId,
        by: Ucid,
    },
    /// Successful player dynamic-cargo delivery (absorb / To stock). Dashboard wiring later;
    /// Supply Runs also receive a paired `SupplyTransfer` on the same event.
    DynamicCargoDelivery {
        from: ObjectiveId,
        to: ObjectiveId,
        by: Ucid,
        #[serde(default)]
        weight_kg: f64,
    },
    /// One downed pilot delivered to a friendly base (coalition or POW).
    CsarRescue {
        by: Ucid,
        enemy: bool,
    },
    EquipmentInventory {
        id: ObjectiveId,
        item: String,
        amount: u32,
    },
    LiquidInventory {
        id: ObjectiveId,
        item: LiquidType,
        amount: u32,
    },
    Action {
        by: Ucid,
        gid: Option<GroupId>,
        action: String,
    },
    DeployTroop {
        by: Ucid,
        troop: String,
        gid: GroupId,
    },
    DeployGroup {
        by: Ucid,
        gid: GroupId,
        deployable: String,
        #[serde(default)]
        aircraft: Option<String>,
        #[serde(default)]
        method: Option<String>,
    },
    DeployFarp {
        by: Ucid,
        oid: ObjectiveId,
        deployable: String,
    },
    ObjectiveHealth {
        id: ObjectiveId,
        last_change: DateTime<Utc>,
        health: u8,
        logi: u8,
        /// OPR capacity % (Production sites). Older JSONL omit this.
        #[serde(default)]
        production: Option<u8>,
        /// Threat flag for dashboard/map. Older JSONL omit this.
        #[serde(default)]
        threatened: Option<bool>,
    },
    ObjectiveSupply {
        id: ObjectiveId,
        supply: u8,
        fuel: u8,
    },
    ObjectiveDestroyed {
        id: ObjectiveId,
    },
    Register {
        name: String,
        id: Ucid,
        side: Side,
        initial_points: i32,
    },
    Sideswitch {
        id: Ucid,
        side: Side,
    },
    Connect {
        id: Ucid,
        addr: String,
        name: String,
        /// Player coalition when already registered (lobby/slot). Older JSONL omit this.
        #[serde(default)]
        side: Option<Side>,
    },
    Disconnect {
        id: Ucid,
    },
    Slot {
        id: Ucid,
        slot: SlotId,
        typ: Option<Unit>,
        /// Occupying player's coalition. Older JSONL omit this.
        #[serde(default)]
        side: Option<Side>,
    },
    Deslot {
        id: Ucid,
    },
    Unit {
        id: EnId,
        gid: Option<GroupId>,
        owner: Side,
        typ: Unit,
        pos: Pos,
    },
    GroupDeleted {
        id: GroupId,
    },
    Position {
        id: EnId,
        pos: Pos,
    },
    Detected {
        id: EnId,
        detected: bool,
        source: DetectionSource,
    },
    Takeoff {
        id: Ucid,
    },
    Land {
        id: Ucid,
    },
    Life {
        id: Ucid,
        lives: MapS<LifeType, (DateTime<Utc>, u8)>,
    },
    Kill(Dead),
    /// Victim-side war-loss bucket (Discord War losses taxonomy). Dashboard source of truth.
    WarLoss {
        side: Side,
        bucket: WarLossBucket,
        #[serde(default = "war_loss_count_one")]
        count: u32,
        time: DateTime<Utc>,
    },
    /// ME objective static or OPR factory destroyed by a player (not unit ShotDb).
    StaticKill {
        by: Ucid,
        side: Side,
        #[serde(default)]
        shooter_typ: Option<String>,
        #[serde(default)]
        weapon_name: Option<String>,
        target_typ: String,
        objective: String,
        objective_id: ObjectiveId,
        kind: StaticKillKind,
        #[serde(default)]
        points: i32,
        time: DateTime<Utc>,
        owner: Side,
        unit_id: UnitId,
    },
    Points {
        id: Ucid,
        /// Delta applied this event (`0` for absolute sync-only rows).
        points: i32,
        /// Absolute balance after this event. Missing in legacy JSONL → bfdb uses `+= points`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        balance: Option<i32>,
        reason: String,
    },
    PointsTransfer {
        from: Ucid,
        to: Ucid,
        points: u32,
    },
    PointsTransferToObjective {
        from: Ucid,
        to: ObjectiveId,
        points: u32,
    },
    Bind {
        id: Ucid,
        token: String,
    },
    /// Dashboard web-account claim / password-recovery code (`-linkme <code>`).
    WebClaim {
        id: Ucid,
        code: String,
    },
    ConvoyDestroyed {
        from: ObjectiveId,
        to: ObjectiveId,
        side: Side,
        killer: Option<Ucid>,
    },
    AirRouteDelivered {
        from: ObjectiveId,
        to: ObjectiveId,
        side: Side,
    },
    AirRouteDestroyed {
        from: ObjectiveId,
        to: ObjectiveId,
        side: Side,
    },
    SeaRouteDelivered {
        from: ObjectiveId,
        to: ObjectiveId,
        side: Side,
    },
    SeaRouteDestroyed {
        from: ObjectiveId,
        to: ObjectiveId,
        side: Side,
    },
    CampaignEvent {
        event_type: String,
        side: Side,
    },
    PilotXp {
        id: Ucid,
        xp: u32,
        reason: String,
    },
    Weather {
        temp_c: f64,
        wind_speed_kts: f64,
        wind_from_deg: f64,
        cloud_base_m: f64,
        qnh_hpa: f64,
        #[serde(default)]
        cloud_density: Option<u8>,
        #[serde(default)]
        visibility_m: Option<f64>,
    },
    GciPicture(crate::gci::GciPicture),
}

fn war_loss_count_one() -> u32 {
    1
}
