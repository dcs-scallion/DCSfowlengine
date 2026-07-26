//! Spawn external `setmissionstartdatetime` after round end to patch ME date/time in the on-disk `.miz`.

use anyhow::{anyhow, Context, Result};
use bfprotocols::cfg::{MissionDateOnNewCampaign, SetMissionStartDatetimeCfg};
use chrono::NaiveDate;
use log::{info, warn};
use regex::Regex;
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use zip::ZipArchive;

static SPAWNED_THIS_MISSION: AtomicBool = AtomicBool::new(false);

pub fn reset_spawn_state() {
    SPAWNED_THIS_MISSION.store(false, Ordering::Release);
}

pub struct SpawnArgs<'a> {
    pub cfg: &'a SetMissionStartDatetimeCfg,
    pub campaign_stats_enabled: bool,
    pub campaign_rounds: u32,
    pub rounds_per_day: u32,
    pub miz_path: &'a Path,
    /// Admin campaign wipe / round reset (next mission is a new campaign).
    pub new_campaign: bool,
}

/// Detached spawn after MissionEnd / admin shutdown (does not block).
pub fn maybe_spawn(args: SpawnArgs<'_>) -> Result<()> {
    if SPAWNED_THIS_MISSION.load(Ordering::Acquire) {
        return Ok(());
    }
    let cfg = args.cfg;
    if !cfg.enabled {
        return Ok(());
    }
    let bat = cfg
        .skript_path
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("setmissionstartdatetime enabled but skript_path empty"))?;
    if !Path::new(bat).is_file() {
        warn!("setmissionstartdatetime.skript_path not found, skipping: {bat}");
        return Ok(());
    }
    if cfg.mission_start_time_cycle.is_empty() {
        warn!("setmissionstartdatetime.mission_start_time_cycle empty, skipping");
        return Ok(());
    }
    if !(1..=1800).contains(&cfg.post_round_delay_secs) {
        warn!(
            "setmissionstartdatetime.post_round_delay_secs {} out of range, skipping",
            cfg.post_round_delay_secs
        );
        return Ok(());
    }
    if !args.miz_path.is_file() {
        warn!(
            "setmissionstartdatetime: mission .miz not found, skipping: {:?}",
            args.miz_path
        );
        return Ok(());
    }

    let next_round = if args.new_campaign {
        1
    } else {
        args.campaign_rounds.saturating_add(1).max(1)
    };
    let cycle = &cfg.mission_start_time_cycle;
    let time_idx = (next_round.saturating_sub(1) as usize) % cycle.len();
    let time = cycle[time_idx].trim();

    let date = if args.campaign_stats_enabled {
        Some(compute_next_mission_date(cfg, &args, next_round)?)
    } else {
        None
    };

    spawn_detached(
        bat,
        cfg.post_round_delay_secs,
        date.as_deref(),
        time,
        args.miz_path,
    )?;
    SPAWNED_THIS_MISSION.store(true, Ordering::Release);
    Ok(())
}

fn compute_next_mission_date(
    cfg: &SetMissionStartDatetimeCfg,
    args: &SpawnArgs<'_>,
    next_round: u32,
) -> Result<String> {
    let miz_date = read_miz_mission_date(args.miz_path)
        .with_context(|| format!("read ME date from {:?}", args.miz_path))?;
    let base = if let Some(ref s) = cfg.mission_date_base {
        NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
            .map_err(|e| anyhow!("mission_date_base: {e}"))?
    } else {
        miz_date
    };

    let date = if args.new_campaign {
        match cfg.mission_date_on_new_campaign {
            MissionDateOnNewCampaign::Reset => base,
            MissionDateOnNewCampaign::Continue => miz_date,
        }
    } else {
        let rpd = args.rounds_per_day.max(1);
        let days = 1 + (next_round.saturating_sub(1)) / rpd;
        base.checked_add_signed(chrono::Duration::days((days.saturating_sub(1)) as i64))
            .ok_or_else(|| anyhow!("mission date overflow"))?
    };
    Ok(date.format("%Y-%m-%d").to_string())
}

fn read_miz_mission_date(miz_path: &Path) -> Result<NaiveDate> {
    let file = File::open(miz_path).with_context(|| format!("open {:?}", miz_path))?;
    let mut zip = ZipArchive::new(file).with_context(|| format!("zip {:?}", miz_path))?;
    let mut mission = zip
        .by_name("mission")
        .with_context(|| format!("mission member in {:?}", miz_path))?;
    let mut buf = String::new();
    std::io::Read::read_to_string(&mut mission, &mut buf)?;
    let year = capture_int(&buf, r#"\["Year"\]\s*=\s*(\d+)"#)?;
    let month = capture_int(&buf, r#"\["Month"\]\s*=\s*(\d+)"#)?;
    let day = capture_int(&buf, r#"\["Day"\]\s*=\s*(\d+)"#)?;
    NaiveDate::from_ymd_opt(year, month as u32, day as u32)
        .ok_or_else(|| anyhow!("invalid ME date {year}-{month}-{day}"))
}

fn capture_int(text: &str, pat: &str) -> Result<i32> {
    let re = Regex::new(pat)?;
    let caps = re
        .captures(text)
        .ok_or_else(|| anyhow!("pattern not found: {pat}"))?;
    caps.get(1)
        .ok_or_else(|| anyhow!("missing capture"))?
        .as_str()
        .parse()
        .map_err(|e| anyhow!("parse int: {e}"))
}

#[cfg(windows)]
fn spawn_detached(
    bat: &str,
    delay_secs: u32,
    date: Option<&str>,
    time: &str,
    miz_path: &Path,
) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    let delay = delay_secs.to_string();
    let miz = miz_path
        .to_str()
        .ok_or_else(|| anyhow!("miz path is not valid UTF-8"))?;
    let mut args = vec![
        "/C".into(),
        "start".into(),
        "/B".into(),
        "".into(),
        "cmd".into(),
        "/C".into(),
        bat.to_string(),
        "bflib".into(),
        delay,
    ];
    match date {
        Some(d) => {
            args.push(d.to_string());
            args.push(time.to_string());
        }
        None => {
            args.push("keep-date".into());
            args.push(time.to_string());
        }
    }
    args.push(miz.to_string());

    let child = Command::new("cmd")
        .args(&args)
        .creation_flags(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
        .map_err(|e| anyhow!("spawn {bat}: {e}"))?;
    info!(
        "setmissionstartdatetime: spawned {bat} delay={delay_secs}s date={:?} time={time} miz={miz} (launcher pid {})",
        date,
        child.id()
    );
    Ok(())
}

#[cfg(not(windows))]
fn spawn_detached(
    bat: &str,
    _delay_secs: u32,
    _date: Option<&str>,
    _time: &str,
    _miz_path: &Path,
) -> Result<()> {
    let _ = bat;
    anyhow::bail!("setmissionstartdatetime spawn is only supported on Windows DCS hosts")
}
