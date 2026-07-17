//! Mapbox base fetch, icon compositing, interactive HTML, Discord webhook (background thread).

use super::discord_map_font::MapLabelFont;
use super::discord_map_http;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use bfprotocols::discord_map_icon_manifest::DiscordMapIconManifestRuntime;
use bfprotocols::discord_map_viewport::MapViewport;
use dcso3::coord::LLPos;
use image::RgbaImage;
use log::{info, warn};
use once_cell::sync::Lazy;
use reqwest::multipart;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;

const SIDEBAR_WIDTH_PX: u32 = 300;
/// Campaign stats sidebar (wider than left sidebar for 999999 : 999999).
const CAMPAIGN_STATS_SIDEBAR_WIDTH_PX: u32 = 360;
const CAMPAIGN_STATS_VALUE_COL_PX: u32 = 84;
const SIDEBAR_RANK_COL_PX: u32 = 25;
const LAYOUT_GAP_PX: u32 = 4;
const STAT_BARS_GAP_PX: i32 = 3;
const HEALTH_BAR_MAP_PX: f32 = 30.0;
const THREAT_RING_SCALE: f32 = 1.35;
const THREAT_RING_PAD_PX: f32 = 4.0;
const THREAT_RING_MIN_PX: u32 = 8;
const LABEL_GAP_PX: i32 = 4;
const LABEL_FONT_PX: i32 = 8;
const MAP_CORNER_GUARD_LOGICAL: f32 = 40.0;

static MAP_LABEL_FONT: Lazy<MapLabelFont> = Lazy::new(MapLabelFont::embedded);
static MAP_LABEL_GR_B64: Lazy<String> = Lazy::new(|| {
    B64.encode(include_bytes!(
        "../../../assets/discord-objective-map/png/map-label-gr.png"
    ))
});
static MAP_LABEL_B64: Lazy<String> = Lazy::new(|| {
    B64.encode(include_bytes!(
        "../../../assets/discord-objective-map/png/map-label.png"
    ))
});
const MAP_LABEL_SRC_W: u32 = 458;
const MAP_LABEL_SRC_H: u32 = 200;

fn map_label_display_w() -> u32 {
    SIDEBAR_WIDTH_PX
}

fn map_label_display_h() -> u32 {
    ((MAP_LABEL_SRC_H as u64 * SIDEBAR_WIDTH_PX as u64) / MAP_LABEL_SRC_W as u64) as u32
}

static MAP_FLY_FIGHT_WIN_B64: Lazy<String> = Lazy::new(|| {
    B64.encode(include_bytes!(
        "../../../assets/discord-objective-map/png/map-Fly-Fight-Win.png"
    ))
});
const MAP_FLY_FIGHT_WIN_SRC_W: u32 = 434;
const MAP_FLY_FIGHT_WIN_SRC_H: u32 = 40;

fn map_fly_fight_win_display_w() -> u32 {
    SIDEBAR_WIDTH_PX
}

fn map_fly_fight_win_display_h() -> u32 {
    ((MAP_FLY_FIGHT_WIN_SRC_H as u64 * SIDEBAR_WIDTH_PX as u64) / MAP_FLY_FIGHT_WIN_SRC_W as u64) as u32
}

const HDR_LINK_ICON_H: u32 = 38;

fn hdr_link_icon_w(src_w: u32, src_h: u32) -> u32 {
    ((src_w as u64 * HDR_LINK_ICON_H as u64) / src_h.max(1) as u64).max(1) as u32
}

