//! Načtení exportu z DCS hooku (Fowl engine 2.0): `fowl_weapon_bridge.json` nebo
//! `fowl_weapon_bridge-DCS.version.*.json` — mapa deskriptor → wsType.
//! Hook dumpuje prakticky celé `db.Weapons` (katalog DCS), ne allowlist z warehouse šablon;
//! Payload allowlist: pylons vs blocked `restricted`, coalition vote (`mission_edit.rs`).

use anyhow::{Context, Result};
use serde_derive::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

const ZERO_WS: [i32; 4] = [0, 0, 0, 0];

fn is_fueltank_ws(ws: [i32; 4]) -> bool {
    ws[0] == 1 && ws[1] == 3
}

fn contains_empty_token(s: &str) -> bool {
    s.to_ascii_lowercase().contains("empty")
}

fn normalize_dash_chars(c: char) -> char {
    match c {
        '\u{2010}'..='\u{2015}' | '\u{2212}' | '\u{FE58}' | '\u{FE63}' | '\u{FF0D}' => '-',
        _ => c,
    }
}

fn normalize_weapon_descriptor_str(s: &str) -> String {
    s.chars().map(normalize_dash_chars).collect()
}

/// Brace-inner payload CLSID as `_`+upper needle for substring match against normalized bridge keys.
fn descriptor_substring_needle(descriptor: &str) -> Option<(String, bool)> {
    let s = normalize_weapon_descriptor_str(descriptor.trim());
    let is_braced = s.starts_with('{') && s.ends_with('}') && s.len() >= 2;
    let inner = if is_braced { &s[1..s.len() - 1] } else { s.as_str() };
    // Non-braced fallback: only weapon-like tokens, avoid generic payload keys.
    if !is_braced {
        let has_alpha = inner.chars().any(|c| c.is_ascii_alphabetic());
        let has_digit = inner.chars().any(|c| c.is_ascii_digit());
        let has_sep = inner.contains('-') || inner.contains('_');
        if !(has_alpha && has_digit && has_sep) {
            return None;
        }
    }
    let t = normalize_weapon_descriptor_str(inner)
        .replace('-', "_")
        .to_uppercase();
    (t.len() >= 4).then_some((t, is_braced))
}

fn normalized_bridge_key_upper(k: &str) -> String {
    normalize_weapon_descriptor_str(k)
        .replace('-', "_")
        .to_uppercase()
}

fn normalized_aircraft_key_upper(k: &str) -> String {
    k.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect::<String>()
}

#[derive(Debug, Deserialize)]
pub struct WeaponBridgeFile {
    #[serde(default)]
    #[allow(dead_code)]
    pub schema_version: u32,
    #[serde(default)]
    #[allow(dead_code)]
    pub dcs_version: Option<String>,
    #[serde(default)]
    pub by_descriptor: HashMap<String, [i32; 4]>,
    #[serde(default)]
    #[allow(dead_code)]
    pub fueltank_by_aircraft: HashMap<String, Vec<[i32; 4]>>,
    #[serde(default)]
    #[allow(dead_code)]
    pub weapon_ws_by_aircraft: HashMap<String, Vec<[i32; 4]>>,
    #[serde(default)]
    #[allow(dead_code)]
    pub aircraft_by_ws: HashMap<String, Vec<String>>,
}

const VERSIONED_PREFIX: &str = "fowl_weapon_bridge-DCS.version.";
const VERSIONED_SUFFIX: &str = ".json";

/// Written by **bftools** next to the weapon bridge JSON from `weapon*.miz` payload tables.
pub const FOWL_WEAPON_PAYLOAD_WS: &str = "fowl_weapon_payload_ws.json";

/// Per-coalition, per-aircraft-type wsTypes from slot template `payload.pylons` / `payload.restricted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FowlWeaponPayloadWsFile {
    pub schema_version: u32,
    #[serde(default)]
    pub pylon_ws_by_side: HashMap<String, HashMap<String, Vec<[i32; 4]>>>,
    #[serde(default)]
    pub restricted_ws_by_side: HashMap<String, HashMap<String, Vec<[i32; 4]>>>,
}

