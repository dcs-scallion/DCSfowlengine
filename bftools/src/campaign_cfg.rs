//! Optional Fowl campaign JSON subset: `default_warehouse_*` keys (same names as `bfprotocols::Cfg`).
//!
//! wsType → cfg bucket: DCS weapon branch uses `level1 == 4`; `level2` follows in-game weapon families.
//! Missile AA vs AG split uses `level3` heuristics (tune against `Warehouse.getResourceMap` if counts look wrong).

use anyhow::{bail, Context, Result};
use mlua::Table;
use serde_derive::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WarehouseDefaultsFromCfg {
    #[serde(default, rename = "default_warehouse_AAmissiles")]
    pub aa_missiles: u32,
    #[serde(default, rename = "default_warehouse_AGmissiles")]
    pub ag_missiles: u32,
    #[serde(default, rename = "default_warehouse_AGrockets")]
    pub ag_rockets: u32,
    #[serde(default, rename = "default_warehouse_AGbombs")]
    pub ag_bombs: u32,
    #[serde(default, rename = "default_warehouse_AGguidedbombs")]
    pub ag_guided_bombs: u32,
    #[serde(default, rename = "default_warehouse_Fueltanks")]
    pub fueltanks: u32,
    #[serde(default, rename = "Fueltanks_empty")]
    pub fueltanks_empty: bool,
    #[serde(default, rename = "default_warehouse_Misc")]
    pub misc: u32,
}

impl WarehouseDefaultsFromCfg {
    /// Avoid applying all-zero caps when campaign JSON omits `default_warehouse_*` keys.
    pub fn has_any_nonzero_cap(&self) -> bool {
        self.aa_missiles != 0
            || self.ag_missiles != 0
            || self.ag_rockets != 0
            || self.ag_bombs != 0
            || self.ag_guided_bombs != 0
            || self.fueltanks != 0
            || self.misc != 0
    }
}

/// Convenience: defaults only (same file as [`load_overlay`]).
#[allow(dead_code)]
pub fn load(path: &Path) -> Result<WarehouseDefaultsFromCfg> {
    Ok(load_overlay(path)?.defaults)
}

/// Optional `warehouse.{hub_max,airbase_max,fob_max,carrier_airbase_max}` from Fowl campaign JSON
/// (same file as `default_warehouse_*`). Also accepts those four keys at JSON root if `warehouse` is absent.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WarehouseMultipliersFromCfg {
    pub hub_max: Option<u32>,
    pub airbase_max: Option<u32>,
    pub fob_max: Option<u32>,
    pub carrier_airbase_max: Option<u32>,
}

pub struct CampaignWarehouseOverlay {
    pub defaults: WarehouseDefaultsFromCfg,
    pub warehouse_multipliers: Option<WarehouseMultipliersFromCfg>,
    pub campaign_decade: Option<String>,
    /// Which `default_warehouse_*` keys were missing from the CFG JSON entirely.
    /// This usually indicates a typo in the key name.
    pub missing_default_warehouse_keys: Vec<&'static str>,
}

pub const ALLOWED_CAMPAIGN_DECADES: &[&str] = &[
    "1940s", "1950s", "1960s", "1970s", "1980s", "1990s", "2000s", "2010s", "2020s",
    "2030s", "2040s", "2050s",
];

