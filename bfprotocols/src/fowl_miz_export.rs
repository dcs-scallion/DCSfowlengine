//! Build-time JSON written beside the output `.miz`; runtime loads from the same folder as `*_CFG`
//! (`writedir` / sortie stem, e.g. `Saved Games\DCS\<sortie>_fowl_export.json`).
//!
//! **Allowlist source (FowlTools):** with weapon bridge + payload templates, **all** allowed `wsType`
//! quads for that side (same set as rebuilt **BDEFAULT**/**RDEFAULT** / inventory validation). Without
//! a bridge, falls back to **BINVENTORY**/**RINVENTORY** rows with **`initialAmount > 0`** only.

use anyhow::{bail, Context as AnyhowContext, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ObjectiveWarehouseDefaults {
    /// Aircraft module types allowed for blue ownership on this objective/base.
    #[serde(default)]
    pub blue_aircraft: Vec<String>,
    /// Aircraft module types allowed for red ownership on this objective/base.
    #[serde(default)]
    pub red_aircraft: Vec<String>,
    /// Allowed weapon `wsType` quads for blue ownership on this objective/base.
    #[serde(default)]
    pub blue_weapon_ws: Vec<[i32; 4]>,
    /// Allowed weapon `wsType` quads for red ownership on this objective/base.
    #[serde(default)]
    pub red_weapon_ws: Vec<[i32; 4]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FowlMizExport {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// True when FowlTools had weapon template + bridge (payload/bridge union used for inventory validation).
    #[serde(default)]
    pub weapon_bridge_used: bool,
    /// Allowed DCS `wsType` quads (weapon rows) for blue coalition from payload + bridge. Empty: **bflib** mirrors [`Self::red_weapon_ws`] when that is non-empty; otherwise no blue filter.
    #[serde(default)]
    pub blue_weapon_ws: Vec<[i32; 4]>,
    /// Allowed DCS `wsType` quads for red coalition.
    #[serde(default)]
    pub red_weapon_ws: Vec<[i32; 4]>,
    /// Optional per-objective warehouse defaults precomputed by FowlTools.
    /// Key is objective name (from `O*` trigger zones without prefix, e.g. `Kobuleti`).
    #[serde(default)]
    pub objective_defaults: HashMap<String, ObjectiveWarehouseDefaults>,
}

fn default_schema_version() -> u32 {
    3
}

impl Default for FowlMizExport {
    fn default() -> Self {
        Self {
            schema_version: 3,
            weapon_bridge_used: false,
            blue_weapon_ws: Vec::new(),
            red_weapon_ws: Vec::new(),
            objective_defaults: HashMap::new(),
        }
    }
}

impl FowlMizExport {
    /// Same parent directory as [`crate::cfg::Cfg::path`]`(miz_state_path)` (`*_CFG`), file `{sortie}_fowl_export.json`.
    pub fn path(miz_state_path: &Path) -> PathBuf {
        let stem = miz_state_path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "mission".into());
        let parent = miz_state_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        parent.join(format!("{stem}_fowl_export.json"))
    }

    /// Path next to the assembled mission file (`<stem>.miz` → `<stem>_fowl_export.json`).
    pub fn path_next_to_miz(output_miz: &Path) -> Result<PathBuf> {
        let stem = output_miz
            .file_stem()
            .and_then(|s| s.to_str())
            .with_context(|| format!("output .miz path has no file stem: {:?}", output_miz))?;
        Ok(output_miz.with_file_name(format!("{stem}_fowl_export.json")))
    }

    pub fn write_next_to_miz(&self, output_miz: &Path) -> Result<PathBuf> {
        let path = Self::path_next_to_miz(output_miz)?;
        let mut f = File::create(&path).with_context(|| format!("create {:?}", path))?;
        serde_json::to_writer_pretty(&mut f, self).with_context(|| format!("write {:?}", path))?;
        f.write_all(b"\n")?;
        Ok(path)
    }

    /// `Ok(None)` if the file is missing; parse errors propagate.
    pub fn load_if_present(miz_state_path: &Path) -> Result<Option<Self>> {
        let path = Self::path(miz_state_path);
        if !path.exists() {
            return Ok(None);
        }
        let file = File::open(&path).with_context(|| format!("open {:?}", path))?;
        let v = serde_json::from_reader(file).with_context(|| format!("decode {:?}", path))?;
        Ok(Some(v))
    }

    /// Required at mission start. Missing or invalid JSON fails load.
    pub fn load_required(miz_state_path: &Path) -> Result<Self> {
        let path = Self::path(miz_state_path);
        if !path.exists() {
            bail!(
                "Fowl: required mission export file is missing: {}\n\
                 Expected: <sortie_stem>_fowl_export.json next to <sortie_stem>_CFG in Saved Games\\DCS (sortie_stem is from Mission Editor / l10n dictionary for mission.sortie, not the .miz file name by itself).\n\
                 FowlTools sets dictionary[ sortie key ] to the --output .miz stem; rebuild the mission with FowlTools if you renamed the output file.\n\
                 This file is required to filter warehouse stock for Fowl logistics.",
                path.display()
            );
        }
        let file = File::open(&path).with_context(|| format!("open {:?}", path))?;
        serde_json::from_reader(file).with_context(|| format!("decode {:?}", path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn path_uses_sortie_stem_next_to_writedir() {
        let miz_state = Path::new(r"C:\Users\x\Saved Games\DCS\Caucasus1987");
        assert_eq!(
            FowlMizExport::path(miz_state),
            Path::new(r"C:\Users\x\Saved Games\DCS\Caucasus1987_fowl_export.json")
        );
    }
}
