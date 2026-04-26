//! Descriptor strings from unit `payload` for warehouse allowlist generation.
//! We keep per-module split:
//! - `supported`: descriptors seen under `pylons` or `restricted` (module is relevant for these)
//! - `restricted`: descriptors explicitly blocked by payload restrictions
//! Next step maps descriptors to `wsType` via the weapon bridge.

use mlua::{Table, Value};
use std::collections::HashSet;

fn collect_strings_recursive(t: &Table, out: &mut HashSet<String>) {
    for pair in t.clone().pairs::<Value, Value>() {
        let Ok((k, v)) = pair else {
            continue;
        };
        if let Value::String(s) = k {
            if let Ok(st) = s.to_str() {
                if !st.is_empty() {
                    out.insert(st.to_string());
                }
            }
        }
        match v {
            Value::String(s) => {
                if let Ok(st) = s.to_str() {
                    if !st.is_empty() {
                        out.insert(st.to_string());
                    }
                }
            }
            Value::Table(sub) => collect_strings_recursive(&sub, out),
            _ => {}
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct ModuleDescriptors {
    pub supported: HashSet<String>,
    pub restricted: HashSet<String>,
}

/// Collect descriptor strings for one module payload.
///
/// - `supported`: union of strings under `pylons` and `restricted`
/// - `restricted`: strings under `restricted` only
/// Strings under `pylons` only (excludes `restricted`-only entries).
pub fn collect_pylon_descriptors(payload: &Table) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Ok(t) = payload.raw_get::<_, Table>("pylons") {
        collect_strings_recursive(&t, &mut out);
    }
    out
}

/// Strings under `payload.restricted`.
pub fn collect_restricted_descriptors(payload: &Table) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Ok(t) = payload.raw_get::<_, Table>("restricted") {
        collect_strings_recursive(&t, &mut out);
    }
    out
}

pub fn collect_module_descriptors(payload: &Table) -> ModuleDescriptors {
    let mut supported = HashSet::new();
    let mut restricted = HashSet::new();
    if let Ok(t) = payload.raw_get::<_, Table>("pylons") {
        collect_strings_recursive(&t, &mut supported);
    }
    if let Ok(t) = payload.raw_get::<_, Table>("restricted") {
        collect_strings_recursive(&t, &mut restricted);
        supported.extend(restricted.iter().cloned());
    }
    ModuleDescriptors { supported, restricted }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    #[test]
    fn restricted_and_pylons_strings() {
        let lua = Lua::new();
        let payload: Table = lua
            .load(
                r#"
            return {
                pylons = {
                    [1] = { CLSID = "{FOO}" },
                },
                restricted = {
                    [1] = { [1] = "{BAR}", [2] = "B_8V20A_CM" },
                },
            }
        "#,
            )
            .eval()
            .unwrap();
        let s = collect_module_descriptors(&payload);
        assert!(s.supported.contains("{FOO}"));
        assert!(s.supported.contains("{BAR}"));
        assert!(s.supported.contains("B_8V20A_CM"));
        assert!(!s.restricted.contains("{FOO}"));
        assert!(s.restricted.contains("{BAR}"));
        assert!(s.restricted.contains("B_8V20A_CM"));
    }

    #[test]
    fn pylon_vs_restricted_helpers() {
        let lua = Lua::new();
        let payload: Table = lua
            .load(
                r#"
            return {
                pylons = { [1] = { CLSID = "{ON_PYLON}" } },
                restricted = { [1] = "{BANNED}" },
            }
        "#,
            )
            .eval()
            .unwrap();
        let pyl = collect_pylon_descriptors(&payload);
        let rst = collect_restricted_descriptors(&payload);
        assert!(pyl.contains("{ON_PYLON}"));
        assert!(!pyl.contains("{BANNED}"));
        assert!(rst.contains("{BANNED}"));
    }

    #[test]
    fn collects_descriptor_from_string_key() {
        let lua = Lua::new();
        let payload: Table = lua
            .load(
                r#"
            return {
                pylons = {
                    [1] = { ["{PTB-KEYED}"] = { count = 1 } },
                },
            }
        "#,
            )
            .eval()
            .unwrap();
        let pyl = collect_pylon_descriptors(&payload);
        assert!(pyl.contains("{PTB-KEYED}"));
    }
}
