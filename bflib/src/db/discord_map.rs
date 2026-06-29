//! Discord objective map: ME viewport zones, icon pack from the mission `.miz` in `Missions/`, Mapbox cache, Discord posts.

use super::Db;
use crate::balanced_side_gain;
use crate::bg::{self, DiscordMapPostJob, Task};
use crate::db::csar::{life_type_map_abbrev, LIFE_TYPE_DISPLAY_ORDER};
use anyhow::{anyhow, bail, Context, Result};
use bfprotocols::{
    cfg::{Cfg, DiscordMapCfg, UnitTag},
    db::objective::ObjectiveKind,
    discord_map_icon_manifest::{
        DiscordMapIconManifest, DiscordMapIconManifestRuntime, MIZ_MANIFEST,
    },
    discord_map_viewport::{
        viewport_from_corners, MapViewport, SETTINGS_DISCORD_MAP_NW, SETTINGS_DISCORD_MAP_SE,
    },
};
use chrono::{Duration, NaiveDate, prelude::*};
use dcso3::{
    coalition::{Coalition, Side},
    coord::{Coord, LLPos},
    dcs::Dcs,
    env::miz::{Miz, TriggerZone},
    group::GroupCategory,
    lfs::Lfs,
    timer::Timer,
    LuaEnv, LuaVec3, MizLua, Vector3,
};
use mlua::Value;
use bfprotocols::db::group::GroupId;
use fxhash::{FxHashMap, FxHashSet};
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zip::ZipArchive;

use crate::db::objective::{Objective, ObjGroupClass};

/// Live session inputs for map HTML clocks and pilot lists (not persisted in Db).
#[derive(Debug, Clone)]
pub struct DiscordMapPilot {
    pub name: String,
    pub ping: u32,
}

/// Live session inputs for map HTML clocks and pilot lists (not persisted in Db).
#[derive(Debug, Clone)]
pub struct DiscordMapLiveCtx {
    pub generated_at: DateTime<Utc>,
    pub shutdown_when: Option<DateTime<Utc>>,
    pub online_red: u32,
    pub online_blue: u32,
    pub blue_pilots: Vec<DiscordMapPilot>,
    pub red_pilots: Vec<DiscordMapPilot>,
    pub spectators: Vec<DiscordMapPilot>,
}

pub const POST_DEBOUNCE_SECS: i64 = 45;
pub const PERIODIC_REFRESH_WITH_PLAYERS_SECS: i64 = 300;
pub const PERIODIC_REFRESH_EMPTY_SECS: i64 = 3600;

