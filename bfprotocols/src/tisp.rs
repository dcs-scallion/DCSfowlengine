//! `TISP` trigger zones: initial naval ship placement (editor + FowlTools + bflib).
//!
//! Zone names: one ship template group in the ME (e.g. `BFrigate`), multiple zones
//! `TISPBFrigate`, `TISPBFrigate-1`, `TISPBFrigate-2`, … (numeric suffix only after the last `-`).

pub const TISP_PREFIX: &str = "TISP";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TispZoneRef<'a> {
    pub full_name: &'a str,
    /// Miz ship group / CFG `DeployableKind::Group` template (e.g. `BTarawa`, `BFrigate`).
    pub template: &'a str,
    /// `0` for `TISPBFrigate`; `N` for `TISPBFrigate-N` (decimal digits only).
    pub instance_index: u32,
}

/// Suffix `-` + ASCII digits at end of `body` → template stem + index; else whole `body` is template with index `0`.
fn split_trailing_zone_index(body: &str) -> Option<(&str, u32)> {
    let dash = body.rfind('-')?;
    let suf = body.get(dash + 1..)?;
    if suf.is_empty() || !suf.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let stem = body.get(..dash)?;
    if stem.len() < 2 {
        return None;
    }
    if !matches!(stem.as_bytes().first().copied(), Some(b'B' | b'R')) {
        return None;
    }
    let n: u32 = suf.parse().ok()?;
    if n == 0 {
        return None;
    }
    Some((stem, n))
}

/// Parsed `TISP` + `{B|R}…` + optional trailing `-N` (ME-unique zones). Returns `None` if not a valid TISP zone name.
pub fn parse_tisp_zone_name(name: &str) -> Option<TispZoneRef<'_>> {
    let body = name.strip_prefix(TISP_PREFIX)?;
    if body.len() < 2 {
        return None;
    }
    if !matches!(body.as_bytes().first().copied(), Some(b'B' | b'R')) {
        return None;
    }
    let (template, instance_index) = match split_trailing_zone_index(body) {
        Some((t, n)) => (t, n),
        None => (body, 0u32),
    };
    Some(TispZoneRef {
        full_name: name,
        template,
        instance_index,
    })
}

pub fn starts_with_tisp_prefix(name: &str) -> bool {
    name.starts_with(TISP_PREFIX)
}
