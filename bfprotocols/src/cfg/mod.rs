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

use anyhow::{anyhow, bail, Context, Result};
use chrono::prelude::*;
use compact_str::format_compact;
use dcso3::{coalition::Side, controller::AltType, net::Ucid, String};
use enumflags2::{bitflags, BitFlags};
use fxhash::{FxBuildHasher, FxHashMap, FxHashSet};
use indexmap::IndexMap;
use netidx::path::Path as NetIdxPath;
use regex::Regex;
use crate::db::objective::ObjectiveKind;
use serde_derive::{Deserialize, Serialize};
use std::{
    borrow::Borrow,
    fmt,
    fs::{self, File},
    io,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    str::FromStr,
};

mod example;

#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Vehicle(pub String);

impl fmt::Display for Vehicle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<'a> From<&'a str> for Vehicle {
    fn from(value: &'a str) -> Self {
        Self(value.into())
    }
}

impl From<String> for Vehicle {
    fn from(value: String) -> Self {
        Vehicle(value)
    }
}

impl Borrow<str> for Vehicle {
    fn borrow(&self) -> &str {
        &*self.0
    }
}

impl Vehicle {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Rule {
    Whitelist { allowed: FxHashMap<Ucid, String> },
    Blacklist { denied: FxHashMap<Ucid, String> },
    AlwaysAllowed,
    NeverAllowed,
}

impl Default for Rule {
    fn default() -> Self {
        Self::AlwaysAllowed
    }
}

impl Rule {
    pub fn check(&self, ucid: &Ucid) -> bool {
        match self {
            Self::Whitelist { allowed } => allowed.contains_key(ucid),
            Self::Blacklist { denied } => !denied.contains_key(&ucid),
            Self::AlwaysAllowed => true,
            Self::NeverAllowed => false,
        }
    }

    #[allow(dead_code)]
    pub fn blacklist(&mut self, ucid: Ucid, name: String) {
        match self {
            Self::Blacklist { denied } => {
                denied.insert(ucid, name);
            }
            Self::Whitelist { allowed } => {
                allowed.remove(&ucid);
            }
            Self::AlwaysAllowed => {
                let denied = FxHashMap::from_iter([(ucid, name)]);
                *self = Self::Blacklist { denied };
            }
            Self::NeverAllowed => (),
        }
    }

    #[allow(dead_code)]
    pub fn whitelist(&mut self, ucid: Ucid, name: String) {
        match self {
            Self::Blacklist { denied } => {
                denied.remove(&ucid);
            }
            Self::Whitelist { allowed } => {
                allowed.insert(ucid, name);
            }
            Self::NeverAllowed => {
                let allowed = FxHashMap::from_iter([(ucid, name)]);
                *self = Self::Whitelist { allowed };
            }
            Self::AlwaysAllowed => (),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[bitflags]
#[repr(u64)]
pub enum UnitTag {
    SAM,
    AAA,
    Armor,
    APC,
    Logistics,
    Infantry,
    EWR,
    Aircraft,
    Helicopter,
    LR,
    SR,
    MR,
    IRGuided,
    RadarGuided,
    OpticallyGuided,
    EngagesWeapons,
    Unguided,
    TrackRadar,
    SearchRadar,
    AuxRadarUnit,
    ControlUnit,
    Launcher,
    ATGM,
    Artillery,
    LightCannon,
    HeavyCannon,
    RPG,
    SmallArms,
    Unarmed,
    Invincible,
    Driveable,
    AWACS,
    Link16,
    Boat,
    CALCM,
    NavalSpawnPoint,
    /// `shipsNoHeliport_kill` — plain naval hull (author assigns per DCS type).
    ShipNoHeliport,
    /// `shipsWithHeliport_kill` — naval hull with heliport and warehouse role.
    ShipWithHeliport,
    /// `carrier_kill` — aircraft carrier hull tier.
    ShipCarrier,
    /// OPR production factory static (`production_factory_units`).
    Factory,
    /// Objective ME static (`objective_static_units`).
    Structure,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(from = "Vec<UnitTag>", into = "Vec<UnitTag>")]
pub struct UnitTags(pub BitFlags<UnitTag>);

impl fmt::Display for UnitTags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let len = self.0.len();
        write!(f, "[")?;
        for (i, tag) in self.0.iter().enumerate() {
            if i < len - 1 {
                write!(f, "{tag:?}, ")?
            } else {
                write!(f, "{tag:?}")?
            }
        }
        write!(f, "]")
    }
}

impl Deref for UnitTags {
    type Target = BitFlags<UnitTag>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for UnitTags {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<Vec<UnitTag>> for UnitTags {
    fn from(value: Vec<UnitTag>) -> Self {
        Self(value.into_iter().collect())
    }
}

impl From<BitFlags<UnitTag>> for UnitTags {
    fn from(value: BitFlags<UnitTag>) -> Self {
        Self(value)
    }
}

impl Into<Vec<UnitTag>> for UnitTags {
    fn into(self) -> Vec<UnitTag> {
        self.0.into_iter().collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash)]
pub enum LifeType {
    Standard,
    Intercept,
    Logistics,
    Attack,
    Recon,
    /// Combined Arms: player controlling a coalition deployable, troop, or objective ground unit.
    CombinedArms,
}

impl fmt::Display for LifeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Standard => "standard",
            Self::Intercept => "intercept",
            Self::Logistics => "logistics",
            Self::Attack => "attack",
            Self::Recon => "recon",
            Self::CombinedArms => "combined arms",
        };
        write!(f, "{s}")
    }
}

impl LifeType {
    pub fn up(&self) -> Option<LifeType> {
        match self {
            LifeType::Recon => Some(LifeType::Logistics),
            LifeType::Logistics => Some(LifeType::Intercept),
            LifeType::Intercept => Some(LifeType::Attack),
            LifeType::Attack => Some(LifeType::Standard),
            LifeType::Standard => None,
            LifeType::CombinedArms => None,
        }
    }