fn periodic_refresh_interval_secs(mission_has_players: bool) -> i64 {
    if mission_has_players {
        PERIODIC_REFRESH_WITH_PLAYERS_SECS
    } else {
        PERIODIC_REFRESH_EMPTY_SECS
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordMapMetaFile {
    pub bbox: [f64; 4],
    pub width: u32,
    pub height: u32,
    pub style: String,
}

#[derive(Debug, Clone)]
pub struct DiscordMapIconPack {
    pub manifest: DiscordMapIconManifestRuntime,
    pub pngs: FxHashMap<String, Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct DiscordMapRuntime {
    pub viewport: MapViewport,
    /// ME zone centers; excluded from composited markers (viewport anchors only).
    pub corner_nw: LLPos,
    pub corner_se: LLPos,
    pub icons: Arc<DiscordMapIconPack>,
    pub base_png_path: PathBuf,
    pub composited_png_path: PathBuf,
    pub html_path: PathBuf,
    pub map_version_path: PathBuf,
    pub meta_path: PathBuf,
    pub webhook_message_path: PathBuf,
    pub mission_name: String,
    pub http_port: u16,
}

/// Mission `.miz` on disk (icon pack). `DCS` global is often nil in mission Lua — fallback to `Missions/{sortie}.miz`.
pub fn resolve_mission_miz_path(lua: MizLua, sortie_state_path: &Path) -> Result<PathBuf> {
    if let Ok(Value::Table(_)) = LuaEnv::inner(lua).globals().raw_get::<_, Value>("DCS") {
        if let Ok(dcs) = Dcs::from_lua_env(lua) {
            if let Ok(fname) = dcs.get_mission_filename() {
                let path = PathBuf::from(fname.as_str());
                if path.is_file() {
                    return Ok(path);
                }
            }
        }
    }
    let stem = sortie_state_path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("sortie state path has no file name"))?;
    let path = PathBuf::from(Lfs::singleton(lua)?.writedir()?.as_str())
        .join("Missions")
        .join(format!("{stem}.miz"));
    if !path.is_file() {
        bail!(
            "discord map: mission archive not found at {:?}",
            path
        );
    }
    Ok(path)
}

pub fn base_png_path(sortie_state_path: &Path) -> PathBuf {
    sortie_state_path.with_extension("discord_map_base.png")
}

pub fn composited_png_path(sortie_state_path: &Path) -> PathBuf {
    sortie_state_path.with_extension("discord_map.png")
}

pub fn html_path(sortie_state_path: &Path) -> PathBuf {
    sortie_state_path.with_extension("discord_map.html")
}

pub fn map_version_path(sortie_state_path: &Path) -> PathBuf {
    sortie_state_path.with_extension("discord_map_version.txt")
}

pub fn meta_path(sortie_state_path: &Path) -> PathBuf {
    sortie_state_path.with_extension("discord_map_meta.json")
}

pub fn webhook_message_path(sortie_state_path: &Path) -> PathBuf {
    sortie_state_path.with_extension("discord_map_webhook.json")
}

pub fn virtual_resupply_decay_png_path(writedir: &Path) -> PathBuf {
    writedir.join("virtual_resupply_decay.png")
}

pub fn mission_name_from_sortie_path(sortie_state_path: &Path) -> String {
    sortie_state_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("Campaign")
        .to_string()
}

pub fn discord_map_interactive_url(bind_address: &str, http_port: u16) -> Result<String> {
    let host = super::server_settings::public_bind_host(bind_address)?;
    let base = if host.contains(':') && !host.starts_with('[') {
        format!("http://[{host}]:{http_port}")
    } else {
        format!("http://{host}:{http_port}")
    };
    Ok(format!("{base}/map"))
}

pub fn build_discord_map_caption(
    mission_name: &str,
    bind_address: &str,
    http_port: u16,
) -> Result<(String, String, String)> {
    let ts = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let map_url = discord_map_interactive_url(bind_address, http_port)?;
    let caption = format!(
        "Campaign objective map : {mission_name}\nObjectives status as of {ts} UTC\nInteractive HTML campaign map: {map_url}"
    );
    Ok((caption, ts, map_url))
}

fn zone_center_ll(lua: MizLua, zone: TriggerZone) -> Result<LLPos> {
    let pos = zone.pos()?;
    Coord::singleton(lua)?.lo_to_ll(LuaVec3(Vector3::new(pos.x, 0., pos.y)))
}

pub fn read_corner_zones(lua: MizLua, miz: &Miz) -> Result<(LLPos, LLPos)> {
    let mut nw: Option<LLPos> = None;
    let mut se: Option<LLPos> = None;
    for zone in miz.triggers()? {
        let zone = zone?;
        let name = zone.name()?;
        if name.as_str() == SETTINGS_DISCORD_MAP_NW {
            if nw.replace(zone_center_ll(lua, zone)?).is_some() {
                bail!("duplicate ME trigger zone {SETTINGS_DISCORD_MAP_NW}");
            }
        } else if name.as_str() == SETTINGS_DISCORD_MAP_SE {
            if se.replace(zone_center_ll(lua, zone)?).is_some() {
                bail!("duplicate ME trigger zone {SETTINGS_DISCORD_MAP_SE}");
            }
        }
    }
    let nw = nw.with_context(|| format!("missing ME trigger zone {SETTINGS_DISCORD_MAP_NW}"))?;
    let se = se.with_context(|| format!("missing ME trigger zone {SETTINGS_DISCORD_MAP_SE}"))?;
    Ok((nw, se))
}

/// Icons embedded by bftools from `assets/discord-objective-map/png/<canvas_px>/` into `l10n/DEFAULT/fowl_discord_map/`.
pub fn load_icon_pack_from_miz(miz_path: &Path) -> Result<DiscordMapIconPack> {
    let file = File::open(miz_path)
        .with_context(|| format!("open mission archive {:?}", miz_path))?;
    let mut archive =
        ZipArchive::new(file).with_context(|| format!("read mission zip {:?}", miz_path))?;
    let mut manifest_bytes = Vec::new();
    archive
        .by_name(MIZ_MANIFEST)
        .with_context(|| {
            format!(
                "mission {:?} has no {MIZ_MANIFEST} (rebuild with current bftools)",
                miz_path
            )
        })?
        .read_to_end(&mut manifest_bytes)
        .context("read discord map manifest from miz")?;
    let runtime: DiscordMapIconManifestRuntime = serde_json::from_slice(&manifest_bytes)
        .context("parse discord map manifest from miz")?;
    let manifest = DiscordMapIconManifest {
        schema_version: runtime.schema_version,
        description: None,
        canvas_px: runtime.canvas_px,
        kinds: runtime
            .kinds
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    bfprotocols::discord_map_icon_manifest::DiscordMapIconKind {
                        shape: None,
                        files: v.files.clone(),
                    },
                )
            })
            .collect(),
        palette: None,
    };
    manifest.validate()?;
    let mut pngs = FxHashMap::default();
    for stem in manifest.png_stems() {
        let zip_path = DiscordMapIconManifest::miz_png_path(&stem);
        let mut png = Vec::new();
        archive
            .by_name(&zip_path)
            .with_context(|| format!("missing {zip_path} in mission {:?}", miz_path))?
            .read_to_end(&mut png)
            .with_context(|| format!("read {zip_path} from miz"))?;
        pngs.insert(stem, png);
    }
    Ok(DiscordMapIconPack {
        manifest: runtime,
        pngs,
    })
}

