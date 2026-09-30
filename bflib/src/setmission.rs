//! Spawn external `setmission` after round end: one unpack/inject/repack for
//! ME datetime and/or static weather in the on-disk `.miz`.

use anyhow::{anyhow, bail, Context, Result};
use bfprotocols::cfg::{
    MissionDateOnNewCampaign, SetMissionCfg, WeatherProfileCfg, WeatherValueRange,
};
use chrono::NaiveDate;
use log::{info, warn};
use rand::Rng;
use regex::Regex;
use serde::Serialize;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use zip::ZipArchive;

static SPAWNED_THIS_MISSION: AtomicBool = AtomicBool::new(false);

pub fn reset_spawn_state() {
    SPAWNED_THIS_MISSION.store(false, Ordering::Release);
}

pub struct SpawnArgs<'a> {
    pub cfg: &'a SetMissionCfg,
    pub campaign_stats_enabled: bool,
    pub campaign_rounds: u32,
    pub rounds_per_day: u32,
    pub miz_path: &'a Path,
    /// Admin campaign wipe / round reset (next mission is a new campaign).
    pub new_campaign: bool,
    /// Persisted weather rung; `None` means use CFG `weather_start_index`.
    pub weather_index: Option<u32>,
}

pub struct SpawnOutcome {
    /// Updated weather index when weather is enabled (always set in that case).
    pub weather_index: Option<u32>,
}

/// Detached spawn after MissionEnd / admin shutdown (does not block).
pub fn maybe_spawn(args: SpawnArgs<'_>) -> Result<Option<SpawnOutcome>> {
    if SPAWNED_THIS_MISSION.load(Ordering::Acquire) {
        return Ok(None);
    }
    let cfg = args.cfg;
    let datetime = &cfg.setmissionstartdatetime;
    let weather = &cfg.setmissionweather;

    if !datetime.enabled && !weather.enabled {
        return Ok(None);
    }

    let bat = cfg
        .skript_path
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("setmission enabled but skript_path empty"))?;
    if !Path::new(bat).is_file() {
        warn!("setmission.skript_path not found, skipping: {bat}");
        return Ok(None);
    }
    if !(1..=1800).contains(&cfg.post_round_delay_secs) {
        warn!(
            "setmission.post_round_delay_secs {} out of range, skipping",
            cfg.post_round_delay_secs
        );
        return Ok(None);
    }
    if !args.miz_path.is_file() {
        warn!(
            "setmission: mission .miz not found, skipping: {:?}",
            args.miz_path
        );
        return Ok(None);
    }

    let date_arg: String;
    let time_arg: String;
    if datetime.enabled {
        if datetime.mission_start_time_cycle.is_empty() {
            warn!("setmission.setmissionstartdatetime.mission_start_time_cycle empty, skipping");
            return Ok(None);
        }
        let next_round = if args.new_campaign {
            1
        } else {
            args.campaign_rounds.saturating_add(1).max(1)
        };
        let cycle = &datetime.mission_start_time_cycle;
        let time_idx = (next_round.saturating_sub(1) as usize) % cycle.len();
        time_arg = cycle[time_idx].trim().to_string();
        date_arg = if args.campaign_stats_enabled {
            compute_next_mission_date(cfg, &args, next_round)?
        } else {
            String::from("keep-date")
        };
    } else {
        date_arg = String::from("keep-date");
        time_arg = String::from("keep-time");
    }

    let mut rng = rand::thread_rng();
    let mut weather_json_path: Option<PathBuf> = None;
    let mut new_weather_index: Option<u32> = None;

    if weather.enabled {
        if weather.profiles.len() < 3 {
            warn!("setmission.setmissionweather.profiles < 3, skipping weather");
        } else {
            let start = weather.weather_start_index.min(weather.profiles.len() - 1);
            let current = if args.new_campaign {
                start
            } else {
                args.weather_index
                    .map(|i| i as usize)
                    .filter(|&i| i < weather.profiles.len())
                    .unwrap_or(start)
            };
            let next = vote_neighbor_index(current, &weather.profiles, &mut rng);
            new_weather_index = Some(next as u32);
            if next != current {
                let patch = build_weather_patch(&weather.profiles[next], &mut rng)?;
                let path = write_weather_patch_file(&patch)?;
                let label = weather.profiles[next]
                    .id
                    .as_deref()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| weather.profiles[next].preset.to_string());
                info!("setmission weather: {} -> {} ({})", current, next, label);
                weather_json_path = Some(path);
            } else {
                info!("setmission weather: stay at index {current}");
            }
        }
    }

    // Nothing to write into the .miz
    if date_arg == "keep-date" && time_arg == "keep-time" && weather_json_path.is_none() {
        SPAWNED_THIS_MISSION.store(true, Ordering::Release);
        return Ok(Some(SpawnOutcome {
            weather_index: new_weather_index,
        }));
    }

    spawn_detached(
        bat,
        cfg.post_round_delay_secs,
        &date_arg,
        &time_arg,
        args.miz_path,
        weather_json_path.as_deref(),
    )?;
    SPAWNED_THIS_MISSION.store(true, Ordering::Release);
    Ok(Some(SpawnOutcome {
        weather_index: new_weather_index,
    }))
}