    #[allow(dead_code)]
    pub fn down(&self) -> Option<LifeType> {
        match self {
            LifeType::Recon => None,
            LifeType::Logistics => Some(LifeType::Recon),
            LifeType::Intercept => Some(LifeType::Logistics),
            LifeType::Attack => Some(LifeType::Attack),
            LifeType::Standard => Some(LifeType::Attack),
            LifeType::CombinedArms => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PersistTyp {
    /// The deployable persists until it is destroyed
    Forever,
    /// The deployable doesn't persist across restarts
    UntilRestart,
    /// The deployable persists for the specified number of
    /// real world seconds
    WallTime(f32),
    /// The deployable persists for the the specified number
    /// of server restart cycles
    Restarts(u32),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LimitEnforceTyp {
    /// Handle the limit by removing the oldest instance of the deployable when
    /// a new one is unpacked. (lifo)
    DeleteOldest,
    /// Handle the limit by refusing to spawn new construction crates for
    /// the deployable
    DenyCrate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Crate {
    /// The name of the crate in the menu
    pub name: String,
    /// The weight of the crate in kg
    pub weight: u32,
    /// The number of crates of this type required to build the deployable
    pub required: u32,
    /// The type of unit in the associated deployable group that will inherit
    /// this crate's position when the deployable is spawned. This is only
    /// needed for multi unit groups with distinct parts.
    pub pos_unit: Option<String>,
    /// the maximum height in meters agl that the user can drop this crate from
    pub max_drop_height_agl: u32,
    /// the maximum speed in m/s that the user can be going when they drop this
    /// cargo
    pub max_drop_speed: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployableObjective {
    pub pad_templates: Vec<String>,
    #[serde(default)]
    pub defenses_template: Option<String>,
    #[serde(default)]
    pub ammo_template: Option<String>,
    #[serde(default)]
    pub fuel_template: Option<String>,
    #[serde(default)]
    pub barracks_template: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct DeployableEwr {
    /// range for likely detection (Meters)
    pub range: u32,
    // CR estokes: Actual radar simulation ...
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub enum EwrMode {
    /// Original EWR implementation with immediate track updates
    Original,
    /// EWR with configurable delay on track updates
    Delayed,
}

impl Default for EwrMode {
    fn default() -> Self {
        Self::Original
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeployableJtac {
    /// jtac detection and lasing range (Meters)
    pub range: u32,
    /// if true line of sight checks are not required, the jtac will
    /// see every unit in range regardless of terrain or cover
    #[serde(default)]
    pub nolos: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeployableKind {
    Group { template: String },
    Objective(DeployableObjective),
}

impl DeployableKind {
    pub fn is_group(&self) -> bool {
        match self {
            Self::Group { .. } => true,
            Self::Objective(_) => false,
        }
    }

    pub fn is_objective(&self) -> bool {
        match self {
            Self::Objective(_) => true,
            Self::Group { .. } => false,
        }
    }
}

fn default_deployable_kind() -> DeployableKind {
    DeployableKind::Group {
        template: "".into(),
    }
}

fn default_deployable_spawn_delay_secs() -> u32 {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Deployable {
    /// The full menu path of the deployable in the menu
    pub path: Vec<String>,
    /// The type of deployable
    #[serde(default = "default_deployable_kind")]
    pub kind: DeployableKind,
    /// How the deployable should persist across restarts
    pub persist: PersistTyp,
    /// How many instances are allowed at the same time
    pub limit: u32,
    /// How to deal with it when the max number of instances are deployed and
    /// a player wants to deploy a new instance
    pub limit_enforce: LimitEnforceTyp,
    /// What crates are required to build the deployable
    pub crates: Vec<Crate>,
    /// Can the damaged deployable be repaired, and if so, by which crate.
    pub repair_crate: Option<Crate>,
    /// How much does the damaged deployable cost to repair
    #[serde(default)]
    pub repair_cost: u32,
    /// How many points does this deployable cost (if any)
    #[serde(default)]
    pub cost: u32,
    /// Seconds between unpack and unit spawn (FARP-style clear window). `0` = immediate.
    #[serde(default = "default_deployable_spawn_delay_secs")]
    pub spawn_delay_secs: u32,
    /// Is this unit an early warning radar
    pub ewr: Option<DeployableEwr>,
    /// Is this unit a jtac
    pub jtac: Option<DeployableJtac>,
    #[serde(default)]
    #[serde(rename = "template")]
    pub deprecated_template: Option<String>,
    #[serde(default)]
    #[serde(rename = "logistics")]
    pub deprecated_logistics: Option<DeployableObjective>,
}

impl Deployable {
    /// ME ship group name for TISP / FowlTools: `Group`, carrier-style `Objective.pad_templates`, or legacy top-level `"template"`.
    pub fn provides_tisp_ship_template(&self, ship_template: &str) -> bool {
        let from_kind = match &self.kind {
            DeployableKind::Group { template } => template.as_str() == ship_template,
            DeployableKind::Objective(parts) => parts
                .pad_templates
                .iter()
                .any(|p| p.as_str() == ship_template),
        };
        let from_legacy = self
            .deprecated_template
            .as_ref()
            .map(|t| t.as_str())
            == Some(ship_template);
        from_kind || from_legacy
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Troop {
    /// The name of the squad in the menu
    pub name: String,
    /// The name of the template used to spawn the group
    pub template: String,
    /// How the troops will persist
    pub persist: PersistTyp,
    /// Can the troops capture objectives?
    pub can_capture: bool,
    /// How many simultaneous instances of the group are allowed
    pub limit: u32,
    /// How to deal with it when the max number of instances are deployed and the user
    /// wants to deploy an additional instance
    pub limit_enforce: LimitEnforceTyp,
    /// How much weight does the group add to the carrier unit
    pub weight: u32,
    /// How many points does this troop cost
    #[serde(default)]
    pub cost: u32,
    /// Can laser designate and scout
    pub jtac: Option<DeployableJtac>,
    /// May walk to and detain an enemy downed pilot for CSAR extraction.
    #[serde(default)]
    pub can_capture_csar: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CargoConfig {
    /// How many troop slots does this vehicle have
    pub troop_slots: u8,
    /// How many crate slots does this vehicle have
    pub crate_slots: u8,
    /// How many total troops and crates can this vehicle carry.
    /// e.g. if troop_slots is 1, crate_slots is 1, and total_slots is 1
    /// then the vehicle can carry either a troop or a crate but not both.
    pub total_slots: u16,
}

/// Hub-to-objective virtual resupply efficiency vs distance (exponential decay with floor).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VirtualResupplyDecayConfig {
    /// Reference distance in km where efficiency equals `efficiency_at_reference_pct`.
    #[serde(default = "default_virtual_resupply_decay_reference_distance_km")]
    pub reference_distance_km: u32,
    /// Delivery efficiency (0-100) at `reference_distance_km`.
    #[serde(default = "default_virtual_resupply_decay_efficiency_at_reference_pct")]
    pub efficiency_at_reference_pct: u8,
    /// Minimum delivery efficiency (0-100); never zero supply from distance alone.
    #[serde(default = "default_virtual_resupply_decay_efficiency_floor_pct")]
    pub efficiency_floor_pct: u8,
}

fn default_virtual_resupply_decay_reference_distance_km() -> u32 {
    250
}

fn default_virtual_resupply_decay_efficiency_at_reference_pct() -> u8 {
    25
}

fn default_virtual_resupply_decay_efficiency_floor_pct() -> u8 {
    3
}

impl Default for VirtualResupplyDecayConfig {
    fn default() -> Self {
        Self {
            reference_distance_km: default_virtual_resupply_decay_reference_distance_km(),
            efficiency_at_reference_pct: default_virtual_resupply_decay_efficiency_at_reference_pct(),
            efficiency_floor_pct: default_virtual_resupply_decay_efficiency_floor_pct(),
        }
    }
}

impl VirtualResupplyDecayConfig {
    fn decay_rate(&self) -> f64 {
        let floor = f64::from(self.efficiency_floor_pct);
        let at_ref = f64::from(self.efficiency_at_reference_pct);
        let ref_km = f64::from(self.reference_distance_km.max(1));
        let numer = (at_ref - floor).max(f64::EPSILON);
        let denom = (100.0 - floor).max(f64::EPSILON);
        -((numer / denom).ln()) / ref_km
    }

    /// Whole-percent delivery efficiency at `distance_km` (hub center to objective center).
    pub fn efficiency_at_distance_km(&self, distance_km: f64) -> u8 {
        let floor = f64::from(self.efficiency_floor_pct);
        let at_ref = f64::from(self.efficiency_at_reference_pct);
        if distance_km <= 0. {
            return 100;
        }
        if at_ref <= floor {
            return self.efficiency_floor_pct;
        }
        let eff = floor + (100.0 - floor) * (-self.decay_rate() * distance_km).exp();
        eff.round().clamp(floor, 100.) as u8
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WarehouseConfig {
    /// Logistics hub max supply stock as a multiple of the delivery amount
    pub hub_max: u32,
    /// Airbase max supply stock as a multiple of the delivery amount
    pub airbase_max: u32,
    /// FOB (OFO objectives): stock multiple of delivery amount (typically lower than airfields).
    #[serde(default = "default_fob_max")]
    pub fob_max: u32,
    /// FARP objectives (incl. DEP* template / player-deployed FARPs): stock multiple vs base row.
    #[serde(default = "default_fob_max")]
    pub farp_max: u32,
    /// Airbase on water / carrier deck (OAB with surface water under zone center): multiple of delivery.
    #[serde(default = "default_carrier_airbase_max")]
    pub carrier_airbase_max: u32,
    /// Logistics tick in minutes. Supplies move automatically every tick
    pub tick: u32,
    /// How many logistics ticks does it take before supplies are delivered
    /// from outside
    pub ticks_per_delivery: u32,
    /// Hub→FARP **non-airframe** Supply % gate (weapons + other; 1–100). Default 100.
    /// Fuel stays at 100%. Ground DEP FARPs; naval mobile pads use carrier.
    #[serde(default = "default_tick_kind_minimum_global_supply_percentage")]
    pub tick_farp_minimum_global_supply_percentage: u8,
    /// Hub→FOB (OFO) non-airframe Supply % gate (1–100). Default 100.
    #[serde(default = "default_tick_kind_minimum_global_supply_percentage")]
    pub tick_fob_minimum_global_supply_percentage: u8,
    /// Hub→carrier non-airframe Supply % gate (1–100). Default 100.
    #[serde(default = "default_tick_kind_minimum_global_supply_percentage")]
    pub tick_carrier_minimum_global_supply_percentage: u8,
    /// Hub→land airbase non-airframe Supply % gate (1–100). Default 100.
    #[serde(default = "default_tick_kind_minimum_global_supply_percentage")]
    pub tick_airbase_minimum_global_supply_percentage: u8,
    /// Occupied hub non-airframe Supply % gate (1–100). Default 100.
    #[serde(default = "default_tick_kind_minimum_global_supply_percentage")]
    pub tick_occupiedhub_minimum_global_supply_percentage: u8,
    /// Hub→FARP **airframe** Supply % gate (1–100). Default 100.
    #[serde(default = "default_tick_kind_minimum_global_supply_percentage")]
    pub tick_farp_minimum_airframe_global_supply_percentage: u8,
    /// Hub→FOB airframe Supply % gate (1–100). Default 100.
    #[serde(default = "default_tick_kind_minimum_global_supply_percentage")]
    pub tick_fob_minimum_airframe_global_supply_percentage: u8,
    /// Hub→carrier airframe Supply % gate (1–100). Default 100.
    #[serde(default = "default_tick_kind_minimum_global_supply_percentage")]
    pub tick_carrier_minimum_airframe_global_supply_percentage: u8,
    /// Hub→land airbase airframe Supply % gate (1–100). Default 100.
    #[serde(default = "default_tick_kind_minimum_global_supply_percentage")]
    pub tick_airbase_minimum_airframe_global_supply_percentage: u8,
    /// Occupied hub airframe Supply % gate (1–100). Default 100.
    #[serde(default = "default_tick_kind_minimum_global_supply_percentage")]
    pub tick_occupiedhub_minimum_airframe_global_supply_percentage: u8,
    /// The supply transfer crate
    pub supply_transfer_crate: FxHashMap<Side, Crate>,
    /// The percentage of supply that is transfered by a transfer crate
    pub supply_transfer_size: u8,
    /// The name of the warehouse that is the source of supply every
    /// restart
    pub supply_source: FxHashMap<Side, String>,
    /// Airframes that do not play nice with the warehouse that are exempt from the
    /// warehouse check
    #[serde(default)]
    pub exempt_airframes: FxHashSet<String>,
    /// Initial stock percentage (0-100) for player-deployed dynamic FARPs.
    /// Applied to both equipment and liquids at FARP creation time.
    #[serde(
        default = "default_dynamic_farps_initial_stock_percentage",
        rename = "dynamicFARPs_InitialStockPercentage"
    )]
    pub dynamic_farps_initial_stock_percentage: u8,
    /// After capture of an OFO (Fob): fill still-empty template rows to this % of capacity (0-100).
    #[serde(
        default = "default_captured_stock_percentage",
        rename = "CapturedStockPercentage_OFO"
    )]
    pub captured_stock_percentage_ofo: u8,
    /// After capture of an OAB (Airbase): fill still-empty template rows to this % of capacity (0-100).
    #[serde(
        default = "default_captured_stock_percentage",
        rename = "CapturedStockPercentage_OAB"
    )]
    pub captured_stock_percentage_oab: u8,
    /// After capture of an OLO (Logistics hub): fill still-empty template rows to this % of capacity (0-100).
    #[serde(
        default = "default_captured_stock_percentage",
        rename = "CapturedStockPercentage_OLO"
    )]
    pub captured_stock_percentage_olo: u8,
}

fn default_front_line_grid_size_meters() -> f64 {
    2500.
}

fn default_fob_max() -> u32 {
    1
}

fn default_carrier_airbase_max() -> u32 {
    1
}

fn default_dynamic_farps_initial_stock_percentage() -> u8 {
    100
}

fn default_captured_stock_percentage() -> u8 {
    0
}

fn default_tick_kind_minimum_global_supply_percentage() -> u8 {
    100
}

impl WarehouseConfig {
    /// Non-airframe (weapons + other) destination Supply % gate (clamped 1–100).
    /// Occupied hubs: use `tick_occupiedhub_*` via the caller.
    pub fn tick_minimum_global_supply_percentage_for(
        &self,
        kind: &ObjectiveKind,
        airbase_on_water: bool,
    ) -> u8 {
        let raw = match kind {
            ObjectiveKind::Fob => self.tick_fob_minimum_global_supply_percentage,
            ObjectiveKind::Farp { mobile: true, .. } => {
                self.tick_carrier_minimum_global_supply_percentage
            }
            ObjectiveKind::Farp { .. } => self.tick_farp_minimum_global_supply_percentage,
            ObjectiveKind::Airbase => {
                if airbase_on_water {
                    self.tick_carrier_minimum_global_supply_percentage
                } else {
                    self.tick_airbase_minimum_global_supply_percentage
                }
            }
            ObjectiveKind::Logistics | ObjectiveKind::Production => 100,
        };
        raw.clamp(1, 100)
    }

    /// Airframe-only destination Supply % gate (clamped 1–100).
    pub fn tick_minimum_airframe_global_supply_percentage_for(
        &self,
        kind: &ObjectiveKind,
        airbase_on_water: bool,
    ) -> u8 {
        let raw = match kind {
            ObjectiveKind::Fob => self.tick_fob_minimum_airframe_global_supply_percentage,
            ObjectiveKind::Farp { mobile: true, .. } => {
                self.tick_carrier_minimum_airframe_global_supply_percentage
            }
            ObjectiveKind::Farp { .. } => self.tick_farp_minimum_airframe_global_supply_percentage,
            ObjectiveKind::Airbase => {
                if airbase_on_water {
                    self.tick_carrier_minimum_airframe_global_supply_percentage
                } else {
                    self.tick_airbase_minimum_airframe_global_supply_percentage
                }
            }
            ObjectiveKind::Logistics | ObjectiveKind::Production => 100,
        };
        raw.clamp(1, 100)
    }

    pub fn tick_occupiedhub_minimum_global_supply_percentage_clamped(&self) -> u8 {
        self.tick_occupiedhub_minimum_global_supply_percentage
            .clamp(1, 100)
    }

    pub fn tick_occupiedhub_minimum_airframe_global_supply_percentage_clamped(&self) -> u8 {
        self.tick_occupiedhub_minimum_airframe_global_supply_percentage
            .clamp(1, 100)
    }

    /// `airbase_on_water`: true when objective is `Airbase` and zone center is over water (carrier).
    pub fn capacity_multiplier(&self, kind: &ObjectiveKind, airbase_on_water: bool) -> u32 {
        match kind {
            ObjectiveKind::Logistics => self.hub_max,
            ObjectiveKind::Fob => self.fob_max,
            ObjectiveKind::Airbase => {
                if airbase_on_water {
                    self.carrier_airbase_max
                } else {
                    self.airbase_max
                }
            }
            ObjectiveKind::Farp { .. } => self.farp_max,
            ObjectiveKind::Production => 0,
        }
    }

    pub fn capacity(&self, kind: &ObjectiveKind, airbase_on_water: bool, qty: u32) -> u32 {
        qty.saturating_mul(self.capacity_multiplier(kind, airbase_on_water))
    }
}

fn default_tk_window() -> u32 {
    24
}

/// ME static in OFO / OLO / OAB zones (`objective_static_units` CFG map key = DCS unit type).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectiveStaticUnitCfg {
    /// O+kind letters from trigger zone names: `FO`, `LO`, `AB`.
    pub zones: FxHashSet<String>,
    /// Points awarded to the enemy coalition when this static is destroyed.
    pub kill_points: u32,
    /// DCS max HP (`getLife0`); ME static repair queue orders lowest first when not cached on unit.
    #[serde(default)]
    pub max_life: Option<i64>,
}

impl ObjectiveStaticUnitCfg {
    pub fn allows_kind(&self, kind: &ObjectiveKind) -> bool {
        self.zones
            .iter()
            .any(|z| objective_zone_kind_matches(z, kind))
    }
}

/// `FO` / `LO` / `AB` letters from O+kind coalition objective zone names.
pub fn objective_zone_kind_matches(zone_letter: &str, kind: &ObjectiveKind) -> bool {
    matches!(
        (zone_letter, kind),
        ("FO", ObjectiveKind::Fob)
            | ("LO", ObjectiveKind::Logistics)
            | ("AB", ObjectiveKind::Airbase)
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PointsCfg {
    /// Bonus issued to new players when they register
    pub new_player_join: u32,
    /// Points awarded for each air kill
    pub air_kill: u32,
    /// Base points awared for each ground kill
    pub ground_kill: u32,
    /// Points for destroying a naval unit without heliport and warehouse
    #[serde(rename = "shipsNoHeliport_kill")]
    pub ships_no_heliport_kill: u32,
    /// Points for destroying a naval unit with heliport and warehouse (not an aircraft carrier)
    #[serde(rename = "shipsWithHeliport_kill")]
    pub ships_with_heliport_kill: u32,
    /// Points for destroying an aircraft carrier with warehouse
    pub carrier_kill: u32,
    /// Points for destroying a static factory unit in an OPR* zone
    pub production_kill: u32,
    /// Points for destroying an EWR radar unit
    pub ewr_kill: u32,
    /// Points for destroying an AWACS aircraft
    pub awacs_kill: u32,
    /// Bonus points awarded to heavy sam kills
    pub lr_sam_bonus: u32,
    /// Points for repairing base logistics (negative deducts from the player)
    pub logistics_repair: i32,
    /// Points awarded for logistics transfers
    pub logistics_transfer: u32,
    /// Capture reward: OFO* FOB and deployable FARP
    pub capture_fob: u32,
    /// Capture reward: OAB* airbase
    pub capture_airbase: u32,
    /// Capture reward: OLO* logistics hub
    pub capture_hub: u32,
    /// How many hours before previous team kills are forgotten for
    /// the purposes of computing the penalty of a team kill.
    #[serde(default = "default_tk_window")]
    pub tk_window: u32,
    /// If provisional is true then points earned in a sortie are only
    /// committed to the player's points balance when they land at a
    /// friendly objective
    #[serde(default)]
    pub provisional: bool,
    /// If strict is true then the player cannot take off when their
    /// loadout or airframe costs more points than they have. They
    /// will be deleted on takeoff, and no points or lives will be
    /// deducted. If struct is false then the player's points will go
    /// negative if they take off with an airframe/loadout that
    /// exceeds their current balance.
    #[serde(default)]
    pub strict: bool,
    /// How many points does it cost to slot in a given airframe. This
    /// need not cover all airframes on the server, and the default is 0.
    #[serde(default)]
    pub airframe_cost: FxHashMap<Vehicle, u32>,
    /// How many points does it cost to load a given weapon. This need
    /// not cover all weapons, and the default is zero.
    #[serde(default)]
    pub weapon_cost: FxHashMap<String, u32>,
    /// Points to the rescuer for delivering a same-coalition downed pilot.
    #[serde(default)]
    pub csar_delivery_coalition_pilot: u32,
    /// Points to the rescuer for delivering a captured enemy downed pilot.
    #[serde(default)]
    pub csar_delivery_enemy_pilot: u32,
    /// How many points do connected players automatically gain per
    /// time interval. This is a pair of the number of points with the
    /// interval in seconds. The number of points CAN be negative, the
    /// interval must be positive. The default is (0, 0)
    #[serde(default)]
    pub periodic_point_gain: (i32, u32),
    /// Scale periodic awards by opposing-team player count so smaller
    /// teams gain more per player. Requires positive `periodic_point_gain.0`;
    /// when enabled, periodic awards are never negative (other point
    /// deductions — team kills, airborne deslot, deploy costs, etc. — are
    /// unchanged).
    #[serde(default)]
    pub balancing_point_gain: bool,
    /// When true, periodic awards are paid only to players airborne in an
    /// aircraft or helicopter (`unit.in_air()` at payout). Applies whenever
    /// `periodic_point_gain` pays out (with or without `balancing_point_gain`).
    #[serde(default)]
    pub periodic_award_airborne: bool,
}

impl PointsCfg {
    pub fn capture_points_for(&self, kind: &ObjectiveKind) -> u32 {
        match kind {
            ObjectiveKind::Fob | ObjectiveKind::Farp { .. } => self.capture_fob,
            ObjectiveKind::Airbase => self.capture_airbase,
            ObjectiveKind::Logistics => self.capture_hub,
            ObjectiveKind::Production => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum AiPlaneKind {
    FixedWing,
    Helicopter,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiPlaneCfg {
    pub kind: AiPlaneKind,
    pub duration: Option<u32>,
    pub template: String,
    pub altitude: f64,
    pub altitude_typ: AltType,
    pub speed: f64,
    #[serde(default)]
    pub freq: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwacsCfg {
    pub ewr: DeployableEwr,
    pub plane: AiPlaneCfg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BomberCfg {
    pub targets: u32,
    pub power: u32,
    // in meters radius around the target point
    pub accuracy: u32,
    pub plane: AiPlaneCfg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployableCfg {
    pub name: String,
    pub plane: Option<AiPlaneCfg>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DroneCfg {
    pub jtac: DeployableJtac,
    pub plane: AiPlaneCfg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NukeCfg {
    /// using a nuke reduces the cost of nukes for everyone by this
    /// factor. e.g. cost_scale: 4, with initial cost 1000. The first
    /// nuke would cost 1000 points. The next nuke would cost 250
    /// points. The next nuke would cost 62 points, and so on until a
    /// nuke costs 1 point at which point it stops scaling.
    pub cost_scale: u8,
    /// in Kilotons of TNT
    pub power: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveCfg {
    /// max distance for troop moves in meters per unit cost
    pub troop: u32,
    /// max distance for ground deployable moves in meters per unit cost
    pub deployable: u32,
    /// naval deployable / mobile FARP step; defaults to `deployable` when omitted
    #[serde(default)]
    pub deployable_naval: Option<u32>,
}

impl MoveCfg {
    pub fn deployable_naval(&self) -> u32 {
        self.deployable_naval.unwrap_or(self.deployable)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ActionKind {
    Tanker(AiPlaneCfg),
    Awacs(AwacsCfg),
    Bomber(BomberCfg),
    Fighters(AiPlaneCfg),
    Attackers(AiPlaneCfg),
    Sead(AiPlaneCfg),
    CruiseMissileSpawn(AiPlaneCfg),
    CruiseMissileWaypoint,
    Drone(DroneCfg),
    Nuke(NukeCfg),
    FighersWaypoint,
    AttackersWaypoint,
    SeadWaypoint,
    DroneWaypoint,
    TankerWaypoint,
    AwacsWaypoint,
    Paratrooper(DeployableCfg),
    Deployable(DeployableCfg),
    LogisticsRepair(AiPlaneCfg),
    LogisticsTransfer(AiPlaneCfg),
    Move(MoveCfg),
    Rtb,
    /// Resume AI air unit from `AwaitingLaunch` to last waypoint (`-action start <gid>`).
    Start,
    /// Live fuel and pylon store report for an action group (`-action status <gid>`).
    Status,
    /// Rearm from hub warehouse (`-action rearm <gid>`).
    Rearm,
}

impl ActionKind {
    pub fn is_calcm_deploy(&self) -> bool {
        matches!(
            self,
            ActionKind::CruiseMissileSpawn(_) | ActionKind::CruiseMissileWaypoint
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ActionGeoLimit {
    Unlimited,
    /// This action can only be run within `max` in meters of a friendly objective
    NearFriendlyObjective {
        max: u32,
    },
}

impl Default for ActionGeoLimit {
    fn default() -> Self {
        Self::Unlimited
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub kind: ActionKind,
    pub cost: u32,
    pub penalty: Option<u32>,
    pub limit: Option<u32>,
    /// defines where this action is allowed to run
    #[serde(default)]
    pub geo_limit: ActionGeoLimit,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Rules {
    /// who can use actions
    pub actions: Rule,
    /// who gets the cargo menu
    pub cargo: Rule,
    /// who gets the troops menu
    pub troops: Rule,
    /// who gets the jtac menu
    pub jtac: Rule,
    /// who can access the jtac slots
    pub ca: Rule,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NameFilter(Regex);

impl TryFrom<String> for NameFilter {
    type Error = anyhow::Error;

    fn try_from(value: String) -> Result<Self> {
        Ok(Self(Regex::new(&value)?))
    }
}

impl TryFrom<&str> for NameFilter {
    type Error = anyhow::Error;

    fn try_from(value: &str) -> Result<Self> {
        Ok(Self(Regex::new(value)?))
    }
}

impl Into<String> for NameFilter {
    fn into(self) -> String {
        self.0.as_str().into()
    }
}

impl NameFilter {
    /// Check if a name is allowed
    pub fn check(&self, name: &str) -> bool {
        self.0.is_match(name)
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum VictoryCondition {
    /// Victory is triggered when the specified percentage of the map
    /// is owned by a given team, or is neutral. Every objective is
    /// considered equally in this calculation. Must be between 0 and 1
    MapOwned { fraction: f64 },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AutoResetOnVictory {
    /// What victory condition triggers an automatic reset
    pub condition: VictoryCondition,
    /// How long, in seconds, must the condition hold before reset is
    /// tiggered
    pub delay: u32,
}

fn default_msgs_per_second() -> usize {
    5
}

fn default_cull_after() -> u32 {
    1800
}

fn default_groups_spawn_queue_stretch() -> u32 {
    5
}

fn default_deployable_unpack_min_base_distance_m() -> u32 {
    0
}

fn default_production_logistics_exclusion() -> u32 {
    10000
}

fn default_production_ground_vehicle_cull_distance() -> u32 {
    10000
}

fn default_lock_sides() -> bool {
    true
}

fn default_limited_lives() -> bool {
    true
}

fn default_lives_birth() -> bool {
    false
}

fn default_airborne_deslot_block() -> bool {
    false
}

fn default_airborne_deslot_penalty_secs() -> u32 {
    300
}

fn default_airborne_deslot_penalty_points() -> u32 {
    0
}

fn default_discord_map_width() -> u32 {
    1280
}

fn default_discord_map_style() -> String {
    String::from("mapbox/dark-v11")
}

fn default_discord_map_retina() -> bool {
    true
}

fn default_discord_map_http_port() -> u16 {
    17841
}

/// Interactive map HTML refresh when the mission has connected players.
pub const DISCORD_MAP_REFRESH_INTERVAL_MIN_SECS: u32 = 30;
pub const DISCORD_MAP_REFRESH_INTERVAL_MAX_SECS: u32 = 300;
pub const DISCORD_MAP_REFRESH_INTERVAL_DEFAULT_SECS: u32 = 120;

fn default_rounds_per_day() -> u32 {
    1
}

fn default_top10_a2_closed() -> u32 {
    3
}

fn default_top10_a2_open() -> u32 {
    10
}

fn default_top10_g2_closed() -> u32 {
    1
}

fn default_top10_g2s_closed() -> u32 {
    0
}

fn default_top10_g2_open() -> u32 {
    10
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct DiscordMapCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub webhook_url: Option<String>,
    #[serde(default)]
    pub mapbox_access_token: Option<String>,
    #[serde(default = "default_discord_map_style")]
    pub style: String,
    /// Mapbox request width (1–1280); height is computed from ME corner zones.
    #[serde(default = "default_discord_map_width")]
    pub width: u32,
    #[serde(default = "default_discord_map_retina")]
    pub retina: bool,
    #[serde(default)]
    pub padding: u32,
    /// Read-only interactive map HTTP listen port (`0.0.0.0`, GET `/map` and `/map.png` only).
    /// Used as the Discord link only when `public_map_url` is empty.
    #[serde(default = "default_discord_map_http_port")]
    pub http_port: u16,
    /// Public HTTPS URL for Discord / player links (e.g. `https://fowl-ta.duckdns.org/map`).
    /// Empty: fall back to `http://{bind_address}:{http_port}/map`. One URL per DCS server.
    #[serde(default)]
    pub public_map_url: String,
    /// Draw campaign front line on interactive `/map` HTML (SVG only, not Discord PNG).
    /// Requires `front_line: true` in CFG root; validated at mission start.
    #[serde(default)]
    pub front_line_in_map: bool,
    /// Show Tacview ACMI download link in interactive map header (icon + label).
    #[serde(default)]
    pub dowload_acmi: bool,
    /// Public URL to Tacview ACMI downloads (Google Drive folder or file list).
    #[serde(default)]
    pub dowload_acmi_url: String,
    /// Interactive map header link (empty = hidden).
    #[serde(default)]
    pub discord_url: String,
    #[serde(default)]
    pub stats_url: String,
    #[serde(default)]
    pub manual_url: String,
    #[serde(default)]
    pub bugs_report_url: String,
    /// Periodic interactive map refresh while players are connected (30–300 s; else 120).
    #[serde(default)]
    pub refresh_interval_secs: Option<u32>,
    /// Persisted coalition counters for the interactive map right sidebar.
    #[serde(default)]
    pub campaign_stats: bool,
    /// Persisted per-player Top 10 boards for the interactive map left sidebar.
    #[serde(default)]
    pub campaign_top10: bool,
    /// Always-visible rows when the board is collapsed (`0` = all hidden until expand).
    #[serde(default = "default_top10_a2_closed")]
    pub campaign_top10_A2A_closed: u32,
    /// Expanded row count and title number (`Top N Killboard`).
    #[serde(default = "default_top10_a2_open")]
    pub campaign_top10_A2A_open: u32,
    #[serde(default = "default_top10_a2_closed")]
    pub campaign_top10_A2G_closed: u32,
    #[serde(default = "default_top10_a2_open")]
    pub campaign_top10_A2G_open: u32,
    #[serde(default = "default_top10_a2_closed")]
    pub campaign_top10_A2S_closed: u32,
    #[serde(default = "default_top10_a2_open")]
    pub campaign_top10_A2S_open: u32,
    #[serde(default = "default_top10_a2_closed")]
    pub campaign_top10_LOG_closed: u32,
    #[serde(default = "default_top10_a2_open")]
    pub campaign_top10_LOG_open: u32,
    #[serde(default = "default_top10_g2_closed")]
    pub campaign_top10_G2A_closed: u32,
    #[serde(default = "default_top10_g2_open")]
    pub campaign_top10_G2A_open: u32,
    #[serde(default = "default_top10_g2_closed")]
    pub campaign_top10_G2G_closed: u32,
    #[serde(default = "default_top10_g2_open")]
    pub campaign_top10_G2G_open: u32,
    #[serde(default = "default_top10_g2s_closed")]
    pub campaign_top10_G2S_closed: u32,
    #[serde(default = "default_top10_g2_open")]
    pub campaign_top10_G2S_open: u32,
    /// Campaign day length: `1 + (campaign_rounds - 1) / rounds_per_day`.
    #[serde(default = "default_rounds_per_day")]
    pub rounds_per_day: u32,
}

impl Default for DiscordMapCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            webhook_url: None,
            mapbox_access_token: None,
            style: default_discord_map_style(),
            width: default_discord_map_width(),
            retina: true,
            padding: 0,
            http_port: default_discord_map_http_port(),
            public_map_url: Default::default(),
            front_line_in_map: false,
            dowload_acmi: false,
            dowload_acmi_url: Default::default(),
            discord_url: Default::default(),
            stats_url: Default::default(),
            manual_url: Default::default(),
            bugs_report_url: Default::default(),
            refresh_interval_secs: None,
            campaign_stats: false,
            campaign_top10: false,
            campaign_top10_A2A_closed: default_top10_a2_closed(),
            campaign_top10_A2A_open: default_top10_a2_open(),
            campaign_top10_A2G_closed: default_top10_a2_closed(),
            campaign_top10_A2G_open: default_top10_a2_open(),
            campaign_top10_A2S_closed: default_top10_a2_closed(),
            campaign_top10_A2S_open: default_top10_a2_open(),
            campaign_top10_LOG_closed: default_top10_a2_closed(),
            campaign_top10_LOG_open: default_top10_a2_open(),
            campaign_top10_G2A_closed: default_top10_g2_closed(),
            campaign_top10_G2A_open: default_top10_g2_open(),
            campaign_top10_G2G_closed: default_top10_g2_closed(),
            campaign_top10_G2G_open: default_top10_g2_open(),
            campaign_top10_G2S_closed: default_top10_g2s_closed(),
            campaign_top10_G2S_open: default_top10_g2_open(),
            rounds_per_day: default_rounds_per_day(),
        }
    }
}

/// Discord / player HTTPS URL. Empty string means use `http://{bind_address}:{http_port}/map`.
pub fn normalize_discord_map_public_url(raw: &str) -> Result<Option<std::string::String>> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let lower = t.to_ascii_lowercase();
    if !lower.starts_with("https://") {
        bail!("discord_map.public_map_url must start with https://");
    }
    let hostport = t[8..].split('/').next().unwrap_or("");
    if hostport.is_empty() || (hostport.starts_with('[') && !hostport.contains(']')) {
        bail!("discord_map.public_map_url needs a hostname");
    }
    if hostport == "localhost" || hostport.starts_with("127.") || hostport == "[::1]" {
        bail!("discord_map.public_map_url must be a public hostname, not {hostport}");
    }
    let mut url = t.trim_end_matches('/').to_string();
    let last = url.rsplit('/').next().unwrap_or("");
    if !last.eq_ignore_ascii_case("map") {
        url.push_str("/map");
    }
    Ok(Some(url))
}

impl DiscordMapCfg {
    /// Normalized refresh interval for periodic map posts when players are online.
    pub fn refresh_interval_secs(&self) -> u32 {
        match self.refresh_interval_secs {
            Some(v) if (DISCORD_MAP_REFRESH_INTERVAL_MIN_SECS..=DISCORD_MAP_REFRESH_INTERVAL_MAX_SECS)
                .contains(&v) =>
            {
                v
            }
            _ => DISCORD_MAP_REFRESH_INTERVAL_DEFAULT_SECS,
        }
    }

    /// Interactive map front line requires both discord_map and campaign front_line CFG flags.
    pub fn front_line_map_active(&self, front_line: bool) -> bool {
        self.front_line_in_map && front_line
    }

    pub fn validate_enabled(&self, front_line: bool) -> Result<()> {
        if self.front_line_in_map && !front_line {
            bail!("discord_map.front_line_in_map requires front_line: true in CFG");
        }
        if !self.enabled {
            return Ok(());
        }
        if self
            .webhook_url
            .as_ref()
            .is_none_or(|s| s.trim().is_empty())
        {
            bail!("discord_map.enabled requires discord_map.webhook_url in CFG");
        }
        if self
            .mapbox_access_token
            .as_ref()
            .is_none_or(|s| s.trim().is_empty())
        {
            bail!("discord_map.enabled requires discord_map.mapbox_access_token in CFG");
        }
        if self.width == 0 || self.width > crate::discord_map_viewport::MAPBOX_STATIC_MAX_PX
        {
            bail!(
                "discord_map.width must be 1..={}",
                crate::discord_map_viewport::MAPBOX_STATIC_MAX_PX
            );
        }
        if self.http_port == 0 {
            bail!("discord_map.http_port must be > 0 when discord_map.enabled");
        }
        normalize_discord_map_public_url(&self.public_map_url)?;
        if self.campaign_stats && self.rounds_per_day == 0 {
            bail!("discord_map.rounds_per_day must be >= 1 when discord_map.campaign_stats is true");
        }
        self.validate_campaign_top10_sizes()?;
        Ok(())
    }

    /// `closed` must be strictly less than `open` for every Top 10 board.
    pub fn validate_campaign_top10_sizes(&self) -> Result<()> {
        if !self.campaign_top10 {
            return Ok(());
        }
        let boards = [
            (
                "A2A",
                self.campaign_top10_A2A_closed,
                self.campaign_top10_A2A_open,
            ),
            (
                "A2G",
                self.campaign_top10_A2G_closed,
                self.campaign_top10_A2G_open,
            ),
            (
                "A2S",
                self.campaign_top10_A2S_closed,
                self.campaign_top10_A2S_open,
            ),
            (
                "LOG",
                self.campaign_top10_LOG_closed,
                self.campaign_top10_LOG_open,
            ),
            (
                "G2A",
                self.campaign_top10_G2A_closed,
                self.campaign_top10_G2A_open,
            ),
            (
                "G2G",
                self.campaign_top10_G2G_closed,
                self.campaign_top10_G2G_open,
            ),
            (
                "G2S",
                self.campaign_top10_G2S_closed,
                self.campaign_top10_G2S_open,
            ),
        ];
        for (name, closed, open) in boards {
            if closed >= open {
                bail!(
                    "discord_map.campaign_top10_{name}_closed ({closed}) must be < campaign_top10_{name}_open ({open})"
                );
            }
        }
        Ok(())
    }
}

fn default_acmi_post_round_delay_secs() -> u32 {
    60
}

/// Post-round Tacview ACMI sanitization (standalone process_inbox.bat on the DCS host).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcmiSanitizeCfg {
    /// Absolute path to process_inbox.bat. Empty or omitted: bflib does not spawn.
    #[serde(default)]
    pub process_inbox_bat: Option<String>,
    /// Seconds to wait before sanitize when spawned from bflib (1..=1800).
    #[serde(default = "default_acmi_post_round_delay_secs")]
    pub post_round_delay_secs: u32,
}

impl Default for AcmiSanitizeCfg {
    fn default() -> Self {
        Self {
            process_inbox_bat: None,
            post_round_delay_secs: default_acmi_post_round_delay_secs(),
        }
    }
}

impl AcmiSanitizeCfg {
    pub fn validate(&self) -> Result<()> {
        if self
            .process_inbox_bat
            .as_ref()
            .is_some_and(|p| !p.trim().is_empty())
            && !(1..=1800).contains(&self.post_round_delay_secs)
        {
            bail!(
                "acmi_sanitize.post_round_delay_secs {} out of range 1-1800",
                self.post_round_delay_secs
            );
        }
        Ok(())
    }
}

/// Writedir housekeeping run once at mission start (`Lfs.writedir()`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServerMaintenanceCfg {
    /// Delete `.trk` under `Tracks/Multiplayer` older than this many days (mtime).
    /// Omit / null: disabled. `0` is treated as `1` (minimum retain age).
    #[serde(default)]
    pub tracks_multiplayer_retain_days: Option<u32>,
    /// Delete `.txt` / `.log` under `Logs` older than this many days (mtime).
    /// Omit / null: disabled. `0` is treated as `1` (minimum retain age).
    #[serde(default)]
    pub logs_multiplayer_retain_days: Option<u32>,
}

fn default_mission_datetime_post_round_delay_secs() -> u32 {
    15
}

fn default_mission_date_on_new_campaign() -> MissionDateOnNewCampaign {
    MissionDateOnNewCampaign::Reset
}

/// ME calendar when a new campaign starts (`setmissionstartdatetime`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MissionDateOnNewCampaign {
    /// Jump ME date back to `mission_date_base` (or date currently in the .miz).
    Reset,
    /// Keep advancing ME date across campaign wipes (no jump back).
    Continue,
}

/// Cycle ME mission start time (and optionally date) after each round via external script.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetMissionStartDatetimeCfg {
    #[serde(default)]
    pub enabled: bool,
    /// Absolute path to `setmissionstartdatetime.BAT` (or `.py`).
    #[serde(default, alias = "script_path")]
    pub skript_path: Option<String>,
    /// Wall-clock start times of day to rotate, e.g. `["06:00", "15:00"]`.
    #[serde(default)]
    pub mission_start_time_cycle: Vec<String>,
    /// Base ME calendar date (`YYYY-MM-DD`). Used when `discord_map.campaign_stats` is true.
    /// If omitted, the date currently stored in the `.miz` is used as the base.
    #[serde(default)]
    pub mission_date_base: Option<String>,
    #[serde(default = "default_mission_date_on_new_campaign")]
    pub mission_date_on_new_campaign: MissionDateOnNewCampaign,
    /// Wait after MissionEnd before rewriting the `.miz` (DCS file lock). 1..=1800.
    #[serde(default = "default_mission_datetime_post_round_delay_secs")]
    pub post_round_delay_secs: u32,
}

impl Default for SetMissionStartDatetimeCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            skript_path: None,
            mission_start_time_cycle: Vec::new(),
            mission_date_base: None,
            mission_date_on_new_campaign: default_mission_date_on_new_campaign(),
            post_round_delay_secs: default_mission_datetime_post_round_delay_secs(),
        }
    }
}

impl SetMissionStartDatetimeCfg {
    pub fn validate(&self) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let path = self
            .skript_path
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("setmissionstartdatetime.skript_path is required when enabled"))?;
        let _ = path;
        if self.mission_start_time_cycle.is_empty() {
            bail!("setmissionstartdatetime.mission_start_time_cycle must not be empty when enabled");
        }
        for t in &self.mission_start_time_cycle {
            parse_hh_mm(t).map_err(|_| {
                anyhow!(
                    "setmissionstartdatetime.mission_start_time_cycle entry {:?} is invalid (use HH:MM)",
                    t
                )
            })?;
        }
        if let Some(ref d) = self.mission_date_base {
            NaiveDate::parse_from_str(d.trim(), "%Y-%m-%d").map_err(|_| {
                anyhow!(
                    "setmissionstartdatetime.mission_date_base {:?} is invalid (use YYYY-MM-DD)",
                    d
                )
            })?;
        }
        if !(1..=1800).contains(&self.post_round_delay_secs) {
            bail!(
                "setmissionstartdatetime.post_round_delay_secs {} out of range 1-1800",
                self.post_round_delay_secs
            );
        }
        Ok(())
    }
}

fn parse_hh_mm(s: &str) -> Result<(u32, u32)> {
    let parts: Vec<_> = s.trim().split(':').collect();
    if parts.len() != 2 {
        bail!("expected HH:MM");
    }
    let h: u32 = parts[0].parse()?;
    let m: u32 = parts[1].parse()?;
    if h > 23 || m > 59 {
        bail!("out of range");
    }
    Ok((h, m))
}

/// Mirror of DCSServerBot `scheduler.yaml` `action.times` for Discord map countdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DcsserverBotScheduledRestartCfg {
    pub timezone: String,
    pub times: Vec<String>,
}

impl DcsserverBotScheduledRestartCfg {
    pub fn validate(&self) -> Result<()> {
        if self.timezone.trim().is_empty() {
            bail!("DCSServerBot_scheduled_restart.timezone must not be empty");
        }
        chrono_tz::Tz::from_str(self.timezone.trim())
            .map_err(|_| anyhow!("DCSServerBot_scheduled_restart.timezone {:?} is invalid", self.timezone))?;
        if self.times.is_empty() {
            bail!("DCSServerBot_scheduled_restart.times must not be empty");
        }
        for t in &self.times {
            parse_scheduled_restart_hh_mm(t)
                .map_err(|_| anyhow!("DCSServerBot_scheduled_restart.times entry {:?} is invalid (use HH:MM)", t))?;
        }
        Ok(())
    }

    /// Next wall-clock restart in UTC (same day-or-next rule as DCSServerBot scheduler `times`).
    pub fn next_restart_utc(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let tz = chrono_tz::Tz::from_str(self.timezone.trim()).ok()?;
        let now_local = now.with_timezone(&tz);
        let today = now_local.date_naive();
        let check_floor = today.and_hms_opt(
            now_local.hour(),
            now_local.minute(),
            0,
        )?;
        let mut best: Option<DateTime<Utc>> = None;
        for t in &self.times {
            let (h, m) = parse_scheduled_restart_hh_mm(t).ok()?;
            let time = chrono::NaiveTime::from_hms_opt(h, m, 0)?;
            let mut candidate = today.and_time(time);
            if candidate <= check_floor {
                candidate += chrono::Duration::days(1);
            }
            let local = tz
                .from_local_datetime(&candidate)
                .single()
                .or_else(|| tz.from_local_datetime(&candidate).earliest())?;
            let utc = local.with_timezone(&Utc);
            if best.is_none_or(|b| utc < b) {
                best = Some(utc);
            }
        }
        best
    }
}

fn parse_scheduled_restart_hh_mm(s: &str) -> Result<(u32, u32)> {
    let s = s.trim();
    let (h, m) = match s.split_once(':') {
        Some((h, m)) => (h, m),
        None => bail!("missing ':'"),
    };
    let h: u32 = h.parse()?;
    let m: u32 = m.parse()?;
    if h > 24 || m > 59 {
        bail!("hour or minute out of range");
    }
    if h == 24 && m != 0 {
        bail!("24:xx is invalid");
    }
    Ok((h % 24, m))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CsarCfg {
    /// Keep ejected pilots in the world and hide them from JTAC.
    #[serde(default)]
    pub enabled: bool,
    /// Late-activated ME group name for downed pilot (MOOSE CSAR template), red side.
    #[serde(default)]
    pub pilot_template_red: Option<String>,
    /// Late-activated ME group name for downed pilot (MOOSE CSAR template), blue side.
    #[serde(default)]
    pub pilot_template_blue: Option<String>,
    /// Max distance (m) from helo to issue Extract / load a captured enemy.
    #[serde(default = "default_csar_pickup_distance_m")]
    pub pickup_distance_m: u32,
    /// Extract / enemy load requires the helo on the ground.
    #[serde(default = "default_csar_pickup_requires_landed")]
    pub pickup_requires_landed: bool,
    /// Internal cargo mass (kg) per onboard downed pilot.
    #[serde(default = "default_csar_downed_pilot_weight_kg")]
    pub downed_pilot_weight_kg: u32,
    /// Restore a friendly life on delivery when the class is not already full.
    #[serde(default = "default_csar_restore_life_on_rescue")]
    pub restore_life_on_rescue: bool,
    /// Do not restore a life that would exceed `default_lives` for that class.
    #[serde(default = "default_csar_restore_life_cap_at_default")]
    pub restore_life_cap_at_default: bool,
    /// Minutes until an unrescued downed pilot is removed (0 = disabled).
    #[serde(default)]
    pub capture_timer: u32,
    /// Seconds before the same player can request smoke again.
    #[serde(default = "default_csar_smoke_cooldown")]
    pub smoke_cooldown: u32,
    /// Max distance (m) from an unloaded capture squad to an enemy downed pilot.
    #[serde(default = "default_csar_capture_start_distance_m")]
    pub capture_start_distance_m: u32,
    /// Distance (m) at which a walking pilot/squad is treated as arrived.
    #[serde(default = "default_csar_board_distance_m")]
    pub board_distance_m: u32,
    /// Max range (m) for CSAR List nearby / Smoke nearest (friendly only).
    #[serde(default = "default_csar_list_smoke_range_m")]
    pub list_smoke_range_m: u32,
}

impl Default for CsarCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            pilot_template_red: None,
            pilot_template_blue: None,
            pickup_distance_m: default_csar_pickup_distance_m(),
            pickup_requires_landed: default_csar_pickup_requires_landed(),
            downed_pilot_weight_kg: default_csar_downed_pilot_weight_kg(),
            restore_life_on_rescue: default_csar_restore_life_on_rescue(),
            restore_life_cap_at_default: default_csar_restore_life_cap_at_default(),
            capture_timer: 0,
            smoke_cooldown: default_csar_smoke_cooldown(),
            capture_start_distance_m: default_csar_capture_start_distance_m(),
            board_distance_m: default_csar_board_distance_m(),
            list_smoke_range_m: default_csar_list_smoke_range_m(),
        }
    }
}

fn default_csar_pickup_distance_m() -> u32 {
    60
}

fn default_csar_pickup_requires_landed() -> bool {
    true
}

fn default_csar_downed_pilot_weight_kg() -> u32 {
    100
}

fn default_csar_restore_life_on_rescue() -> bool {
    true
}

fn default_csar_restore_life_cap_at_default() -> bool {
    true
}

fn default_csar_smoke_cooldown() -> u32 {
    300
}

fn default_csar_capture_start_distance_m() -> u32 {
    250
}

fn default_csar_board_distance_m() -> u32 {
    20
}

fn default_csar_list_smoke_range_m() -> u32 {
    10_000
}

/// ED dynamic cargo crates: Fowl registry, To stock → objective warehouse, points.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DynamicCargoDeliveryCfg {
    /// Master switch (registry, persistence, Supplies → To stock).
    #[serde(default)]
    pub enabled: bool,
    /// Soft cap of registered crates per coalition; spawning over limit destroys oldest (FIFO).
    #[serde(default = "default_maximum_dynamic_crates_per_coalition")]
    pub maximum_dynamic_crates_per_coalition: u32,
    /// Radius (m) around the player for Supplies → To stock.
    #[serde(default = "default_to_stock_dynamic_crate_distance")]
    pub to_stock_dynamic_crate_distance: u32,
    /// Points per ton (1000 kg) to the deliverer on cross-objective To stock / DCS absorb.
    /// Final award is `round(max(kg/1000, 1) * rate)`.
    #[serde(default = "default_to_stock_points_per_ton")]
    pub to_stock_points_per_ton: u32,
    /// Points per ton to the registered spawner on cross-objective To stock / DCS absorb.
    #[serde(default = "default_source_spawner_points_per_ton")]
    pub source_spawner_points_per_ton: u32,
    /// DCS type names that use ED F8/bay for Fowl crates (no Fowl Load/Unload) when `enabled`.
    /// ED still enforces bay geometry/weight; CFG `cargo` slot limits apply to Fowl crates + troops
    /// (excess Fowl crates are rejected back to the ground). Warehouse ED supply/fuel crates are unaffected.
    #[serde(default)]
    pub shared_ed_cargo_airframes: FxHashSet<String>,
}

impl Default for DynamicCargoDeliveryCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            maximum_dynamic_crates_per_coalition: default_maximum_dynamic_crates_per_coalition(),
            to_stock_dynamic_crate_distance: default_to_stock_dynamic_crate_distance(),
            to_stock_points_per_ton: default_to_stock_points_per_ton(),
            source_spawner_points_per_ton: default_source_spawner_points_per_ton(),
            shared_ed_cargo_airframes: FxHashSet::default(),
        }
    }
}

fn default_maximum_dynamic_crates_per_coalition() -> u32 {
    100
}

fn default_to_stock_dynamic_crate_distance() -> u32 {
    50
}

fn default_to_stock_points_per_ton() -> u32 {
    5
}

fn default_source_spawner_points_per_ton() -> u32 {
    15
}

fn default_supply_transfer_players() -> bool {
    true
}

fn default_ewr_delay() -> u32 {
    60
}

fn default_ewr_antenna_height_m() -> u32 {
    10
}

fn default_ewr_aspect_hysteresis_deg() -> f64 {
    5.
}

fn default_jtac_default_code_blue() -> u16 {
    1688
}

fn default_jtac_default_code_red() -> u16 {
    1113
}

fn default_calcm_mission() -> bool {
    true
}


#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cfg {
    #[serde(default)]
    pub netidx_base: Option<NetIdxPath>,
    /// if specified, automatically reset the server state and record
    /// a victory in the stats when the condition is met.
    #[serde(default)]
    pub auto_reset: Option<AutoResetOnVictory>,
    /// ucids in this list are able to run admin commands
    #[serde(default)]
    pub admins: FxHashMap<Ucid, String>,
    /// ucids in this list are banned
    #[serde(default)]
    pub banned: FxHashMap<Ucid, (Option<DateTime<Utc>>, String)>,
    /// who can do what
    #[serde(default)]
    pub rules: Rules,
    /// Optional player-name regex checked on connect. Fowl 2.0 default bans only
    /// Unicode control / line-separator characters (crash / log / Lua FFI risk);
    /// printable ASCII including `-` and letters with diacritics are allowed.
    #[serde(default)]
    pub name_filter: Option<NameFilter>,
    /// The maximum number of messages, including markup, we will push to dcs
    /// per second.
    #[serde(default = "default_msgs_per_second")]
    pub max_msgs_per_second: usize,
    /// shutdown after the specified number of hours, don't shutdown
    /// if None.
    #[serde(default)]
    pub shutdown: Option<u32>,
    /// When `shutdown` is null, `-time` replies with this text (e.g. DCSServerBot `.timeleft`).
    #[serde(default)]
    pub chat_time_command: Option<String>,
    /// DCSServerBot scheduler mirror for map restart countdown; ignored when `shutdown` is set.
    #[serde(default, rename = "DCSServerBot_scheduled_restart")]
    pub dcsserver_bot_scheduled_restart: Option<DcsserverBotScheduledRestartCfg>,
    /// how many points are various actions worth (if any)
    #[serde(default)]
    pub points: Option<PointsCfg>,
    /// do not attempt to get the target of any weapon in this list
    #[serde(default)]
    pub weapon_target_exclusions: FxHashSet<String>,
    /// how often a base will repair if it has full logistics (Seconds)
    pub repair_time: u32,
    /// OPR repair crate: added to unpacker's points (negative deducts).
    #[serde(default)]
    pub production_repair_crate_cost: i32,
    /// OPR repair tick interval in seconds.
    #[serde(default)]
    pub production_repair_rate_seconds: u32,
    /// ME objective static repair tick interval in seconds (`ObjectiveStatic` queue from repair crates).
    #[serde(default)]
    pub static_repair_rate_seconds: u32,
    /// ME static repair crate: added to unpacker's points (negative deducts).
    #[serde(default)]
    pub static_repair_crate_cost: i32,
    /// DCS unit types counted as OPR factory statics (must also be in `unit_classification`).
    #[serde(default)]
    pub production_factory_units: FxHashSet<String>,
    /// ME statics in OFO / OLO / OAB zones (map key = DCS unit type; must be in `unit_classification`).
    #[serde(default)]
    pub objective_static_units: FxHashMap<String, ObjectiveStaticUnitCfg>,
    /// The base repair crate
    pub repair_crate: FxHashMap<Side, Crate>,
    /// If the warehouse system is to be used then this should be specified,
    /// otherwise warehouses will be ignored and you should set them to unlimited
    pub warehouse: Option<WarehouseConfig>,
    /// F10 Cargo: spawn supply transfer crates. AI LogisticsTransfer actions are unaffected.
    #[serde(default = "default_supply_transfer_players")]
    pub supply_transfer_players: bool,
    /// When true: periodic production into logistics hubs and virtual hub-to-objective
    /// distribution (no 3D convoys). When false: hub production only; no automatic
    /// virtual distribution (for future ground/air supply routes).
    #[serde(default)]
    pub virtual_resupply: bool,
    /// When true with `virtual_resupply`: threatened hubs/objectives skip virtual deliveries
    /// (weapons, equipment, airframes, and fuel/liquids); threatened OLO shows 0% Production;
    /// OPR→OLO feed lines hide while OPR or OLO is threatened. DCS sync-from must not refill
    /// equipment or liquids on threatened objectives (ME/DCS can restore stock e.g. after destroy).
    #[serde(default)]
    pub virtual_resupply_threatened_without_deliveries: bool,
    /// Distance-based hub-to-objective virtual resupply efficiency (ignored when `virtual_resupply` is false).
    #[serde(default)]
    pub virtual_resupply_decay: VirtualResupplyDecayConfig,
    /// F10 map marks on objective base groups (RARMOR, RLOGI, …) for the owning coalition.
    #[serde(default)]
    pub objective_group_marks: bool,
    /// F10 dashed lines between adjacent objectives held by different coalitions.
    #[serde(default)]
    pub front_line: bool,
    /// Grid cell size (meters) for front line computation. Smaller = finer lines, more quads.
    #[serde(default = "default_front_line_grid_size_meters")]
    pub front_line_grid_size_meters: f64,
    /// Decade label for bftools (`weapon{campaign_decade}.miz`); core gameplay ignores unless referenced elsewhere.
    #[serde(default)]
    pub campaign_decade: Option<String>,
    /// Stock targets per wsType subcategory for bftools-generated default warehouse rows (JSON keys match mission CFG).
    #[serde(default, rename = "default_warehouse_AAmissiles")]
    pub default_warehouse_aa_missiles: u32,
    #[serde(default, rename = "default_warehouse_AGmissiles")]
    pub default_warehouse_ag_missiles: u32,
    #[serde(default, rename = "default_warehouse_AGrockets")]
    pub default_warehouse_ag_rockets: u32,
    #[serde(default, rename = "default_warehouse_AGbombs")]
    pub default_warehouse_ag_bombs: u32,
    #[serde(default, rename = "default_warehouse_AGguidedbombs")]
    pub default_warehouse_ag_guided_bombs: u32,
    /// Drop tanks: dcso3 `WSType::category` → (1,3) Droptank, not Weapons (4); bftools must emit aircraft rows; verify vs `getResourceMap` when testing.
    #[serde(default, rename = "default_warehouse_Fueltanks")]
    pub default_warehouse_fueltanks: u32,
    #[serde(default, rename = "Fueltanks_empty")]
    pub fueltanks_empty: bool,
    #[serde(default, rename = "default_warehouse_Misc")]
    pub default_warehouse_misc: u32,
    /// how far must you fly from an objective to spawn deployables
    /// without penalty (Meters)
    pub logistics_exclusion: u32,
    /// Min distance from non-threatened friendly objective centers before group deployable
    /// unpack or deployable unit repair (0 = disabled).
    #[serde(default = "default_deployable_unpack_min_base_distance_m")]
    pub deployable_unpack_min_base_distance_m: u32,
    /// an objective will cull it's units if there are no enemy units
    /// within this distance (Meters)
    pub unit_cull_distance: u32,
    /// an objective will cull it's units if there are no enemy ground units
    /// within this distance (Meters)
    pub ground_vehicle_cull_distance: u32,
    /// OPR (Production) threatened map ring and deploy exclusion radius (Meters).
    #[serde(default = "default_production_logistics_exclusion")]
    pub production_logistics_exclusion: u32,
    /// OPR (Production) enemy ground threat and ground spawn-activation radius (Meters).
    #[serde(default = "default_production_ground_vehicle_cull_distance")]
    pub production_ground_vehicle_cull_distance: u32,
    /// If a base has been inactive for this long then cull it's units (Seconds)
    #[serde(default = "default_cull_after")]
    pub cull_after: u32,
    /// Spread objective group spawn/despawn queue over time (spawn and despawn).
    /// 1 = legacy rate (~1/16 of queue per second). 2..=10 = that many times slower peak.
    /// Default 5. Values outside 1..=10 are treated as 5.
    #[serde(default = "default_groups_spawn_queue_stretch")]
    pub groups_spawn_queue_stretch: u32,
    /// how often to do more expensive checks such as unit culling and
    /// updating unit positions (Seconds)
    pub slow_timed_events_freq: u32,
    /// how close various kinds of enemy units can be (with LOS) for an objective
    /// to be considered threatened. Threatened objectives can't spawn deployables
    /// within the exclusion zone. (Meters)
    pub threatened_distance: FxHashMap<Vehicle, u32>,
    /// how long before threatened is removed if no enemy can be seen
    pub threatened_cooldown: u32,
    /// how far can a crate be from the player and still be
    /// loadable (Meters)
    pub crate_load_distance: u32,
    /// how far crates apart crates can be and still unpack (Meters)
    pub crate_spread: u32,
    /// how close must artillery be to participate in an artillery mission
    /// (meters).
    pub artillery_mission_range: u32,
    /// how close a CALCM unit must be to participate in a CALCM mission
    /// (meters).
    pub calcm_mission_range: u32,
    /// When false, CALCM deploy/waypoint actions are hidden from F10 Actions and chat.
    #[serde(default = "default_calcm_mission")]
    pub calcm_mission: bool,
    /// If true players will be locked to the side they initially
    /// choose for the duration of the round
    #[serde(default = "default_lock_sides")]
    pub lock_sides: bool,
    /// how many times a user may switch sides in a given round,
    /// or None for unlimited side switches
    #[serde(default)]
    pub side_switches: Option<u8>,
    /// How many crates a player may spawn at the same time
    #[serde(default)]
    pub max_crates: Option<u32>,
    /// the life types different vehicles use
    pub life_types: FxHashMap<Vehicle, LifeType>,
    /// the life reset configuration for each life type. A pair
    /// of number of lives per reset, and reset time in seconds.
    pub default_lives: FxHashMap<LifeType, (u8, u32)>,
    /// If true, lives will be limited according to the default_lives
    /// and life_types specification
    #[serde(default = "default_limited_lives")]
    pub limited_lives: bool,
    /// If true, a life is consumed when entering a flyable slot (Birth)
    /// instead of on takeoff.
    #[serde(default = "default_lives_birth")]
    pub lives_birth: bool,
    /// If true, penalize RELEASE SLOT / ejection while airborne: block observer-type
    /// slots for `airborne_deslot_penalty_secs` (wall clock, persisted across reconnect).
    #[serde(default = "default_airborne_deslot_block")]
    pub airborne_deslot_block: bool,
    /// Observer/spectator lockout after leaving an airborne aircraft slot (seconds).
    /// Only used when `airborne_deslot_block` is true; 0 disables the penalty timer.
    #[serde(default = "default_airborne_deslot_penalty_secs")]
    pub airborne_deslot_penalty_secs: u32,
    /// Points deducted when an airborne deslot penalty is applied. 0 disables.
    #[serde(default = "default_airborne_deslot_penalty_points")]
    pub airborne_deslot_penalty_points: u32,
    #[serde(default)]
    pub csar: CsarCfg,
    #[serde(default)]
    pub dynamic_cargo_delivery: DynamicCargoDeliveryCfg,
    #[serde(default)]
    pub acmi_sanitize: AcmiSanitizeCfg,
    /// After each round, rewrite ME start time (and optionally date) in the on-disk `.miz`.
    #[serde(default)]
    pub setmissionstartdatetime: SetMissionStartDatetimeCfg,
    /// Writedir housekeeping at mission start (tracks, future similar ops).
    #[serde(default)]
    pub server_maintenance: ServerMaintenanceCfg,
    /// Available actions per side
    #[serde(default)]
    pub actions: FxHashMap<Side, IndexMap<String, Action, FxBuildHasher>>,
    /// vehicle cargo configuration
    #[serde(default)]
    pub cargo: FxHashMap<Vehicle, CargoConfig>,
    /// The name of the crate group for each side
    #[serde(default)]
    pub crate_template: FxHashMap<Side, String>,
    /// deployables configuration for each side
    #[serde(default)]
    pub deployables: FxHashMap<Side, Vec<Deployable>>,
    /// deployable troops configuration for each side
    pub troops: FxHashMap<Side, Vec<Troop>>,
    /// classification of ground units in the mission
    pub unit_classification: FxHashMap<Vehicle, UnitTags>,
    /// airborne jtacs
    #[serde(default)]
    pub airborne_jtacs: FxHashMap<Vehicle, DeployableJtac>,
    /// The jtac target priority list
    pub jtac_priority: Vec<UnitTags>,
    /// Default laser code for new Blue-coalition JTACs (DCS range 1111–1788).
    #[serde(default = "default_jtac_default_code_blue")]
    pub jtac_default_code_blue: u16,
    /// Default laser code for new Red-coalition JTACs (DCS range 1111–1788).
    #[serde(default = "default_jtac_default_code_red")]
    pub jtac_default_code_red: u16,
    /// Objectives that can host fixed wing even though they aren't
    /// airbases. Used by actions to choose a spawn point. E.G. You
    /// want to make an airbase a logistics hub because it's close to
    /// a port.
    #[serde(default)]
    pub extra_fixed_wing_objectives: FxHashSet<String>,
    /// EWR system mode - controls track update timing
    #[serde(default)]
    pub ewr_mode: EwrMode,
    /// EWR track update delay in seconds (only used when ewr_mode is Delayed)
    #[serde(default = "default_ewr_delay")]
    pub ewr_delay: u32,
    /// Enemy EWR report: min speed (km/h) for low/slow filter. None or 0 = ignore.
    #[serde(default)]
    pub ewr_min_speed_kmh: Option<u32>,
    /// Enemy EWR report: min AGL (m) for low/slow filter. None or 0 = ignore.
    #[serde(default)]
    pub ewr_min_ralt_m: Option<u32>,
    /// Meters added to EWR group centroid height for line-of-sight checks.
    #[serde(default = "default_ewr_antenna_height_m")]
    pub ewr_antenna_height_m: u32,
    /// Aspect bucket hysteresis (degrees). Lower = faster HOT/FLANK/BEAM/DRAG/COLD transitions; 0 = off.
    #[serde(default = "default_ewr_aspect_hysteresis_deg")]
    pub ewr_aspect_hysteresis_deg: f64,
    /// Source paths for per-player UI sounds (embedded into `.miz` by FowlTools).
    #[serde(default)]
    pub sounds_player: FxHashMap<String, String>,
    /// Source paths for sounds played to all players (e.g. restart warnings).
    #[serde(default)]
    pub sounds_all: FxHashMap<String, String>,
    #[serde(default)]
    pub discord_map: DiscordMapCfg,
    /// Test-only: extra Red headcount for `balancing_point_gain` (not Discord map).
    #[serde(default)]
    pub debugging_online_red_players: u32,
    /// Test-only: extra Blue headcount for `balancing_point_gain` (not Discord map).
    #[serde(default)]
    pub debugging_online_blue_players: u32,
    /// Test-only: on new campaign init, swap capturable ME owners (`…R…`↔`…B…`) and load
    /// that side's export warehouse templates into DCS (skip ME preserve SyncFrom).
    #[serde(default)]
    pub debugging_objectives_coalition_switch: bool,
    /// Hours after deploy only the spawning player may run `-action` on that air AI; `0` or omit = no lock.
    #[serde(default)]
    pub ai_air_action_owner_hours: Option<u32>,
}

impl Cfg {
    fn path(miz_state_path: &Path) -> PathBuf {
        let mut path = PathBuf::from(miz_state_path);
        let file_name = path
            .file_name()
            .map(|s| {
                let mut s = s.to_string_lossy().into_owned();
                s.push_str("_CFG");
                s
            })
            .unwrap_or_else(|| "CFG".into());
        path.set_file_name(file_name);
        path
    }

    pub fn load(miz_state_path: &Path) -> Result<Self> {
        let path = Self::path(miz_state_path);
        let file = loop {
            match File::open(&path) {
                Ok(f) => break f,
                Err(e) => match e.kind() {
                    io::ErrorKind::NotFound => {
                        let file = File::create(&path)
                            .map_err(|e| anyhow!("could not create default config {}", e))?;
                        serde_json::to_writer_pretty(file, &Cfg::default())
                            .map_err(|e| anyhow!("could not write default config {}", e))?;
                    }
                    e => {
                        return Err(anyhow!("error opening config file {:?}", e));
                    }
                },
            }
        };
        let mut cfg: Self = serde_json::from_reader(file)
            .map_err(|e| anyhow!("failed to decode cfg file {:?}, {:?}", path, e))?;
        for (_, actions) in &mut cfg.actions {
            actions.sort_by(|name0, _, name1, _| name0.cmp(name1));
        }
        // translate deployables to the new format
        let mut has_deprecated = false;
        for (_, deps) in cfg.deployables.iter_mut() {
            for dep in deps.iter_mut() {
                if let Some(mut parts) = dep.deprecated_logistics.take() {
                    parts.defenses_template = dep.deprecated_template.take();
                    dep.kind = DeployableKind::Objective(parts);
                    has_deprecated = true;
                } else if let Some(template) = dep.deprecated_template.take() {
                    dep.kind = DeployableKind::Group { template };
                    has_deprecated = true;
                }
            }
        }
        if has_deprecated {
            fs::write(path, serde_json::to_string_pretty(&cfg)?)?
        }
        cfg.validate_jtac_default_codes()?;
        cfg.acmi_sanitize.validate()?;
        cfg.setmissionstartdatetime.validate()?;
        cfg.discord_map.validate_campaign_top10_sizes()?;
        if let Some(ref s) = cfg.dcsserver_bot_scheduled_restart {
            s.validate()?;
        }
        Ok(cfg)
    }

    /// Discord map / countdown: bflib `shutdown` wins; else optional DCSServerBot schedule mirror.
    pub fn map_restart_when(
        &self,
        now: DateTime<Utc>,
        bflib_auto_shutdown_when: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        if self.shutdown.is_some() {
            return bflib_auto_shutdown_when;
        }
        self.dcsserver_bot_scheduled_restart
            .as_ref()
            .and_then(|s| s.next_restart_utc(now))
    }

    /// Effective spawn/despawn queue stretch; out-of-range CFG values fall back to default (5).
    pub fn groups_spawn_queue_stretch_effective(&self) -> u32 {
        match self.groups_spawn_queue_stretch {
            1..=10 => self.groups_spawn_queue_stretch,
            _ => default_groups_spawn_queue_stretch(),
        }
    }

    pub fn save(&self, miz_state_path: &Path) -> Result<()> {
        let mut path = Self::path(miz_state_path);
        path.set_extension("bak");
        let fd = File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format_compact!("opening {:?}", path))?;
        serde_json::to_writer_pretty(fd, self).context("serializing cfg")?;
        fs::rename(&path, Self::path(miz_state_path)).context("moving new file into place")?;
        Ok(())
    }

    pub fn logistics_exclusion_for(&self, kind: &ObjectiveKind) -> u32 {
        if matches!(kind, ObjectiveKind::Production) {
            self.production_logistics_exclusion
        } else {
            self.logistics_exclusion
        }
    }

    pub fn ground_vehicle_cull_distance_for(&self, kind: &ObjectiveKind) -> u32 {
        if matches!(kind, ObjectiveKind::Production) {
            self.production_ground_vehicle_cull_distance
        } else {
            self.ground_vehicle_cull_distance
        }
    }

    pub fn check_vehicle_has_threat_distance(&self, vehicle: &Vehicle) -> Result<()> {
        match self.threatened_distance.get(vehicle) {
            Some(_) => (),
            None => bail!(
                "vehicle {:?} doesn't have a configured theatened distance",
                vehicle
            ),
        }
        Ok(())
    }

    pub fn check_vehicle_has_life_type(&self, vehicle: &Vehicle) -> Result<()> {
        match self.life_types.get(vehicle) {
            None => bail!("vehicle {:?} doesn't have a configured life type", vehicle),
            Some(typ) => match self.default_lives.get(&typ) {
                Some((n, f)) if *n > 0 && *f > 0 => (),
                None => bail!("vehicle {:?} has no configured life type", vehicle),
                Some((n, f)) => {
                    bail!(
                        "vehicle {:?} life type {:?} has no configured lives ({n}) or negative reset time ({f})",
                        vehicle, typ
                    )
                }
            },
        }
        Ok(())
    }

    pub fn jtac_default_code(&self, side: Side) -> u16 {
        match side {
            Side::Blue => self.jtac_default_code_blue,
            Side::Red => self.jtac_default_code_red,
            Side::Neutral => self.jtac_default_code_blue,
        }
    }

    fn validate_jtac_default_codes(&self) -> Result<()> {
        for (label, code) in [
            ("blue", self.jtac_default_code_blue),
            ("red", self.jtac_default_code_red),
        ] {
            if !(1111..=1788).contains(&code) {
                bail!("jtac_default_code_{label} {code} out of range 1111-1788");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod discord_map_public_url_tests {
    use super::normalize_discord_map_public_url;

    #[test]
    fn empty_means_fallback() {
        assert_eq!(normalize_discord_map_public_url("").unwrap(), None);
        assert_eq!(normalize_discord_map_public_url("  ").unwrap(), None);
    }

    #[test]
    fn https_host_appends_map() {
        assert_eq!(
            normalize_discord_map_public_url("https://fowl-ta.duckdns.org")
                .unwrap()
                .as_deref(),
            Some("https://fowl-ta.duckdns.org/map")
        );
    }

    #[test]
    fn https_map_path_kept() {
        assert_eq!(
            normalize_discord_map_public_url("https://fowl-sarh.duckdns.org/map/")
                .unwrap()
                .as_deref(),
            Some("https://fowl-sarh.duckdns.org/map")
        );
    }

    #[test]
    fn http_rejected() {
        assert!(normalize_discord_map_public_url("http://fowl-ta.duckdns.org/map").is_err());
    }
}
