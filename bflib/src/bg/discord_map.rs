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

#[derive(Debug, Clone)]
pub struct DiscordMapMarker {
    pub lat: f64,
    pub lon: f64,
    pub kind: String,
    pub coalition: String,
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
    coalition: String,
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
    composited_png_path: PathBuf,
    base_png_path: PathBuf,
) {
    discord_map_http::ensure_map_http_server(port, html_path, composited_png_path, base_png_path).await;
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
    viewport: &MapViewport,
    markers: &[DiscordMapMarker],
    icons: &DiscordMapIconPackJob,
    caption: &str,
    mission_name: &str,
    status_utc: &str,
) -> Result<()> {
    let artifacts = build_map_artifacts(base_png_path, viewport, markers, icons, mission_name, status_utc)
        .context("build discord map artifacts")?;
    if let Some(parent) = composited_png_path.parent() {
        fs::create_dir_all(parent).await.ok();
    }
    fs::write(composited_png_path, &artifacts.png)
        .await
        .with_context(|| format!("write composited map PNG {:?}", composited_png_path))?;
    fs::write(html_path, artifacts.html.as_bytes())
        .await
        .with_context(|| format!("write interactive map HTML {:?}", html_path))?;
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
    icons: &DiscordMapIconPackJob,
    mission_name: &str,
    status_utc: &str,
) -> Result<MapArtifacts> {
    let base_bytes = std::fs::read(base_png_path)
        .with_context(|| format!("read discord map base PNG {:?}", base_png_path))?;
    let (png, layouts, img_w, img_h) = composite_map(&base_bytes, viewport, markers, icons)?;
    let html = build_interactive_html(
        mission_name,
        status_utc,
        img_w,
        img_h,
        &base_bytes,
        &layouts,
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
            .png_stem_for(&marker.kind, &marker.coalition)
        else {
            warn!(
                "discord map: no icon mapping for kind={} coalition={}",
                marker.kind, marker.coalition
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
            coalition: marker.coalition.clone(),
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
            "<tr><td>Health</td><td>{health}%</td></tr><tr><td>Logi</td><td>{logi}%</td></tr>"
        ),
        "logistics" => format!(
            "<tr><td>Production</td><td>{production}%</td></tr><tr><td>Health</td><td>{health}%</td></tr><tr><td>Logi</td><td>{logi}%</td></tr>"
        ),
        "production" => format!("<tr><td>Production</td><td>{production}%</td></tr>"),
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

fn build_interactive_html(
    mission_name: &str,
    status_utc: &str,
    img_w: u32,
    img_h: u32,
    base_png: &[u8],
    markers: &[MarkerLayout],
) -> String {
    let mut body = String::new();
    for m in markers {
        let left_pct = (m.cx / img_w as f32) * 100.;
        let top_pct = (m.cy / img_h as f32) * 100.;
        let tip_class = coalition_tip_class(&m.coalition);
        let rows = tooltip_rows_html(&m.kind, m.health, m.logi, m.production);
        let threat_ring = if m.threatened {
            format!(
                r#"<div class="threat-ring" style="width:{}px;height:{}px"></div>"#,
                m.threat_diameter_px, m.threat_diameter_px
            )
        } else {
            String::new()
        };
        body.push_str(&format!(
            r#"<div class="m" style="left:{left_pct:.4}%;top:{top_pct:.4}%"><div class="m-stack">{threat_ring}<img src="data:image/png;base64,{}" width="{}" height="{}" alt=""></div><div class="tip {tip_class}"><div class="tip-title">{}</div><table>{rows}</table></div></div>"#,
            m.icon_b64,
            m.sw,
            m.sh,
            html_escape(&m.f10_label),
        ));
    }
    let base_b64 = B64.encode(base_png);
    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{mn} — objective map</title>
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Roboto+Condensed:wght@400&display=swap">
<style>
body{{margin:0;background:#111;color:#f4f6fa;font-family:"Roboto Condensed",sans-serif}}
.hdr{{padding:10px 12px;font-size:12px;line-height:1.45}}
#wrap{{position:relative;display:inline-block;line-height:0;max-width:100%}}
#base{{display:block;max-width:100%;height:auto;width:{img_w}px}}
.m{{position:absolute;transform:translate(-50%,-50%);z-index:1}}
.m:hover{{z-index:10000}}
.m-stack{{position:relative;display:inline-block;line-height:0}}
.threat-ring{{position:absolute;left:50%;top:50%;transform:translate(-50%,-50%);border:2px solid rgba(255,220,0,.75);border-radius:50%;box-sizing:border-box;z-index:0;pointer-events:none}}
.m-stack img{{position:relative;z-index:1;display:block;transition:filter .15s}}
.m:hover img{{filter:brightness(2.7)}}
.tip{{display:none;position:absolute;left:calc(100% + 6px);top:50%;transform:translateY(-50%);background:rgba(18,20,26,.94);padding:6px 10px;border-radius:4px;border-width:2px;border-style:solid;font-size:10px;line-height:1.55;white-space:nowrap;pointer-events:none;box-shadow:0 2px 8px rgba(0,0,0,.45)}}
.m:hover .tip{{display:block}}
.tip-title{{text-decoration:underline;margin-bottom:5px;line-height:1.45}}
.tip table{{border-collapse:separate;border-spacing:0 4px}}
.tip td{{padding:1px 10px 1px 0;vertical-align:top;line-height:1.55}}
.tip-red{{border-color:#C43838}}
.tip-blue{{border-color:#2E5AAC}}
.tip-neutral{{border-color:#5C6370}}
</style></head><body>
<div class="hdr"><div>{mn}</div><div>Objectives status as of {status_utc} UTC</div></div>
<div id="wrap"><img id="base" src="data:image/png;base64,{base_b64}" width="{img_w}" height="{img_h}" alt="map">{body}</div>
</body></html>"#,
        mn = html_escape(mission_name),
        status_utc = html_escape(status_utc),
        base_b64 = base_b64,
        img_w = img_w,
        img_h = img_h,
        body = body,
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
