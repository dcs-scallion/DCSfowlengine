//! Trigger zone **names** accepted by Fowl runtime during `mizinit` (second pass over zones).
//! bftools must use the same rules so invalid zones fail at build time.

/// Shown in bftools / bflib errors; keep in sync with `fowl_trigger_zone_name_valid`.
pub const FOWL_TRIGGER_ZONE_EXPECTED_PREFIXES_DISPLAY: &str = "O, G, T, or SETTINGS- prefix";

/// Same predicate as `bflib` `mizinit::init` after the `G…` branch: allowed `O`, `G`, `T`, or `SETTINGS-`.
#[inline]
pub fn fowl_trigger_zone_name_valid(name: &str) -> bool {
    name.starts_with('O')
        || name.starts_with('G')
        || name.starts_with('T')
        || name.starts_with("SETTINGS-")
}