pub fn load_overlay(path: &Path) -> Result<CampaignWarehouseOverlay> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let v: serde_json::Value =
        serde_json::from_slice(&bytes).context("parse campaign cfg as JSON")?;
    let defaults: WarehouseDefaultsFromCfg = serde_json::from_value(v.clone())
        .context("decode default_warehouse_* from campaign cfg")?;

    let mut missing_default_warehouse_keys: Vec<&'static str> = Vec::new();
    for (json_key, _field_name) in [
        ("default_warehouse_AAmissiles", "aa_missiles"),
        ("default_warehouse_AGmissiles", "ag_missiles"),
        ("default_warehouse_AGrockets", "ag_rockets"),
        ("default_warehouse_AGbombs", "ag_bombs"),
        ("default_warehouse_AGguidedbombs", "ag_guided_bombs"),
        ("default_warehouse_Fueltanks", "fueltanks"),
        ("default_warehouse_Misc", "misc"),
    ] {
        if v.get(json_key).is_none() {
            missing_default_warehouse_keys.push(json_key);
        }
    }

    fn parse_u32_int_field(
        field: &'static str,
        node: &serde_json::Value,
    ) -> Result<Option<u32>> {
        let Some(num) = node.as_f64() else {
            // Not a JSON number => treat as "missing" for multiplier purposes.
            return Ok(None);
        };
        if !num.is_finite() {
            bail!("ERROR: warehouse.{field} must be a finite integer (got {num})");
        }
        let rounded = num.round();
        let frac = (num - rounded).abs();
        if frac > 1e-9 {
            bail!(
                "ERROR: warehouse.{field} cannot be loaded because it is not an integer (got {num}); fix CFG and rebuild."
            );
        }
        if rounded < 0.0 || rounded > (u32::MAX as f64) {
            bail!(
                "ERROR: warehouse.{field} is out of range for u32 (got {num}); fix CFG and rebuild."
            );
        }
        Ok(Some(rounded as u32))
    }

    // Multipliers are validated strictly as integers.
    // If `warehouse` object exists, we read from it; otherwise we accept keys at JSON root.
    let source = v.get("warehouse").unwrap_or(&v);
    let mut m = WarehouseMultipliersFromCfg::default();
    let mut any = false;
    for (field, slot) in [
        ("hub_max", &mut m.hub_max),
        ("airbase_max", &mut m.airbase_max),
        ("fob_max", &mut m.fob_max),
        ("carrier_airbase_max", &mut m.carrier_airbase_max),
    ] {
        let Some(node) = source.get(field) else {
            continue;
        };
        if let Some(x) = parse_u32_int_field(field, node)? {
            *slot = Some(x);
            any = true;
        }
    }

    let warehouse_multipliers = if any { Some(m) } else { None };
    let campaign_decade =
        v.get("campaign_decade").and_then(|n| n.as_str()).map(|s| s.to_string());
    Ok(CampaignWarehouseOverlay {
        defaults,
        warehouse_multipliers,
        campaign_decade,
        missing_default_warehouse_keys,
    })
}

fn guided_bomb_l3(l3: i32) -> bool {
    matches!(l3, 36 | 37 | 43 | 44 | 47 | 48)
}

/// `wsType` with `initialAmount > 0` only — use for warehouse allowlist seeding (zero-stock rows are not campaign ordnance).
pub fn collect_weapon_ws_types_positive_initial(
    row: &Table,
) -> Result<HashSet<[i32; 4]>> {
    let mut out = HashSet::new();
    let Ok(weapons) = row.raw_get::<_, Table>("weapons") else {
        return Ok(out);
    };
    for pair in weapons.clone().pairs::<mlua::Value, Table>() {
        let (_, w) = pair?;
        let Ok(wst) = w.raw_get::<_, Table>("wsType") else {
            continue;
        };
        let Some((a, b, c, d)) = read_ws_type4(&wst)? else {
            continue;
        };
        if a == 0 && b == 0 && c == 0 && d == 0 {
            continue;
        }
        let Ok(amt) = w.raw_get::<_, u32>("initialAmount") else {
            continue;
        };
        if amt > 0 {
            out.insert([a, b, c, d]);
        }
    }
    Ok(out)
}

/// Every non-zero `wsType` on a warehouse `weapons` table (`initialAmount` ignored).
pub fn collect_weapon_ws_types_row(row: &Table) -> Result<HashSet<[i32; 4]>> {
    let mut out = HashSet::new();
    let Ok(weapons) = row.raw_get::<_, Table>("weapons") else {
        return Ok(out);
    };
    for pair in weapons.clone().pairs::<mlua::Value, Table>() {
        let (_, w) = pair?;
        let Ok(wst) = w.raw_get::<_, Table>("wsType") else {
            continue;
        };
        let Some((a, b, c, d)) = read_ws_type4(&wst)? else {
            continue;
        };
        if a == 0 && b == 0 && c == 0 && d == 0 {
            continue;
        }
        out.insert([a, b, c, d]);
    }
    Ok(out)
}

fn read_ws_type4(wst: &Table) -> Result<Option<(i32, i32, i32, i32)>> {
    let Ok(l1) = wst.raw_get::<_, i32>(1) else {
        return Ok(None);
    };
    let Ok(l2) = wst.raw_get::<_, i32>(2) else {
        return Ok(None);
    };
    let l3 = wst.raw_get::<_, i32>(3).unwrap_or(0);
    let l4 = wst.raw_get::<_, i32>(4).unwrap_or(0);
    Ok(Some((l1, l2, l3, l4)))
}

