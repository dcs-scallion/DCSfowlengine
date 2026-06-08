//! Discord objective map: ME viewport zones, icon pack from `.miz`, Mapbox cache, Discord posts.

use super::Db;
use crate::bg::{self, DiscordMapPostJob, Task};
use anyhow::{anyhow, bail, Context, Result};
use bfprotocols::{
    cfg::DiscordMapCfg,
    discord_map_icon_manifest::{
        DiscordMapIconManifest, DiscordMapIconManifestRuntime, MIZ_MANIFEST,
    },
    discord_map_viewport::{
        viewport_from_corners, MapViewport, SETTINGS_DISCORD_MAP_NW, SETTINGS_DISCORD_MAP_SE,
    },
};
use chrono::{Duration, prelude::*};
use dcso3::{
    coalition::Side,
    coord::{Coord, LLPos},
    dcs::Dcs,
    env::miz::{Miz, TriggerZone},
    lfs::Lfs,
    LuaEnv, LuaVec3, MizLua, Vector3,
};
use mlua::Value;
use fxhash::FxHashMap;
use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use zip::ZipArchive;

use crate::db::objective::Objective;

pub const POST_DEBOUNCE_SECS: i64 = 45;

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
    pub icons: Arc<DiscordMapIconPack>,
    pub base_png_path: PathBuf,
    pub composited_png_path: PathBuf,
    pub html_path: PathBuf,
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

pub fn meta_path(sortie_state_path: &Path) -> PathBuf {
    sortie_state_path.with_extension("discord_map_meta.json")
}

pub fn webhook_message_path(sortie_state_path: &Path) -> PathBuf {
    sortie_state_path.with_extension("discord_map_webhook.json")
}

pub fn mission_name_from_sortie_path(sortie_state_path: &Path) -> String {
    sortie_state_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("Campaign")
        .to_string()
}

pub fn build_discord_map_caption(mission_name: &str, cfg: &DiscordMapCfg) -> (String, String) {
    let ts = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let public_base = cfg
        .http_public_base_url
        .as_ref()
        .map(|s| s.as_str().trim())
        .unwrap_or("")
        .trim_end_matches('/');
    let map_url = format!("{public_base}/map");
    let caption = format!(
        "Campaign objective map : {mission_name}\nObjectives status as of {ts} UTC\nInteractive HTML map : {map_url}"
    );
    (caption, ts)
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
        _ => "neutral",
    }
}

pub fn collect_markers(lua: MizLua, db: &Db) -> Result<Vec<bg::discord_map::DiscordMapMarker>> {
    let coord = Coord::singleton(lua)?;
    let mut markers = Vec::new();
    for (_, obj) in db.persisted.objectives.into_iter() {
        let Some(kind) = obj.discord_map_icon_kind() else {
            continue;
        };
        markers.push(marker_from_objective(db, &obj, kind, &coord)?);
    }
    Ok(markers)
}

fn marker_from_objective(
    db: &Db,
    obj: &Objective,
    kind: &str,
    coord: &Coord,
) -> Result<bg::discord_map::DiscordMapMarker> {
    let pos = obj.zone().pos();
    let ll = coord.lo_to_ll(LuaVec3(Vector3::new(pos.x, 0., pos.y)))?;
    Ok(bg::discord_map::DiscordMapMarker {
        lat: ll.latitude,
        lon: ll.longitude,
        kind: kind.to_string(),
        coalition: coalition_key(obj.owner()).to_string(),
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

fn discord_map_post_job(
    runtime: &DiscordMapRuntime,
    cfg: &DiscordMapCfg,
    markers: Vec<bg::discord_map::DiscordMapMarker>,
    icons: bg::discord_map::DiscordMapIconPackJob,
) -> DiscordMapPostJob {
    let (caption, status_utc) = build_discord_map_caption(&runtime.mission_name, cfg);
    DiscordMapPostJob {
        webhook_url: cfg.webhook_url.clone().unwrap().to_string(),
        webhook_message_path: runtime.webhook_message_path.clone(),
        base_png_path: runtime.base_png_path.clone(),
        composited_png_path: runtime.composited_png_path.clone(),
        html_path: runtime.html_path.clone(),
        viewport: runtime.viewport,
        markers,
        icons,
        caption,
        mission_name: runtime.mission_name.clone(),
        status_utc,
    }
}

impl Db {
    pub fn discord_map_debounce_post(&mut self, ts: DateTime<Utc>) {
        if self.ephemeral.discord_map.is_none() {
            return;
        }
        self.ephemeral.discord_map_post_due =
            Some(ts + Duration::seconds(POST_DEBOUNCE_SECS));
    }

    pub fn discord_map_maybe_post(&mut self, lua: MizLua) -> Result<()> {
        let due = match self.ephemeral.discord_map_post_due {
            Some(d) if Utc::now() >= d => d,
            _ => return Ok(()),
        };
        self.ephemeral.discord_map_post_due = None;
        self.queue_discord_map_post(lua)?;
        let _ = due;
        Ok(())
    }

    pub fn bootstrap_discord_map(&self, lua: MizLua) -> Result<()> {
        let Some(runtime) = self.ephemeral.discord_map.as_ref() else {
            return Ok(());
        };
        let cfg = &self.ephemeral.cfg.discord_map;
        let markers = collect_markers(lua, self)?;
        let icons = icons_job(runtime.icons.as_ref());
        let meta_json = build_meta(&runtime.viewport, cfg.style.as_str())?;
        let post_job = discord_map_post_job(runtime, cfg, markers, icons);
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

    pub fn queue_discord_map_post(&self, lua: MizLua) -> Result<()> {
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
            runtime,
            cfg,
            markers,
            icons_job(runtime.icons.as_ref()),
        )));
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
    cfg.validate_enabled()?;
    if !cfg.enabled {
        return Ok(());
    }
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
    let meta_path = meta_path(sortie_state_path);
    let webhook_message_path = webhook_message_path(sortie_state_path);
    let mission_name = mission_name_from_sortie_path(sortie_state_path);
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
        icons,
        base_png_path: base_png_path.clone(),
        composited_png_path: composited_png_path.clone(),
        html_path: html_path.clone(),
        meta_path,
        webhook_message_path,
        mission_name,
        http_port: cfg.http_port,
    });
    db.ephemeral.do_bg(Task::StartDiscordMapHttp {
        port: cfg.http_port,
        html_path,
        composited_png_path,
        base_png_path,
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