fn meta_matches(viewport: &MapViewport, style: &str, meta_path: &Path) -> bool {
    let Ok(raw) = std::fs::read_to_string(meta_path) else {
        return false;
    };
    let Ok(meta) = serde_json::from_str::<DiscordMapMetaFile>(&raw) else {
        return false;
    };
    meta.bbox == viewport.bbox
        && meta.width == viewport.width
        && meta.height == viewport.height
        && meta.style == style
}

fn build_meta(viewport: &MapViewport, style: &str) -> Result<String> {
    let meta = DiscordMapMetaFile {
        bbox: viewport.bbox,
        width: viewport.width,
        height: viewport.height,
        style: style.to_string(),
    };
    Ok(serde_json::to_string_pretty(&meta)?)
}

fn coalition_key(side: Side) -> &'static str {
    match side {
        Side::Red => "red",
        Side::Blue => "blue",
        Side::Neutral => "neutral",
    }
}

/// Abandoned captureable sites use neutral map icons; popup border keeps last owner until recapture.
fn discord_map_icon_coalition(obj: &Objective) -> &'static str {
    if obj.owner() == Side::Neutral || obj.captureable() {
        "neutral"
    } else {
        coalition_key(obj.owner())
    }
}

fn discord_map_tip_coalition(obj: &Objective) -> &'static str {
    coalition_key(obj.owner())
}

fn skip_discord_map_corner_marker(
    obj: &Objective,
    lat: f64,
    lon: f64,
    runtime: Option<&DiscordMapRuntime>,
) -> bool {
    if obj.name.as_str() == SETTINGS_DISCORD_MAP_NW || obj.name.as_str() == SETTINGS_DISCORD_MAP_SE {
        return true;
    }
    let Some(rt) = runtime else {
        return false;
    };
    rt.viewport
        .excludes_discord_map_corner_anchor(lat, lon, rt.corner_nw, rt.corner_se)
}

pub fn collect_markers(lua: MizLua, db: &Db) -> Result<Vec<bg::discord_map::DiscordMapMarker>> {
    let coord = Coord::singleton(lua)?;
    let runtime = db.ephemeral.discord_map.as_ref();
    let mut markers = Vec::new();
    for (_, obj) in db.persisted.objectives.into_iter() {
        let Some(kind) = obj.discord_map_icon_kind() else {
            continue;
        };
        let pos = obj.zone().pos();
        let ll = coord.lo_to_ll(LuaVec3(Vector3::new(pos.x, 0., pos.y)))?;
        if skip_discord_map_corner_marker(&obj, ll.latitude, ll.longitude, runtime) {
            continue;
        }
        markers.push(marker_from_objective(db, &obj, kind, ll)?);
    }
    Ok(markers)
}

fn marker_from_objective(
    db: &Db,
    obj: &Objective,
    kind: &str,
    ll: LLPos,
) -> Result<bg::discord_map::DiscordMapMarker> {
    Ok(bg::discord_map::DiscordMapMarker {
        lat: ll.latitude,
        lon: ll.longitude,
        kind: kind.to_string(),
        icon_coalition: discord_map_icon_coalition(obj).to_string(),
        tip_coalition: discord_map_tip_coalition(obj).to_string(),
        label: db.objective_display_name(obj),
        f10_label: db.objective_f10_map_label(obj),
        health: obj.health(),
        logi: obj.logi(),
        production: obj.production,
        threatened: obj.threatened,
    })
}

fn icons_job(icons: &DiscordMapIconPack) -> bg::discord_map::DiscordMapIconPackJob {
    bg::discord_map::DiscordMapIconPackJob {
        canvas_px: icons.manifest.canvas_px,
        pngs: icons
            .pngs
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        manifest: icons.manifest.clone(),
    }
}

