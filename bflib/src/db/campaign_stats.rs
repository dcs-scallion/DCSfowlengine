//! Coalition campaign counters for the Discord interactive map sidebar.

use super::{
    group::{DeployKind, SpawnedGroup},
    objective::ObjGroupClass,
    Db,
};
use anyhow::Result;
use bfprotocols::{
    cfg::{ActionKind, LifeType, UnitTag, Vehicle},
    db::group::GroupId,
};
use chrono::{NaiveDate, prelude::*};
use dcso3::{coalition::Side, net::Ucid, MizLua};
use fxhash::FxHashMap;
use serde_derive::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvestBucket {
    Air,
    Ground,
    Navy,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoalitionLossStats {
    pub planes_standard: u32,
    pub planes_intercept: u32,
    pub planes_attack: u32,
    pub planes_recon: u32,
    pub planes_bomber: u32,
    pub planes_tanker: u32,
    #[serde(default)]
    pub planes_awacs: u32,
    pub drones: u32,
    pub helicopters_atk: u32,
    pub helicopters_log: u32,
    pub armored: u32,
    #[serde(default)]
    pub artillery: u32,
    pub troops: u32,
    #[serde(default)]
    pub utility: u32,
    pub aaa: u32,
    pub sam_sr: u32,
    pub sam_mr: u32,
    pub sam_lr: u32,
    #[serde(default)]
    pub ewr_radars: u32,
    pub ships_small: u32,
    pub ships_medium: u32,
    pub carriers: u32,
    pub building_logi: u32,
    pub building_prod: u32,
    pub building_static: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoalitionPointStats {
    pub balancing_gain: i64,
    pub active_gain: i64,
    pub invested_air: u64,
    pub invested_ground: u64,
    pub invested_navy: u64,
}

/// Cross-objective ED dynamic cargo deliveries (persisted kg; HTML shows floor tons).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoalitionDynamicCargoStats {
    pub deliveries: u32,
    pub tonnage_kg: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoalitionCampaignStats {
    pub objectives_taken: u32,
    pub registered: u32,
    pub player_deaths: u32,
    pub online_secs: u64,
    pub losses: CoalitionLossStats,
    pub points: CoalitionPointStats,
    #[serde(default)]
    pub dynamic_cargo: CoalitionDynamicCargoStats,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CampaignStats {
    pub campaign_rounds: u32,
    #[serde(default)]
    pub campaign_start_date: Option<NaiveDate>,
    #[serde(default)]
    pub blue: CoalitionCampaignStats,
    #[serde(default)]
    pub red: CoalitionCampaignStats,
    #[serde(default)]
    pub entrants: FxHashMap<Ucid, Side>,
}

#[derive(Debug, Clone)]
pub struct CampaignStatsView {
    pub theatre: String,
    pub start_date: String,
    pub current_date: String,
    pub duration_days: u32,
    pub online_hours_blue: u32,
    pub online_hours_red: u32,
    pub stats: CampaignStats,
}

impl CampaignStats {
    pub fn duration_days(&self, rounds_per_day: u32) -> u32 {
        let rpd = rounds_per_day.max(1);
        let restarts = self.campaign_rounds.saturating_sub(1);
        1 + restarts / rpd
    }

    pub fn current_date(&self, rounds_per_day: u32) -> Option<NaiveDate> {
        let start = self.campaign_start_date?;
        let days = self.duration_days(rounds_per_day);
        start.checked_add_signed(chrono::Duration::days((days.saturating_sub(1)) as i64))
    }

    fn coalition_mut(&mut self, side: Side) -> Option<&mut CoalitionCampaignStats> {
        match side {
            Side::Blue => Some(&mut self.blue),
            Side::Red => Some(&mut self.red),
            _ => None,
        }
    }

    fn coalition(&self, side: Side) -> Option<&CoalitionCampaignStats> {
        match side {
            Side::Blue => Some(&self.blue),
            Side::Red => Some(&self.red),
            _ => None,
        }
    }
}

fn format_campaign_date_short(d: NaiveDate) -> String {
    let month = match d.month() {
        1 => "Jan.",
        2 => "Feb.",
        3 => "Mar.",
        4 => "Apr.",
        5 => "May",
        6 => "Jun.",
        7 => "Jul.",
        8 => "Aug.",
        9 => "Sep.",
        10 => "Oct.",
        11 => "Nov.",
        _ => "Dec.",
    };
    format!("{} {}", month, d.day())
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn vs_row(label: &str, blue: u32, red: u32, total: bool) -> String {
    let cls = if total { " stat-row is-total" } else { " stat-row" };
    format!(
        r#"<div class="{cls}"><span class="lbl">{label}</span><span class="stat-blue">{blue}</span><span class="stat-sep">:</span><span class="stat-red">{red}</span></div>"#,
        cls = cls,
        label = html_escape(label),
        blue = blue,
        red = red,
    )
}

fn kv_row(label: &str, value: &str, total: bool) -> String {
    let cls = if total { " stat-kv is-total" } else { " stat-kv" };
    format!(
        r#"<div class="{cls}"><span class="lbl">{label}</span><span class="val">{value}</span></div>"#,
        cls = cls,
        label = html_escape(label),
        value = html_escape(value),
    )
}

fn section_hdr(title: &str) -> String {
    format!(
        r#"<div class="stat-section-hdr"><span class="lbl">{title}</span><span class="blue-h">Blue</span><span class="sep-h">:</span><span class="red-h">Red</span></div>"#,
        title = html_escape(title),
    )
}

fn divider() -> &'static str {
    r#"<div class="stat-divider"></div>"#
}

pub fn render_sidebar_html(view: &CampaignStatsView) -> String {
    let s = &view.stats;
    let mut out = String::new();

    out.push_str(r#"<div class="right-col campaign-stats-col">"#);

    // Campaign
    out.push_str(r#"<div class="stat"><div class="stat-h">Campaign stats</div><div class="stat-body">"#);
    out.push_str(&kv_row("Theatre", &view.theatre, false));
    out.push_str(divider());
    out.push_str(&kv_row("Campaign start", &view.start_date, false));
    out.push_str(&kv_row("Campaign current", &view.current_date, false));
    out.push_str(divider());
    out.push_str(&section_hdr("Objectives taken"));
    out.push_str(&vs_row(
        "Total captures",
        s.blue.objectives_taken,
        s.red.objectives_taken,
        true,
    ));
    out.push_str(divider());
    out.push_str(&section_hdr("Players"));
    out.push_str(&vs_row(
        "Registered",
        s.blue.registered,
        s.red.registered,
        false,
    ));
    out.push_str(&vs_row(
        "Player deaths",
        s.blue.player_deaths,
        s.red.player_deaths,
        false,
    ));
    out.push_str(r#"</div></div>"#);

    // Losses
    let bl = &s.blue.losses;
    let rl = &s.red.losses;
    out.push_str(r#"<div class="stat"><div class="stat-h">War losses</div><div class="stat-body">"#);
    out.push_str(&section_hdr("Loss Type"));
    out.push_str(&vs_row("Planes standard", bl.planes_standard, rl.planes_standard, false));
    out.push_str(&vs_row("Planes intercept", bl.planes_intercept, rl.planes_intercept, false));
    out.push_str(&vs_row("Planes attack", bl.planes_attack, rl.planes_attack, false));
    out.push_str(&vs_row("Planes recon", bl.planes_recon, rl.planes_recon, false));
    out.push_str(&vs_row("Planes bomber", bl.planes_bomber, rl.planes_bomber, false));
    out.push_str(&vs_row("Planes tanker", bl.planes_tanker, rl.planes_tanker, false));
    out.push_str(&vs_row("Planes awacs", bl.planes_awacs, rl.planes_awacs, false));
    out.push_str(&vs_row("Drones", bl.drones, rl.drones, false));
    out.push_str(divider());
    out.push_str(&vs_row("Helicopters atk", bl.helicopters_atk, rl.helicopters_atk, false));
    out.push_str(&vs_row("Helicopters log", bl.helicopters_log, rl.helicopters_log, false));
    out.push_str(divider());
    out.push_str(&vs_row("Armored", bl.armored, rl.armored, false));
    out.push_str(&vs_row("Artillery", bl.artillery, rl.artillery, false));
    out.push_str(&vs_row("Troops", bl.troops, rl.troops, false));
    out.push_str(&vs_row("Utility", bl.utility, rl.utility, false));
    out.push_str(&vs_row("AAA", bl.aaa, rl.aaa, false));
    out.push_str(&vs_row("SAM SR", bl.sam_sr, rl.sam_sr, false));
    out.push_str(&vs_row("SAM MR", bl.sam_mr, rl.sam_mr, false));
    out.push_str(&vs_row("SAM LR", bl.sam_lr, rl.sam_lr, false));
    out.push_str(&vs_row("EWR radars", bl.ewr_radars, rl.ewr_radars, false));
    out.push_str(divider());
    out.push_str(&vs_row("Ships small", bl.ships_small, rl.ships_small, false));
    out.push_str(&vs_row("Ships medium", bl.ships_medium, rl.ships_medium, false));
    out.push_str(&vs_row("Carriers", bl.carriers, rl.carriers, false));
    out.push_str(divider());
    out.push_str(&vs_row("Building logi", bl.building_logi, rl.building_logi, false));
    out.push_str(&vs_row("Building prod", bl.building_prod, rl.building_prod, false));
    out.push_str(&vs_row("Building static", bl.building_static, rl.building_static, false));
    out.push_str(r#"</div></div>"#);

    // Dynamic Cargo Logistics
    let bdc = &s.blue.dynamic_cargo;
    let rdc = &s.red.dynamic_cargo;
    out.push_str(
        r#"<div class="stat"><div class="stat-h">Dynamic Cargo Logistics</div><div class="stat-body">"#,
    );
    out.push_str(&section_hdr("Statistic"));
    out.push_str(&vs_row("Deliveries", bdc.deliveries, rdc.deliveries, false));
    out.push_str(&vs_row(
        "Total Tonnage",
        (bdc.tonnage_kg / 1000) as u32,
        (rdc.tonnage_kg / 1000) as u32,
        false,
    ));
    out.push_str(r#"</div></div>"#);

    // Points
    let bp = &s.blue.points;
    let rp = &s.red.points;
    let total_gain_blue = (bp.balancing_gain + bp.active_gain).max(0) as u32;
    let total_gain_red = (rp.balancing_gain + rp.active_gain).max(0) as u32;
    let total_inv_blue = (bp.invested_air + bp.invested_ground + bp.invested_navy) as u32;
    let total_inv_red = (rp.invested_air + rp.invested_ground + rp.invested_navy) as u32;
    out.push_str(r#"<div class="stat"><div class="stat-h">Points</div><div class="stat-body">"#);
    out.push_str(&section_hdr("Gain (+p)"));
    out.push_str(&vs_row(
        "Balancing",
        bp.balancing_gain.max(0) as u32,
        rp.balancing_gain.max(0) as u32,
        false,
    ));
    out.push_str(&vs_row(
        "Active",
        bp.active_gain.max(0) as u32,
        rp.active_gain.max(0) as u32,
        false,
    ));
    out.push_str(divider());
    out.push_str(&vs_row("Total gain", total_gain_blue, total_gain_red, true));
    out.push_str(divider());
    out.push_str(&section_hdr("Invested (-p)"));
    out.push_str(&vs_row("Air unit", bp.invested_air as u32, rp.invested_air as u32, false));
    out.push_str(&vs_row(
        "Ground unit",
        bp.invested_ground as u32,
        rp.invested_ground as u32,
        false,
    ));
    out.push_str(&vs_row("Navy unit", bp.invested_navy as u32, rp.invested_navy as u32, false));
    out.push_str(divider());
    out.push_str(&vs_row("Total invested", total_inv_blue, total_inv_red, true));
    out.push_str(divider());
    let bal_blue = total_gain_blue as i64 - total_inv_blue as i64;
    let bal_red = total_gain_red as i64 - total_inv_red as i64;
    out.push_str(&format!(
        r#"<div class=" stat-row is-total"><span class="lbl">Total balance</span><span class="stat-blue">{bal_blue}</span><span class="stat-sep">:</span><span class="stat-red">{bal_red}</span></div>"#
    ));
    out.push_str(r#"</div></div></div>"#);

    out
}

impl Db {
    pub fn campaign_stats_active(&self) -> bool {
        self.ephemeral.cfg.discord_map.campaign_stats
    }

    fn loss_bucket_mut<'a>(
        losses: &'a mut CoalitionLossStats,
        bucket: LossBucketField,
    ) -> &'a mut u32 {
        match bucket {
            LossBucketField::PlanesStandard => &mut losses.planes_standard,
            LossBucketField::PlanesIntercept => &mut losses.planes_intercept,
            LossBucketField::PlanesAttack => &mut losses.planes_attack,
            LossBucketField::PlanesRecon => &mut losses.planes_recon,
            LossBucketField::PlanesBomber => &mut losses.planes_bomber,
            LossBucketField::PlanesTanker => &mut losses.planes_tanker,
            LossBucketField::PlanesAwacs => &mut losses.planes_awacs,
            LossBucketField::Drones => &mut losses.drones,
            LossBucketField::HelicoptersAtk => &mut losses.helicopters_atk,
            LossBucketField::HelicoptersLog => &mut losses.helicopters_log,
            LossBucketField::Armored => &mut losses.armored,
            LossBucketField::Artillery => &mut losses.artillery,
            LossBucketField::Troops => &mut losses.troops,
            LossBucketField::Utility => &mut losses.utility,
            LossBucketField::Aaa => &mut losses.aaa,
            LossBucketField::SamSr => &mut losses.sam_sr,
            LossBucketField::SamMr => &mut losses.sam_mr,
            LossBucketField::SamLr => &mut losses.sam_lr,
            LossBucketField::EwrRadars => &mut losses.ewr_radars,
            LossBucketField::ShipsSmall => &mut losses.ships_small,
            LossBucketField::ShipsMedium => &mut losses.ships_medium,
            LossBucketField::Carriers => &mut losses.carriers,
            LossBucketField::BuildingLogi => &mut losses.building_logi,
            LossBucketField::BuildingProd => &mut losses.building_prod,
            LossBucketField::BuildingStatic => &mut losses.building_static,
        }
    }

    fn record_loss(&mut self, side: Side, bucket: LossBucketField) {
        if !self.campaign_stats_active() {
            return;
        }
        if let Some(c) = self.persisted.campaign_stats.coalition_mut(side) {
            *Self::loss_bucket_mut(&mut c.losses, bucket) += 1;
            self.ephemeral.dirty();
        }
    }

    pub fn campaign_on_mission_start(&mut self, _lua: MizLua, resumed: bool) -> Result<()> {
        if !self.campaign_stats_active() {
            return Ok(());
        }
        if resumed {
            self.persisted.campaign_stats.campaign_rounds =
                self.persisted.campaign_stats.campaign_rounds.saturating_add(1);
        } else {
            self.persisted.campaign_stats.campaign_rounds = 1;
            if self.persisted.campaign_stats.campaign_start_date.is_none() {
                self.persisted.campaign_stats.campaign_start_date = Some(Utc::now().date_naive());
            }
        }
        self.ephemeral.dirty();
        Ok(())
    }

    pub fn campaign_on_capture(&mut self, side: Side) {
        if !self.campaign_stats_active() {
            return;
        }
        if let Some(c) = self.persisted.campaign_stats.coalition_mut(side) {
            c.objectives_taken += 1;
            self.ephemeral.dirty();
        }
    }

    pub fn campaign_on_register(&mut self, ucid: Ucid, side: Side) {
        if !self.campaign_stats_active() {
            return;
        }
        use std::collections::hash_map::Entry;
        match self.persisted.campaign_stats.entrants.entry(ucid) {
            Entry::Vacant(e) => {
                e.insert(side);
                if let Some(c) = self.persisted.campaign_stats.coalition_mut(side) {
                    c.registered += 1;
                    self.ephemeral.dirty();
                }
            }
            Entry::Occupied(_) => {}
        }
        // Keep connect-time if already online; otherwise start now (registered mid-session).
        if matches!(side, Side::Blue | Side::Red) {
            self.ephemeral
                .campaign_online_since
                .entry(ucid)
                .or_insert_with(Utc::now);
        }
    }

    pub fn campaign_on_sideswitch(&mut self, ucid: Ucid, new_side: Side) {
        if !self.campaign_stats_active() {
            return;
        }
        let player_points = self
            .persisted
            .players
            .get(&ucid)
            .map(|p| p.points as i64)
            .unwrap_or(0);
        let old = self.persisted.campaign_stats.entrants.insert(ucid, new_side);
        let Some(old) = old else {
            if let Some(c) = self.persisted.campaign_stats.coalition_mut(new_side) {
                c.registered += 1;
                self.ephemeral.dirty();
            }
            if matches!(new_side, Side::Blue | Side::Red) {
                self.ephemeral
                    .campaign_online_since
                    .entry(ucid)
                    .or_insert_with(Utc::now);
            }
            return;
        };
        if old == new_side {
            return;
        }
        // Credit pending online time to the side left behind, then restart for the new side.
        let now = Utc::now();
        self.campaign_credit_online(ucid, now, old, false);
        if matches!(new_side, Side::Blue | Side::Red) {
            self.ephemeral.campaign_online_since.insert(ucid, now);
        } else {
            self.ephemeral.campaign_online_since.remove(&ucid);
        }
        if let Some(c) = self.persisted.campaign_stats.coalition_mut(old) {
            c.registered = c.registered.saturating_sub(1);
            if player_points != 0 {
                c.points.active_gain -= player_points;
            }
        }
        if let Some(c) = self.persisted.campaign_stats.coalition_mut(new_side) {
            c.registered += 1;
            if player_points != 0 {
                c.points.active_gain += player_points;
            }
        }
        self.ephemeral.dirty();
    }

    pub fn campaign_on_player_death(&mut self, side: Side) {
        if !self.campaign_stats_active() {
            return;
        }
        if let Some(c) = self.persisted.campaign_stats.coalition_mut(side) {
            c.player_deaths += 1;
            self.ephemeral.dirty();
        }
    }

    /// Server presence only — aircraft / observer / spectator slot is irrelevant.
    pub fn campaign_on_connect(&mut self, ucid: Ucid, now: DateTime<Utc>) {
        if !self.campaign_stats_active() {
            return;
        }
        // Stamp connect time even before register so mid-session join still counts
        // wall-clock from connect once Blue/Red is known.
        self.ephemeral.campaign_online_since.insert(ucid, now);
    }

    pub fn campaign_on_disconnect(&mut self, ucid: &Ucid, now: DateTime<Utc>) {
        if !self.campaign_stats_active() {
            return;
        }
        let side = self
            .persisted
            .players
            .get(ucid)
            .map(|p| p.side)
            .filter(|s| matches!(s, Side::Blue | Side::Red));
        if let Some(side) = side {
            self.campaign_credit_online(*ucid, now, side, false);
        } else {
            self.ephemeral.campaign_online_since.remove(ucid);
        }
    }

    pub fn campaign_flush_online_before_save(&mut self, now: DateTime<Utc>) {
        if !self.campaign_stats_active() {
            return;
        }
        let ucids: Vec<Ucid> = self.ephemeral.campaign_online_since.keys().copied().collect();
        for ucid in ucids {
            let side = self
                .persisted
                .players
                .get(&ucid)
                .map(|p| p.side)
                .filter(|s| matches!(s, Side::Blue | Side::Red));
            if let Some(side) = side {
                self.campaign_credit_online(ucid, now, side, true);
            }
        }
    }

    /// Credit elapsed online seconds to `side`. If `keep_running`, restart the clock at `now`.
    fn campaign_credit_online(
        &mut self,
        ucid: Ucid,
        now: DateTime<Utc>,
        side: Side,
        keep_running: bool,
    ) {
        let Some(since) = self.ephemeral.campaign_online_since.remove(&ucid) else {
            return;
        };
        if keep_running {
            self.ephemeral.campaign_online_since.insert(ucid, now);
        }
        if !matches!(side, Side::Blue | Side::Red) {
            return;
        }
        let secs = (now - since).num_seconds().max(0) as u64;
        if secs == 0 {
            return;
        }
        if let Some(c) = self.persisted.campaign_stats.coalition_mut(side) {
            c.online_secs += secs;
            self.ephemeral.dirty();
        }
    }

    /// Cross-objective dynamic cargo stocked at `dest_side` (kg floored; deliveries = events).
    pub fn campaign_on_dynamic_cargo_delivery(
        &mut self,
        dest_side: Side,
        weight_kg: f64,
        deliveries: u32,
    ) {
        if !self.campaign_stats_active() || deliveries == 0 {
            return;
        }
        let kg = weight_kg.max(0.).floor() as u64;
        if let Some(c) = self.persisted.campaign_stats.coalition_mut(dest_side) {
            c.dynamic_cargo.deliveries = c.dynamic_cargo.deliveries.saturating_add(deliveries);
            c.dynamic_cargo.tonnage_kg = c.dynamic_cargo.tonnage_kg.saturating_add(kg);
            self.ephemeral.dirty();
        }
    }

    pub fn campaign_on_active_gain(&mut self, side: Side, amount: i64) {
        if !self.campaign_stats_active() || amount == 0 {
            return;
        }
        if let Some(c) = self.persisted.campaign_stats.coalition_mut(side) {
            c.points.active_gain += amount;
            self.ephemeral.dirty();
        }
    }

    pub fn campaign_on_balancing_gain(&mut self, side: Side, amount: i64) {
        if !self.campaign_stats_active() || amount == 0 {
            return;
        }
        if let Some(c) = self.persisted.campaign_stats.coalition_mut(side) {
            c.points.balancing_gain += amount;
            self.ephemeral.dirty();
        }
    }

    pub fn campaign_on_invested(
        &mut self,
        side: Side,
        bucket: InvestBucket,
        player_amount: u32,
        objective_amount: u32,
    ) {
        if !self.campaign_stats_active() {
            return;
        }
        let Some(c) = self.persisted.campaign_stats.coalition_mut(side) else {
            return;
        };
        if player_amount > 0 {
            match bucket {
                InvestBucket::Air => c.points.invested_air += player_amount as u64,
                InvestBucket::Ground => c.points.invested_ground += player_amount as u64,
                InvestBucket::Navy => c.points.invested_navy += player_amount as u64,
            }
        }
        if objective_amount > 0 {
            c.points.invested_ground += objective_amount as u64;
        }
        if player_amount > 0 || objective_amount > 0 {
            self.ephemeral.dirty();
        }
    }

    pub fn campaign_on_refund(
        &mut self,
        side: Side,
        bucket: InvestBucket,
        player_amount: u32,
        objective_amount: u32,
    ) {
        if !self.campaign_stats_active() {
            return;
        }
        let Some(c) = self.persisted.campaign_stats.coalition_mut(side) else {
            return;
        };
        if player_amount > 0 {
            match bucket {
                InvestBucket::Air => {
                    c.points.invested_air = c.points.invested_air.saturating_sub(player_amount as u64)
                }
                InvestBucket::Ground => {
                    c.points.invested_ground =
                        c.points.invested_ground.saturating_sub(player_amount as u64)
                }
                InvestBucket::Navy => {
                    c.points.invested_navy = c.points.invested_navy.saturating_sub(player_amount as u64)
                }
            }
        }
        if objective_amount > 0 {
            c.points.invested_ground =
                c.points.invested_ground.saturating_sub(objective_amount as u64);
        }
        if player_amount > 0 || objective_amount > 0 {
            self.ephemeral.dirty();
        }
    }

    pub fn campaign_on_adjust_points(&mut self, ucid: &Ucid, amount: i32, why: &str) {
        if !self.campaign_stats_active() || amount == 0 {
            return;
        }
        let Some(player) = self.persisted.players.get(ucid) else {
            return;
        };
        let side = player.side;
        if amount > 0 {
            if why.contains("periodic award (balancing)") {
                self.campaign_on_balancing_gain(side, amount as i64);
            } else {
                self.campaign_on_active_gain(side, amount as i64);
            }
        } else if why.contains("airborne deslot penalty")
            || why.contains("for the loss of action group")
        {
            self.campaign_on_active_gain(side, amount as i64);
        }
    }

    pub fn campaign_record_unit_loss(&mut self, gid: GroupId, vehicle: &Vehicle) {
        if !self.campaign_stats_active() {
            return;
        }
        let Ok(group) = self.group(&gid) else {
            return;
        };
        let side = group.side;
        // Still flying to drop WP (destination not yet cleared) and this death wipes the group.
        let para_troops_lost = match &group.origin {
            DeployKind::Action {
                spec,
                destination: Some(_),
                ..
            } if matches!(spec.kind, ActionKind::Paratrooper(_)) => group
                .units
                .into_iter()
                .filter_map(|uid| self.persisted.units.get(uid))
                .all(|u| u.dead),
            _ => false,
        };
        if let Some(bucket) = self.classify_group_loss(&group, vehicle) {
            self.record_loss(side, bucket);
        }
        if para_troops_lost {
            if let Some(c) = self.persisted.campaign_stats.coalition_mut(side) {
                c.losses.troops = c.losses.troops.saturating_add(8);
                self.ephemeral.dirty();
            }
        }
    }

    /// Player airframe loss when `unit_dead` is skipped (airborne_deslot_block).
    pub fn campaign_record_player_airframe_loss(
        &mut self,
        ucid: &Ucid,
        unit_id: &dcso3::object::DcsOid<dcso3::unit::ClassUnit>,
    ) {
        if !self.campaign_stats_active() {
            return;
        }
        if !self
            .ephemeral
            .campaign_airframe_loss_ids
            .insert(unit_id.clone())
        {
            return;
        }
        let Some(player) = self.persisted.players.get(ucid) else {
            return;
        };
        let side = player.side;
        let vehicle = player
            .current_slot
            .as_ref()
            .and_then(|(_, inst)| inst.as_ref().map(|i| i.typ.clone()))
            .or_else(|| {
                self.ephemeral
                    .get_slot_by_object_id(unit_id)
                    .and_then(|slot| self.ephemeral.get_slot_info(slot))
                    .map(|s| s.typ.clone())
            });
        let Some(vehicle) = vehicle else {
            return;
        };
        let helo = vehicle_is_helo(self, &vehicle);
        let bucket = self
            .ephemeral
            .cfg
            .life_types
            .get(&vehicle)
            .copied()
            .map(|lt| life_type_loss_bucket(lt, helo))
            .or_else(|| self.classify_tags_loss(&vehicle));
        if let Some(bucket) = bucket {
            self.record_loss(side, bucket);
        }
    }

    pub fn campaign_record_static_loss(&mut self, gid: GroupId) {
        if !self.campaign_stats_active() {
            return;
        }
        let Ok(group) = self.group(&gid) else {
            return;
        };
        let side = group.side;
        let class = group.class;
        let typ = group
            .units
            .into_iter()
            .next()
            .and_then(|uid| self.persisted.units.get(&uid).map(|u| u.typ.clone()));
        if typ
            .as_ref()
            .is_some_and(|t| self.ephemeral.cfg.objective_static_units.contains_key(t.as_str()))
        {
            self.record_loss(side, LossBucketField::BuildingStatic);
            return;
        }
        if let Some(bucket) = static_class_loss_bucket(class) {
            self.record_loss(side, bucket);
        }
    }

    pub fn campaign_build_view(&self, theatre: &str) -> Option<CampaignStatsView> {
        if !self.campaign_stats_active() {
            return None;
        }
        let rpd = self.ephemeral.cfg.discord_map.rounds_per_day.max(1);
        let stats = self.persisted.campaign_stats.clone();
        let duration_days = stats.duration_days(rpd);
        let start = stats
            .campaign_start_date
            .map(format_campaign_date_short)
            .unwrap_or_else(|| "—".into());
        let current = format_campaign_date_short(Utc::now().date_naive());
        Some(CampaignStatsView {
            theatre: theatre.to_string(),
            start_date: start,
            current_date: current,
            duration_days,
            online_hours_blue: (stats.blue.online_secs / 3600) as u32,
            online_hours_red: (stats.red.online_secs / 3600) as u32,
            stats,
        })
    }

    fn classify_group_loss(&self, group: &SpawnedGroup, vehicle: &Vehicle) -> Option<LossBucketField> {
        if self
            .ephemeral
            .cfg
            .objective_static_units
            .contains_key(vehicle.as_str())
        {
            return Some(LossBucketField::BuildingStatic);
        }
        let helo = vehicle_is_helo(self, vehicle);
        if let DeployKind::Action { spec, .. } = &group.origin {
            return Some(action_kind_loss_bucket(&spec.kind, helo));
        }
        if let Some(lt) = self.ephemeral.cfg.life_types.get(vehicle) {
            return Some(life_type_loss_bucket(*lt, helo));
        }
        self.classify_tags_loss(vehicle)
    }

    /// Player deaths only; unit losses are counted in `unit_dead` / `static_dead`.
    pub fn campaign_on_victim_killed(&mut self, dead: &bfprotocols::shots::Dead) {
        if !self.campaign_stats_active() {
            return;
        }
        use bfprotocols::shots::Who;
        if matches!(dead.victim, Who::Player { .. }) {
            self.campaign_on_player_death(*dead.victim.side());
        }
    }

    fn is_building_logi_facility(vehicle: &Vehicle) -> bool {
        let n = vehicle.0.as_str();
        let lower = n.to_ascii_lowercase();
        n.contains("Invincible")
            || lower.contains("farp")
            || lower.contains("depot")
            || lower.contains("bunker")
            || lower.contains("warehouse")
            || lower.contains("ammo")
            || lower.contains("fuel")
            || lower.contains("container")
            || lower.contains("tent")
            || lower.contains(".command")
            || lower.contains("ammunition")
    }

    fn classify_tags_loss(&self, vehicle: &Vehicle) -> Option<LossBucketField> {
        let tags = self.ephemeral.cfg.unit_classification.get(vehicle)?;
        if tags.contains(UnitTag::ShipCarrier) {
            return Some(LossBucketField::Carriers);
        }
        if tags.contains(UnitTag::ShipWithHeliport) {
            return Some(LossBucketField::ShipsMedium);
        }
        if tags.contains(UnitTag::ShipNoHeliport) | tags.contains(UnitTag::Boat) {
            return Some(LossBucketField::ShipsSmall);
        }
        if tags.contains(UnitTag::Artillery) {
            return Some(LossBucketField::Artillery);
        }
        if tags.contains(UnitTag::Infantry) {
            return Some(LossBucketField::Troops);
        }
        if tags.contains(UnitTag::AAA) {
            return Some(LossBucketField::Aaa);
        }
        if tags.contains(UnitTag::SAM) {
            if tags.contains(UnitTag::LR) {
                return Some(LossBucketField::SamLr);
            }
            if tags.contains(UnitTag::MR) {
                return Some(LossBucketField::SamMr);
            }
            if tags.contains(UnitTag::SR) {
                return Some(LossBucketField::SamSr);
            }
            return Some(LossBucketField::SamSr);
        }
        if tags.contains(UnitTag::EWR) {
            return Some(LossBucketField::EwrRadars);
        }
        if tags.contains(UnitTag::Armor)
            | tags.contains(UnitTag::APC)
            | tags.contains(UnitTag::Launcher)
        {
            return Some(LossBucketField::Armored);
        }
        if tags.contains(UnitTag::Logistics) && tags.contains(UnitTag::Unarmed) {
            if Self::is_building_logi_facility(vehicle) {
                return Some(LossBucketField::BuildingLogi);
            }
            return Some(LossBucketField::Utility);
        }
        if tags.contains(UnitTag::Logistics) {
            return Some(LossBucketField::BuildingLogi);
        }
        if tags.contains(UnitTag::Unarmed) {
            return Some(LossBucketField::Utility);
        }
        if tags.contains(UnitTag::Helicopter) {
            if tags.contains(UnitTag::Logistics) {
                return Some(LossBucketField::HelicoptersLog);
            }
            return Some(LossBucketField::HelicoptersAtk);
        }
        if tags.contains(UnitTag::Aircraft) {
            if tags.contains(UnitTag::AWACS) {
                return Some(LossBucketField::PlanesAwacs);
            }
            return Some(LossBucketField::PlanesStandard);
        }
        None
    }
}

#[derive(Debug, Clone, Copy)]
enum LossBucketField {
    PlanesStandard,
    PlanesIntercept,
    PlanesAttack,
    PlanesRecon,
    PlanesBomber,
    PlanesTanker,
    PlanesAwacs,
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

fn vehicle_is_helo(db: &Db, vehicle: &Vehicle) -> bool {
    db.ephemeral
        .cfg
        .unit_classification
        .get(vehicle)
        .is_some_and(|t| t.contains(UnitTag::Helicopter))
}

fn life_type_loss_bucket(lt: LifeType, helo: bool) -> LossBucketField {
    match lt {
        LifeType::Standard => LossBucketField::PlanesStandard,
        LifeType::Intercept => LossBucketField::PlanesIntercept,
        LifeType::Attack if helo => LossBucketField::HelicoptersAtk,
        LifeType::Attack => LossBucketField::PlanesAttack,
        LifeType::Recon => LossBucketField::PlanesRecon,
        LifeType::Logistics if helo => LossBucketField::HelicoptersLog,
        LifeType::Logistics => LossBucketField::PlanesAttack,
        LifeType::CombinedArms => LossBucketField::PlanesAttack,
    }
}

fn action_kind_loss_bucket(kind: &ActionKind, helo: bool) -> LossBucketField {
    match kind {
        ActionKind::Fighters(_) | ActionKind::FighersWaypoint => LossBucketField::PlanesIntercept,
        ActionKind::Attackers(_)
        | ActionKind::AttackersWaypoint
        | ActionKind::Sead(_)
        | ActionKind::SeadWaypoint => {
            if helo {
                LossBucketField::HelicoptersAtk
            } else {
                LossBucketField::PlanesAttack
            }
        }
        ActionKind::Bomber(_) => LossBucketField::PlanesBomber,
        ActionKind::Tanker(_) | ActionKind::TankerWaypoint => LossBucketField::PlanesTanker,
        ActionKind::Awacs(_) | ActionKind::AwacsWaypoint => LossBucketField::PlanesAwacs,
        ActionKind::Drone(_) | ActionKind::DroneWaypoint => LossBucketField::Drones,
        ActionKind::CruiseMissileSpawn(_) | ActionKind::CruiseMissileWaypoint => {
            LossBucketField::Drones
        }
        ActionKind::LogisticsRepair(_) | ActionKind::LogisticsTransfer(_) => {
            LossBucketField::HelicoptersLog
        }
        ActionKind::Paratrooper(_) => LossBucketField::HelicoptersLog,
        _ => {
            if helo {
                LossBucketField::HelicoptersAtk
            } else {
                LossBucketField::PlanesStandard
            }
        }
    }
}

fn static_class_loss_bucket(class: ObjGroupClass) -> Option<LossBucketField> {
    match class {
        ObjGroupClass::Logi => Some(LossBucketField::BuildingLogi),
        ObjGroupClass::Production => Some(LossBucketField::BuildingProd),
        ObjGroupClass::ObjectiveStatic => Some(LossBucketField::BuildingStatic),
        ObjGroupClass::Aaa => Some(LossBucketField::Aaa),
        ObjGroupClass::Sr => Some(LossBucketField::SamSr),
        ObjGroupClass::Mr => Some(LossBucketField::SamMr),
        ObjGroupClass::Lr => Some(LossBucketField::SamLr),
        ObjGroupClass::Armor => Some(LossBucketField::Armored),
        _ => None,
    }
}

pub fn action_invest_bucket(kind: &ActionKind) -> InvestBucket {
    match kind {
        ActionKind::Tanker(_)
        | ActionKind::TankerWaypoint
        | ActionKind::Awacs(_)
        | ActionKind::AwacsWaypoint
        | ActionKind::Bomber(_)
        | ActionKind::Fighters(_)
        | ActionKind::FighersWaypoint
        | ActionKind::Attackers(_)
        | ActionKind::AttackersWaypoint
        | ActionKind::Sead(_)
        | ActionKind::SeadWaypoint
        | ActionKind::Drone(_)
        | ActionKind::DroneWaypoint
        | ActionKind::CruiseMissileSpawn(_)
        | ActionKind::CruiseMissileWaypoint
        | ActionKind::LogisticsRepair(_)
        | ActionKind::LogisticsTransfer(_) => InvestBucket::Air,
        ActionKind::Paratrooper(_)
        | ActionKind::Deployable(_)
        | ActionKind::Move(_)
        | ActionKind::Nuke(_) => InvestBucket::Ground,
        ActionKind::Rtb
        | ActionKind::Start
        | ActionKind::Status
        | ActionKind::Rearm => InvestBucket::Ground,
    }
}

pub fn deployable_invest_bucket(template: &str, cfg: &bfprotocols::cfg::Cfg) -> InvestBucket {
    for deps in cfg.deployables.values() {
        for dep in deps {
            if let bfprotocols::cfg::DeployableKind::Group { template: t } = &dep.kind {
                if t.as_str() == template {
                    let path = dep.path.join(" ").to_ascii_lowercase();
                    if path.contains("ship")
                        || path.contains("carrier")
                        || path.contains("frigate")
                    {
                        return InvestBucket::Navy;
                    }
                    return InvestBucket::Ground;
                }
            }
        }
    }
    InvestBucket::Ground
}
