//! Per-player Top 10 boards for the Discord interactive map left sidebar.

use super::Db;
use bfprotocols::{
    cfg::{UnitTag, Vehicle},
    shots::{Dead, Who},
};
use chrono::Duration;
use dcso3::{coalition::Side, net::Ucid};
use fxhash::FxHashMap;
use serde_derive::{Deserialize, Serialize};
use smallvec::{smallvec, SmallVec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Top10Bucket {
    A2A,
    A2G,
    A2S,
    G2A,
    G2G,
    G2S,
    Logistics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Air,
    Ground,
    Ship,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayerTop10Stats {
    pub name: String,
    #[serde(default)]
    pub a2a: u32,
    #[serde(default)]
    pub a2g: u32,
    #[serde(default)]
    pub a2s: u32,
    #[serde(default)]
    pub g2a: u32,
    #[serde(default)]
    pub g2g: u32,
    #[serde(default)]
    pub g2s: u32,
    #[serde(default)]
    pub logistics: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CampaignTop10 {
    #[serde(default)]
    pub players: FxHashMap<Ucid, PlayerTop10Stats>,
}

#[derive(Debug, Clone)]
pub struct Top10Row {
    pub name: String,
    pub side: Side,
    pub count: u32,
}

#[derive(Debug, Clone, Default)]
pub struct CampaignTop10View {
    pub a2a: Vec<Top10Row>,
    pub a2g: Vec<Top10Row>,
    pub a2s: Vec<Top10Row>,
    pub g2a: Vec<Top10Row>,
    pub g2g: Vec<Top10Row>,
    pub g2s: Vec<Top10Row>,
    pub logistics: Vec<Top10Row>,
}

impl Db {
    pub fn campaign_top10_active(&self) -> bool {
        self.ephemeral.cfg.discord_map.enabled && self.ephemeral.cfg.discord_map.campaign_top10
    }

    pub fn campaign_top10_on_kill(&mut self, dead: &Dead) {
        if !self.campaign_top10_active() {
            return;
        }
        let Some(target) = classify_target_kind(self, dead) else {
            return;
        };
        let participants = player_kill_participants(self, dead);
        if participants.is_empty() {
            return;
        }
        for (ucid, air) in participants {
            let bucket = bucket_for(air, target);
            self.top10_credit(ucid, bucket);
        }
    }

    /// Player-only static kill (buildings / factories) → A2G or G2G by shooter platform.
    pub fn campaign_top10_on_static_kill(&mut self, killer: &Who, owner: Side) {
        if !self.campaign_top10_active() {
            return;
        }
        let Who::Player { ucid, side, .. } = killer else {
            return;
        };
        if *side == owner {
            return;
        }
        let bucket = if shooter_is_air(self, killer) {
            Top10Bucket::A2G
        } else {
            Top10Bucket::G2G
        };
        self.top10_credit(*ucid, bucket);
    }

    pub fn campaign_top10_on_logistics(&mut self, ucid: Ucid) {
        if !self.campaign_top10_active() {
            return;
        }
        self.top10_credit(ucid, Top10Bucket::Logistics);
    }

    pub fn campaign_top10_on_capture(&mut self, by: &[Ucid]) {
        if !self.campaign_top10_active() {
            return;
        }
        for ucid in by {
            self.top10_credit(*ucid, Top10Bucket::Logistics);
        }
    }

    pub fn campaign_top10_build_view(&self) -> Option<CampaignTop10View> {
        if !self.campaign_top10_active() {
            return None;
        }
        let dm = &self.ephemeral.cfg.discord_map;
        Some(CampaignTop10View {
            a2a: self.top10_board(|p| p.a2a, dm.campaign_top10_A2A_open),
            a2g: self.top10_board(|p| p.a2g, dm.campaign_top10_A2G_open),
            a2s: self.top10_board(|p| p.a2s, dm.campaign_top10_A2S_open),
            g2a: self.top10_board(|p| p.g2a, dm.campaign_top10_G2A_open),
            g2g: self.top10_board(|p| p.g2g, dm.campaign_top10_G2G_open),
            g2s: self.top10_board(|p| p.g2s, dm.campaign_top10_G2S_open),
            logistics: self.top10_board(|p| p.logistics, dm.campaign_top10_LOG_open),
        })
    }

    fn top10_credit(&mut self, ucid: Ucid, bucket: Top10Bucket) {
        let name = self
            .persisted
            .players
            .get(&ucid)
            .map(|p| p.name.to_string())
            .unwrap_or_else(|| ucid.to_string());
        let entry = self
            .persisted
            .campaign_top10
            .players
            .entry(ucid)
            .or_default();
        entry.name = name;
        match bucket {
            Top10Bucket::A2A => entry.a2a = entry.a2a.saturating_add(1),
            Top10Bucket::A2G => entry.a2g = entry.a2g.saturating_add(1),
            Top10Bucket::A2S => entry.a2s = entry.a2s.saturating_add(1),
            Top10Bucket::G2A => entry.g2a = entry.g2a.saturating_add(1),
            Top10Bucket::G2G => entry.g2g = entry.g2g.saturating_add(1),
            Top10Bucket::G2S => entry.g2s = entry.g2s.saturating_add(1),
            Top10Bucket::Logistics => entry.logistics = entry.logistics.saturating_add(1),
        }
        self.ephemeral.dirty();
    }

    fn top10_board(
        &self,
        count_of: impl Fn(&PlayerTop10Stats) -> u32,
        open: u32,
    ) -> Vec<Top10Row> {
        let mut rows: Vec<Top10Row> = self
            .persisted
            .campaign_top10
            .players
            .iter()
            .filter_map(|(ucid, st)| {
                let count = count_of(st);
                if count == 0 {
                    return None;
                }
                let side = self
                    .persisted
                    .players
                    .get(ucid)
                    .map(|p| p.side)
                    .unwrap_or(Side::Neutral);
                if !matches!(side, Side::Blue | Side::Red) {
                    return None;
                }
                Some(Top10Row {
                    name: st.name.clone(),
                    side,
                    count,
                })
            })
            .collect();
        rows.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
        rows.truncate(open as usize);
        rows
    }
}

fn bucket_for(air_shooter: bool, target: TargetKind) -> Top10Bucket {
    match (air_shooter, target) {
        (true, TargetKind::Air) => Top10Bucket::A2A,
        (true, TargetKind::Ground) => Top10Bucket::A2G,
        (true, TargetKind::Ship) => Top10Bucket::A2S,
        (false, TargetKind::Air) => Top10Bucket::G2A,
        (false, TargetKind::Ground) => Top10Bucket::G2G,
        (false, TargetKind::Ship) => Top10Bucket::G2S,
    }
}

fn classify_target_kind(db: &Db, dead: &Dead) -> Option<TargetKind> {
    let typ = dead
        .shots
        .iter()
        .find(|s| !s.target_typ.trim().is_empty())
        .map(|s| s.target_typ.as_str())?;
    let tags = db.ephemeral.cfg.unit_classification.get(&Vehicle::from(typ))?;
    if tags.contains(UnitTag::Aircraft) || tags.contains(UnitTag::Helicopter) {
        return Some(TargetKind::Air);
    }
    if tags.contains(UnitTag::ShipCarrier)
        || tags.contains(UnitTag::ShipWithHeliport)
        || tags.contains(UnitTag::ShipNoHeliport)
        || tags.contains(UnitTag::Boat)
    {
        return Some(TargetKind::Ship);
    }
    Some(TargetKind::Ground)
}

fn shooter_vehicle(db: &Db, who: &Who) -> Option<Vehicle> {
    let Who::Player { unit, ucid, .. } = who else {
        return None;
    };
    if let Some(uid) = db.ephemeral.get_uid_by_object_id(unit) {
        if let Some(su) = db.persisted.units.get(uid) {
            return Some(su.typ.clone());
        }
    }
    db.persisted
        .players
        .get(ucid)
        .and_then(|p| p.current_slot.as_ref())
        .and_then(|(_, inst)| inst.as_ref())
        .map(|inst| inst.typ.clone())
}

/// Aircraft / helicopter → air board; anything else (incl. Combined Arms) → ground board.
fn shooter_is_air(db: &Db, who: &Who) -> bool {
    let Some(typ) = shooter_vehicle(db, who) else {
        return false;
    };
    let Some(tags) = db.ephemeral.cfg.unit_classification.get(&typ) else {
        return false;
    };
    tags.contains(UnitTag::Aircraft) || tags.contains(UnitTag::Helicopter)
}

fn player_kill_participants(db: &Db, dead: &Dead) -> SmallVec<[(Ucid, bool); 8]> {
    let mut out: SmallVec<[(Ucid, bool); 8]> = smallvec![];
    let accept = |shot: &bfprotocols::shots::Shot| -> Option<(Ucid, bool)> {
        let Who::Player { ucid, side, .. } = &shot.shooter else {
            return None;
        };
        if side == dead.victim.side() {
            return None;
        }
        if let Who::Player { ucid: victim, .. } = &dead.victim {
            if victim == ucid {
                return None;
            }
        }
        Some((*ucid, shooter_is_air(db, &shot.shooter)))
    };
    for shot in &dead.shots {
        if shot.hit {
            if let Some((ucid, air)) = accept(shot) {
                if !out.iter().any(|(u, _)| *u == ucid) {
                    out.push((ucid, air));
                }
            }
        }
    }
    if out.is_empty() {
        for shot in &dead.shots {
            if dead.time - shot.time <= Duration::minutes(3) {
                if let Some((ucid, air)) = accept(shot) {
                    if !out.iter().any(|(u, _)| *u == ucid) {
                        out.push((ucid, air));
                    }
                }
            }
        }
    }
    out
}

pub fn render_top10_sidebar_html(
    view: &CampaignTop10View,
    dm: &bfprotocols::cfg::DiscordMapCfg,
) -> String {
    let mut out = String::from(r#"<div class="sidebar-top10">"#);
    out.push_str(&top10_section(
        &format!("Top {} Killboard", dm.campaign_top10_A2A_open),
        "A2A",
        &view.a2a,
        dm.campaign_top10_A2A_closed as usize,
    ));
    out.push_str(&top10_section(
        &format!("Top {} Killboard", dm.campaign_top10_A2G_open),
        "A2G",
        &view.a2g,
        dm.campaign_top10_A2G_closed as usize,
    ));
    out.push_str(&top10_section(
        &format!("Top {} Killboard", dm.campaign_top10_A2S_open),
        "A2S",
        &view.a2s,
        dm.campaign_top10_A2S_closed as usize,
    ));
    out.push_str(&top10_section(
        &format!("Top {} Support", dm.campaign_top10_LOG_open),
        "LOG",
        &view.logistics,
        dm.campaign_top10_LOG_closed as usize,
    ));
    out.push_str(&top10_section(
        &format!("Top {} Killboard  (CA)", dm.campaign_top10_G2A_open),
        "G2A",
        &view.g2a,
        dm.campaign_top10_G2A_closed as usize,
    ));
    out.push_str(&top10_section(
        &format!("Top {} Killboard  (CA)", dm.campaign_top10_G2G_open),
        "G2G",
        &view.g2g,
        dm.campaign_top10_G2G_closed as usize,
    ));
    out.push_str(&top10_section(
        &format!("Top {} Killboard  (CA)", dm.campaign_top10_G2S_open),
        "G2S",
        &view.g2s,
        dm.campaign_top10_G2S_closed as usize,
    ));
    out.push_str("</div>");
    out
}

fn top10_section(title: &str, badge: &str, rows: &[Top10Row], preview: usize) -> String {
    let mut body = String::new();
    for (i, r) in rows.iter().enumerate() {
        let cls = match r.side {
            Side::Blue => "top10-blue",
            Side::Red => "top10-red",
            _ => "top10-neutral",
        };
        let extra = if i < preview {
            " top10-row-preview"
        } else {
            " top10-row-more"
        };
        body.push_str(&format!(
            r#"<div class="pilot-row{extra}"><span class="rank-col" aria-hidden="true"></span><span class="pilot-name {cls}">{name}</span><span class="pilot-ping {cls}">{count}</span></div>"#,
            extra = extra,
            cls = cls,
            name = html_escape(&r.name),
            count = r.count,
        ));
    }
    format!(
        r#"<div class="pilot-block pilot-block-neutral top10-collapsible" data-top10-preview="{preview}"><div class="pilot-hdr-row pilot-hdr-top10"><button type="button" class="top10-toggle" aria-expanded="false" title="Show full Top 10">&#9660;</button><span class="pilot-hdr-title">{title}</span><span class="pilot-hdr-ping">{badge}</span></div><div class="pilot-list">{rows}</div></div>"#,
        preview = preview,
        title = title,
        badge = badge,
        rows = body,
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
