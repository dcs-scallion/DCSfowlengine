use anyhow::Result;
use bfprotocols::cfg::AcmiSanitizeCfg;
use chrono::{SecondsFormat, Utc};
use log::{info, warn};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

static SPAWNED_THIS_MISSION: AtomicBool = AtomicBool::new(false);

/// Allow one spawn per loaded mission (reset on mission load).
pub fn reset_spawn_state() {
    SPAWNED_THIS_MISSION.store(false, Ordering::Release);
}

/// Spawn process_inbox.bat after round end or MissionEnd (detached; does not block shutdown).
pub fn maybe_spawn(cfg: &AcmiSanitizeCfg) -> Result<()> {
    if SPAWNED_THIS_MISSION.load(Ordering::Acquire) {
        return Ok(());
    }
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
    let shutdown_before = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    spawn_detached(bat, cfg.post_round_delay_secs, &shutdown_before)?;
    SPAWNED_THIS_MISSION.store(true, Ordering::Release);
    Ok(())
}

#[cfg(windows)]
fn spawn_detached(bat: &str, delay_secs: u32, shutdown_before: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    let delay = delay_secs.to_string();
    // `start /B` orphans the batch so DCS exit / bot kill does not tear down post_round_delay sleep.
    let child = Command::new("cmd")
        .args([
            "/C",
            "start",
            "/B",
            "",
            "cmd",
            "/C",
            bat,
            "bflib",
            &delay,
            shutdown_before,
            "scheduled",
        ])
        .creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn {bat}: {e}"))?;
    info!(
        "acmi_sanitize: spawned {bat} bflib {delay_secs} {shutdown_before} scheduled (launcher pid {})",
        child.id()
    );
    Ok(())
}

#[cfg(not(windows))]
fn spawn_detached(bat: &str, _delay_secs: u32, _shutdown_before: &str) -> Result<()> {
    let _ = bat;
    anyhow::bail!("acmi_sanitize spawn is only supported on Windows DCS hosts")
}