fn collect_front_line_polygons(
    lua: MizLua,
    db: &Db,
) -> Result<Vec<bg::discord_map::DiscordMapFrontLinePolygon>> {
    let cfg = &db.ephemeral.cfg;
    if !cfg.discord_map.front_line_map_active(cfg.front_line) {
        return Ok(Vec::new());
    }
    let coord = Coord::singleton(lua)?;
    let mut out = Vec::new();
    for quad in db.ephemeral.front_line_map_quads() {
        let coalition = match quad.coalition {
            Side::Red => "red",
            Side::Blue => "blue",
            _ => continue,
        };
        let mut latlon = [(0_f64, 0_f64); 4];
        for (i, corner) in quad.corners.iter().enumerate() {
            let ll = coord.lo_to_ll(LuaVec3(Vector3::new(corner.x, 0., corner.y)))?;
            latlon[i] = (ll.latitude, ll.longitude);
        }
        out.push(bg::discord_map::DiscordMapFrontLinePolygon {
            coalition: coalition.to_string(),
            latlon,
        });
    }
    if !out.is_empty() {
        info!("discord map: {} front line polygon(s) for HTML", out.len());
    }
    Ok(out)
}

fn format_theatre_display(slug: &str) -> String {
    if slug.is_empty() || slug == "unknown" {
        return "Unknown".into();
    }
    let mut chars = slug.chars();
    let Some(first) = chars.next() else {
        return slug.to_string();
    };
    format!("{}{}", first.to_ascii_uppercase(), chars.as_str())
}

/// OAB/FOB/LO held by a coalition (matches map icon: not neutral, not captureable).
fn count_ground_objectives_by_side(db: &Db) -> (u32, u32) {
    let mut red = 0u32;
    let mut blue = 0u32;
    for (_, obj) in db.persisted.objectives.into_iter() {
        let Some(side) = held_ground_objective_side(obj) else {
            continue;
        };
        match side {
            Side::Red => red += 1,
            Side::Blue => blue += 1,
            _ => {}
        }
    }
    (red, blue)
}

fn held_ground_objective_side(obj: &Objective) -> Option<Side> {
    if matches!(obj.kind, ObjectiveKind::Production | ObjectiveKind::Farp { .. }) {
        return None;
    }
    if obj.owner() == Side::Neutral || obj.captureable() {
        return None;
    }
    Some(obj.owner())
}

fn group_has_live_ship_carrier(db: &Db, gid: &GroupId) -> bool {
    let Ok(group) = db.group(gid) else {
        return false;
    };
    for uid in &group.units {
        let Some(unit) = db.persisted.units.get(uid) else {
            continue;
        };
        if unit.dead {
            continue;
        }
        if db
            .ephemeral
            .cfg
            .unit_classification
            .get(unit.typ.as_str())
            .is_some_and(|tags| tags.contains(UnitTag::ShipCarrier))
        {
            return true;
        }
    }
    false
}

/// ME pad group names (e.g. `BTarawa`) with a live carrier hull in campaign persistence.
fn alive_carrier_pad_names(db: &Db) -> FxHashSet<(Side, String)> {
    let mut out = FxHashSet::default();
    for (_, obj) in db.persisted.objectives.into_iter() {
        let ObjectiveKind::Farp { pad_template, .. } = &obj.kind else {
            continue;
        };
        let Some(groups) = obj.groups.get(&obj.owner) else {
            continue;
        };
        let pad_live = groups.into_iter().any(|gid| {
            db.group(gid)
                .ok()
                .is_some_and(|g| {
                    g.template_name.as_str() == pad_template.as_str()
                        && group_has_live_ship_carrier(db, gid)
                })
        });
        if pad_live {
            out.insert((
                obj.owner(),
                std::string::String::from(pad_template.as_str()),
            ));
        }
    }
    out
}

fn count_spawned_ship_carriers(lua: MizLua, db: &Db) -> Result<(u32, u32)> {
    let coalition = Coalition::singleton(lua)?;
    let cfg = &db.ephemeral.cfg;
    let alive_pads = alive_carrier_pad_names(db);
    let mut red = 0u32;
    let mut blue = 0u32;
    for side in [Side::Blue, Side::Red] {
        for group in coalition.get_groups(side)? {
            let group = group?;
            if !group.is_exist()? || group.get_category()? != GroupCategory::Ship {
                continue;
            }
            let gname = std::string::String::from(group.get_name()?.as_str());
            if !alive_pads.contains(&(side, gname)) {
                continue;
            }
            let mut carrier = false;
            for unit in group.get_units()? {
                let unit = unit?;
                if !unit.is_exist()? {
                    continue;
                }
                let typ = unit.get_type_name()?;
                if cfg
                    .unit_classification
                    .get(typ.as_str())
                    .is_some_and(|tags| tags.contains(UnitTag::ShipCarrier))
                {
                    carrier = true;
                    break;
                }
            }
            if !carrier {
                continue;
            }
            match side {
                Side::Red => red += 1,
                Side::Blue => blue += 1,
                _ => {}
            }
        }
    }
    Ok((red, blue))
}

