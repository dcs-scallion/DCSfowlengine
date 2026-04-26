use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use serde_derive::Serialize;
use std::path::PathBuf;

mod campaign_cfg;
mod mission_edit;
mod payload_allowlist;
mod weapon_bridge;

#[derive(Args, Clone, Debug, Serialize)]
struct MizCmd {
    /// the final miz file to output
    #[clap(long)]
    output: PathBuf,
    /// the base mission file
    #[clap(long)]
    base: PathBuf,
    /// the weapon template
    #[clap(long)]
    weapon: PathBuf,
    /// the options template
    //#[clap(long)]
    //options: PathBuf,
    /// With `--campaign-cfg`: optional path in the mission folder used only to locate `warehouse<campaign_decade>.miz` (defaults to the resolved weapon template’s directory). Without `--campaign-cfg`, do not pass; `warehouse.miz` is never loaded.
    #[clap(long)]
    warehouse: Option<PathBuf>,
    #[clap(long, default_value = "BINVENTORY")]
    blue_production_template: String,
    #[clap(long, default_value = "RINVENTORY")]
    red_production_template: String,
    /// Same role as Fowl `warehouse.airbase_max`: scaled stock = BINVENTORY/RINVENTORY base × this (non-hub).
    #[clap(long, default_value_t = 5)]
    warehouse_airbase_max: u32,
    /// Same role as Fowl `warehouse.hub_max`: scaled stock = base × this for logistics hubs (see --warehouse-hub-ids until auto-detection exists).
    #[clap(long, default_value_t = 25)]
    warehouse_hub_max: u32,
    /// Optional comma-separated keys in `warehouses["airports"]` that use hub_max (manual override; default empty = all use airbase_max).
    #[clap(long)]
    warehouse_hub_ids: Option<String>,
    /// Comma-separated `warehouses` table keys (FARP unitIds) that use fob_max (Fowl OFO FOBs).
    #[clap(long)]
    warehouse_fob_ids: Option<String>,
    /// Fowl campaign JSON (same file as DCS `*_CFG`): `default_warehouse_*`, optional `warehouse` multipliers; requires `campaign_decade` and paired `weapon<campaign_decade>.miz` + `warehouse<campaign_decade>.miz` next to the `--weapon` / `--warehouse` anchor paths. CLI overrides JSON only when JSON omits a field.
    #[clap(long)]
    campaign_cfg: Option<PathBuf>,
    /// Weapon bridge JSON from DCS hook (Fowl engine 2.0). If omitted: `fowl_weapon_bridge.json` next to `--weapon`, else newest `fowl_weapon_bridge-DCS.version.*.json` there.
    #[clap(long)]
    weapon_bridge: Option<PathBuf>,
    /// After a successful build, overwrite the resolved `warehouse<campaign_decade>.miz` on disk: copy final generated `weapons` from BDEFAULT/RDEFAULT into that template (ME reference only; edit ordnance policy in `weapon*.miz` + rebuild).
    #[clap(long, default_value_t = false)]
    write_back_warehouse_defaults: bool,
}

#[derive(Subcommand, Clone, Debug, Serialize)]
enum Tools {
    Miz(MizCmd),
}

#[derive(Parser)]
struct BftoolsArgs {
    #[clap(subcommand)]
    tool: Tools,
}

fn main() -> Result<()> {
    let bftools_args = BftoolsArgs::parse();
    env_logger::init();

    match bftools_args.tool {
        Tools::Miz(cfg) => mission_edit::run(&cfg)?,
    };
    Ok(())
}