impl FowlWeaponPayloadWsFile {
    pub fn write(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_vec_pretty(self).context("serialize fowl_weapon_payload_ws.json")?;
        fs::write(path, json).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }
}

fn flatten_side_aircraft_ws(
    by_side: &HashMap<String, HashMap<String, Vec<[i32; 4]>>>,
) -> HashMap<String, HashSet<[i32; 4]>> {
    let mut out: HashMap<String, HashSet<[i32; 4]>> = HashMap::new();
    for (side, inner) in by_side {
        for (aircraft, rows) in inner {
            let k = format!("{side}|{aircraft}");
            let e = out.entry(k).or_default();
            for ws in rows {
                if *ws != ZERO_WS {
                    e.insert(*ws);
                }
            }
        }
    }
    out
}

fn try_read_payload_ws_sidecar(bridge_path: &Path) -> Result<(HashMap<String, HashSet<[i32; 4]>>, HashMap<String, HashSet<[i32; 4]>>)> {
    let p = bridge_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(FOWL_WEAPON_PAYLOAD_WS);
    if !p.is_file() {
        return Ok((HashMap::new(), HashMap::new()));
    }
    let bytes = fs::read(&p).with_context(|| format!("read {}", p.display()))?;
    let f: FowlWeaponPayloadWsFile =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", p.display()))?;
    Ok((
        flatten_side_aircraft_ws(&f.pylon_ws_by_side),
        flatten_side_aircraft_ws(&f.restricted_ws_by_side),
    ))
}

/// `fowl_weapon_bridge.json` má přednost; jinak nejnovější podle data změny souboru mezi
/// `fowl_weapon_bridge-DCS.version.*.json` (přípona verze se v CLI nezadává).
pub fn resolve_auto_bridge_path(weapon_miz_dir: &Path) -> Option<PathBuf> {
    let exact = weapon_miz_dir.join("fowl_weapon_bridge.json");
    if exact.is_file() {
        return Some(exact);
    }
    let rd = fs::read_dir(weapon_miz_dir).ok()?;
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    for ent in rd.flatten() {
        let p = ent.path();
        if !p.is_file() {
            continue;
        }
        let name = ent.file_name();
        let n = name.to_string_lossy();
        if !n.starts_with(VERSIONED_PREFIX) || !n.ends_with(VERSIONED_SUFFIX) {
            continue;
        }
        let m = p.metadata().ok()?.modified().ok()?;
        match &best {
            None => best = Some((p, m)),
            Some((_, t0)) if m > *t0 => best = Some((p, m)),
            _ => {}
        }
    }
    best.map(|(p, _)| p)
}

#[derive(Debug, Clone)]
pub struct WeaponBridgeMap {
    by_descriptor: HashMap<String, [i32; 4]>,
    fueltank_by_aircraft: HashMap<String, Vec<[i32; 4]>>,
    weapon_ws_by_aircraft: HashMap<String, Vec<[i32; 4]>>,
    aircraft_by_ws: HashMap<[i32; 4], HashSet<String>>,
    ws_alias_family: HashMap<[i32; 4], HashSet<[i32; 4]>>,
    /// Keys `blue|A-10C` / `red|MiG-29S` from `fowl_weapon_payload_ws.json` (bftools).
    template_pylon_ws: HashMap<String, HashSet<[i32; 4]>>,
    template_restricted_ws: HashMap<String, HashSet<[i32; 4]>>,
}

impl WeaponBridgeMap {
    fn is_high_confidence_alias_key(descriptor: &str) -> bool {
        let key = descriptor.trim();
        let upper = key.to_ascii_uppercase();
        let braced = key.starts_with('{') && key.ends_with('}') && key.len() >= 3;
        if braced || upper.contains("/BYCLSID/") {
            return true;
        }
        // Bare CLSID-like aliases (e.g. `LAU_117_AGM_65A`).
        key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && key.contains('_')
            && key.chars().any(|c| c.is_ascii_digit())
    }

