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

EWR SYSTEM CONFIGURATION:
The EWR system supports two modes controlled by the 'ewr_mode' configuration option:
- EwrMode::Original: Original implementation with immediate track updates and complex reporting timing
- EwrMode::Delayed: Modified implementation with configurable delay on track updates and simplified reporting

The delay is controlled by the 'ewr_delay' configuration option (in seconds, default: 60).
The default mode is EwrMode::Original to maintain backward compatibility.
*/

use crate::{
    db::{
        Db,
        player::{InstancedPlayer, Player},
    },
    landcache::LandCache,
};
use anyhow::Result;
use bfprotocols::{
    cfg::EwrMode,
    stats::{DetectionSource, EnId, Stat},
};
use chrono::prelude::*;
use dcso3::{
    LuaVec2, MizLua, Position3, Vector2, Vector3, azumith2d_to, azumith3d, coalition::Side,
    land::Land, net::Ucid, radians_to_degrees,
};
use fxhash::FxHashMap;
use smallvec::{SmallVec, smallvec};
use std::fmt;

#[derive(Debug, Clone)]
pub struct GibBraa {
    pub bearing: u16,
    pub range: u32,
    pub altitude: u32,
    pub heading: u16,
    pub speed: u16,
    pub aspect: String,
    pub age: u16,
    pub units: EwrUnits,
    converted: bool,
}

const COL_BRG: usize = 4;
const COL_ASP: usize = 7;
const ASP_HYST_DEG: f64 = 5.;

/// DCS panel message display time for EWR contact reports (seconds).
pub const EWR_PANEL_DISPLAY_SECS: i64 = 20;

struct NumUnitCol {
    num_w: usize,
    unit_w: usize,
}

const COL_RNG: NumUnitCol = NumUnitCol {
    num_w: 7,
    unit_w: 2,
};
const COL_ALT: NumUnitCol = NumUnitCol {
    num_w: 7,
    unit_w: 2,
};
const COL_SPD: NumUnitCol = NumUnitCol {
    num_w: 7,
    unit_w: 3,
};
const COL_AGE: NumUnitCol = NumUnitCol {
    num_w: 4,
    unit_w: 1,
};

impl NumUnitCol {
    fn format_label(&self, label: &str) -> String {
        format!("{:>num_w$} {units}", label, units = " ".repeat(self.unit_w), num_w = self.num_w)
    }

    fn format_value(&self, value: u32, unit: &str) -> String {
        format!(
            "{:>num_w$} {unit:<unit_w$}",
            format_thousands(value),
            unit = unit,
            num_w = self.num_w,
            unit_w = self.unit_w,
        )
    }
}

