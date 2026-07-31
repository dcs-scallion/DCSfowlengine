//! Writedir housekeeping at mission start (`server_maintenance` CFG).

use bfprotocols::cfg::ServerMaintenanceCfg;
use log::{info, warn};
use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime},
};

pub fn run(writedir: &Path, cfg: &ServerMaintenanceCfg) {
    if let Some(days) = cfg.tracks_multiplayer_retain_days {
        // CFG 0 → 1 day minimum (never "delete all").
        let days = days.max(1);
        cleanup_by_age(
            &writedir.join("Tracks").join("Multiplayer"),
            days,
            &["trk"],
            "Tracks/Multiplayer",
        );
    }
    if let Some(days) = cfg.logs_multiplayer_retain_days {
        let days = days.max(1);
        cleanup_by_age(&writedir.join("Logs"), days, &["txt", "log"], "Logs");
    }
}

fn cleanup_by_age(dir: &Path, retain_days: u32, extensions: &[&str], label: &str) {
    if !dir.is_dir() {
        info!("server_maintenance: {label} missing ({:?}), skip", dir);
        return;
    }
    let Some(cutoff) =
        SystemTime::now().checked_sub(Duration::from_secs(u64::from(retain_days) * 86_400))
    else {
        warn!("server_maintenance: retain_days={retain_days} underflow, skip");
        return;
    };
    let mut deleted = 0u32;
    let mut skipped = 0u32;
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            warn!("server_maintenance: cannot read {:?}: {e}", dir);
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                warn!("server_maintenance: dir entry error in {:?}: {e}", dir);
                continue;
            }
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let ext_ok = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| {
                extensions
                    .iter()
                    .any(|want| e.eq_ignore_ascii_case(want))
            });
        if !ext_ok {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                warn!("server_maintenance: metadata {:?}: {e}", path);
                skipped += 1;
                continue;
            }
        };
        let mtime = match meta.modified() {
            Ok(t) => t,
            Err(e) => {
                warn!("server_maintenance: mtime {:?}: {e}", path);
                skipped += 1;
                continue;
            }
        };
        if mtime >= cutoff {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => deleted += 1,
            Err(e) => {
                warn!("server_maintenance: delete {:?}: {e}", path);
                skipped += 1;
            }
        }
    }
    info!(
        "server_maintenance: {label} retain_days={retain_days} deleted={deleted} skipped={skipped} ({:?})",
        dir
    );
}