    fn family_tokens_for_descriptor(descriptor: &str) -> HashSet<String> {
        fn is_alpha_only(s: &str) -> bool {
            !s.is_empty() && s.chars().all(|c| c.is_ascii_alphabetic())
        }
        fn is_alnum(s: &str) -> bool {
            !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric())
        }
        fn has_alpha_and_digit(s: &str) -> bool {
            s.chars().any(|c| c.is_ascii_alphabetic()) && s.chars().any(|c| c.is_ascii_digit())
        }
        fn blocked_prefix(prefix: &str) -> bool {
            matches!(
                prefix,
                "LAU"
                    | "BRU"
                    | "TER"
                    | "MER"
                    | "AKU"
                    | "APU"
                    | "MBD"
                    | "POD"
                    | "RACK"
                    | "PYLON"
                    | "SHOULDER"
            )
        }

        if !Self::is_high_confidence_alias_key(descriptor) {
            return HashSet::new();
        }
        let mut out = HashSet::new();
        let mut cleaned = String::with_capacity(descriptor.len());
        for c in normalized_bridge_key_upper(descriptor).chars() {
            if c.is_ascii_alphanumeric() || c == '_' {
                cleaned.push(c);
            } else {
                cleaned.push('_');
            }
        }
        let parts: Vec<&str> = cleaned.split('_').filter(|p| !p.is_empty()).collect();
        for i in 0..parts.len() {
            let p = parts[i];
            if is_alnum(p) && has_alpha_and_digit(p) && p.len() >= 3 {
                out.insert(p.to_string());
            }
            if i + 1 < parts.len() {
                let p0 = parts[i];
                let p1 = parts[i + 1];
                if is_alpha_only(p0) && !blocked_prefix(p0) && is_alnum(p1) && has_alpha_and_digit(p1)
                {
                    out.insert(format!("{p0}_{p1}"));
                }
            }
            if i + 2 < parts.len() {
                let p0 = parts[i];
                let p1 = parts[i + 1];
                let p2 = parts[i + 2];
                if is_alpha_only(p0)
                    && !blocked_prefix(p0)
                    && is_alnum(p1)
                    && has_alpha_and_digit(p1)
                    && is_alnum(p2)
                    && has_alpha_and_digit(p2)
                {
                    out.insert(format!("{p0}_{p1}_{p2}"));
                }
            }
        }
        out
    }

    fn build_ws_alias_family(
        by_descriptor: &HashMap<String, [i32; 4]>,
    ) -> HashMap<[i32; 4], HashSet<[i32; 4]>> {
        let mut token_to_ws: HashMap<String, HashSet<[i32; 4]>> = HashMap::new();
        for (desc, ws) in by_descriptor {
            if *ws == ZERO_WS {
                continue;
            }
            for token in Self::family_tokens_for_descriptor(desc) {
                token_to_ws.entry(token).or_default().insert(*ws);
            }
        }
        let mut out: HashMap<[i32; 4], HashSet<[i32; 4]>> = HashMap::new();
        for set in token_to_ws.values() {
            if set.len() < 2 {
                continue;
            }
            for ws in set {
                out.entry(*ws).or_default().extend(set.iter().copied());
            }
        }
        out
    }

    pub fn load(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("read weapon bridge {}", path.display()))?;
        let parsed: WeaponBridgeFile =
            serde_json::from_slice(&bytes).context("parse weapon bridge JSON")?;
        let ws_alias_family = Self::build_ws_alias_family(&parsed.by_descriptor);
        let mut aircraft_by_ws: HashMap<[i32; 4], HashSet<String>> = HashMap::new();
        for (ws_key, aircrafts) in &parsed.aircraft_by_ws {
            let parts: Vec<_> = ws_key.split(',').map(str::trim).collect();
            if parts.len() != 4 {
                continue;
            }
            let Ok(a) = parts[0].parse::<i32>() else {
                continue;
            };
            let Ok(b) = parts[1].parse::<i32>() else {
                continue;
            };
            let Ok(c) = parts[2].parse::<i32>() else {
                continue;
            };
            let Ok(d) = parts[3].parse::<i32>() else {
                continue;
            };
            let ws = [a, b, c, d];
            let entry = aircraft_by_ws.entry(ws).or_default();
            for typ in aircrafts {
                entry.insert(normalized_aircraft_key_upper(typ));
            }
        }
        let (template_pylon_ws, template_restricted_ws) = try_read_payload_ws_sidecar(path)?;
        Ok(Self {
            by_descriptor: parsed.by_descriptor,
            fueltank_by_aircraft: parsed.fueltank_by_aircraft,
            weapon_ws_by_aircraft: parsed.weapon_ws_by_aircraft,
            aircraft_by_ws,
            ws_alias_family,
            template_pylon_ws,
            template_restricted_ws,
        })
    }

    /// Reload `fowl_weapon_payload_ws.json` after bftools wrote it in the same process.
    pub fn reload_template_payload_ws(&mut self, bridge_path: &Path) -> Result<()> {
        let (p, r) = try_read_payload_ws_sidecar(bridge_path)?;
        self.template_pylon_ws = p;
        self.template_restricted_ws = r;
        Ok(())
    }

    pub fn has_template_payload_ws(&self) -> bool {
        !self.template_pylon_ws.is_empty()
    }

    fn template_payload_key(side: &str, unit_type: &str) -> String {
        format!("{side}|{unit_type}")
    }

    pub fn template_pylon_ws_union_for_side(
        &self,
        side: &str,
        slot_types: &HashSet<String>,
    ) -> HashSet<[i32; 4]> {
        let mut out = HashSet::new();
        for t in slot_types {
            let k = Self::template_payload_key(side, t.as_str());
            if let Some(s) = self.template_pylon_ws.get(&k) {
                out.extend(s.iter().copied());
            }
        }
        out
    }

    pub fn template_restricted_ws_union_for_side(
        &self,
        side: &str,
        slot_types: &HashSet<String>,
    ) -> HashSet<[i32; 4]> {
        let mut out = HashSet::new();
        for t in slot_types {
            let k = Self::template_payload_key(side, t.as_str());
            if let Some(s) = self.template_restricted_ws.get(&k) {
                out.extend(s.iter().copied());
            }
        }
        out
    }

    /// Ordnance allowlist from **pylon-mounted** ws only: `fowl_weapon_payload_ws` pylons ∪ `lua_pylon_ws`,
    /// alias-expanded, then **intersected** with `weapon_ws_by_aircraft` for `slot_types`.
    ///
    /// The old `(base ∪ cap)` retain let every `cap` weapon into BDEFAULT even when never on a template pylon
    /// (payload `restricted` / blocked stores are unrelated — those must not enter via `cap` alone).
    pub fn template_ordnance_allow_ws(
        &self,
        side: &str,
        slot_types: &HashSet<String>,
        lua_pylon_ws: &HashSet<[i32; 4]>,
    ) -> HashSet<[i32; 4]> {
        let mut base = self.template_pylon_ws_union_for_side(side, slot_types);
        base.extend(lua_pylon_ws.iter().copied());
        let cap = self.weapon_ws_for_aircraft_keys_only(slot_types);
        let mut out = self.expand_ws_alias_family(&base);
        out.retain(|w| {
            *w != ZERO_WS
                && w[0] == 4
                && ((4..=8).contains(&w[1]) || w[1] == 15)
                && cap.contains(w)
        });
        out
    }

    pub fn len(&self) -> usize {
        self.by_descriptor.len()
    }

    pub fn display_names_for_ws_type(&self, ws: [i32; 4], max_names: usize) -> Vec<String> {
        fn looks_guid_like(s: &str) -> bool {
            s.len() >= 32
                && s.chars()
                    .all(|c| c.is_ascii_hexdigit() || c == '-' || c == '{' || c == '}')
        }

        fn display_score(s: &str) -> (i32, usize, String) {
            let trimmed = s.trim();
            let upper = trimmed.to_ascii_uppercase();
            let braced = trimmed.starts_with('{') && trimmed.ends_with('}');
            let numeric_only = trimmed.chars().all(|c| c.is_ascii_digit());
            let mut score = 0i32;
            if trimmed.starts_with("db/") {
                score += 100;
            }
            if upper.contains("/BYCLSID/") || upper.contains("/CATEGORIES/") {
                score += 80;
            }
            if braced {
                score += 40;
            }
            if numeric_only || looks_guid_like(trimmed) {
                score += 90;
            }
            if trimmed.contains(' ') {
                score -= 20;
            }
            if trimmed.contains(" - ") {
                score -= 20;
            }
            if trimmed.contains('_') && !trimmed.contains(' ') {
                score += 15;
            }
            (score, trimmed.len(), upper)
        }

        let mut names: Vec<String> = self
            .by_descriptor
            .iter()
            .filter_map(|(name, mapped_ws)| (*mapped_ws == ws).then(|| name.clone()))
            .collect();
        names.sort_by_key(|name| display_score(name));
        names.dedup();
        names.truncate(max_names);
        names
    }

    /// Vrátí wsType pro přesný klíč z payloadu (`restricted` / `pylons`).
    pub fn ws_type_for_descriptor(&self, key: &str) -> Option<[i32; 4]> {
        self.by_descriptor.get(key).copied()
    }

    /// Exact map, else every `wsType` whose bridge key contains the token (payload short CLSID vs rack key).
    pub fn ws_types_for_descriptor_or_key_substring(&self, descriptor: &str) -> HashSet<[i32; 4]> {
        let mut out = HashSet::new();
        if let Some(ws) = self.ws_type_for_descriptor(descriptor) {
            if ws != ZERO_WS {
                out.insert(ws);
            }
            return out;
        }
        let Some((needle, needle_braced)) = descriptor_substring_needle(descriptor) else {
            return out;
        };
        for (k, v) in &self.by_descriptor {
            if *v == ZERO_WS {
                continue;
            }
            let hay = normalized_bridge_key_upper(k);
            if needle_braced {
                if !(k.starts_with('{') && k.ends_with('}')) {
                    continue;
                }
                if hay.contains(needle.as_str()) {
                    out.insert(*v);
                }
                continue;
            }
            if k.contains('/') {
                continue;
            }
            if hay == needle || hay.starts_with(&(needle.clone() + "_")) {
                out.insert(*v);
            }
        }
        out
    }

    /// Union of [`Self::ws_types_for_descriptor_or_key_substring`] over many descriptors.
    pub fn ws_types_for_restricted_descriptor_union(
        &self,
        descriptors: &std::collections::HashSet<String>,
    ) -> HashSet<[i32; 4]> {
        let mut out = HashSet::new();
        for d in descriptors {
            out.extend(self.ws_types_for_descriptor_or_key_substring(d));
        }
        out
    }

    /// Fuel wsTypes that have at least one non-empty descriptor/key in bridge JSON.
    pub fn fueltank_ws_non_empty(&self) -> HashSet<[i32; 4]> {
        let mut has_non_empty = HashSet::new();
        let mut has_empty_alias = HashSet::new();
        for (k, v) in &self.by_descriptor {
            if *v == ZERO_WS {
                continue;
            }
            if !is_fueltank_ws(*v) {
                continue;
            }
            if contains_empty_token(k) {
                has_empty_alias.insert(*v);
            } else {
                has_non_empty.insert(*v);
            }
        }
        has_non_empty
            .into_iter()
            .filter(|ws| !has_empty_alias.contains(ws))
            .collect()
    }

    pub fn fueltank_ws_empty(&self) -> HashSet<[i32; 4]> {
        let mut out = HashSet::new();
        for (k, v) in &self.by_descriptor {
            if *v == ZERO_WS || !is_fueltank_ws(*v) {
                continue;
            }
            if contains_empty_token(k) {
                out.insert(*v);
            }
        }
        out
    }

    pub fn fueltank_ws_for_aircrafts(&self, aircraft_types: &HashSet<String>) -> HashSet<[i32; 4]> {
        let mut out = HashSet::new();
        let normalized_index: HashMap<String, &Vec<[i32; 4]>> = self
            .fueltank_by_aircraft
            .iter()
            .map(|(k, v)| (normalized_aircraft_key_upper(k), v))
            .collect();
        for typ in aircraft_types {
            let v = self
                .fueltank_by_aircraft
                .get(typ.as_str())
                .or_else(|| normalized_index.get(&normalized_aircraft_key_upper(typ)).copied());
            let Some(v) = v else {
                continue;
            };
            for ws in v {
                if is_fueltank_ws(*ws) {
                    out.insert(*ws);
                }
            }
        }
        out
    }

    /// Rows from `weapon_ws_by_aircraft` for the given unit type strings only (no `aircraft_by_ws` reverse map).
    pub fn weapon_ws_for_aircraft_keys_only(
        &self,
        aircraft_types: &HashSet<String>,
    ) -> HashSet<[i32; 4]> {
        let mut out = HashSet::new();
        let normalized_index: HashMap<String, &Vec<[i32; 4]>> = self
            .weapon_ws_by_aircraft
            .iter()
            .map(|(k, v)| (normalized_aircraft_key_upper(k), v))
            .collect();
        for typ in aircraft_types {
            let v = self
                .weapon_ws_by_aircraft
                .get(typ.as_str())
                .or_else(|| normalized_index.get(&normalized_aircraft_key_upper(typ)).copied());
            let Some(v) = v else {
                continue;
            };
            for ws in v {
                if *ws != ZERO_WS {
                    out.insert(*ws);
                }
            }
        }
        out
    }

    /// Rows from `weapon_ws_by_aircraft` for one unit type string only (no reverse map).
    pub fn weapon_ws_for_aircraft_key_only(&self, aircraft_type: &str) -> HashSet<[i32; 4]> {
        let normalized_index: HashMap<String, &Vec<[i32; 4]>> = self
            .weapon_ws_by_aircraft
            .iter()
            .map(|(k, v)| (normalized_aircraft_key_upper(k), v))
            .collect();
        let Some(v) = self
            .weapon_ws_by_aircraft
            .get(aircraft_type)
            .or_else(|| normalized_index.get(&normalized_aircraft_key_upper(aircraft_type)).copied())
        else {
            return HashSet::new();
        };
        v.iter().copied().filter(|ws| *ws != ZERO_WS).collect()
    }

    pub fn template_restricted_ws_for_side_type(
        &self,
        side: &str,
        unit_type: &str,
    ) -> HashSet<[i32; 4]> {
        let k = Self::template_payload_key(side, unit_type);
        self.template_restricted_ws
            .get(&k)
            .cloned()
            .unwrap_or_default()
    }

    pub fn weapon_ws_for_aircrafts(&self, aircraft_types: &HashSet<String>) -> HashSet<[i32; 4]> {
        let mut out = self.weapon_ws_for_aircraft_keys_only(aircraft_types);
        let requested_norm: HashSet<String> = aircraft_types
            .iter()
            .map(|s| normalized_aircraft_key_upper(s))
            .collect();
        // Reverse mapping from bridge: keep ws when any allowed aircraft can carry it.
        // This catches aliases that might be absent in `weapon_ws_by_aircraft`.
        for (ws, aircrafts) in &self.aircraft_by_ws {
            if *ws == ZERO_WS {
                continue;
            }
            if aircrafts.iter().any(|a| requested_norm.contains(a)) {
                out.insert(*ws);
            }
        }
        out
    }

    /// Expand ws set by bridge-derived descriptor family aliases.
    /// Example: keep LAU-117 + missile-body variants in the same validation family.
    pub fn expand_ws_alias_family(&self, ws_set: &HashSet<[i32; 4]>) -> HashSet<[i32; 4]> {
        let mut out = ws_set.clone();
        for ws in ws_set {
            if let Some(family) = self.ws_alias_family.get(ws) {
                out.extend(family.iter().copied());
            }
        }
        out
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn parses_minimal() {
        let j = r#"{"schema_version":1,"by_descriptor":{"{ABC}":[4,4,7,1]}}"#;
        let f: WeaponBridgeFile = serde_json::from_str(j).unwrap();
        let m = WeaponBridgeMap {
            by_descriptor: f.by_descriptor,
            fueltank_by_aircraft: HashMap::new(),
            weapon_ws_by_aircraft: HashMap::new(),
            aircraft_by_ws: HashMap::new(),
            ws_alias_family: HashMap::new(),
            template_pylon_ws: HashMap::new(),
            template_restricted_ws: HashMap::new(),
        };
        assert_eq!(m.ws_type_for_descriptor("{ABC}"), Some([4, 4, 7, 1]));
    }

    #[test]
    fn resolve_prefers_exact_json() {
        let dir = std::env::temp_dir().join("fowl_weapon_bridge_test_exact");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let exact = dir.join("fowl_weapon_bridge.json");
        let ver = dir.join("fowl_weapon_bridge-DCS.version.9_9_9.json");
        fs::write(&ver, r#"{"by_descriptor":{}}"#).unwrap();
        thread::sleep(Duration::from_millis(50));
        fs::write(&exact, r#"{"by_descriptor":{}}"#).unwrap();
        assert_eq!(resolve_auto_bridge_path(&dir), Some(exact));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn short_payload_gbu_maps_via_substring_to_rack_keys() {
        let j = r#"{"by_descriptor":{
            "{BRU-32 GBU_31_V_2B}":[4,5,36,85],
            "{GBU-39}":[1,2,3,4]
        }}"#;
        let f: WeaponBridgeFile = serde_json::from_str(j).unwrap();
        let m = WeaponBridgeMap {
            by_descriptor: f.by_descriptor,
            fueltank_by_aircraft: HashMap::new(),
            weapon_ws_by_aircraft: HashMap::new(),
            aircraft_by_ws: HashMap::new(),
            ws_alias_family: HashMap::new(),
            template_pylon_ws: HashMap::new(),
            template_restricted_ws: HashMap::new(),
        };
        let s = m.ws_types_for_descriptor_or_key_substring("{GBU-31}");
        assert!(s.contains(&[4, 5, 36, 85]));
        assert!(!s.contains(&[1, 2, 3, 4]));
    }

    #[test]
    fn aim54c_short_descriptor_maps_shoulder_keys() {
        let j = r#"{"by_descriptor":{
            "{SHOULDER AIM_54C_Mk47 L}":[4,4,7,322],
            "{SHOULDER AIM_54C_Mk47 R}":[4,4,7,322]
        }}"#;
        let f: WeaponBridgeFile = serde_json::from_str(j).unwrap();
        let m = WeaponBridgeMap {
            by_descriptor: f.by_descriptor,
            fueltank_by_aircraft: HashMap::new(),
            weapon_ws_by_aircraft: HashMap::new(),
            aircraft_by_ws: HashMap::new(),
            ws_alias_family: HashMap::new(),
            template_pylon_ws: HashMap::new(),
            template_restricted_ws: HashMap::new(),
        };
        let s = m.ws_types_for_descriptor_or_key_substring("{AIM-54C_Mk47}");
        assert!(s.contains(&[4, 4, 7, 322]));
    }

    #[test]
    fn unicode_hyphen_in_descriptor_matches_ascii_hyphen_in_bridge_key() {
        let desc = format!("{{GBU\u{2212}31}}");
        let j = r#"{"by_descriptor":{"{BRU-32 GBU_31_V_2B}":[4,5,36,85]}}"#;
        let f: WeaponBridgeFile = serde_json::from_str(j).unwrap();
        let m = WeaponBridgeMap {
            by_descriptor: f.by_descriptor,
            fueltank_by_aircraft: HashMap::new(),
            weapon_ws_by_aircraft: HashMap::new(),
            aircraft_by_ws: HashMap::new(),
            ws_alias_family: HashMap::new(),
            template_pylon_ws: HashMap::new(),
            template_restricted_ws: HashMap::new(),
        };
        let s = m.ws_types_for_descriptor_or_key_substring(&desc);
        assert!(s.contains(&[4, 5, 36, 85]));
    }


    #[test]
    fn resolve_picks_newest_versioned() {
        let dir = std::env::temp_dir().join("fowl_weapon_bridge_test_ver");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let older = dir.join("fowl_weapon_bridge-DCS.version.1_0_0.json");
        let newer = dir.join("fowl_weapon_bridge-DCS.version.2_0_0.json");
        fs::write(&older, r#"{"by_descriptor":{}}"#).unwrap();
        thread::sleep(Duration::from_millis(80));
        fs::write(&newer, r#"{"by_descriptor":{}}"#).unwrap();
        assert_eq!(resolve_auto_bridge_path(&dir), Some(newer));
        let _ = fs::remove_dir_all(&dir);
    }

}