static TACVIEW_HDR_ICON_OFF_B64: Lazy<String> = Lazy::new(|| {
    B64.encode(include_bytes!(
        "../../../assets/discord-objective-map/png/Tacview.png"
    ))
});
static TACVIEW_HDR_ICON_ON_B64: Lazy<String> = Lazy::new(|| {
    B64.encode(include_bytes!(
        "../../../assets/discord-objective-map/png/Tacview_select.png"
    ))
});
static DISCORD_HDR_ICON_OFF_B64: Lazy<String> = Lazy::new(|| {
    B64.encode(include_bytes!(
        "../../../assets/discord-objective-map/png/discord.png"
    ))
});
static DISCORD_HDR_ICON_ON_B64: Lazy<String> = Lazy::new(|| {
    B64.encode(include_bytes!(
        "../../../assets/discord-objective-map/png/discord_select.png"
    ))
});
static STATS_HDR_ICON_OFF_B64: Lazy<String> = Lazy::new(|| {
    B64.encode(include_bytes!(
        "../../../assets/discord-objective-map/png/statistics.png"
    ))
});
static STATS_HDR_ICON_ON_B64: Lazy<String> = Lazy::new(|| {
    B64.encode(include_bytes!(
        "../../../assets/discord-objective-map/png/statistics_select.png"
    ))
});
static MANUAL_HDR_ICON_OFF_B64: Lazy<String> = Lazy::new(|| {
    B64.encode(include_bytes!(
        "../../../assets/discord-objective-map/png/manual.png"
    ))
});
static MANUAL_HDR_ICON_ON_B64: Lazy<String> = Lazy::new(|| {
    B64.encode(include_bytes!(
        "../../../assets/discord-objective-map/png/manual_select.png"
    ))
});
static DELIVERIES_HDR_ICON_OFF_B64: Lazy<String> = Lazy::new(|| {
    B64.encode(include_bytes!(
        "../../../assets/discord-objective-map/png/deliveries.png"
    ))
});
static DELIVERIES_HDR_ICON_ON_B64: Lazy<String> = Lazy::new(|| {
    B64.encode(include_bytes!(
        "../../../assets/discord-objective-map/png/deliveries _select.png"
    ))
});
static BUGS_HDR_ICON_OFF_B64: Lazy<String> = Lazy::new(|| {
    B64.encode(include_bytes!(
        "../../../assets/discord-objective-map/png/bugs.png"
    ))
});
static BUGS_HDR_ICON_ON_B64: Lazy<String> = Lazy::new(|| {
    B64.encode(include_bytes!(
        "../../../assets/discord-objective-map/png/bugs_select.png"
    ))
});
const TACVIEW_HDR_ICON_SRC: (u32, u32) = (76, 76);
const DISCORD_HDR_ICON_SRC: (u32, u32) = (96, 96);
const STATS_HDR_ICON_SRC: (u32, u32) = (96, 89);
const MANUAL_HDR_ICON_SRC: (u32, u32) = (96, 96);
const DELIVERIES_HDR_ICON_SRC: (u32, u32) = (96, 96);
const BUGS_HDR_ICON_SRC: (u32, u32) = (96, 94);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordMapPilotEntry {
    pub name: String,
    pub ping: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordMapStatusBar {
    pub mission_name: String,
    pub status_utc: String,
    pub mission_date: String,
    pub mission_tod_secs: u32,
    pub mission_elapsed_days: u32,
    pub gen_utc_ms: i64,
    pub restart_utc_ms: Option<i64>,
    /// Added to HTML Time to restart when CFG `shutdown` is null (mission load skew).
    pub restart_display_skew_secs: u32,
    pub online_red: u32,
    pub online_blue: u32,
    pub blue_pilots: Vec<DiscordMapPilotEntry>,
    pub red_pilots: Vec<DiscordMapPilotEntry>,
    pub spectators: Vec<DiscordMapPilotEntry>,
    pub ground_red: u32,
    pub ground_blue: u32,
    pub carrier_red: u32,
    pub carrier_blue: u32,
    pub factories_red: u32,
    pub factories_blue: u32,
    pub production_red: Option<u8>,
    pub production_blue: Option<u8>,
    pub balancing_blue: i32,
    pub balancing_red: i32,
    pub lives: String,
    pub lives_reset: String,
    pub deslot_penalty: String,
    pub player_crates: String,
    pub threatened: String,
    pub bases_repair: String,
    pub factory_repair: String,
    pub static_repair: String,
    pub supply_to_bases: String,
    pub delivery_to_hubs: String,
    pub dcs_bind_address: String,
    pub dcs_port: String,
    pub dcs_name: String,
    pub dowload_acmi: bool,
    pub dowload_acmi_url: String,
    pub discord_url: String,
    pub stats_url: String,
    pub manual_url: String,
    pub bugs_report_url: String,
    /// Relative map HTTP path when `virtual_resupply_decay.png` exists beside CFG.
    pub deliveries_url: String,
    pub campaign_stats_enabled: bool,
    pub campaign_duration_days: u32,
    pub campaign_online_hours_blue: u32,
    pub campaign_online_hours_red: u32,
    pub campaign_stats_sidebar_html: String,
    /// Empty when `campaign_top10` is off.
    pub campaign_top10_sidebar_html: String,
}

#[derive(Debug, Clone)]
pub struct DiscordMapFrontLinePolygon {
    pub coalition: String,
    pub latlon: [(f64, f64); 4],
}

#[derive(Debug, Clone)]
pub struct DiscordMapMarker {
    pub lat: f64,
    pub lon: f64,
    pub kind: String,
    pub icon_coalition: String,
    pub tip_coalition: String,
    /// PNG side label (display alias).
    pub label: String,
    /// F10 map mark title (alias + kind suffix).
    pub f10_label: String,
    pub health: u8,
    pub logi: u8,
    pub production: u8,
    pub threatened: bool,
}

#[derive(Debug, Clone)]
pub struct DiscordMapIconPackJob {
    pub canvas_px: u32,
    pub pngs: HashMap<String, Vec<u8>>,
    pub manifest: DiscordMapIconManifestRuntime,
}

#[derive(Debug, Clone)]
struct MarkerLayout {
    cx: f32,
    cy: f32,
    sw: u32,
    sh: u32,
    icon_b64: String,
    tip_coalition: String,
    kind: String,
    f10_label: String,
    health: u8,
    logi: u8,
    production: u8,
}

struct MapArtifacts {
    png: Vec<u8>,
    html: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiscordWebhookMessageState {
    message_id: String,
}

pub async fn start_map_http_server(
    port: u16,
    html_path: PathBuf,
    map_version_path: PathBuf,
    composited_png_path: PathBuf,
    base_png_path: PathBuf,
    virtual_resupply_decay_path: PathBuf,
) {
    discord_map_http::ensure_map_http_server(
        port,
        html_path,
        map_version_path,
        composited_png_path,
        base_png_path,
        virtual_resupply_decay_path,
    )
    .await;
}

pub async fn fetch_mapbox_base(url: &str, base_path: &Path, meta_path: &Path, meta_json: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .context("build HTTP client for Mapbox")?;
    let resp = client
        .get(url)
        .send()
        .await
        .context("Mapbox static image request")?;
    let status = resp.status();
    let bytes = resp.bytes().await.context("read Mapbox response body")?;
    if !status.is_success() {
        let preview = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
        bail_mapbox(status.as_u16(), preview.as_ref());
    }
    if let Some(parent) = base_path.parent() {
        fs::create_dir_all(parent).await.ok();
    }
    fs::write(base_path, &bytes)
        .await
        .with_context(|| format!("write discord map base PNG {:?}", base_path))?;
    fs::write(meta_path, meta_json.as_bytes())
        .await
        .with_context(|| format!("write discord map meta {:?}", meta_path))?;
    info!(
        "discord map: cached Mapbox base PNG {:?} ({} bytes)",
        base_path,
        bytes.len()
    );
    Ok(())
}

fn write_tmp_path(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    parent.join(format!("{name}.tmp"))
}

fn bail_mapbox(status: u16, body: &str) -> Result<()> {
    if status == 422 {
        anyhow::bail!(
            "Mapbox static image rejected (422): {body}. Reduce discord_map.width in CFG if dimensions exceed 1280px."
        );
    }
    anyhow::bail!("Mapbox static image failed HTTP {status}: {body}");
}

pub async fn publish_and_post(
    webhook_url: &str,
    webhook_message_path: &Path,
    base_png_path: &Path,
    composited_png_path: &Path,
    html_path: &Path,
    map_version_path: &Path,
    viewport: &MapViewport,
    corner_nw: LLPos,
    corner_se: LLPos,
    markers: &[DiscordMapMarker],
    front_line: &[DiscordMapFrontLinePolygon],
    icons: &DiscordMapIconPackJob,
    caption: &str,
    mission_name: &str,
    status_utc: &str,
    status_bar: &DiscordMapStatusBar,
) -> Result<()> {
    let artifacts = build_map_artifacts(
        base_png_path,
        viewport,
        corner_nw,
        corner_se,
        markers,
        front_line,
        icons,
        mission_name,
        status_utc,
        status_bar,
    )
    .context("build discord map artifacts")?;
    if let Some(parent) = composited_png_path.parent() {
        fs::create_dir_all(parent).await.ok();
    }
    fs::write(composited_png_path, &artifacts.png)
        .await
        .with_context(|| format!("write composited map PNG {:?}", composited_png_path))?;
    let html_tmp = write_tmp_path(html_path);
    fs::write(&html_tmp, artifacts.html.as_bytes())
        .await
        .with_context(|| format!("write interactive map HTML temp {:?}", html_tmp))?;
    fs::rename(&html_tmp, html_path)
        .await
        .with_context(|| format!("publish interactive map HTML {:?}", html_path))?;
    let version_tmp = write_tmp_path(map_version_path);
    fs::write(&version_tmp, status_utc.as_bytes())
        .await
        .with_context(|| format!("write discord map version temp {:?}", version_tmp))?;
    fs::rename(&version_tmp, map_version_path)
        .await
        .with_context(|| format!("publish discord map version {:?}", map_version_path))?;
    post_discord_map(webhook_url, webhook_message_path, caption).await
}

async fn post_discord_map(
    webhook_url: &str,
    webhook_message_path: &Path,
    caption: &str,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("build HTTP client for Discord")?;
    let webhook_base = webhook_url_base(webhook_url)?;

    if let Some(message_id) = load_webhook_message_id(webhook_message_path).await? {
        let patch_url = format!("{webhook_base}/messages/{message_id}");
        let form = discord_webhook_form(caption, true)?;
        let resp = client
            .patch(&patch_url)
            .multipart(form)
            .send()
            .await
            .context("Discord webhook PATCH")?;
        if resp.status().is_success() {
            info!("discord map: updated Discord message {message_id}");
            return Ok(());
        }
        if resp.status() == StatusCode::NOT_FOUND {
            warn!("discord map: stored message {message_id} missing, posting new");
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Discord webhook PATCH failed HTTP {status}: {body}");
        }
    }

    let post_url = webhook_url_with_wait(webhook_url);
    let form = discord_webhook_form(caption, false)?;
    let resp = client
        .post(&post_url)
        .multipart(form)
        .send()
        .await
        .context("Discord webhook POST")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Discord webhook POST failed HTTP {status}: {body}");
    }
    let body_text = resp.text().await.context("read Discord webhook response")?;
    let body: serde_json::Value =
        serde_json::from_str(&body_text).context("parse Discord webhook response")?;
    let message_id = body
        .get("id")
        .and_then(|v| v.as_str())
        .context("Discord webhook response missing message id")?;
    save_webhook_message_id(webhook_message_path, message_id).await?;
    info!("discord map: posted map link to Discord (message {message_id})");
    Ok(())
}

fn webhook_url_base(url: &str) -> Result<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let base = trimmed.split('?').next().unwrap_or(trimmed);
    if !base.contains("/api/webhooks/") {
        bail!("discord webhook URL must be https://discord.com/api/webhooks/{{id}}/{{token}}");
    }
    Ok(base.to_string())
}

fn webhook_url_with_wait(url: &str) -> String {
    let trimmed = url.trim();
    if trimmed.contains('?') {
        format!("{trimmed}&wait=true")
    } else {
        format!("{trimmed}?wait=true")
    }
}

fn discord_webhook_form(caption: &str, edit: bool) -> Result<multipart::Form> {
    let payload = if edit {
        serde_json::json!({
            "content": caption,
            "embeds": [],
            "attachments": []
        })
    } else {
        serde_json::json!({
            "content": caption
        })
    };
    Ok(multipart::Form::new().text("payload_json", payload.to_string()))
}

async fn load_webhook_message_id(path: &Path) -> Result<Option<String>> {
    let Ok(raw) = fs::read_to_string(path).await else {
        return Ok(None);
    };
    let state: DiscordWebhookMessageState =
        serde_json::from_str(&raw).context("parse discord webhook message state")?;
    if state.message_id.is_empty() {
        return Ok(None);
    }
    Ok(Some(state.message_id))
}

async fn save_webhook_message_id(path: &Path, message_id: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await.ok();
    }
    let state = DiscordWebhookMessageState {
        message_id: message_id.to_string(),
    };
    let json = serde_json::to_string_pretty(&state)?;
    fs::write(path, json.as_bytes())
        .await
        .with_context(|| format!("write discord webhook message state {:?}", path))?;
    Ok(())
}

