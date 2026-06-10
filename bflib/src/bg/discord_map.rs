//! Mapbox base fetch, icon compositing, interactive HTML, Discord webhook (background thread).

use super::discord_map_font::MapLabelFont;
use super::discord_map_http;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use bfprotocols::discord_map_icon_manifest::DiscordMapIconManifestRuntime;
use bfprotocols::discord_map_viewport::MapViewport;
use image::{imageops, RgbaImage};
use log::{info, warn};
use once_cell::sync::Lazy;
use reqwest::multipart;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::fs;

const ICON_SCALE: f32 = 0.5;
const LABEL_GAP_PX: i32 = 4;
const LABEL_FONT_PX: i32 = 8;

static MAP_LABEL_FONT: Lazy<MapLabelFont> = Lazy::new(MapLabelFont::embedded);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordMapStatusBar {
    pub mission_name: String,
    pub status_utc: String,
    pub theatre: String,
    pub mission_date: String,
    pub mission_tod_secs: u32,
    pub gen_utc_ms: i64,
    pub restart_utc_ms: Option<i64>,
    pub online_red: u32,
    pub online_blue: u32,
    pub ground_red: u32,
    pub ground_blue: u32,
    pub carrier_red: u32,
    pub carrier_blue: u32,
    pub factories_red: u32,
    pub factories_blue: u32,
    pub production_red: Option<u8>,
    pub production_blue: Option<u8>,
    pub supply_to_bases: String,
    pub delivery_to_hubs: String,
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
    threatened: bool,
    threat_diameter_px: u32,
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
) {
    discord_map_http::ensure_map_http_server(
        port,
        html_path,
        map_version_path,
        composited_png_path,
        base_png_path,
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
    post_composited_map(
        webhook_url,
        webhook_message_path,
        artifacts.png,
        caption,
    )
    .await
}

async fn post_composited_map(
    webhook_url: &str,
    webhook_message_path: &Path,
    png: Vec<u8>,
    caption: &str,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .context("build HTTP client for Discord")?;
    let webhook_base = webhook_url_base(webhook_url)?;

    if let Some(message_id) = load_webhook_message_id(webhook_message_path).await? {
        let patch_url = format!("{webhook_base}/messages/{message_id}");
        let form = discord_multipart_form(caption, png.clone(), true)?;
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
    let form = discord_multipart_form(caption, png, false)?;
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
    info!("discord map: posted composited map to Discord (message {message_id})");
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

fn discord_multipart_form(caption: &str, png: Vec<u8>, edit: bool) -> Result<multipart::Form> {
    let payload = if edit {
        serde_json::json!({
            "content": caption,
            "attachments": [{
                "id": 0,
                "filename": "objective_map.png"
            }]
        })
    } else {
        serde_json::json!({ "content": caption })
    };
    Ok(multipart::Form::new()
        .part(
            "files[0]",
            multipart::Part::bytes(png)
                .file_name("objective_map.png")
                .mime_str("image/png")
                .context("discord map png mime")?,
        )
        .text("payload_json", payload.to_string()))
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
    markers: &[DiscordMapMarker],
    front_line: &[DiscordMapFrontLinePolygon],
    icons: &DiscordMapIconPackJob,
    mission_name: &str,
    status_utc: &str,
    status_bar: &DiscordMapStatusBar,
) -> Result<MapArtifacts> {
    let base_bytes = std::fs::read(base_png_path)
        .with_context(|| format!("read discord map base PNG {:?}", base_png_path))?;
    let (png, layouts, img_w, img_h) = composite_map(&base_bytes, viewport, markers, icons)?;
    let html = build_interactive_html(
        mission_name,
        status_utc,
        img_w,
        img_h,
        viewport,
        &base_bytes,
        &layouts,
        front_line,
        status_bar,
    );
    Ok(MapArtifacts { png, html })
}

fn composite_map(
    base_png: &[u8],
    viewport: &MapViewport,
    markers: &[DiscordMapMarker],
    icons: &DiscordMapIconPackJob,
) -> Result<(Vec<u8>, Vec<MarkerLayout>, u32, u32)> {
    let mut base = image::load_from_memory(base_png)
        .context("decode base map PNG")?
        .to_rgba8();
    let (bw, bh) = base.dimensions();
    let px_scale = bw as f32 / viewport.width as f32;
    let label_font_px = LABEL_FONT_PX as f32 * px_scale;
    let label_gap_px = (LABEL_GAP_PX as f32 * px_scale).round() as i32;
    let mut layouts = Vec::new();
    for marker in markers {
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
        let sw = ((iw as f32) * ICON_SCALE).round().max(1.) as u32;
        let sh = ((ih as f32) * ICON_SCALE).round().max(1.) as u32;
        let scaled = imageops::resize(&icon, sw, sh, imageops::FilterType::Triangle);
        let threat_diameter_px =
            ((sw.max(sh) as f32 * 1.7 + 10.).round().max(14.) as u32) | 1;
        if marker.threatened {
            draw_threat_ring(
                &mut base,
                cx,
                cy,
                threat_diameter_px as f32 / 2.,
            );
        }
        let ox = (cx - sw as f32 / 2.).round() as i32;
        let oy = (cy - sh as f32 / 2.).round() as i32;
        overlay_clipped(&mut base, &scaled, ox, oy);
        if !marker.label.is_empty() {
            let label_x = ox + sw as i32 + label_gap_px;
            let label_y = oy + (sh as i32 - label_font_px.round() as i32) / 2;
            MAP_LABEL_FONT.draw_white(&mut base, label_x, label_y, &marker.label, label_font_px);
        }
        let mut icon_buf = Vec::new();
        image::DynamicImage::ImageRgba8(scaled)
            .write_to(
                &mut std::io::Cursor::new(&mut icon_buf),
                image::ImageFormat::Png,
            )
            .context("encode marker icon png")?;
        layouts.push(MarkerLayout {
            cx,
            cy,
            sw,
            sh,
            icon_b64: B64.encode(icon_buf),
            tip_coalition: marker.tip_coalition.clone(),
            kind: marker.kind.clone(),
            f10_label: marker.f10_label.clone(),
            health: marker.health,
            logi: marker.logi,
            production: marker.production,
            threatened: marker.threatened,
            threat_diameter_px,
        });
    }
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(base)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .context("encode composited PNG")?;
    Ok((out, layouts, bw, bh))
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

fn status_bar_html(bar: &DiscordMapStatusBar) -> String {
    let clock_json = serde_json::to_string(bar).unwrap_or_else(|_| "{}".into());
    let restart_initial = bar
        .restart_utc_ms
        .map(|ms| {
            let rem = (ms - bar.gen_utc_ms).max(0) / 1000;
            let h = rem / 3600;
            let m = (rem % 3600) / 60;
            let s = rem % 60;
            format!("{h}:{m:02}:{s:02}")
        })
        .unwrap_or_else(|| "—".into());
    let mission_h = bar.mission_tod_secs / 3600;
    let mission_m = (bar.mission_tod_secs % 3600) / 60;
    let mission_time_initial = format!("{mission_h}:{mission_m:02}");
    format!(
        r#"<div class="map-hdr">
<div>{mission_name}</div>
<div>Objectives status as of {status_utc} UTC</div>
</div>
<div class="stats">
  <div class="stat"><div class="stat-h">Theatre</div><div class="stat-v stat-plain">{theatre}</div></div>
  <div class="stat"><div class="stat-h">Date in mission</div><div class="stat-v stat-plain" id="mission-date">{mission_date}</div></div>
  <div class="stat"><div class="stat-h">Time</div><div class="stat-v stat-plain" id="mission-time">{mission_time_initial}</div></div>
  <div class="stat"><div class="stat-h">Time to restart</div><div class="stat-v stat-accent" id="restart-time">{restart_initial}</div></div>
  <div class="stat"><div class="stat-h">Online pilots</div><div class="stat-v">{online}</div></div>
  <div class="stat"><div class="stat-h">Ground objectives</div><div class="stat-v">{ground}</div></div>
  <div class="stat"><div class="stat-h">Carrier objectives</div><div class="stat-v">{carrier}</div></div>
  <div class="stat"><div class="stat-h">Factories</div><div class="stat-v">{factories}</div></div>
  <div class="stat"><div class="stat-h">Production %</div><div class="stat-v">{production}</div></div>
  <div class="stat"><div class="stat-h">Supply to bases</div><div class="stat-v stat-accent">{supply_to_bases}</div></div>
  <div class="stat"><div class="stat-h">Delivery to HUBs</div><div class="stat-v stat-accent">{delivery_to_hubs}</div></div>
</div>
<script type="application/json" id="fowl-map-clock">{clock_json}</script>"#,
        mission_name = html_escape(&bar.mission_name),
        status_utc = html_escape(&bar.status_utc),
        theatre = html_escape(&bar.theatre),
        mission_date = html_escape(&bar.mission_date),
        mission_time_initial = mission_time_initial,
        online = status_vs_html(bar.online_blue, bar.online_red),
        ground = status_vs_html(bar.ground_blue, bar.ground_red),
        carrier = status_vs_html(bar.carrier_blue, bar.carrier_red),
        factories = status_vs_html(bar.factories_blue, bar.factories_red),
        production = status_production_html(bar.production_blue, bar.production_red),
        restart_initial = restart_initial,
        supply_to_bases = html_escape(&bar.supply_to_bases),
        delivery_to_hubs = html_escape(&bar.delivery_to_hubs),
        clock_json = clock_json.replace("</", "<\\/"),
    )
}

fn build_interactive_html(
    mission_name: &str,
    status_utc: &str,
    img_w: u32,
    img_h: u32,
    viewport: &MapViewport,
    base_png: &[u8],
    markers: &[MarkerLayout],
    front_line: &[DiscordMapFrontLinePolygon],
    status_bar: &DiscordMapStatusBar,
) -> String {
    let front_svg = front_line_svg(viewport, img_w, img_h, front_line);
    let mut body = String::new();
    for m in markers {
        let left_pct = (m.cx / img_w as f32) * 100.;
        let top_pct = (m.cy / img_h as f32) * 100.;
        let tip_class = coalition_tip_class(&m.tip_coalition);
        let rows = tooltip_rows_html(&m.kind, m.health, m.logi, m.production);
        let threat_ring = if m.threatened {
            format!(
                r#"<div class="threat-ring" style="width:{}px;height:{}px"></div>"#,
                m.threat_diameter_px, m.threat_diameter_px
            )
        } else {
            String::new()
        };
        let stat_bars = marker_stat_bars_html(&m.kind, m.health, m.logi, m.production);
        body.push_str(&format!(
            r#"<div class="m" style="left:{left_pct:.4}%;top:{top_pct:.4}%"><div class="m-stack">{threat_ring}<img src="data:image/png;base64,{}" width="{}" height="{}" alt="">{stat_bars}</div><div class="tip {tip_class}"><div class="tip-title">{}</div><div class="tip-body"><table>{rows}</table></div></div></div>"#,
            m.icon_b64,
            m.sw,
            m.sh,
            html_escape(&m.f10_label),
        ));
    }
    let base_b64 = B64.encode(base_png);
    let stats_html = status_bar_html(status_bar);
    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="fowl-map-version" content="{status_utc}">
<title>{mn} — objective map</title>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Roboto:wght@400;700&display=swap">
<style>
body{{margin:0;background:#000;color:#686a6e;font-family:Roboto,sans-serif;font-size:16px}}
.map-panel{{display:block;width:min({img_w}px,100%);box-sizing:border-box}}
.map-hdr{{width:100%;padding:0 0 8px 0;line-height:1.35;color:#686a6e;font-size:clamp(10px,calc(100vw*16/{img_w}),16px)}}
.stats{{display:flex;flex-wrap:nowrap;gap:4px;width:100%;margin-bottom:4px;box-sizing:border-box;font-size:clamp(8px,calc(100vw*16/{img_w}),16px)}}
.stat{{flex:1 1 0;min-width:0;border:1px solid #2e3138;box-sizing:border-box;display:flex;flex-direction:column}}
.stat-h{{background:#15161a;color:#686a6e;line-height:1.2;padding:5px 2px;text-align:center;white-space:normal;word-break:break-word;overflow:hidden;border-bottom:1px solid #2e3138}}
.stat-v{{background:#000;color:#686a6e;line-height:1.3;padding:6px 4px;text-align:center;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;flex:1}}
.stat-plain{{color:#686a6e}}
.stat-red{{color:#C43838}}
.stat-blue{{color:#6eb5ff}}
.stat-accent{{color:#e8c547}}
.map-frame{{border:1px solid #2e3138;box-sizing:border-box;display:block;line-height:0;width:100%}}
#wrap{{position:relative;display:block;line-height:0;width:100%}}
#base{{display:block;width:100%;height:auto}}
.front-line{{position:absolute;left:0;top:0;width:100%;height:100%;pointer-events:none;z-index:0}}
.m{{position:absolute;transform:translate(-50%,-50%);z-index:1}}
.m:hover{{z-index:10000}}
.m-stack{{position:relative;display:inline-flex;flex-direction:column;align-items:center;line-height:0}}
.threat-ring{{position:absolute;left:50%;top:50%;transform:translate(-50%,-50%);border:2px solid rgba(255,220,0,.75);border-radius:50%;box-sizing:border-box;z-index:0;pointer-events:none}}
.m-stack img{{position:relative;z-index:1;display:block;transition:filter .15s}}
.m:hover img{{filter:brightness(1.5)}}
.stat-bars{{display:flex;flex-direction:column;gap:3px;margin-top:2px;z-index:1;flex-shrink:0}}
.health-bar{{width:clamp(15px,calc(100vw*30/{img_w}),30px);height:3px;background:#15161a;flex-shrink:0}}
.health-bar-fill{{height:100%;max-width:100%}}
@keyframes health-blink{{
  0%,100%{{opacity:1}}
  50%{{opacity:.2}}
}}
.health-red{{background:#C43838;animation:health-blink 1s ease-in-out infinite}}
.health-orange{{background:#e07a2a}}
.health-green{{background:#3d9e5a}}
.tip{{display:none;position:absolute;left:calc(100% + 6px);top:50%;transform:translateY(-50%);color:#686a6e;padding:0;border-radius:4px;border-width:2px;border-style:solid;font-size:clamp(8px,calc(100vw*16/{img_w}),16px);line-height:1.55;white-space:nowrap;pointer-events:none;box-shadow:0 2px 8px rgba(0,0,0,.45);overflow:hidden}}
.tip-left{{left:auto;right:calc(100% + 6px)}}
.m:hover .tip{{display:block}}
.tip-title{{text-decoration:none;margin:0;padding:6px 10px;line-height:1.45;font-weight:700}}
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
<div class="map-panel">{stats_html}<div class="map-frame"><div id="wrap"><img id="base" src="data:image/png;base64,{base_b64}" width="{img_w}" height="{img_h}" alt="map">{front_svg}{body}</div></div></div>
<script>
(function(){{
  var wrap=document.getElementById('wrap');
  if(!wrap){{return;}}
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
  var timeEl=document.getElementById('mission-time');
  var dateEl=document.getElementById('mission-date');
  var restartEl=document.getElementById('restart-time');
  function pad2(n){{return n<10?'0'+n:''+n;}}
  function tick(){{
    var now=Date.now();
    var elapsed=(now-(anchor.gen_utc_ms||0))/1000;
    var total=((anchor.mission_tod_secs||0)+elapsed);
    var days=Math.floor(total/86400);
    var tod=Math.floor(total%86400);
    if(timeEl){{
      var h=Math.floor(tod/3600);
      var m=Math.floor((tod%3600)/60);
      timeEl.textContent=h+':'+pad2(m);
    }}
    if(dateEl&&anchor.mission_date){{
      var p=anchor.mission_date.split('-');
      if(p.length===3){{
        var d=new Date(Date.UTC(+p[0],+p[1]-1,+p[2]+days));
        dateEl.textContent=d.getUTCFullYear()+'-'+pad2(d.getUTCMonth()+1)+'-'+pad2(d.getUTCDate());
      }}
    }}
    if(restartEl&&anchor.restart_utc_ms){{
      var rem=Math.max(0,Math.floor((anchor.restart_utc_ms-now)/1000));
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
        base_b64 = base_b64,
        img_w = img_w,
        img_h = img_h,
        body = body,
        stats_html = stats_html,
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
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

fn overlay_clipped(base: &mut RgbaImage, icon: &RgbaImage, ox: i32, oy: i32) {
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
            let p = icon.get_pixel(x, y);
            if p[3] == 0 {
                continue;
            }
            base.put_pixel(bx as u32, by as u32, *p);
        }
    }
}
