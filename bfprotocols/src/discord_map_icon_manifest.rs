//! Discord objective map icon pack (`l10n/DEFAULT/fowl_discord_map/` inside the assembled `.miz`).

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Zip path prefix inside the mission archive (forward slashes).
pub const MIZ_DIR: &str = "l10n/DEFAULT/fowl_discord_map";

/// Runtime manifest entry inside the mission archive.
pub const MIZ_MANIFEST: &str = "l10n/DEFAULT/fowl_discord_map/manifest.json";

/// Only schema version supported by current Fowl tooling/runtime.
pub const SUPPORTED_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscordMapIconManifest {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub canvas_px: u32,
    pub kinds: BTreeMap<String, DiscordMapIconKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub palette: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscordMapIconKind {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shape: Option<String>,
    pub files: DiscordMapIconFiles,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscordMapIconFiles {
    pub red: String,
    pub blue: String,
    pub neutral: String,
}

/// Stripped manifest written into the assembled mission (icons + compositing metadata only).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscordMapIconManifestRuntime {
    pub schema_version: u32,
    pub canvas_px: u32,
    pub kinds: BTreeMap<String, DiscordMapIconKindRuntime>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscordMapIconKindRuntime {
    pub files: DiscordMapIconFiles,
}

impl DiscordMapIconManifestRuntime {
    /// PNG stem in the mission pack for a kind + coalition (e.g. logistics + blue -> hub_blue).
    pub fn png_stem_for(&self, kind: &str, coalition: &str) -> Option<&str> {
        let spec = self.kinds.get(kind)?;
        let stem = match coalition {
            "red" => &spec.files.red,
            "blue" => &spec.files.blue,
            _ => &spec.files.neutral,
        };
        Some(stem.as_str())
    }
}

impl DiscordMapIconManifest {
    pub fn load(path: &Path) -> Result<Self> {
        let mut f = File::open(path)
            .with_context(|| format!("open discord map icon manifest {:?}", path))?;
        let mut s = String::new();
        f.read_to_string(&mut s)
            .with_context(|| format!("read discord map icon manifest {:?}", path))?;
        let manifest: Self = serde_json::from_str(&s)
            .with_context(|| format!("parse discord map icon manifest {:?}", path))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SUPPORTED_SCHEMA_VERSION {
            bail!(
                "discord map icon manifest schema_version {} is unsupported (expected {})",
                self.schema_version,
                SUPPORTED_SCHEMA_VERSION
            );
        }
        if self.canvas_px == 0 {
            bail!("discord map icon manifest canvas_px must be > 0");
        }
        if self.kinds.is_empty() {
            bail!("discord map icon manifest kinds must not be empty");
        }
        for (kind, spec) in &self.kinds {
            for (coalition, stem) in [
                ("red", spec.files.red.as_str()),
                ("blue", spec.files.blue.as_str()),
                ("neutral", spec.files.neutral.as_str()),
            ] {
                if stem.is_empty() {
                    bail!("discord map icon kind {kind}: {coalition} file stem is empty");
                }
                if stem.contains('/') || stem.contains('\\') || stem.contains('.') {
                    bail!(
                        "discord map icon kind {kind}: {coalition} stem {:?} must be a bare filename without path or extension",
                        stem
                    );
                }
            }
        }
        Ok(())
    }

    pub fn to_runtime(&self) -> DiscordMapIconManifestRuntime {
        DiscordMapIconManifestRuntime {
            schema_version: self.schema_version,
            canvas_px: self.canvas_px,
            kinds: self
                .kinds
                .iter()
                .map(|(k, v)| {
                    (
                        k.clone(),
                        DiscordMapIconKindRuntime {
                            files: v.files.clone(),
                        },
                    )
                })
                .collect(),
        }
    }

    /// Unique PNG stems referenced by the manifest (without `.png`).
    pub fn png_stems(&self) -> Vec<String> {
        let mut stems: Vec<String> = self
            .kinds
            .values()
            .flat_map(|kind| {
                [
                    kind.files.red.clone(),
                    kind.files.blue.clone(),
                    kind.files.neutral.clone(),
                ]
            })
            .collect();
        stems.sort();
        stems.dedup();
        stems
    }

    pub fn miz_png_path(stem: &str) -> String {
        format!("{MIZ_DIR}/{stem}.png")
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn parse_repo_manifest() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../assets/discord-objective-map/manifest.json");
        if !path.is_file() {
            return;
        }
        let manifest = DiscordMapIconManifest::load(&path).unwrap();
        assert_eq!(manifest.schema_version, 1);
        assert_eq!(manifest.canvas_px, 96);
        assert_eq!(manifest.png_stems().len(), 12);
    }
}
