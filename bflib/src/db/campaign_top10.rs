//! Per-player Top 10 boards for the Discord interactive map left sidebar.

use super::Db;
use bfprotocols::{
    cfg::UnitTag,
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
    Naval,
    Logistics,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayerTop10Stats {
    pub name: String,
    pub a2a: u32,
    pub a2g: u32,
    pub naval: u32,
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
    pub naval: Vec<Top10Row>,
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
        let Some(bucket) = classify_unit_kill_bucket(self, dead) else {
            return;
        };
        let participants = player_kill_participants(dead);
        if participants.is_empty() {
            return;
        }
        for ucid in participants {
            self.top10_credit(ucid, bucket);
        }
    }

    /// Player-only static kill (buildings / factories) → A2G.
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
        self.top10_credit(*ucid, Top10Bucket::A2G);
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
        Some(CampaignTop10View {
            a2a: self.top10_board(|p| p.a2a),
            a2g: self.top10_board(|p| p.a2g),
            naval: self.top10_board(|p| p.naval),
            logistics: self.top10_board(|p| p.logistics),
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
            Top10Bucket::Naval => entry.naval = entry.naval.saturating_add(1),
            Top10Bucket::Logistics => entry.logistics = entry.logistics.saturating_add(1),
        }
        self.ephemeral.dirty();
    }

    fn top10_board(&self, count_of: impl Fn(&PlayerTop10Stats) -> u32) -> Vec<Top10Row> {
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
        rows.truncate(10);
        rows
    }
}

fn classify_unit_kill_bucket(db: &Db, dead: &Dead) -> Option<Top10Bucket> {
    let typ = dead
        .shots
        .iter()
        .find(|s| !s.target_typ.trim().is_empty())
        .map(|s| s.target_typ.as_str())?;
    let tags = db.ephemeral.cfg.unit_classification.get(typ)?;
    if tags.contains(UnitTag::Aircraft) || tags.contains(UnitTag::Helicopter) {
        return Some(Top10Bucket::A2A);
    }
    if tags.contains(UnitTag::ShipCarrier)
        || tags.contains(UnitTag::ShipWithHeliport)
        || tags.contains(UnitTag::ShipNoHeliport)
        || tags.contains(UnitTag::Boat)
    {
        return Some(Top10Bucket::Naval);
    }
    Some(Top10Bucket::A2G)
}

fn player_kill_participants(dead: &Dead) -> SmallVec<[Ucid; 8]> {
    let mut out: SmallVec<[Ucid; 8]> = smallvec![];
    let accept = |shot: &bfprotocols::shots::Shot| -> Option<Ucid> {
        let Who::Player {
            ucid, side, ..
        } = &shot.shooter
        else {
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
        Some(*ucid)
    };
    for shot in &dead.shots {
        if shot.hit {
            if let Some(ucid) = accept(shot) {
                if !out.contains(&ucid) {
                    out.push(ucid);
                }
            }
        }
    }
    if out.is_empty() {
        for shot in &dead.shots {
            if dead.time - shot.time <= Duration::minutes(3) {
                if let Some(ucid) = accept(shot) {
                    if !out.contains(&ucid) {
                        out.push(ucid);
                    }
                }
            }
        }
    }
    out
}

pub fn render_top10_sidebar_html(view: &CampaignTop10View) -> String {
    let mut out = String::from(r#"<div class="sidebar-top10">"#);
    out.push_str(&top10_section("Top 10 Killboard", "A2A", &view.a2a));
    out.push_str(&top10_section("Top 10 Killboard", "A2G", &view.a2g));
    out.push_str(&top10_section("Top 10 Killboard", "A2S", &view.naval));
    out.push_str(&top10_section("Top 10 Support", "LOG", &view.logistics));
    out.push_str("</div>");
    out
}

fn top10_section(title: &str, badge: &str, rows: &[Top10Row]) -> String {
    let mut body = String::new();
    for r in rows {
        let cls = match r.side {
            Side::Blue => "top10-blue",
            Side::Red => "top10-red",
            _ => "top10-neutral",
        };
        body.push_str(&format!(
            r#"<div class="pilot-row"><span class="rank-col" aria-hidden="true"></span><span class="pilot-name {cls}">{name}</span><span class="pilot-ping {cls}">{count}</span></div>"#,
            cls = cls,
            name = html_escape(&r.name),
            count = r.count,
        ));
    }
    format!(
        r#"<div class="pilot-block pilot-block-neutral"><div class="pilot-hdr-row pilot-hdr-top10"><span class="rank-col" aria-hidden="true"></span><span class="pilot-hdr-title">{title}</span><span class="pilot-hdr-ping">{badge}</span></div><div class="pilot-list">{rows}</div></div>"#,
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
