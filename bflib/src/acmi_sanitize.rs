use anyhow::{bail, Result};
use bfprotocols::cfg::AcmiSanitizeCfg;
use log::{info, warn};
use std::path::Path;

/// Spawn process_inbox.bat after round end (detached; does not block shutdown).
pub fn maybe_spawn(cfg: &AcmiSanitizeCfg) -> Result<()> {
    let Some(bat) = cfg
        .process_inbox_bat
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    else {
        return Ok(());
    };
    if !Path::new(bat).is_file() {
        warn!("acmi_sanitize.process_inbox_bat not found, skipping: {bat}");
        return Ok(());
    }
    if !(1..=1800).contains(&cfg.post_round_delay_secs) {
        warn!(
            "acmi_sanitize.post_round_delay_secs {} out of range 1-1800, skipping spawn",
            cfg.post_round_delay_secs
        );
        return Ok(());
    }
    spawn_detached(bat, cfg.post_round_delay_secs)
}

#[cfg(windows)]
fn spawn_detached(bat: &str, delay_secs: u32) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let delay = delay_secs.to_string();
    let child = Command::new("cmd")
        .args(["/C", bat, "bflib", &delay, "scheduled"])
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn {bat}: {e}"))?;
    info!(
        "acmi_sanitize: spawned {bat} bflib {delay_secs} scheduled (pid {})",
        child.id()
    );
    Ok(())
}

#[cfg(not(windows))]
fn spawn_detached(bat: &str, _delay_secs: u32) -> Result<()> {
    let _ = bat;
    bail!("acmi_sanitize spawn is only supported on Windows DCS hosts")
}