struct MissionClock {
    date: NaiveDate,
    tod_secs: u32,
    elapsed_days: u32,
}

fn mission_clock(lua: MizLua) -> Result<MissionClock> {
    let timer = Timer::singleton(lua)?;
    let abs = timer.get_abs_time()?;
    let t0 = timer.get_time0()?;
    let l = lua.inner();
    let (year, month, day, start_time): (i32, u32, u32, u32) = l
        .load(
            r#"local m = rawget(_G, "mission") or (env and env.mission)
if type(m) ~= "table" or type(m.date) ~= "table" then
  return 1970, 1, 1, 0
end
local st = m.start_time or 0
return m.date.Year or 1970, m.date.Month or 1, m.date.Day or 1, st"#,
        )
        .eval()
        .unwrap_or((1970, 1, 1, 0));
    let elapsed = f64::from(abs - t0);
    let total = f64::from(start_time) + elapsed;
    let extra_days = total.div_euclid(86400.) as i64;
    let tod_secs = total.rem_euclid(86400.) as u32;
    let start = NaiveDate::from_ymd_opt(year, month, day)
        .ok_or_else(|| anyhow!("invalid mission date {year}-{month}-{day}"))?;
    Ok(MissionClock {
        date: start + Duration::days(extra_days),
        tod_secs,
        elapsed_days: extra_days as u32,
    })
}

fn live_factory_count(db: &Db, obj: &Objective) -> u32 {
    let Some(groups) = obj.groups.get(&obj.owner) else {
        return 0;
    };
    let mut n = 0u32;
    for gid in groups {
        let Ok(group) = db.group(gid) else {
            continue;
        };
        if group.class != ObjGroupClass::Production {
            continue;
        }
        for uid in &group.units {
            if let Some(unit) = db.persisted.units.get(uid) {
                if !unit.dead {
                    n += 1;
                }
            }
        }
    }
    n
}

fn factory_counts(db: &Db) -> (u32, u32) {
    let mut red = 0u32;
    let mut blue = 0u32;
    for (_, obj) in db.persisted.objectives.into_iter() {
        if !matches!(obj.kind, ObjectiveKind::Production) {
            continue;
        }
        let n = live_factory_count(db, obj);
        match obj.owner() {
            Side::Red => red += n,
            Side::Blue => blue += n,
            _ => {}
        }
    }
    (red, blue)
}

fn avg_logistics_production(db: &Db, side: Side) -> Option<u8> {
    let mut sum = 0u32;
    let mut n = 0u32;
    for (_, obj) in db.persisted.objectives.into_iter() {
        if obj.owner() != side || !matches!(obj.kind, ObjectiveKind::Logistics) {
            continue;
        }
        sum += obj.production as u32;
        n += 1;
    }
    if n == 0 {
        None
    } else {
        Some((sum / n) as u8)
    }
}