fn compute_next_mission_date(
    cfg: &SetMissionCfg,
    args: &SpawnArgs<'_>,
    next_round: u32,
) -> Result<String> {
    let datetime = &cfg.setmissionstartdatetime;
    let miz_date = read_miz_mission_date(args.miz_path)
        .with_context(|| format!("read ME date from {:?}", args.miz_path))?;
    let base = if let Some(ref s) = datetime.mission_date_base {
        NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
            .map_err(|e| anyhow!("mission_date_base: {e}"))?
    } else {
        miz_date
    };

    let date = if args.new_campaign {
        match datetime.mission_date_on_new_campaign {
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

fn vote_neighbor_index(
    current: usize,
    profiles: &[WeatherProfileCfg],
    rng: &mut impl Rng,
) -> usize {
    let n = profiles.len();
    let mut cands: Vec<(usize, f64)> = Vec::with_capacity(3);
    cands.push((current, profiles[current].weight));
    if current > 0 {
        cands.push((current - 1, profiles[current - 1].weight));
    }
    if current + 1 < n {
        cands.push((current + 1, profiles[current + 1].weight));
    }
    let total: f64 = cands.iter().map(|(_, w)| *w).sum();
    if total <= 0.0 {
        return current;
    }
    let mut pick = rng.gen_range(0.0..total);
    for (idx, w) in cands {
        if pick < w {
            return idx;
        }
        pick -= w;
    }
    current
}

fn sample_range(r: &WeatherValueRange, rng: &mut impl Rng) -> Result<f64> {
    if r.min > r.max {
        bail!("invalid range min>max");
    }
    let v = if (r.max - r.min).abs() < f64::EPSILON {
        r.min
    } else {
        rng.gen_range(r.min..=r.max)
    };
    let step = 10f64.powi(r.round as i32);
    let rounded = (v / step).round() * step;
    Ok(rounded.clamp(r.min, r.max))
}

fn sample_dir_deg(r: &WeatherValueRange, rng: &mut impl Rng) -> Result<u32> {
    let v = sample_range(r, rng)?;
    let mut d = v.round() as i64 % 360;
    if d < 0 {
        d += 360;
    }
    Ok(d as u32)
}

#[derive(Debug, Serialize)]
struct WeatherWindLayerPatch {
    speed: f64,
    dir: u32,
}

#[derive(Debug, Serialize)]
struct WeatherWindPatch {
    #[serde(rename = "atGround")]
    at_ground: WeatherWindLayerPatch,
    #[serde(rename = "at2000")]
    at_2000: WeatherWindLayerPatch,
    #[serde(rename = "at8000")]
    at_8000: WeatherWindLayerPatch,
}

#[derive(Debug, Serialize)]
struct WeatherPatch {
    atmosphere_type: u8,
    preset: String,
    base: f64,
    temperature: f64,
    qnh: f64,
    wind: WeatherWindPatch,
}

fn build_weather_patch(profile: &WeatherProfileCfg, rng: &mut impl Rng) -> Result<WeatherPatch> {
    Ok(WeatherPatch {
        atmosphere_type: 0,
        preset: profile.preset.to_string(),
        base: sample_range(&profile.base_m, rng)?,
        temperature: sample_range(&profile.temperature_c, rng)?,
        qnh: sample_range(&profile.qnh_mmhg, rng)?,
        wind: WeatherWindPatch {
            at_ground: WeatherWindLayerPatch {
                speed: sample_range(&profile.wind.at_ground.speed_ms, rng)?,
                dir: sample_dir_deg(&profile.wind.at_ground.dir_deg, rng)?,
            },
            at_2000: WeatherWindLayerPatch {
                speed: sample_range(&profile.wind.at_2000.speed_ms, rng)?,
                dir: sample_dir_deg(&profile.wind.at_2000.dir_deg, rng)?,
            },
            at_8000: WeatherWindLayerPatch {
                speed: sample_range(&profile.wind.at_8000.speed_ms, rng)?,
                dir: sample_dir_deg(&profile.wind.at_8000.dir_deg, rng)?,
            },
        },
    })
}

fn write_weather_patch_file(patch: &WeatherPatch) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!(
        "fowl_setmission_weather_{}_{}.json",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ));
    let mut f = File::create(&path).with_context(|| format!("create {:?}", path))?;
    serde_json::to_writer_pretty(&mut f, patch).context("write weather patch json")?;
    f.flush()?;
    Ok(path)
}

#[cfg(windows)]
fn spawn_detached(
    bat: &str,
    delay_secs: u32,
    date_arg: &str,
    time_arg: &str,
    miz_path: &Path,
    weather_json: Option<&Path>,
) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::Command;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    let delay = delay_secs.to_string();
    let miz = miz_path
        .to_str()
        .ok_or_else(|| anyhow!("miz path is not valid UTF-8"))?;
    let weather = weather_json
        .map(|p| {
            p.to_str()
                .map(|s| s.to_string())
                .ok_or_else(|| anyhow!("weather json path is not valid UTF-8"))
        })
        .transpose()?
        .unwrap_or_else(|| String::from("none"));

    let mut cmd = Command::new(bat);
    cmd.arg("bflib")
        .arg(&delay)
        .arg(date_arg)
        .arg(time_arg)
        .arg(miz)
        .arg(&weather);
    let child = cmd
        .creation_flags(
            CREATE_NO_WINDOW
                | CREATE_BREAKAWAY_FROM_JOB
                | DETACHED_PROCESS
                | CREATE_NEW_PROCESS_GROUP,
        )
        .spawn()
        .map_err(|e| anyhow!("spawn {bat}: {e}"))?;
    info!(
        "setmission: spawned {bat} delay={delay_secs}s date={date_arg} time={time_arg} weather={weather} miz={miz} (pid {})",
        child.id()
    );
    Ok(())
}

