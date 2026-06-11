//! Embed Discord map icons from `assets/discord-objective-map/png/<canvas_px>/` into the assembled `.miz`.

use anyhow::{bail, Context, Result};
use bfprotocols::discord_map_icon_manifest::{
    DiscordMapIconManifest, MIZ_DIR, MIZ_MANIFEST, SUPPORTED_SCHEMA_VERSION, ASSETS_REL,
};
use dcso3::String as MizString;
use log::{info, warn};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// When campaign CFG enables discord map, require ME corner zones in the base mission.
pub fn validate_discord_map_zones(mission: &dcso3::env::miz::Miz<'_>, campaign_cfg: Option<&Path>) -> Result<()> {
    let Some(cfg_path) = campaign_cfg else {
        return Ok(());
    };
    let bytes = fs::read(cfg_path)
        .with_context(|| format!("read campaign cfg {:?}", cfg_path))?;
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).context("parse campaign cfg for discord_map check")?;
    let enabled = v
        .get("discord_map")
        .and_then(|d| d.get("enabled"))
        .and_then(|e| e.as_bool())
        .unwrap_or(false);
    if !enabled {
        return Ok(());
    }
    let mut names = Vec::new();
    for zone in mission.triggers()? {
        let zone = zone?;
        names.push(zone.name()?.as_str().to_string());
    }
    bfprotocols::discord_map_viewport::validate_corner_zones_present(names.iter().map(String::as_str))
        .context("discord_map.enabled in campaign CFG requires ME corner zones in base.miz")?;
    Ok(())
}

pub fn find_assets_dir(anchor: &Path) -> Option<PathBuf> {
    let start = anchor.canonicalize().ok()?;
    let mut cur = if start.is_dir() {
        start
    } else {
        start.parent()?.to_path_buf()
    };
    for _ in 0..16 {
        let candidate = cur.join(ASSETS_REL);
        if candidate.join("manifest.json").is_file() {
            return Some(candidate);
        }
        cur = cur.parent()?.to_path_buf();
    }
    None
}

pub fn embed_into_miz(
    miz_root: &Path,
    files: &mut HashMap<MizString, PathBuf>,
    anchor: &Path,
) -> Result<()> {
    let Some(assets_dir) = find_assets_dir(anchor) else {
        warn!(
            "discord map icons: no {ASSETS_REL} found walking up from {:?}; skipping embed",
            anchor
        );
        return Ok(());
    };
    let manifest_path = assets_dir.join("manifest.json");
    let manifest = DiscordMapIconManifest::load(&manifest_path)?;
    let png_dir = manifest.assets_png_dir(&assets_dir);
    let runtime = manifest.to_runtime();
    let runtime_json = serde_json::to_string_pretty(&runtime).context("serialize runtime manifest")?;

    let miz_manifest_dir = miz_root.join(MIZ_DIR.replace('/', std::path::MAIN_SEPARATOR_STR));
    fs::create_dir_all(&miz_manifest_dir)
        .with_context(|| format!("create discord map dir {:?}", miz_manifest_dir))?;

    let manifest_dest = miz_manifest_dir.join("manifest.json");
    fs::write(&manifest_dest, runtime_json.as_bytes())
        .with_context(|| format!("write runtime manifest {:?}", manifest_dest))?;
    files.insert(MizString::from(MIZ_MANIFEST), manifest_dest);

    let mut embedded = 0usize;
    for stem in manifest.png_stems() {
        let zip_path = DiscordMapIconManifest::miz_png_path(&stem);
        let dest = miz_root.join(zip_path.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create discord map png dir {:?}", parent))?;
        }
        let png_bytes = load_png(&png_dir, &stem)
            .with_context(|| format!("discord map icon {stem}"))?;
        fs::write(&dest, &png_bytes)
            .with_context(|| format!("write discord map png {:?}", dest))?;
        files.insert(MizString::from(zip_path.as_str()), dest);
        embedded += 1;
    }

    info!(
        "discord map icons: embedded {embedded} PNG(s) from {:?} + manifest (schema_version {}) under {MIZ_DIR}/",
        png_dir,
        SUPPORTED_SCHEMA_VERSION
    );
    Ok(())
}

fn load_png(png_dir: &Path, stem: &str) -> Result<Vec<u8>> {
    let path = png_dir.join(format!("{stem}.png"));
    if !path.is_file() {
        bail!(
            "missing discord map icon {:?}; edit PNGs in {}png/<canvas_px>/ and rebuild the mission",
            path,
            ASSETS_REL
        );
    }
    fs::read(&path).with_context(|| format!("read discord map icon {:?}", path))
}