fn build_map_artifacts(
    base_png_path: &Path,
    viewport: &MapViewport,
    corner_nw: LLPos,
    corner_se: LLPos,
    markers: &[DiscordMapMarker],
    front_line: &[DiscordMapFrontLinePolygon],
    icons: &DiscordMapIconPackJob,
    mission_name: &str,
    status_utc: &str,
    status_bar: &DiscordMapStatusBar,
) -> Result<MapArtifacts> {
    let base_bytes = std::fs::read(base_png_path)
        .with_context(|| format!("read discord map base PNG {:?}", base_png_path))?;
    let (png, layouts, img_w, img_h) =
        composite_map(&base_bytes, viewport, corner_nw, corner_se, markers, icons)?;
    let html = build_interactive_html(
        mission_name,
        status_utc,
        img_w,
        img_h,
        viewport,
        &png,
        &layouts,
        front_line,
        status_bar,
    );
    Ok(MapArtifacts { png, html })
}

fn composite_map(
    base_png: &[u8],
    viewport: &MapViewport,
    corner_nw: LLPos,
    corner_se: LLPos,
    markers: &[DiscordMapMarker],
    icons: &DiscordMapIconPackJob,
) -> Result<(Vec<u8>, Vec<MarkerLayout>, u32, u32)> {
    let mut base = image::load_from_memory(base_png)
        .context("decode base map PNG")?
        .to_rgba8();
    let pristine = base.clone();
    let (bw, bh) = base.dimensions();
    let px_scale = bw as f32 / viewport.width as f32;
    let label_font_px = LABEL_FONT_PX as f32 * px_scale;
    let label_gap_px = (LABEL_GAP_PX as f32 * px_scale).round() as i32;
    let mut layouts = Vec::new();
    for marker in markers {
        if viewport.excludes_discord_map_corner_anchor(
            marker.lat,
            marker.lon,
            corner_nw,
            corner_se,
        ) {
            continue;
        }
        let Some(stem) = icons
            .manifest
            .png_stem_for(&marker.kind, &marker.icon_coalition)
        else {
            warn!(
                "discord map: no icon mapping for kind={} coalition={}",
                marker.kind, marker.icon_coalition
            );
            continue;
        };
        let Some(icon_bytes) = icons.pngs.get(stem) else {
            warn!("discord map: missing icon PNG for {stem}");
            continue;
        };
        let icon = image::load_from_memory(icon_bytes)
            .with_context(|| format!("decode icon {stem}"))?
            .to_rgba8();
        let (cx, cy) = viewport.ll_to_pixel_in(marker.lat, marker.lon, bw, bh);
        let (iw, ih) = icon.dimensions();
        let ox = (cx - iw as f32 / 2.).round() as i32;
        let oy = (cy - ih as f32 / 2.).round() as i32;
        if composite_skip_corner_draw(cx, cy, ox, oy, iw, ih, marker.threatened, px_scale) {
            continue;
        }
        let threat_raster_px = threat_ring_diameter_px(iw, ih);
        if marker.threatened {
            draw_threat_ring(
                &mut base,
                cx,
                cy,
                threat_raster_px as f32 / 2.,
            );
        }
        overlay_icon_alpha(&mut base, &icon, ox, oy);
        if !marker.label.is_empty() {
            let label_x = ox + iw as i32 + label_gap_px;
            let label_y = oy + (ih as i32 - label_font_px.round() as i32) / 2;
            MAP_LABEL_FONT.draw_white(&mut base, label_x, label_y, &marker.label, label_font_px);
        }
        layouts.push(MarkerLayout {
            cx,
            cy,
            sw: iw,
            sh: ih,
            icon_b64: B64.encode(icon_bytes),
            tip_coalition: marker.tip_coalition.clone(),
            kind: marker.kind.clone(),
            f10_label: marker.f10_label.clone(),
            health: marker.health,
            logi: marker.logi,
            production: marker.production,
        });
    }
    restore_map_nw_corner(&mut base, &pristine, px_scale);
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(base)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .context("encode composited PNG")?;
    Ok((out, layouts, bw, bh))
}

/// Skip marker raster when drawable bbox overlaps NW corner guard.
fn composite_skip_corner_draw(
    cx: f32,
    cy: f32,
    ox: i32,
    oy: i32,
    iw: u32,
    ih: u32,
    threatened: bool,
    px_scale: f32,
) -> bool {
    let guard = MAP_CORNER_GUARD_LOGICAL * px_scale;
    let mut left = ox as f32;
    let mut top = oy as f32;
    if threatened {
        let r = threat_ring_diameter_px(iw, ih) as f32 / 2.;
        left = left.min(cx - r);
        top = top.min(cy - r);
    }
    // Label halo can extend 1px up/left from the label box; icon is left of label.
    left -= 1.0;
    top -= 1.0;
    left < guard && top < guard
}

/// Restore NW corner from pristine Mapbox base (removes any compositing artifact).
fn restore_map_nw_corner(base: &mut RgbaImage, pristine: &RgbaImage, px_scale: f32) {
    let guard = (MAP_CORNER_GUARD_LOGICAL * px_scale).ceil() as u32;
    let (bw, bh) = base.dimensions();
    let w = guard.min(bw);
    let h = guard.min(bh);
    for y in 0..h {
        for x in 0..w {
            base.put_pixel(x, y, *pristine.get_pixel(x, y));
        }
    }
}

fn tooltip_rows_html(kind: &str, health: u8, logi: u8, production: u8) -> String {
    match kind {
        "airbase" | "fob" => format!(
            "<tr><td>Health</td><td>{health} %</td></tr><tr><td>Logi</td><td>{logi} %</td></tr>"
        ),
        "logistics" => format!(
            "<tr><td>Production</td><td>{production} %</td></tr><tr><td>Health</td><td>{health} %</td></tr><tr><td>Logi</td><td>{logi} %</td></tr>"
        ),
        "production" => format!("<tr><td>Production</td><td>{production} %</td></tr>"),
        _ => String::new(),
    }
}

fn coalition_tip_class(coalition: &str) -> &'static str {
    match coalition {
        "red" => "tip-red",
        "blue" => "tip-blue",
        _ => "tip-neutral",
    }
}

fn stat_bar_class(value: u8) -> &'static str {
    match value {
        0..=32 => "health-red",
        33..=66 => "health-orange",
        _ => "health-green",
    }
}

fn stat_bar_html(value: u8) -> String {
    let fill_class = stat_bar_class(value);
    format!(
        r#"<div class="health-bar"><div class="health-bar-fill {fill_class}" style="width:{value}%"></div></div>"#
    )
}

fn marker_stat_bars_html(kind: &str, health: u8, logi: u8, production: u8) -> String {
    let bars: String = match kind {
        "logistics" => {
            let mut s = stat_bar_html(production);
            s.push_str(&stat_bar_html(health));
            s.push_str(&stat_bar_html(logi));
            s
        }
        "airbase" | "fob" => {
            let mut s = stat_bar_html(health);
            s.push_str(&stat_bar_html(logi));
            s
        }
        "production" => stat_bar_html(production),
        _ => String::new(),
    };
    if bars.is_empty() {
        String::new()
    } else {
        format!(r#"<div class="stat-bars">{bars}</div>"#)
    }
}

fn front_line_svg(
    viewport: &MapViewport,
    img_w: u32,
    img_h: u32,
    polys: &[DiscordMapFrontLinePolygon],
) -> String {
    if polys.is_empty() {
        return String::new();
    }
    let mut inner = String::new();
    for poly in polys {
        let fill = match poly.coalition.as_str() {
            "red" => "rgba(196,56,56,0.35)",
            "blue" => "rgba(46,90,172,0.35)",
            _ => continue,
        };
        let mut pts = String::new();
        for (i, (lat, lon)) in poly.latlon.iter().enumerate() {
            let (x, y) = viewport.ll_to_pixel_in(*lat, *lon, img_w, img_h);
            if i > 0 {
                pts.push(' ');
            }
            use std::fmt::Write as _;
            let _ = write!(pts, "{x:.3},{y:.3}");
        }
        inner.push_str(&format!(
            r#"<polygon points="{pts}" fill="{fill}" stroke="none"/>"#
        ));
    }
    if inner.is_empty() {
        return String::new();
    }
    format!(
        r#"<svg class="front-line" viewBox="0 0 {img_w} {img_h}" preserveAspectRatio="none" shape-rendering="geometricPrecision" aria-hidden="true">{inner}</svg>"#
    )
}

fn status_vs_html(blue: u32, red: u32) -> String {
    format!(
        r#"<span class="stat-blue">{blue}</span> vs <span class="stat-red">{red}</span>"#
    )
}

fn status_production_html(blue: Option<u8>, red: Option<u8>) -> String {
    let blue_s = blue.map(|v| v.to_string()).unwrap_or_else(|| "—".into());
    let red_s = red.map(|v| v.to_string()).unwrap_or_else(|| "—".into());
    format!(
        r#"<span class="stat-blue">{blue_s}</span> vs <span class="stat-red">{red_s}</span>"#
    )
}

fn status_balancing_html(blue: i32, red: i32) -> String {
    format!(
        r#"<span class="stat-blue">{blue}</span> vs <span class="stat-red">{red}</span>"#
    )
}

fn ping_class(ping: u32) -> &'static str {
    match ping {
        0..=70 => "ping-green",
        71..=130 => "ping-yellow",
        131..=200 => "ping-orange",
        _ => "ping-red",
    }
}

