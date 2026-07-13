//! Embed campaign CFG sound files into the assembled `.miz` (`sounds/<basename>`).

use anyhow::{Context, Result};
use dcso3::String as MizString;
use log::{info, warn};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub const MIZ_SOUNDS_PREFIX: &str = "sounds";

pub fn load_from_campaign_cfg(path: &Path) -> Result<(HashMap<String, String>, HashMap<String, String>)> {
    let bytes = fs::read(path).with_context(|| format!("read campaign cfg {}", path.display()))?;
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).context("parse campaign cfg JSON for sounds")?;
    Ok((
        parse_sound_map(v.get("sounds_player")),
        parse_sound_map(v.get("sounds_all")),
    ))
}

fn parse_sound_map(node: Option<&serde_json::Value>) -> HashMap<String, String> {
    node.and_then(|n| serde_json::from_value(n.clone()).ok())
        .unwrap_or_default()
}

pub fn embed_into_miz(
    miz_root: &Path,
    files: &mut HashMap<MizString, PathBuf>,
    anchor: &Path,
    campaign_cfg: &Path,
) -> Result<(HashMap<String, String>, HashMap<String, String>)> {
    let (player, all) = load_from_campaign_cfg(campaign_cfg)?;
    if player.is_empty() && all.is_empty() {
        return Ok((HashMap::new(), HashMap::new()));
    }
    let mut cache: HashMap<String, Option<String>> = HashMap::new();
    let resolved_player = resolve_keys(miz_root, files, anchor, &player, &mut cache)?;
    let resolved_all = resolve_keys(miz_root, files, anchor, &all, &mut cache)?;
    info!(
        "sounds: embedded {} file(s); resolved player={} all={}",
        cache.values().filter(|p| p.is_some()).count(),
        resolved_player.len(),
        resolved_all.len()
    );
    Ok((resolved_player, resolved_all))
}

fn resolve_keys(
    miz_root: &Path,
    files: &mut HashMap<MizString, PathBuf>,
    anchor: &Path,
    keys: &HashMap<String, String>,
    cache: &mut HashMap<String, Option<String>>,
) -> Result<HashMap<String, String>> {
    let mut resolved = HashMap::new();
    for (key, src) in keys {
        let miz_path = cache
            .entry(src.clone())
            .or_insert_with(|| embed_one(miz_root, files, anchor, key, src).ok());
        if let Some(path) = miz_path {
            resolved.insert(key.clone(), path.clone());
        }
    }
    Ok(resolved)
}

fn embed_one(
    miz_root: &Path,
    files: &mut HashMap<MizString, PathBuf>,
    anchor: &Path,
    key: &str,
    src: &str,
) -> Result<String> {
    let source = resolve_source_path(anchor, src).with_context(|| {
        format!("sounds: missing source for key {key:?} ({src})")
    })?;
    let file_name = source
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid sound file name {:?}", source))?;
    let zip_path = format!("{MIZ_SOUNDS_PREFIX}/{file_name}");
    let dest = miz_root.join(zip_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create sounds dir {:?}", parent))?;
    }
    fs::copy(&source, &dest)
        .with_context(|| format!("copy sound {:?} -> {:?}", source, dest))?;
    files.insert(MizString::from(zip_path.as_str()), dest);
    Ok(zip_path)
}

fn resolve_source_path(anchor: &Path, cfg_path: &str) -> Result<PathBuf> {
    let rel = cfg_path.replace('\\', "/");
    let start = anchor
        .canonicalize()
        .unwrap_or_else(|_| anchor.to_path_buf());
    let mut cur = if start.is_dir() {
        start
    } else {
        start
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(start)
    };
    for _ in 0..16 {
        let candidate = cur.join(&rel);
        if candidate.is_file() {
            return Ok(candidate);
        }
        let candidate = cur.join(rel.trim_start_matches("assets/"));
        if candidate.is_file() {
            return Ok(candidate);
        }
        cur = match cur.parent() {
            Some(p) => p.to_path_buf(),
            None => break,
        };
    }
    warn!("sounds: file not found for path {cfg_path:?} (walked up from {anchor:?})");
    Err(anyhow::anyhow!("sound file not found: {cfg_path}"))
}