fn format_duration_seconds(secs: u32) -> String {
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

fn format_lives_display(cfg: &Cfg) -> String {
    if !cfg.limited_lives {
        return "unlimited lives".into();
    }
    let mut parts = String::new();
    for lt in LIFE_TYPE_DISPLAY_ORDER {
        let Some((count, _)) = cfg.default_lives.get(&lt) else {
            continue;
        };
        if !parts.is_empty() {
            parts.push(' ');
        }
        parts.push_str(&format!("[{} {count}]", life_type_map_abbrev(lt)));
    }
    if parts.is_empty() {
        "—".into()
    } else {
        parts
    }
}

fn format_lives_reset(cfg: &Cfg) -> String {
    if !cfg.limited_lives {
        return "no".into();
    }
    let secs: Vec<u32> = LIFE_TYPE_DISPLAY_ORDER
        .iter()
        .filter_map(|lt| cfg.default_lives.get(lt).map(|(_, s)| *s))
        .collect();
    if secs.is_empty() {
        return "—".into();
    }
    let min = *secs.iter().min().unwrap();
    let max = *secs.iter().max().unwrap();
    if min == max {
        format_duration_seconds(min)
    } else {
        format!(
            "{} - {}",
            format_duration_seconds(min),
            format_duration_seconds(max)
        )
    }
}

fn format_deslot_penalty(cfg: &Cfg) -> String {
    let secs = cfg.airborne_deslot_penalty_secs;
    let points = cfg.airborne_deslot_penalty_points;
    let time = if secs == 0 {
        "0 min".into()
    } else {
        format_duration_seconds(secs)
    };
    format!("{time} / -{points} p.")
}

fn format_max_crates(cfg: &Cfg) -> String {
    cfg.max_crates.map(|n| n.to_string()).unwrap_or_else(|| "—".into())
}

fn balancing_points_display(cfg: &Cfg, online_red: u32, online_blue: u32) -> (i32, i32) {
    let Some(points) = cfg.points.as_ref() else {
        return (0, 0);
    };
    let (gain, period) = points.periodic_point_gain;
    if period == 0 || gain <= 0 {
        return (0, 0);
    }
    if points.balancing_point_gain {
        let red_award = balanced_side_gain(online_red, online_blue, gain);
        let blue_award = balanced_side_gain(online_blue, online_red, gain);
        (blue_award.amount, red_award.amount)
    } else {
        (gain, gain)
    }
}

fn format_logistics_interval_minutes(minutes: u32) -> String {
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

fn warehouse_logistics_intervals(db: &Db) -> (String, String) {
    let Some(wh) = db.ephemeral.cfg.warehouse.as_ref() else {
        return ("—".into(), "—".into());
    };
    (
        format_logistics_interval_minutes(wh.tick),
        format_logistics_interval_minutes(wh.tick.saturating_mul(wh.ticks_per_delivery)),
    )
}

pub fn collect_map_status_bar(
    lua: MizLua,
    db: &Db,
    live: &DiscordMapLiveCtx,
    mission_name: &str,
    status_utc: &str,
) -> Result<bg::discord_map::DiscordMapStatusBar> {
    let clock = mission_clock(lua).context("discord map mission clock")?;
    let (ground_red, ground_blue) = count_ground_objectives_by_side(db);
    let (carrier_red, carrier_blue) =
        count_spawned_ship_carriers(lua, db).context("discord map carrier ships")?;
    let (factories_red, factories_blue) = factory_counts(db);
    let (supply_to_bases, delivery_to_hubs) = warehouse_logistics_intervals(db);
    let cfg = &db.ephemeral.cfg;
    let server = super::server_settings::load_server_settings(lua);
    let (balancing_blue, balancing_red) =
        balancing_points_display(cfg, live.online_red, live.online_blue);
    let map_pilot = |p: &DiscordMapPilot| bg::discord_map::DiscordMapPilotEntry {
        name: p.name.clone(),
        ping: p.ping,
    };
    Ok(bg::discord_map::DiscordMapStatusBar {
        mission_name: mission_name.to_string(),
        status_utc: status_utc.to_string(),
        mission_date: clock.date.format("%Y-%m-%d").to_string(),
        mission_tod_secs: clock.tod_secs,
        mission_elapsed_days: clock.elapsed_days,
        gen_utc_ms: live.generated_at.timestamp_millis(),
        restart_utc_ms: live.shutdown_when.map(|t| t.timestamp_millis()),
        online_red: live.online_red,
        online_blue: live.online_blue,
        blue_pilots: live.blue_pilots.iter().map(map_pilot).collect(),
        red_pilots: live.red_pilots.iter().map(map_pilot).collect(),
        spectators: live.spectators.iter().map(map_pilot).collect(),
        ground_red,
        ground_blue,
        carrier_red,
        carrier_blue,
        factories_red,
        factories_blue,
        production_red: avg_logistics_production(db, Side::Red),
        production_blue: avg_logistics_production(db, Side::Blue),
        balancing_blue,
        balancing_red,
        lives: format_lives_display(cfg),
        lives_reset: format_lives_reset(cfg),
        deslot_penalty: format_deslot_penalty(cfg),
        player_crates: format_max_crates(cfg),
        threatened: format_duration_seconds(cfg.threatened_cooldown),
        bases_repair: format_duration_seconds(cfg.repair_time),
        factory_repair: format_duration_seconds(cfg.production_repair_rate_seconds),
        static_repair: format_duration_seconds(cfg.static_repair_rate_seconds),
        supply_to_bases,
        delivery_to_hubs,
        dcs_bind_address: server.bind_address,
        dcs_port: server.port,
        dcs_name: server.name,
        dowload_acmi: cfg.discord_map.dowload_acmi,
        dowload_acmi_url: cfg.discord_map.dowload_acmi_url.to_string(),
        discord_url: cfg.discord_map.discord_url.to_string(),
        stats_url: cfg.discord_map.stats_url.to_string(),
        manual_url: cfg.discord_map.manual_url.to_string(),
        bugs_report_url: cfg.discord_map.bugs_report_url.to_string(),
        deliveries_url: {
            let writedir = PathBuf::from(Lfs::singleton(lua)?.writedir()?.as_str());
            let decay = virtual_resupply_decay_png_path(&writedir);
            if decay.is_file() {
                "/virtual_resupply_decay.png".to_string()
            } else {
                String::new()
            }
        },
    })
}

fn discord_map_post_job(
    lua: MizLua,
    db: &Db,
    runtime: &DiscordMapRuntime,
    cfg: &DiscordMapCfg,
    markers: Vec<bg::discord_map::DiscordMapMarker>,
    icons: bg::discord_map::DiscordMapIconPackJob,
    live: &DiscordMapLiveCtx,
) -> Result<DiscordMapPostJob> {
    let server = super::server_settings::load_server_settings(lua);
    let (caption, status_utc, _) =
        build_discord_map_caption(&runtime.mission_name, &server.bind_address, cfg.http_port)?;
    let status_bar = collect_map_status_bar(lua, db, live, &runtime.mission_name, &status_utc)?;
    Ok(DiscordMapPostJob {
        webhook_url: cfg.webhook_url.clone().unwrap().to_string(),
        webhook_message_path: runtime.webhook_message_path.clone(),
        base_png_path: runtime.base_png_path.clone(),
        composited_png_path: runtime.composited_png_path.clone(),
        html_path: runtime.html_path.clone(),
        map_version_path: runtime.map_version_path.clone(),
        viewport: runtime.viewport,
        corner_nw: runtime.corner_nw,
        corner_se: runtime.corner_se,
        markers,
        front_line: collect_front_line_polygons(lua, db)?,
        icons,
        caption,
        mission_name: runtime.mission_name.clone(),
        status_utc,
        status_bar,
    })
}

impl Db {
    pub fn discord_map_debounce_post(&mut self, ts: DateTime<Utc>) {
        if self.ephemeral.discord_map.is_none() {
            return;
        }
        self.ephemeral.discord_map_post_due =
            Some(ts + Duration::seconds(POST_DEBOUNCE_SECS));
    }

    pub fn schedule_discord_map_periodic(&mut self, from: DateTime<Utc>, mission_has_players: bool) {
        if self.ephemeral.discord_map.is_none() {
            return;
        }
        let interval = periodic_refresh_interval_secs(mission_has_players);
        self.ephemeral.discord_map_periodic_due = Some(from + Duration::seconds(interval));
    }

    fn adapt_discord_map_periodic_due(&mut self, ts: DateTime<Utc>, mission_has_players: bool) {
        if self.ephemeral.discord_map.is_none() {
            return;
        }
        let next = ts + Duration::seconds(periodic_refresh_interval_secs(mission_has_players));
        match self.ephemeral.discord_map_periodic_due {
            None => self.ephemeral.discord_map_periodic_due = Some(next),
            Some(due) if mission_has_players && due > next => {
                self.ephemeral.discord_map_periodic_due = Some(next);
            }
            _ => {}
        }
    }

    pub fn discord_map_tick(
        &mut self,
        lua: MizLua,
        ts: DateTime<Utc>,
        mission_has_players: bool,
        live: &DiscordMapLiveCtx,
    ) -> Result<()> {
        if self.ephemeral.discord_map.is_none() {
            return Ok(());
        }
        self.adapt_discord_map_periodic_due(ts, mission_has_players);
        let debounce_due = matches!(self.ephemeral.discord_map_post_due, Some(d) if ts >= d);
        let periodic_due = matches!(self.ephemeral.discord_map_periodic_due, Some(d) if ts >= d);
        if !debounce_due && !periodic_due {
            return Ok(());
        }
        self.ephemeral.discord_map_post_due = None;
        self.queue_discord_map_post(lua, live)?;
        self.schedule_discord_map_periodic(ts, mission_has_players);
        Ok(())
    }

    fn sync_front_line_for_discord_map(&mut self) {
        if self
            .ephemeral
            .cfg
            .discord_map
            .front_line_map_active(self.ephemeral.cfg.front_line)
        {
            self.ephemeral.sync_front_line(&self.persisted);
        }
    }

    pub fn bootstrap_discord_map(&mut self, lua: MizLua, live: &DiscordMapLiveCtx) -> Result<()> {
        self.sync_front_line_for_discord_map();
        let Some(runtime) = self.ephemeral.discord_map.as_ref() else {
            return Ok(());
        };
        let cfg = &self.ephemeral.cfg.discord_map;
        let markers = collect_markers(lua, self)?;
        let icons = icons_job(runtime.icons.as_ref());
        let meta_json = build_meta(&runtime.viewport, cfg.style.as_str())?;
        let post_job = discord_map_post_job(lua, self, runtime, cfg, markers, icons, live)?;
        let cache_ok = runtime.base_png_path.is_file()
            && meta_matches(&runtime.viewport, cfg.style.as_str(), &runtime.meta_path);
        if cache_ok {
            info!("discord map: reusing cached base PNG {:?}", runtime.base_png_path);
            self.ephemeral.do_bg(Task::DiscordMapPost(post_job));
            return Ok(());
        }
        let token = cfg.mapbox_access_token.as_deref().unwrap();
        let url = runtime.viewport.mapbox_static_url(
            cfg.style.as_str(),
            token,
            cfg.retina,
            cfg.padding,
        );
        self.ephemeral.do_bg(Task::FetchDiscordMapBase {
            url,
            base_png_path: runtime.base_png_path.clone(),
            meta_path: runtime.meta_path.clone(),
            meta_json,
            post: Some(post_job),
        });
        Ok(())
    }

    pub fn queue_discord_map_post(&mut self, lua: MizLua, live: &DiscordMapLiveCtx) -> Result<()> {
        self.sync_front_line_for_discord_map();
        let Some(runtime) = self.ephemeral.discord_map.as_ref() else {
            return Ok(());
        };
        if !runtime.base_png_path.is_file() {
            warn!("discord map: skip post — base PNG not ready yet");
            return Ok(());
        }
        let cfg = &self.ephemeral.cfg.discord_map;
        let markers = collect_markers(lua, self)?;
        self.ephemeral.do_bg(Task::DiscordMapPost(discord_map_post_job(
            lua,
            self,
            runtime,
            cfg,
            markers,
            icons_job(runtime.icons.as_ref()),
            live,
        )?));
        Ok(())
    }
}

pub fn init_discord_map(
    lua: MizLua,
    db: &mut Db,
    miz: &Miz,
    miz_path: &Path,
    sortie_state_path: &Path,
) -> Result<()> {
    let cfg = &db.ephemeral.cfg.discord_map;
    cfg.validate_enabled(db.ephemeral.cfg.front_line)?;
    if !cfg.enabled {
        return Ok(());
    }
    let server = super::server_settings::load_server_settings(lua);
    let map_url = discord_map_interactive_url(&server.bind_address, cfg.http_port)?;
    info!("discord map: interactive URL {map_url}");
    let (nw, se) = read_corner_zones(lua, miz)?;
    let viewport = viewport_from_corners(nw, se, cfg.width).with_context(|| {
        format!(
            "discord map viewport from {SETTINGS_DISCORD_MAP_NW} / {SETTINGS_DISCORD_MAP_SE}"
        )
    })?;
    let icons = Arc::new(load_icon_pack_from_miz(miz_path).with_context(|| {
        format!("load discord map icons from mission {:?}", miz_path)
    })?);
    let base_png_path = base_png_path(sortie_state_path);
    let composited_png_path = composited_png_path(sortie_state_path);
    let html_path = html_path(sortie_state_path);
    let map_version_path = map_version_path(sortie_state_path);
    let meta_path = meta_path(sortie_state_path);
    let webhook_message_path = webhook_message_path(sortie_state_path);
    let mission_name = mission_name_from_sortie_path(sortie_state_path);
    let writedir = PathBuf::from(Lfs::singleton(lua)?.writedir()?.as_str());
    let virtual_resupply_decay_path = virtual_resupply_decay_png_path(&writedir);
    info!(
        "discord map: viewport {}x{} bbox [{:.4},{:.4},{:.4},{:.4}] icons={} http_port={}",
        viewport.width,
        viewport.height,
        viewport.lon_min(),
        viewport.lat_min(),
        viewport.lon_max(),
        viewport.lat_max(),
        icons.pngs.len(),
        cfg.http_port
    );
    db.ephemeral.discord_map = Some(DiscordMapRuntime {
        viewport,
        corner_nw: nw,
        corner_se: se,
        icons,
        base_png_path: base_png_path.clone(),
        composited_png_path: composited_png_path.clone(),
        html_path: html_path.clone(),
        map_version_path: map_version_path.clone(),
        meta_path,
        webhook_message_path,
        mission_name,
        http_port: cfg.http_port,
    });
    db.ephemeral.do_bg(Task::StartDiscordMapHttp {
        port: cfg.http_port,
        html_path,
        map_version_path,
        composited_png_path,
        base_png_path,
        virtual_resupply_decay_path,
    });
    Ok(())
}

pub fn validate_corner_zones_present(miz: &Miz) -> Result<()> {
    let mut names = Vec::new();
    for zone in miz.triggers()? {
        let zone = zone?;
        names.push(zone.name()?.as_str().to_string());
    }
    bfprotocols::discord_map_viewport::validate_corner_zones_present(
        names.iter().map(String::as_str),
    )
}