fn cap_for_weapon_ws_type(
    l1: i32,
    l2: i32,
    l3: i32,
    caps: &WarehouseDefaultsFromCfg,
) -> Option<u32> {
    if l1 == 1 && l2 == 3 {
        return Some(caps.fueltanks);
    }
    if l1 != 4 {
        return None;
    }
    match l2 {
        4 => {
            // DCS uses mixed l3 values under l2=4; keep AG missiles on l3=32 and treat others as AA.
            if l3 == 32 || l3 == 8 {
                Some(caps.ag_missiles)
            } else if l3 > 0 {
                Some(caps.aa_missiles)
            } else {
                Some(caps.misc)
            }
        }
        5 => Some(if caps.ag_guided_bombs > 0 && guided_bomb_l3(l3) {
            caps.ag_guided_bombs
        } else {
            caps.ag_bombs
        }),
        6 => Some(caps.ag_bombs),
        7 => Some(caps.ag_rockets),
        _ => Some(caps.misc),
    }
}

/// Sets `initialAmount` on `weapons` entries from `default_warehouse_*` (BDEFAULT/RDEFAULT rows only).
pub fn apply_default_counts_to_weapons(
    row: &Table,
    caps: &WarehouseDefaultsFromCfg,
) -> Result<()> {
    let Ok(weapons) = row.raw_get::<_, Table>("weapons") else {
        return Ok(());
    };
    for pair in weapons.clone().pairs::<mlua::Value, Table>() {
        let (_, w) = pair?;
        let Ok(wst) = w.raw_get::<_, Table>("wsType") else {
            continue;
        };
        let Some((l1, l2, l3, _)) = read_ws_type4(&wst)? else {
            continue;
        };
        if let Some(n) = cap_for_weapon_ws_type(l1, l2, l3, caps) {
            w.raw_set("initialAmount", n)?;
        }
    }
    Ok(())
}

/// After BINVENTORY/RINVENTORY scaling, fill `initialAmount` from cfg only where amount is still 0
/// (e.g. production inventory had no stock for that wsType).
pub fn fill_zero_weapon_amounts_from_cfg(
    row: &Table,
    caps: &WarehouseDefaultsFromCfg,
    mult: u32,
) -> Result<()> {
    if !caps.has_any_nonzero_cap() {
        return Ok(());
    }
    let m = mult.max(1);
    let Ok(weapons) = row.raw_get::<_, Table>("weapons") else {
        return Ok(());
    };
    for pair in weapons.clone().pairs::<mlua::Value, Table>() {
        let (_, w) = pair?;
        let Ok(cur) = w.raw_get::<_, u32>("initialAmount") else {
            continue;
        };
        if cur != 0 {
            continue;
        }
        let Ok(wst) = w.raw_get::<_, Table>("wsType") else {
            continue;
        };
        let Some((l1, l2, l3, _)) = read_ws_type4(&wst)? else {
            continue;
        };
        if let Some(n) = cap_for_weapon_ws_type(l1, l2, l3, caps) {
            if n != 0 {
                w.raw_set("initialAmount", n.saturating_mul(m))?;
            }
        }
    }
    Ok(())
}

/// Rows cloned from BDEFAULT/RDEFAULT carry raw cfg caps; multiply by Fowl warehouse capacity when still at baseline.
pub fn scale_weapon_amounts_matching_cfg_cap(
    row: &Table,
    caps: &WarehouseDefaultsFromCfg,
    mult: u32,
) -> Result<()> {
    if !caps.has_any_nonzero_cap() || mult <= 1 {
        return Ok(());
    }
    let Ok(weapons) = row.raw_get::<_, Table>("weapons") else {
        return Ok(());
    };
    for pair in weapons.clone().pairs::<mlua::Value, Table>() {
        let (_, w) = pair?;
        let Ok(wst) = w.raw_get::<_, Table>("wsType") else {
            continue;
        };
        let Some((l1, l2, l3, _)) = read_ws_type4(&wst)? else {
            continue;
        };
        let Some(cap) = cap_for_weapon_ws_type(l1, l2, l3, caps) else {
            continue;
        };
        if cap == 0 {
            continue;
        }
        let Ok(cur) = w.raw_get::<_, u32>("initialAmount") else {
            continue;
        };
        if cur == cap {
            w.raw_set("initialAmount", cap.saturating_mul(mult))?;
        }
    }
    Ok(())
}