fn pilot_list_rows(pilots: &[DiscordMapPilotEntry]) -> String {
    let mut rows = String::new();
    for p in pilots {
        let cls = ping_class(p.ping);
        rows.push_str(&format!(
            r#"<div class="pilot-row"><span class="rank-col" aria-hidden="true"></span><span class="pilot-name">{name}</span><span class="pilot-ping {cls}">{ping}</span></div>"#,
            name = html_escape(&p.name),
            cls = cls,
            ping = p.ping,
        ));
    }
    rows
}

fn pilot_section(
    title: &str,
    header_class: &str,
    block_class: &str,
    pilots: &[DiscordMapPilotEntry],
    title_bold: bool,
) -> String {
    let title_class = if title_bold {
        "pilot-hdr-title pilot-hdr-title-bold"
    } else {
        "pilot-hdr-title"
    };
    format!(
        r#"<div class="pilot-block {block_class}"><div class="pilot-hdr-row {header_class}"><span class="rank-col" aria-hidden="true"></span><span class="{title_class}">{title}</span><span class="pilot-hdr-ping">ping</span></div><div class="pilot-list">{rows}</div></div>"#,
        title = title,
        header_class = header_class,
        block_class = block_class,
        title_class = title_class,
        rows = pilot_list_rows(pilots),
    )
}

fn sidebar_online_stat_html(bar: &DiscordMapStatusBar) -> String {
    format!(
        r#"<div class="stat sidebar-online-stat"><div class="stat-h">Online pilots</div><div class="stat-v">{online}</div></div>"#,
        online = status_vs_html(bar.online_blue, bar.online_red),
    )
}

fn sidebar_pilots_html(bar: &DiscordMapStatusBar) -> String {
    format!(
        r#"<div class="sidebar-pilots">
{pilots_blue}
{pilots_red}
{pilots_spec}
{top10}
</div>"#,
        pilots_blue = pilot_section(
            "BLUE pilots",
            "pilot-hdr-blue",
            "pilot-block-blue",
            &bar.blue_pilots,
            true,
        ),
        pilots_red = pilot_section(
            "RED pilots",
            "pilot-hdr-red",
            "pilot-block-red",
            &bar.red_pilots,
            true,
        ),
        pilots_spec = pilot_section(
            "SPECTATORS",
            "pilot-hdr-neutral",
            "pilot-block-neutral",
            &bar.spectators,
            false,
        ),
        top10 = bar.campaign_top10_sidebar_html,
    )
}

fn sidebar_fly_fight_win_html() -> String {
    let w = map_fly_fight_win_display_w();
    let h = map_fly_fight_win_display_h();
    format!(
        r#"<div class="sidebar-ffw" aria-hidden="true"><img class="sidebar-ffw-img" src="data:image/png;base64,{b64}" width="{w}" height="{h}" alt=""></div>"#,
        b64 = MAP_FLY_FIGHT_WIN_B64.as_str(),
        w = w,
        h = h,
    )
}

fn sidebar_pilots_col_html(bar: &DiscordMapStatusBar) -> String {
    format!(
        r#"<div class="left-col">{ffw}{pilots}</div>"#,
        ffw = sidebar_fly_fight_win_html(),
        pilots = sidebar_pilots_html(bar),
    )
}

fn restart_display_rem_secs(bar: &DiscordMapStatusBar) -> Option<u64> {
    bar.restart_utc_ms.map(|ms| {
        (ms - bar.gen_utc_ms).max(0) as u64 / 1000 + u64::from(bar.restart_display_skew_secs)
    })
}

fn format_restart_hms(rem_secs: u64) -> String {
    let h = rem_secs / 3600;
    let m = (rem_secs % 3600) / 60;
    let s = rem_secs % 60;
    format!("{h}:{m:02}:{s:02}")
}

fn stats_row_html(bar: &DiscordMapStatusBar, top: bool) -> String {
    let restart_initial = restart_display_rem_secs(bar)
        .map(format_restart_hms)
        .unwrap_or_else(|| "—".into());
    let mission_h = bar.mission_tod_secs / 3600;
    let mission_m = (bar.mission_tod_secs % 3600) / 60;
    let mission_datetime_initial = format!(
        "{date} {h}:{m:02}",
        date = bar.mission_date,
        h = mission_h,
        m = mission_m,
    );
    let mission_day_initial = if bar.campaign_stats_enabled {
        bar.campaign_duration_days
    } else {
        bar.mission_elapsed_days + 1
    };
    let online_hours = if bar.campaign_stats_enabled {
        format!(
            r#"<span class="stat-blue">{blue}</span> vs <span class="stat-red">{red}</span>"#,
            blue = bar.campaign_online_hours_blue,
            red = bar.campaign_online_hours_red,
        )
    } else {
        r#"<span class="stat-blue">?</span> vs <span class="stat-red">?</span>"#.into()
    };
    if top {
        format!(
            r#"<div class="stats stats-top">
  <div class="stat"><div class="stat-h">Date and time in mission</div><div class="stat-v stat-plain" id="mission-datetime">{mission_datetime_initial}</div></div>
  <div class="stat"><div class="stat-h">Duration</div><div class="stat-v stat-accent" id="mission-duration">Day {mission_day_initial}</div></div>
  <div class="stat"><div class="stat-h">Time to restart</div><div class="stat-v stat-accent" id="restart-time">{restart_initial}</div></div>
  <div class="stat"><div class="stat-h">Ground objectives</div><div class="stat-v">{ground}</div></div>
  <div class="stat"><div class="stat-h">Carrier objectives</div><div class="stat-v">{carrier}</div></div>
  <div class="stat"><div class="stat-h">Factories</div><div class="stat-v">{factories}</div></div>
  <div class="stat"><div class="stat-h">Production %</div><div class="stat-v">{production}</div></div>
  <div class="stat"><div class="stat-h">Balancing points</div><div class="stat-v">{balancing}</div></div>
  <div class="stat"><div class="stat-h">Online hours</div><div class="stat-v stat-plain">{online_hours}</div></div>
</div>"#,
            mission_datetime_initial = mission_datetime_initial,
            mission_day_initial = mission_day_initial,
            restart_initial = restart_initial,
            ground = status_vs_html(bar.ground_blue, bar.ground_red),
            carrier = status_vs_html(bar.carrier_blue, bar.carrier_red),
            factories = status_vs_html(bar.factories_blue, bar.factories_red),
            production = status_production_html(bar.production_blue, bar.production_red),
            balancing = status_balancing_html(bar.balancing_blue, bar.balancing_red),
            online_hours = online_hours,
        )
    } else {
        format!(
            r#"<div class="stats stats-bottom">
  <div class="stat"><div class="stat-h">Lives</div><div class="stat-v stat-accent">{lives}</div></div>
  <div class="stat"><div class="stat-h">Lives reset</div><div class="stat-v stat-accent">{lives_reset}</div></div>
  <div class="stat"><div class="stat-h">Air-deslot penalty</div><div class="stat-v stat-accent">{deslot}</div></div>
  <div class="stat"><div class="stat-h">Player crates</div><div class="stat-v stat-accent">{crates}</div></div>
  <div class="stat"><div class="stat-h">Threatened</div><div class="stat-v stat-accent">{threatened}</div></div>
  <div class="stat"><div class="stat-h">Bases repair</div><div class="stat-v stat-accent">{bases_repair}</div></div>
  <div class="stat"><div class="stat-h">Factory repair</div><div class="stat-v stat-accent">{factory_repair}</div></div>
  <div class="stat"><div class="stat-h">Static repair</div><div class="stat-v stat-accent">{static_repair}</div></div>
  <div class="stat"><div class="stat-h">Supply to bases</div><div class="stat-v stat-accent">{supply}</div></div>
  <div class="stat"><div class="stat-h">Delivery to HUBs</div><div class="stat-v stat-accent">{delivery}</div></div>
</div>"#,
            lives = html_escape(&bar.lives),
            lives_reset = html_escape(&bar.lives_reset),
            deslot = html_escape(&bar.deslot_penalty),
            crates = html_escape(&bar.player_crates),
            threatened = html_escape(&bar.threatened),
            bases_repair = html_escape(&bar.bases_repair),
            factory_repair = html_escape(&bar.factory_repair),
            static_repair = html_escape(&bar.static_repair),
            supply = html_escape(&bar.supply_to_bases),
            delivery = html_escape(&bar.delivery_to_hubs),
        )
    }
}