fn format_thousands(n: u32) -> String {
    let s = n.to_string();
    if s.len() <= 3 {
        return s;
    }
    let mut out = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push('.');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

pub fn report_header() -> String {
    format!(
        "{:>COL_BRG$} {} {} {} {:>COL_ASP$} {}",
        "BRG",
        COL_RNG.format_label("RNG"),
        COL_ALT.format_label("ALT"),
        COL_SPD.format_label("SPD"),
        "ASP",
        COL_AGE.format_label("AGE"),
        COL_BRG = COL_BRG,
        COL_ASP = COL_ASP,
    )
    .to_string()
}

fn round_display_altitude(alt: u32) -> u32 {
    if alt >= 1000 {
        ((alt + 50) / 100) * 100
    } else {
        ((alt + 5) / 10) * 10
    }
}

fn round_display_speed(speed: u16) -> u16 {
    ((speed + 5) / 10) * 10
}

impl fmt::Display for GibBraa {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (range_u, alt_u, spd_u) = match self.units {
            EwrUnits::Imperial => ("nm", "ft", "kt"),
            EwrUnits::Metric => ("km", "m", "kmh"),
        };
        write!(
            f,
            "{:>COL_BRG$} {} {} {} {:>COL_ASP$} {}",
            self.bearing,
            COL_RNG.format_value(self.range, range_u),
            COL_ALT.format_value(round_display_altitude(self.altitude), alt_u),
            COL_SPD.format_value(round_display_speed(self.speed).into(), spd_u),
            self.aspect,
            COL_AGE.format_value(self.age as u32, "s"),
            COL_BRG = COL_BRG,
            COL_ASP = COL_ASP,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ewr_low_slow_filter_thresholds() {
        assert!(!excluded_by_ewr_low_slow_filter(30., 30., None, None));
        assert!(!excluded_by_ewr_low_slow_filter(30., 30., Some(0), Some(0)));
        assert!(excluded_by_ewr_low_slow_filter(20., 100., Some(100), None));
        assert!(!excluded_by_ewr_low_slow_filter(40., 100., Some(100), None));
        assert!(excluded_by_ewr_low_slow_filter(40., 30., None, Some(50)));
        assert!(!excluded_by_ewr_low_slow_filter(40., 80., None, Some(50)));
        assert!(excluded_by_ewr_low_slow_filter(20., 30., Some(100), Some(50)));
        assert!(!excluded_by_ewr_low_slow_filter(40., 80., Some(100), Some(50)));
    }

    #[test]
    fn format_thousands_groups_from_right() {
        assert_eq!(format_thousands(25), "25");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(1234), "1.234");
        assert_eq!(format_thousands(15200), "15.200");
        assert_eq!(format_thousands(60000), "60.000");
    }

    #[test]
    fn report_columns_align_with_header() {
        let header = report_header();
        let row = format!(
            "{}",
            GibBraa {
                bearing: 45,
                range: 25,
                altitude: 15200,
                heading: 270,
                speed: 450,
                aspect: "HOT".into(),
                age: 12,
                units: EwrUnits::Imperial,
                converted: true,
            }
        );
        assert_eq!(
            header,
            " BRG     RNG        ALT        SPD         ASP  AGE  ",
            "header mismatch"
        );
        assert_eq!(
            row,
            "  45      25 nm  15.200 ft     450 kt      HOT   12 s",
            "row mismatch"
        );
        assert_eq!(header.len(), row.len(), "column width drift");
    }

    #[test]
    fn round_display_speed_to_tens() {
        assert_eq!(round_display_speed(626), 630);
        assert_eq!(round_display_speed(673), 670);
        assert_eq!(round_display_speed(455), 460);
    }

    #[test]
    fn aspect_label_buckets() {
        assert_eq!(format_aspect_label(15., 'L'), "HOT");
        assert_eq!(format_aspect_label(45., 'R'), "FLANK R");
        assert_eq!(format_aspect_label(90., 'L'), "BEAM L");
        assert_eq!(format_aspect_label(140., 'R'), "DRAG R");
        assert_eq!(format_aspect_label(170., 'L'), "COLD");
    }

    #[test]
    fn round_display_altitude_buckets() {
        assert_eq!(round_display_altitude(950), 950);
        assert_eq!(round_display_altitude(954), 950);
        assert_eq!(round_display_altitude(956), 960);
        assert_eq!(round_display_altitude(1000), 1000);
        assert_eq!(round_display_altitude(15234), 15200);
        assert_eq!(round_display_altitude(15250), 15300);
    }
}

impl GibBraa {
    fn convert(&mut self, unit: EwrUnits) {
        if self.converted {
            return;
        }
        self.converted = true;
        match unit {
            EwrUnits::Metric => {
                self.range = self.range / 1000;
                self.speed = ((self.speed as f64) * 3.6) as u16;
            }
            EwrUnits::Imperial => {
                self.range = self.range / 1852;
                self.altitude = (self.altitude as f64 * 3.28084) as u32;
                self.speed = ((self.speed as f64) * 1.94384) as u16;
            }
        }
        self.units = unit;
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct Track {
    pos: Position3,
    velocity: Vector3,
    agl: f64,
    last: DateTime<Utc>,          // Last detection time (for age calculation)
    last_update: DateTime<Utc>,   // Last data update time (for delay mechanism)
    side: Side,
    was_detected: bool,
    detected: bool,
}

fn track_heading_deg(track: &Track) -> u16 {
    let vx = track.velocity.x;
    let vz = track.velocity.z;
    let mag_sq = vx * vx + vz * vz;
    if mag_sq > 1. {
        return normalize_heading_deg(radians_to_degrees(vx.atan2(vz)));
    }
    normalize_heading_deg(radians_to_degrees(azumith3d(track.pos.x.0)))
}

fn normalize_heading_deg(deg: f64) -> u16 {
    deg.rem_euclid(360.) as u16
}

fn compute_target_aspect(track: &Track, player_pos: Vector2) -> (f64, char) {
    let target = Vector2::new(track.pos.p.x, track.pos.p.z);
    let dx = player_pos.x - target.x;
    let dz = player_pos.y - target.y;
    let dist = (dx * dx + dz * dz).sqrt();
    if dist < 1. {
        return (0., ' ');
    }
    let to_x = dx / dist;
    let to_z = dz / dist;
    let (fwd_x, fwd_z) = track_forward_xz(track);
    let dot = (fwd_x * to_x + fwd_z * to_z).clamp(-1., 1.);
    let angle = dot.acos().to_degrees();
    let cross = fwd_x * to_z - fwd_z * to_x;
    let lr = if cross >= 0. { 'R' } else { 'L' };
    (angle, lr)
}

fn track_forward_xz(track: &Track) -> (f64, f64) {
    let vx = track.velocity.x;
    let vz = track.velocity.z;
    let mag = (vx * vx + vz * vz).sqrt();
    if mag > 1. {
        return (vx / mag, vz / mag);
    }
    let hd = radians_to_degrees(azumith3d(track.pos.x.0));
    let rad = hd.to_radians();
    (rad.sin(), rad.cos())
}

fn format_aspect_label(angle: f64, lr: char) -> String {
    if angle <= 30. {
        "HOT".into()
    } else if angle >= 160. {
        "COLD".into()
    } else if angle <= 70. {
        format!("FLANK {lr}")
    } else if angle <= 120. {
        format!("BEAM {lr}")
    } else {
        format!("DRAG {lr}")
    }
}

fn aspect_bucket_index(label: &str) -> u8 {
    if label.starts_with("HOT") {
        0
    } else if label.starts_with("FLANK") {
        1
    } else if label.starts_with("BEAM") {
        2
    } else if label.starts_with("DRAG") {
        3
    } else {
        4
    }
}

fn aspect_boundary_between(a: u8, b: u8) -> f64 {
    match (a.min(b), a.max(b)) {
        (0, 1) => 30.,
        (1, 2) => 70.,
        (2, 3) => 120.,
        (3, 4) => 160.,
        _ => 0.,
    }
}

fn stable_aspect_label(
    cache: &mut FxHashMap<(Ucid, EnId), String>,
    ucid: &Ucid,
    id: EnId,
    angle: f64,
    lr: char,
) -> String {
    let label = format_aspect_label(angle, lr);
    let key = (ucid.clone(), id);
    if let Some(prev) = cache.get(&key) {
        if prev == &label {
            return label;
        }
        let prev_bucket = aspect_bucket_index(prev);
        let new_bucket = aspect_bucket_index(&label);
        if prev_bucket != new_bucket {
            let boundary = aspect_boundary_between(prev_bucket, new_bucket);
            if boundary > 0. && (angle - boundary).abs() <= ASP_HYST_DEG {
                return prev.clone();
            }
        }
    }
    cache.insert(key, label.clone());
    label
}

fn excluded_by_ewr_low_slow_filter(
    speed_ms: f64,
    agl_m: f64,
    min_speed_kmh: Option<u32>,
    min_ralt_m: Option<u32>,
) -> bool {
    let speed_min = min_speed_kmh.filter(|v| *v > 0);
    let ralt_min = min_ralt_m.filter(|v| *v > 0);
    if speed_min.is_none() && ralt_min.is_none() {
        return false;
    }
    let slow = speed_min
        .map(|m| speed_ms * 3.6 < f64::from(m))
        .unwrap_or(true);
    let low = ralt_min.map(|m| agl_m < f64::from(m)).unwrap_or(true);
    slow && low
}

#[derive(Debug, Clone, Copy)]
pub enum EwrUnits {
    Imperial,
    Metric,
}

impl Default for EwrUnits {
    fn default() -> Self {
        Self::Metric
    }
}

#[derive(Debug, Clone, Copy)]
struct PlayerState {
    enabled: bool,
    units: EwrUnits,
    last: DateTime<Utc>,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            enabled: true,
            units: EwrUnits::default(),
            last: DateTime::default(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Ewr {
    tracks: FxHashMap<Side, FxHashMap<EnId, Track>>,
    player_state: FxHashMap<Ucid, PlayerState>,
    aspect_labels: FxHashMap<(Ucid, EnId), String>,
}

impl Ewr {
    pub fn update_tracks(
        &mut self,
        lua: MizLua,
        landcache: &mut LandCache,
        db: &Db,
        now: DateTime<Utc>,
        ewr_mode: EwrMode,
        ewr_delay: u32,
        ewr_antenna_height_m: u32,
    ) -> Result<()> {
        let land = Land::singleton(lua)?;
        let aircraft: SmallVec<[(EnId, Side, Position3, Vector3); 128]> = {
            let players = db
                .instanced_players()
                .filter(|(ucid, _, _)| !db.csar_downed_pilot(ucid))
                .filter(|(_, _, inst)| inst.in_air)
                .map(|(ucid, player, inst)| {
                    (
                        EnId::Player(*ucid),
                        player.side,
                        inst.position,
                        inst.velocity,
                    )
                });
            let actions = db
                .persisted
                .actions
                .into_iter()
                .filter_map(|gid| db.persisted.groups.get(gid))
                .flat_map(|sg| {
                    sg.units
                        .into_iter()
                        .filter_map(|uid| db.persisted.units.get(uid).map(|u| (*uid, u)))
                        .filter_map(|(uid, su)| {
                            su.airborne_velocity
                                .map(|v| (EnId::Unit(uid), sg.side, su.position, v))
                        })
                });
            players.chain(actions).collect()
        };
        for tracks in self.tracks.values_mut() {
            for track in tracks.values_mut() {
                track.detected = false;
            }
        }
        for (mut ewr_pos, ewr_side, ewr) in db.ewrs() {
            let range = (ewr.range as f64).powi(2);
            let tracks = self.tracks.entry(ewr_side).or_default();
            ewr_pos.y += f64::from(ewr_antenna_height_m);
            for (id, obj_side, pos, velocity) in &aircraft {
                let track = tracks.entry(*id).or_default();
                if track.last != now {
                    let dist = na::distance_squared(&ewr_pos.into(), &pos.p.0.into());
                    if dist <= range {
                        if landcache.is_visible(&land, dist.sqrt(), ewr_pos, pos.p.0)? {
                            match ewr_mode {
                                EwrMode::Original => {
                                    track.pos = *pos;
                                    track.velocity = *velocity;
                                    track.agl = pos.p.y
                                        - land.get_height(LuaVec2::new(pos.p.x, pos.p.z))?;
                                    track.last_update = now;
                                }
                                EwrMode::Delayed => {
                                    let time_since_update =
                                        (now - track.last_update).num_seconds();
                                    if time_since_update >= ewr_delay as i64
                                        || track.last_update == DateTime::<Utc>::UNIX_EPOCH
                                    {
                                        track.pos = *pos;
                                        track.velocity = *velocity;
                                        track.agl = pos.p.y
                                            - land.get_height(LuaVec2::new(pos.p.x, pos.p.z))?;
                                        track.last_update = now;
                                    }
                                }
                            }
                            track.last = now;
                            track.side = *obj_side;
                            track.detected |= ewr_side != *obj_side;
                        }
                    }
                }
            }
        }
        for tracks in self.tracks.values_mut() {
            for (id, track) in tracks.iter_mut() {
                if track.was_detected != track.detected {
                    track.was_detected = track.detected;
                    db.ephemeral.stat(Stat::Detected {
                        id: *id,
                        detected: track.was_detected,
                        source: DetectionSource::EWR,
                    })
                }
            }
        }
        Ok(())
    }

    pub fn toggle(&mut self, ucid: &Ucid) -> bool {
        let st = self.player_state.entry(ucid.clone()).or_default();
        st.enabled = !st.enabled;
        st.enabled
    }

    pub fn set_units(&mut self, ucid: &Ucid, units: EwrUnits) {
        self.player_state.entry(ucid.clone()).or_default().units = units;
    }

    pub fn where_chicken(
        &mut self,
        now: DateTime<Utc>,
        friendly: bool,
        force: bool,
        ucid: &Ucid,
        player: &Player,
        inst: &InstancedPlayer,
        db: &Db,
        ewr_mode: EwrMode,
        ewr_delay: u32,
    ) -> SmallVec<[GibBraa; 64]> {
        let side = player.side;
        let pos = Vector2::new(inst.position.p.x, inst.position.p.z);
        let mut reports: SmallVec<[GibBraa; 64]> = smallvec![];
        let tracks = match self.tracks.get_mut(&side) {
            Some(t) => t,
            None => return reports,
        };
        let state = self.player_state.entry(ucid.clone()).or_default();
        if !force && !state.enabled {
            return reports;
        }
        let ownship = EnId::Player(*ucid);
        tracks.retain(|tucid, track| {
            let age = (now - track.last).num_seconds();
            let include = (friendly && track.side == side) || (!friendly && track.side != side);
            if include && age <= 120 && tucid != &ownship {
                if let EnId::Player(pid) = tucid {
                    if db.csar_downed_pilot(pid) {
                        return age <= 120;
                    }
                }
                let cpos = Vector2::new(track.pos.p.x, track.pos.p.z);
                let range = na::distance(&pos.into(), &cpos.into());
                let bearing = radians_to_degrees(azumith2d_to(pos, cpos));
                let speed = track.velocity.magnitude();
                let altitude = track.pos.p.y;
                if !friendly
                    && excluded_by_ewr_low_slow_filter(
                        speed,
                        track.agl,
                        db.ephemeral.cfg.ewr_min_speed_kmh,
                        db.ephemeral.cfg.ewr_min_ralt_m,
                    )
                {
                    return age <= 120;
                }
                let heading = track_heading_deg(track);
                let (aspect_angle, aspect_lr) = compute_target_aspect(track, pos);
                let aspect = stable_aspect_label(
                    &mut self.aspect_labels,
                    ucid,
                    *tucid,
                    aspect_angle,
                    aspect_lr,
                );
                reports.push(GibBraa {
                    range: range as u32,
                    heading,
                    altitude: altitude as u32,
                    bearing: bearing as u16,
                    age: age as u16,
                    speed: speed as u16,
                    aspect,
                    units: EwrUnits::Metric,
                    converted: false,
                })
            }
            age <= 120
        });
        if reports.is_empty() {
            return reports;
        }
        reports.sort_by_key(|r| r.range);
        while reports.len() > 10 {
            reports.pop();
        }
        let since_last = (now - state.last).num_seconds();
        match ewr_mode {
            EwrMode::Original => {
                // Original reporting logic with complex timing rules
                if force
                    || since_last >= 60
                    || (reports[0].range <= 20000 && reports[0].age <= 10)
                    || (reports[0].range <= 40000 && reports[0].age <= 10 && since_last >= 30)
                {
                    state.last = now;
                    reports.iter_mut().for_each(|r| r.convert(state.units));
                    reports
                } else {
                    smallvec![]
                }
            }
            EwrMode::Delayed => {
                // With configurable track update delay, we can simplify the reporting logic
                // Reports are sent every delay period or when forced
                if force || since_last >= ewr_delay as i64 {
                    state.last = now;
                    reports.iter_mut().for_each(|r| r.convert(state.units));
                    reports
                } else {
                    smallvec![]
                }
            }
        }
    }
}