#[cfg(not(windows))]
fn spawn_detached(
    bat: &str,
    _delay_secs: u32,
    _date_arg: &str,
    _time_arg: &str,
    _miz_path: &Path,
    _weather_json: Option<&Path>,
) -> Result<()> {
    let _ = bat;
    anyhow::bail!("setmission spawn is only supported on Windows DCS hosts")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bfprotocols::cfg::{WeatherValueRange, WeatherWindCfg, WeatherWindLayerCfg};

    fn profile(weight: f64) -> WeatherProfileCfg {
        let r = WeatherValueRange {
            min: 1.0,
            max: 2.0,
            round: 0,
        };
        let layer = WeatherWindLayerCfg {
            speed_ms: r.clone(),
            dir_deg: WeatherValueRange {
                min: 0.0,
                max: 90.0,
                round: 1,
            },
        };
        WeatherProfileCfg {
            id: None,
            preset: "Preset1".into(),
            weight,
            base_m: r.clone(),
            temperature_c: r.clone(),
            qnh_mmhg: WeatherValueRange {
                min: 750.0,
                max: 760.0,
                round: 0,
            },
            wind: WeatherWindCfg {
                at_ground: layer.clone(),
                at_2000: layer.clone(),
                at_8000: layer,
            },
        }
    }

    #[test]
    fn neighbor_vote_stays_in_bounds() {
        let profiles = vec![profile(40.0), profile(30.0), profile(20.0)];
        let mut rng = rand::thread_rng();
        for _ in 0..50 {
            let n = vote_neighbor_index(0, &profiles, &mut rng);
            assert!(n == 0 || n == 1);
            let n = vote_neighbor_index(2, &profiles, &mut rng);
            assert!(n == 1 || n == 2);
        }
    }

    #[test]
    fn sample_round_hundreds() {
        let mut rng = rand::thread_rng();
        let r = WeatherValueRange {
            min: 1300.0,
            max: 1500.0,
            round: 2,
        };
        for _ in 0..30 {
            let v = sample_range(&r, &mut rng).unwrap();
            assert!((1300.0..=1500.0).contains(&v));
            assert_eq!(v % 100.0, 0.0);
        }
    }
}