fn map_header_html(bar: &DiscordMapStatusBar) -> String {
    let status_time = bar
        .status_utc
        .rsplit_once(' ')
        .map(|(_, time)| time)
        .unwrap_or(bar.status_utc.as_str());
    format!(
        r#"<div class="map-hdr"><div class="map-hdr-left">{left}</div><div class="map-hdr-right">{links}<span class="map-hdr-status-sep">          </span>Campaign status as of {status_time} UTC&nbsp;&nbsp;</div></div>"#,
        left = map_header_left_html(bar),
        links = map_header_links_html(bar),
        status_time = html_escape(status_time),
    )
}

fn dcs_bind_display(bind: &str) -> &str {
    if bind.is_empty() {
        "    .    .    .    .    "
    } else {
        bind
    }
}

fn map_header_link_html(
    url: &str,
    label: &str,
    off_b64: &str,
    on_b64: &str,
    src_w: u32,
    src_h: u32,
) -> String {
    let w = hdr_link_icon_w(src_w, src_h);
    format!(
        r#"<a class="map-hdr-link" href="{url}" target="_blank" rel="noopener noreferrer"><span class="map-hdr-link-label">{label}</span><span class="map-hdr-link-icons" style="width:{w}px;height:{h}px" aria-hidden="true"><img class="map-hdr-link-off" src="data:image/png;base64,{off}" width="{w}" height="{h}" alt=""><img class="map-hdr-link-on" src="data:image/png;base64,{on}" width="{w}" height="{h}" alt=""></span></a>"#,
        url = html_escape(url),
        label = html_escape(label),
        off = off_b64,
        on = on_b64,
        w = w,
        h = HDR_LINK_ICON_H,
    )
}

fn map_header_link_if_url(
    url: &str,
    label: &str,
    off_b64: &str,
    on_b64: &str,
    src_w: u32,
    src_h: u32,
) -> String {
    if url.trim().is_empty() {
        return String::new();
    }
    map_header_link_html(url.trim(), label, off_b64, on_b64, src_w, src_h)
}

