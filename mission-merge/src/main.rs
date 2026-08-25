//! Copy leftover groups and trigger zones from one DCS `mission` Lua table into another.
//! Standalone crate: no dependency on bftools, never reads or writes `warehouses`.

mod io;
mod merge;
mod relocate;
mod serialize;

use anyhow::{Context, Result};
use clap::Parser;
use log::{info, warn};
use mlua::{Lua, Value};
use std::path::PathBuf;

/// Copy leftover groups and trigger zones from a cleaned DCS mission into another map's mission.
/// Does not touch warehouses. Not part of bftools.
#[derive(Parser, Debug)]
#[command(name = "mission-merge", version)]
struct Args {
    /// Source `mission` file or `.miz` (only the `mission` entry is read).
    #[arg(long)]
    source: PathBuf,
    /// Destination `mission` file or `.miz` (theatre and other dest fields are kept).
    #[arg(long)]
    dest: PathBuf,
    /// Output path (same kind as `--dest`: `mission` Lua or `.miz`).
    #[arg(long)]
    output: PathBuf,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let source_lua = io::read_mission_lua(&args.source)
        .with_context(|| format!("reading --source {}", args.source.display()))?;
    let dest_lua = io::read_mission_lua(&args.dest)
        .with_context(|| format!("reading --dest {}", args.dest.display()))?;

    let lua = Lua::new();
    lua.gc_stop();

    let source = merge::load_mission_table(&lua, &source_lua, "source")?;
    let dest = merge::load_mission_table(&lua, &dest_lua, "dest")?;
    let stats = merge::merge_missions(&lua, source, dest.clone())?;

    info!(
        "source theatre: {:?}; dest theatre kept: {:?}",
        stats.source_theatre, stats.dest_theatre
    );
    info!(
        "copied {} group(s), {} unit(s), {} zone(s); created {} country record(s)",
        stats.groups, stats.units, stats.zones, stats.countries_created
    );
    if stats.clusters_moved > 0 || stats.clusters_kept > 0 {
        info!(
            "map fit: moved {} cluster(s) onto dest view, kept {} already on-map",
            stats.clusters_moved, stats.clusters_kept
        );
    }
    for w in &stats.warnings {
        warn!("{w}");
    }

    let serialized = serialize::serialize_mission(&Value::Table(dest))?;
    io::write_mission_output(&args.dest, &args.output, &serialized)
        .with_context(|| format!("writing --output {}", args.output.display()))?;
    info!("wrote {}", args.output.display());
    Ok(())
}