fn map_header_links_html(bar: &DiscordMapStatusBar) -> String {
    let mut links = String::new();
    if bar.dowload_acmi {
        links.push_str(&map_header_link_if_url(
            &bar.dowload_acmi_url,
            "Tacview",
            TACVIEW_HDR_ICON_OFF_B64.as_str(),
            TACVIEW_HDR_ICON_ON_B64.as_str(),
            TACVIEW_HDR_ICON_SRC.0,
            TACVIEW_HDR_ICON_SRC.1,
        ));
    }
    links.push_str(&map_header_link_if_url(
        &bar.discord_url,
        "Discord",
        DISCORD_HDR_ICON_OFF_B64.as_str(),
        DISCORD_HDR_ICON_ON_B64.as_str(),
        DISCORD_HDR_ICON_SRC.0,
        DISCORD_HDR_ICON_SRC.1,
    ));
    links.push_str(&map_header_link_if_url(
        &bar.stats_url,
        "Stats",
        STATS_HDR_ICON_OFF_B64.as_str(),
        STATS_HDR_ICON_ON_B64.as_str(),
        STATS_HDR_ICON_SRC.0,
        STATS_HDR_ICON_SRC.1,
    ));
    links.push_str(&map_header_link_if_url(
        &bar.manual_url,
        "Manual",
        MANUAL_HDR_ICON_OFF_B64.as_str(),
        MANUAL_HDR_ICON_ON_B64.as_str(),
        MANUAL_HDR_ICON_SRC.0,
        MANUAL_HDR_ICON_SRC.1,
    ));
    links.push_str(&map_header_link_if_url(
        &bar.deliveries_url,
        "Deliveries",
        DELIVERIES_HDR_ICON_OFF_B64.as_str(),
        DELIVERIES_HDR_ICON_ON_B64.as_str(),
        DELIVERIES_HDR_ICON_SRC.0,
        DELIVERIES_HDR_ICON_SRC.1,
    ));
    links.push_str(&map_header_link_if_url(
        &bar.bugs_report_url,
        "Bugs",
        BUGS_HDR_ICON_OFF_B64.as_str(),
        BUGS_HDR_ICON_ON_B64.as_str(),
        BUGS_HDR_ICON_SRC.0,
        BUGS_HDR_ICON_SRC.1,
    ));
    if links.is_empty() {
        return links;
    }
    format!(r#"<span class="map-hdr-links">{links}</span>"#)
}

fn map_header_left_html(bar: &DiscordMapStatusBar) -> String {
    let brand_w = map_label_display_w();
    let brand_h = map_label_display_h();
    format!(
        r#"<span class="map-hdr-brand" aria-hidden="true"><span class="map-hdr-brand-icons" style="width:{brand_w}px;height:{brand_h}px"><img class="map-hdr-brand-off" src="data:image/png;base64,{icon_off}" width="{brand_w}" height="{brand_h}" alt=""><img class="map-hdr-brand-on" src="data:image/png;base64,{icon_on}" width="{brand_w}" height="{brand_h}" alt=""></span></span><span class="map-hdr-text"><span class="map-hdr-part">DCS server IP {bind}:{port}</span><span class="map-hdr-part map-hdr-name">{name}</span><span class="map-hdr-part">{mission}</span></span>"#,
        icon_off = MAP_LABEL_GR_B64.as_str(),
        icon_on = MAP_LABEL_B64.as_str(),
        brand_w = brand_w,
        brand_h = brand_h,
        bind = html_escape(dcs_bind_display(&bar.dcs_bind_address)),
        port = html_escape(&bar.dcs_port),
        name = html_escape(&bar.dcs_name),
        mission = html_escape(&bar.mission_name),
    )
}

fn map_clock_script_html(bar: &DiscordMapStatusBar) -> String {
    let clock_json = serde_json::to_string(bar).unwrap_or_else(|_| "{}".into());
    format!(
        r#"<script type="application/json" id="fowl-map-clock">{clock_json}</script>"#,
        clock_json = clock_json.replace("</", "<\\/"),
    )
}

fn build_interactive_html(
    mission_name: &str,
    status_utc: &str,
    img_w: u32,
    img_h: u32,
    viewport: &MapViewport,
    display_png: &[u8],
    markers: &[MarkerLayout],
    front_line: &[DiscordMapFrontLinePolygon],
    status_bar: &DiscordMapStatusBar,
) -> String {
    let front_svg = front_line_svg(viewport, img_w, img_h, front_line);
    let mut body = String::new();
    for m in markers {
        let tip_class = coalition_tip_class(&m.tip_coalition);
        let rows = tooltip_rows_html(&m.kind, m.health, m.logi, m.production);
        let stat_bars = marker_stat_bars_html(&m.kind, m.health, m.logi, m.production);
        body.push_str(&format!(
            r#"<div class="m" data-cx="{cx:.3}" data-cy="{cy:.3}" data-sw="{sw}" data-sh="{sh}"><div class="m-stack"><div class="icon-hit"><img class="map-icon-hover" src="data:image/png;base64,{icon_b64}" alt="" aria-hidden="true"></div>{stat_bars}</div><div class="tip {tip_class}"><div class="tip-title">{label}</div><div class="tip-body"><table>{rows}</table></div></div></div>"#,
            cx = m.cx,
            cy = m.cy,
            sw = m.sw,
            sh = m.sh,
            icon_b64 = m.icon_b64,
            label = html_escape(&m.f10_label),
            stat_bars = stat_bars,
            tip_class = tip_class,
            rows = rows,
        ));
    }
    let base_b64 = B64.encode(display_png);
    let campaign_w = if status_bar.campaign_stats_enabled {
        CAMPAIGN_STATS_SIDEBAR_WIDTH_PX + LAYOUT_GAP_PX
    } else {
        0
    };
    let main_w = SIDEBAR_WIDTH_PX + LAYOUT_GAP_PX + img_w;
    let panel_w = main_w + campaign_w;
    let map_header = map_header_html(status_bar);
    let online_stat = sidebar_online_stat_html(status_bar);
    let stats_top = stats_row_html(status_bar, true);
    let sidebar_pilots_col = sidebar_pilots_col_html(status_bar);
    let map_clock = map_clock_script_html(status_bar);
    let stats_bottom = stats_row_html(status_bar, false);
    let campaign_stats_col = &status_bar.campaign_stats_sidebar_html;
    let map_core_bottom = format!(
        r#"<div class="map-body-bottom">{sidebar_pilots_col}<div class="main-col">{map_clock}<div class="map-frame"><div id="wrap" data-rw="{img_w}" data-rh="{img_h}" data-health-bar-px="{health_bar_map_px}"><img id="base" src="data:image/png;base64,{base_b64}" width="{img_w}" height="{img_h}" alt="map">{front_svg}<div id="overlay">{body}</div></div></div>{stats_bottom}</div></div>"#,
        sidebar_pilots_col = sidebar_pilots_col,
        map_clock = map_clock,
        img_w = img_w,
        img_h = img_h,
        health_bar_map_px = HEALTH_BAR_MAP_PX,
        base_b64 = base_b64,
        front_svg = front_svg,
        body = body,
        stats_bottom = stats_bottom,
    );
    let map_body = if status_bar.campaign_stats_enabled {
        format!(
            r#"<div class="map-body-row"><div class="map-core"><div class="map-body-top">{online_stat}{stats_top}</div>{map_core_bottom}</div>{campaign_stats_col}</div>"#,
            online_stat = online_stat,
            stats_top = stats_top,
            map_core_bottom = map_core_bottom,
            campaign_stats_col = campaign_stats_col,
        )
    } else {
        format!(
            r#"<div class="map-body"><div class="map-body-top">{online_stat}{stats_top}</div>{map_core_bottom}</div>"#,
            online_stat = online_stat,
            stats_top = stats_top,
            map_core_bottom = map_core_bottom,
        )
    };
    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="fowl-map-version" content="{status_utc}">
<title>{mn} — objective map</title>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Roboto+Condensed:wght@400;700&display=swap">
<style>
body{{margin:0;background:#000;color:#686a6e;font-family:"Roboto Condensed",Roboto,sans-serif;font-size:24px;overflow-x:auto}}
.map-panel{{display:flex;flex-direction:column;gap:{layout_gap}px;width:{panel_w}px;min-width:{panel_w}px;box-sizing:border-box}}
.map-hdr{{display:flex;justify-content:space-between;align-items:center;gap:8px;width:100%;min-height:{brand_h}px;padding:0;line-height:1;color:#686a6e;font-size:clamp(15px,calc(100vw*24/{panel_w}),24px)}}
.map-hdr-left{{display:flex;flex-direction:row;align-items:center;gap:0;text-align:left;flex:1 1 auto;min-width:0;overflow:hidden;align-self:stretch}}
.map-hdr-brand{{flex:0 0 auto;display:inline-flex;align-items:center;align-self:center;max-width:{sidebar_w}px}}
.map-hdr-brand-icons{{position:relative;display:inline-block;flex:0 0 auto;max-width:{sidebar_w}px}}
.map-hdr-brand-off,.map-hdr-brand-on{{position:absolute;left:0;top:0;width:100%;height:100%;max-width:{sidebar_w}px;object-fit:contain;image-rendering:auto}}
.map-hdr-brand-on{{display:none}}
.map-hdr-brand:hover .map-hdr-brand-off{{display:none}}
.map-hdr-brand:hover .map-hdr-brand-on{{display:block}}
.map-hdr-text{{display:flex;flex-direction:row;flex-wrap:nowrap;align-items:center;align-self:stretch;gap:1.5em;margin-left:1.5em;overflow:hidden;min-width:0;padding-top:15px}}
.map-hdr-part{{white-space:nowrap;overflow:hidden;text-overflow:ellipsis;min-width:0;flex:0 1 auto}}
.map-hdr-name{{font-weight:700}}
.map-hdr-links{{display:inline-flex;flex-direction:row;flex-wrap:nowrap;align-items:center;gap:0.75em;flex:0 0 auto}}
.map-hdr-link{{display:inline-flex;flex-direction:row;align-items:center;flex:0 0 auto;gap:0.35em;text-decoration:none;color:inherit;vertical-align:middle}}
.map-hdr-link-icons{{position:relative;display:inline-block;flex:0 0 auto;order:2}}
.map-hdr-link-label{{display:none;white-space:nowrap;order:1}}
.map-hdr-link-off,.map-hdr-link-on{{position:absolute;left:0;top:0;width:100%;height:100%;object-fit:contain;image-rendering:pixelated}}
.map-hdr-link-on{{display:none}}
.map-hdr-link:hover .map-hdr-link-off{{display:none}}
.map-hdr-link:hover .map-hdr-link-on{{display:block}}
.map-hdr-link:hover .map-hdr-link-label{{display:inline}}
.map-hdr-right{{display:flex;flex-direction:row;flex-wrap:nowrap;align-items:center;align-self:stretch;justify-content:flex-end;text-align:right;flex:0 1 auto;white-space:nowrap;padding-top:15px}}
.map-hdr-status-sep{{white-space:pre}}
.map-body{{display:flex;flex-direction:column;gap:{layout_gap}px;width:100%}}
.map-body-row{{display:flex;flex-direction:row;align-items:stretch;gap:{layout_gap}px;width:100%;box-sizing:border-box}}
.map-core{{display:flex;flex-direction:column;gap:{layout_gap}px;flex:0 0 {main_w}px;width:{main_w}px;min-width:{main_w}px;box-sizing:border-box}}
.map-body-top{{display:flex;flex-direction:row;align-items:stretch;gap:{layout_gap}px;width:100%;box-sizing:border-box}}
.map-body-bottom{{display:flex;flex-direction:row;align-items:flex-start;gap:{layout_gap}px;width:100%;box-sizing:border-box}}
.left-col{{flex:0 0 {sidebar_w}px;width:{sidebar_w}px;min-width:{sidebar_w}px;box-sizing:border-box}}
.right-col{{flex:0 0 {campaign_sidebar_w}px;width:{campaign_sidebar_w}px;min-width:{campaign_sidebar_w}px;box-sizing:border-box;display:flex;flex-direction:column;gap:{layout_gap}px;align-self:stretch;font-size:clamp(12px,calc(100vw*24/{img_w}),24px)}}
.campaign-stats-col .stat-section-hdr,.campaign-stats-col .stat-row{{grid-template-columns:1fr {value_col}px 10px {value_col}px}}
.campaign-stats-col .stat{{flex:0 0 auto;width:100%;min-width:0;border:1px solid #2e3138;box-sizing:border-box;display:flex;flex-direction:column}}
.campaign-stats-col .stat-body{{background:#000;color:#686a6e;padding:4px 0 6px;flex:0 0 auto;min-height:0}}
.campaign-stats-col .stat-h{{font-weight:700}}
.stat-body{{background:#000;color:#686a6e;padding:4px 0 6px;flex:1 1 auto;min-height:0}}
.stat-divider{{height:0;border-top:1px solid #2e3138;margin:4px 6px}}
.stat-section-hdr{{display:grid;grid-template-columns:1fr 48px 10px 48px;gap:0 2px;align-items:center;padding:4px 6px 2px;color:#686a6e;font-weight:700;font-size:.95em}}
.stat-section-hdr .lbl{{grid-column:1;text-align:left}}
.stat-section-hdr .blue-h{{grid-column:2;text-align:right;color:#2E5AAC}}
.stat-section-hdr .sep-h{{grid-column:3;text-align:center;color:#686a6e}}
.stat-section-hdr .red-h{{grid-column:4;text-align:left;color:#C43838}}
.stat-row{{display:grid;grid-template-columns:1fr 48px 10px 48px;gap:0 2px;align-items:baseline;padding:2px 6px;line-height:1.3}}
.stat-row .lbl{{text-align:left;padding-right:2px;word-break:break-word}}
.stat-row.is-total .lbl,.stat-kv.is-total .lbl{{font-weight:700}}
.stat-blue,.stat-red{{font-weight:700;font-variant-numeric:tabular-nums;white-space:nowrap}}
.stat-blue{{color:#2E5AAC;text-align:right}}
.stat-red{{color:#C43838;text-align:left}}
.stat-sep{{color:#686a6e;text-align:center;font-weight:400}}
.stat-kv{{display:flex;flex-direction:row;justify-content:space-between;gap:6px;padding:2px 6px;line-height:1.3}}
.stat-kv .lbl{{flex:1 1 auto;text-align:left}}
.stat-kv .val{{flex:0 0 auto;font-weight:700;color:#686a6e;font-variant-numeric:tabular-nums;text-align:right}}
.main-col{{display:flex;flex-direction:column;gap:{layout_gap}px;flex:0 0 {img_w}px;width:{img_w}px;min-width:{img_w}px;box-sizing:border-box}}
.stats{{display:flex;flex-wrap:nowrap;gap:4px;width:100%;box-sizing:border-box;font-size:clamp(12px,calc(100vw*24/{img_w}),24px)}}
.stats-top{{flex:1 1 0;min-width:0}}
.stats .stat{{flex:1 1 0;min-width:0;border:1px solid #2e3138;box-sizing:border-box;display:flex;flex-direction:column;min-height:0}}
.sidebar-online-stat{{flex:0 0 {sidebar_w}px;width:{sidebar_w}px;min-width:{sidebar_w}px;min-height:0;border:1px solid #2e3138;box-sizing:border-box;display:flex;flex-direction:column;font-size:clamp(12px,calc(100vw*24/{img_w}),24px)}}
.stat-h{{background:#15161a;color:#686a6e;line-height:1.2;padding:5px 2px;text-align:center;white-space:normal;word-break:break-word;overflow:hidden;border-bottom:1px solid #2e3138;font-weight:400;flex:0 0 auto}}
.stat-v{{background:#000;color:#686a6e;line-height:1.3;padding:6px 4px;text-align:center;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;flex:1 1 auto;min-height:0;font-weight:700}}
.stat-plain{{color:#686a6e}}
.stat-red{{color:#C43838}}
.stat-blue{{color:#2E5AAC}}
.stat-accent{{color:#e8c547}}
.sidebar-ffw{{flex:0 0 auto;width:100%;line-height:0;margin-bottom:{layout_gap}px}}
.sidebar-ffw-img{{display:block;width:100%;height:auto;object-fit:contain}}
.sidebar-pilots{{display:flex;flex-direction:column;gap:{layout_gap}px;box-sizing:border-box;font-size:clamp(12px,calc(100vw*21/{img_w}),21px)}}
.pilot-block{{box-sizing:border-box;border:1px solid #2e3138}}
.pilot-block-blue{{border-color:#2E5AAC}}
.pilot-block-red{{border-color:#C43838}}
.pilot-block-neutral{{border-color:#2e3138}}
.pilot-hdr-row{{display:flex;flex-direction:row;flex-wrap:nowrap;align-items:center;width:100%;box-sizing:border-box;color:#fff;line-height:1.2;white-space:nowrap}}
.pilot-hdr-blue{{background:rgba(46,90,172,.9)}}
.pilot-hdr-red{{background:rgba(196,56,56,.9)}}
.pilot-hdr-neutral{{background:rgba(46,49,56,.9)}}
.pilot-hdr-top10{{background:#15161a;color:#686a6e}}
.pilot-hdr-row .rank-col,.pilot-row .rank-col{{flex:0 0 {rank_col}px;width:{rank_col}px;min-width:{rank_col}px}}
.pilot-hdr-title{{flex:1 1 auto;min-width:0;text-align:left;padding:5px 4px;font-weight:400;overflow:hidden;text-overflow:ellipsis}}
.pilot-hdr-title-bold{{font-weight:700}}
.pilot-hdr-ping{{flex:0 0 36px;width:36px;text-align:right;padding:5px 4px;font-weight:400}}
.pilot-row{{display:flex;flex-direction:row;flex-wrap:nowrap;align-items:center;width:100%;box-sizing:border-box;line-height:1.3}}
.pilot-row .pilot-name{{flex:1 1 auto;min-width:0;padding:3px 4px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}}
.pilot-row .pilot-ping{{flex:0 0 36px;width:36px;padding:3px 4px;text-align:right}}
.ping-green{{color:#3d9e5a}}
.ping-yellow{{color:#e8c547}}
.ping-orange{{color:#e07a2a}}
.ping-red{{color:#C43838}}
.sidebar-top10{{display:contents}}
.top10-blue{{color:#2E5AAC}}
.top10-red{{color:#C43838}}
.top10-neutral{{color:#686a6e}}
.map-frame{{border:1px solid #2e3138;box-sizing:border-box;display:block;line-height:0;width:100%}}
#wrap{{position:relative;display:block;line-height:0;width:100%}}
#base{{display:block;width:100%;height:auto}}
.front-line{{position:absolute;left:0;top:0;width:100%;height:100%;pointer-events:none;z-index:0}}
#overlay{{position:absolute;left:0;top:0;width:100%;height:100%;pointer-events:none;z-index:1}}
.m{{position:absolute;pointer-events:auto}}
.m:hover{{z-index:10000}}
.m-stack{{position:relative;display:inline-flex;flex-direction:column;align-items:center;line-height:0}}
.icon-hit{{position:relative;display:block;line-height:0;flex-shrink:0;overflow:hidden}}
.map-icon-hover{{display:block;opacity:0;box-sizing:border-box;pointer-events:none;transition:opacity .15s,filter .15s}}
.m:hover .map-icon-hover{{opacity:1;filter:brightness(1.1)}}
.stat-bars{{display:flex;flex-direction:column;gap:3px;margin-top:{stat_bars_gap}px;z-index:1;flex-shrink:0}}
.health-bar{{height:3px;background:#000;flex-shrink:0}}
.health-bar-fill{{height:100%;max-width:100%}}
@keyframes health-blink{{
  0%,100%{{opacity:1}}
  50%{{opacity:.2}}
}}
.health-red{{background:#C43838;animation:health-blink 1s ease-in-out infinite}}
.health-orange{{background:#e07a2a}}
.health-green{{background:#3d9e5a}}
.tip{{display:none;position:absolute;left:calc(100% + 6px);top:50%;transform:translateY(-50%);color:#686a6e;padding:0;border-radius:4px;border-width:2px;border-style:solid;font-size:clamp(12px,calc(100vw*24/{img_w}),24px);line-height:1.55;white-space:nowrap;pointer-events:none;box-shadow:0 2px 8px rgba(0,0,0,.45);overflow:hidden}}
.tip-left{{left:auto;right:calc(100% + 6px)}}
.m:hover .tip{{display:block}}
.tip-title{{color:#fff;text-decoration:none;margin:0;padding:6px 10px;line-height:1.45;font-weight:700}}
.tip-body{{background:rgba(9,10,13,.9);padding:4px 10px 6px 10px}}
.tip table{{border-collapse:separate;border-spacing:0 4px}}
.tip td{{padding:1px 10px 1px 0;vertical-align:top;line-height:1.55}}
.tip-red{{border-color:#C43838}}
.tip-red .tip-title{{background:rgba(196,56,56,.9)}}
.tip-blue{{border-color:#2E5AAC}}
.tip-blue .tip-title{{background:rgba(46,90,172,.9)}}
.tip-neutral{{border-color:#2e3138}}
.tip-neutral .tip-title{{background:rgba(46,49,56,.9)}}
</style></head><body>
<div class="map-panel">{map_header}{map_body}</div>
<script>
(function(){{
  var wrap=document.getElementById('wrap');
  if(!wrap){{return;}}
  var rw=+wrap.dataset.rw||1;
  var rh=+wrap.dataset.rh||1;
  var healthBarPx=+wrap.dataset.healthBarPx||30;
  function mapDisplayScale(){{
    var baseImg=document.getElementById('base');
    if(baseImg&&baseImg.naturalWidth){{
      return baseImg.getBoundingClientRect().width/baseImg.naturalWidth;
    }}
    var w=wrap.getBoundingClientRect().width;
    return w?w/rw:0;
  }}
  function layoutMarkers(){{
    var scale=mapDisplayScale();
    if(!scale){{return;}}
    wrap.querySelectorAll('.m').forEach(function(m){{
      var cx=+m.dataset.cx;
      var cy=+m.dataset.cy;
      var sw=+m.dataset.sw;
      var sh=+m.dataset.sh;
      var iconW=Math.max(1,Math.round(sw*scale));
      var iconH=Math.max(1,Math.round(sh*scale));
      m.style.left=(cx/rw*100)+'%';
      m.style.top=(cy/rh*100)+'%';
      var stack=m.querySelector('.m-stack');
      if(stack){{stack.style.transform='translate(-50%,'+(-(iconH/2))+'px)';}}
      var iconHit=m.querySelector('.icon-hit');
      if(iconHit){{
        iconHit.style.width=iconW+'px';
        iconHit.style.height=iconH+'px';
      }}
      var hoverImg=m.querySelector('.map-icon-hover');
      if(hoverImg){{
        hoverImg.style.width=iconW+'px';
        hoverImg.style.height=iconH+'px';
      }}
      var bars=m.querySelector('.stat-bars');
      var barW=Math.max(2,healthBarPx*scale);
      if(bars){{
        bars.style.width=barW+'px';
      }}
      m.querySelectorAll('.health-bar').forEach(function(bar){{
        bar.style.width=barW+'px';
        bar.style.height=Math.max(2,3*scale)+'px';
      }});
    }});
  }}
  function scheduleLayout(){{
    requestAnimationFrame(layoutMarkers);
  }}
  scheduleLayout();
  window.addEventListener('resize',scheduleLayout);
  if(window.visualViewport){{
    window.visualViewport.addEventListener('resize',scheduleLayout);
    window.visualViewport.addEventListener('scroll',scheduleLayout);
  }}
  if(typeof ResizeObserver!=='undefined'){{
    new ResizeObserver(scheduleLayout).observe(wrap);
  }}
  var baseImg=document.getElementById('base');
  if(baseImg){{
    if(baseImg.complete){{scheduleLayout();}}
    else{{baseImg.addEventListener('load',scheduleLayout);}}
  }}
  wrap.querySelectorAll('.m').forEach(function(m){{
    m.addEventListener('mouseenter',function(){{
      var tip=m.querySelector('.tip');
      if(!tip){{return;}}
      requestAnimationFrame(function(){{
        tip.classList.remove('tip-left');
        var wr=wrap.getBoundingClientRect();
        var tr=tip.getBoundingClientRect();
        if(tr.right>wr.right-1){{tip.classList.add('tip-left');}}
        tr=tip.getBoundingClientRect();
        if(tr.left<wr.left+1){{tip.classList.remove('tip-left');}}
      }});
    }});
  }});
}})();
(function(){{
  var el=document.getElementById('fowl-map-clock');
  if(!el){{return;}}
  var anchor;
  try{{anchor=JSON.parse(el.textContent||'{{}}');}}catch(e){{return;}}
  var datetimeEl=document.getElementById('mission-datetime');
  var durationEl=document.getElementById('mission-duration');
  var restartEl=document.getElementById('restart-time');
  function pad2(n){{return n<10?'0'+n:''+n;}}
  function tick(){{
    var now=Date.now();
    var elapsed=(now-(anchor.gen_utc_ms||0))/1000;
    var total=((anchor.mission_tod_secs||0)+elapsed);
    var days=Math.floor(total/86400);
    var tod=Math.floor(total%86400);
    var h=Math.floor(tod/3600);
    var m=Math.floor((tod%3600)/60);
    var dateStr='';
    if(anchor.mission_date){{
      var p=anchor.mission_date.split('-');
      if(p.length===3){{
        var d=new Date(Date.UTC(+p[0],+p[1]-1,+p[2]+days));
        dateStr=d.getUTCFullYear()+'-'+pad2(d.getUTCMonth()+1)+'-'+pad2(d.getUTCDate());
      }}
    }}
    if(datetimeEl){{
      datetimeEl.textContent=(dateStr?dateStr+' ':'')+h+':'+pad2(m);
    }}
    if(durationEl){{
      if(anchor.campaign_stats_enabled){{
        durationEl.textContent='Day '+(anchor.campaign_duration_days||1);
      }}else{{
        var dayNum=(anchor.mission_elapsed_days||0)+days+1;
        durationEl.textContent='Day '+dayNum;
      }}
    }}
    if(restartEl&&anchor.restart_utc_ms){{
      var skew=anchor.restart_display_skew_secs||0;
      var rem=Math.max(0,Math.floor((anchor.restart_utc_ms-now)/1000)+skew);
      var rh=Math.floor(rem/3600);
      var rm=Math.floor((rem%3600)/60);
      var rs=rem%60;
      restartEl.textContent=rh+':'+pad2(rm)+':'+pad2(rs);
    }}
  }}
  tick();
  setInterval(tick,1000);
}})();
(function(){{
  if(location.protocol==='file:'){{return;}}
  var meta=document.querySelector('meta[name="fowl-map-version"]');
  if(!meta){{return;}}
  var current=meta.getAttribute('content')||'';
  function poll(){{
    fetch('/map-version',{{cache:'no-store'}}).then(function(r){{return r.ok?r.text():'';}}).then(function(v){{
      v=(v||'').trim();
      if(v&&v!==current){{location.reload();}}
    }}).catch(function(){{}});
  }}
  setInterval(poll,45000);
}})();
</script>
</body></html>"#,
        mn = html_escape(mission_name),
        status_utc = html_escape(status_utc),
        panel_w = panel_w,
        main_w = main_w,
        brand_h = map_label_display_h(),
        img_w = img_w,
        map_header = map_header,
        map_body = map_body,
        layout_gap = LAYOUT_GAP_PX,
        sidebar_w = SIDEBAR_WIDTH_PX,
        campaign_sidebar_w = CAMPAIGN_STATS_SIDEBAR_WIDTH_PX,
        value_col = CAMPAIGN_STATS_VALUE_COL_PX,
        rank_col = SIDEBAR_RANK_COL_PX,
        stat_bars_gap = STAT_BARS_GAP_PX,
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn threat_ring_diameter_px(iw: u32, ih: u32) -> u32 {
    let max_side = iw.max(ih) as f32;
    (max_side * THREAT_RING_SCALE + THREAT_RING_PAD_PX)
        .round()
        .max(THREAT_RING_MIN_PX as f32) as u32
        | 1
}

fn draw_threat_ring(base: &mut RgbaImage, cx: f32, cy: f32, radius: f32) {
    const YELLOW: image::Rgba<u8> = image::Rgba([255, 220, 0, 191]);
    let steps = (radius * 14.).max(56.) as u32;
    for i in 0..steps {
        let a = std::f32::consts::TAU * i as f32 / steps as f32;
        for dr in [-1.0_f32, 0.0, 1.0] {
            let r = radius + dr;
            let x = (cx + r * a.cos()).round() as i32;
            let y = (cy + r * a.sin()).round() as i32;
            put_pixel_clipped(base, x, y, YELLOW);
        }
    }
}

fn put_pixel_clipped(base: &mut RgbaImage, x: i32, y: i32, color: image::Rgba<u8>) {
    let (bw, bh) = base.dimensions();
    if x < 0 || y < 0 || x >= bw as i32 || y >= bh as i32 {
        return;
    }
    base.put_pixel(x as u32, y as u32, color);
}

fn overlay_icon_alpha(base: &mut RgbaImage, icon: &RgbaImage, ox: i32, oy: i32) {
    let (bw, bh) = base.dimensions();
    let (iw, ih) = icon.dimensions();
    for y in 0..ih {
        let by = oy + y as i32;
        if by < 0 || by >= bh as i32 {
            continue;
        }
        for x in 0..iw {
            let bx = ox + x as i32;
            if bx < 0 || bx >= bw as i32 {
                continue;
            }
            let src = icon.get_pixel(x, y);
            if src[3] == 0 {
                continue;
            }
            let dst = *base.get_pixel(bx as u32, by as u32);
            let sa = src[3] as f32 / 255.0;
            let inv = 1.0 - sa;
            let r = (src[0] as f32 * sa + dst[0] as f32 * inv).round() as u8;
            let g = (src[1] as f32 * sa + dst[1] as f32 * inv).round() as u8;
            let b = (src[2] as f32 * sa + dst[2] as f32 * inv).round() as u8;
            base.put_pixel(bx as u32, by as u32, image::Rgba([r, g, b, 255]));
        }
    }
}
