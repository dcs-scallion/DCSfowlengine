//! Multi-instance registry.
//!
//! One bfdb process can front several DCS server instances on the same machine.
//! Configured with `--instances <file.json>`. Without `--instances`, the legacy
//! single-server flags synthesize one instance whose id is [`DEFAULT_INSTANCE`].
//! Untagged rounds in the Sled DB belong to that default instance.
use anyhow::{bail, Context, Result};
use netidx::path::Path as NetidxPath;
use serde_derive::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Instance id for pre-multi-instance data and legacy single-server CLI.
pub(crate) const DEFAULT_INSTANCE: &str = "default";

/// Default UDP port for DCS `Export.lua` (first / single instance).
pub(crate) const DEFAULT_EXPORT_PORT: u16 = 42001;

pub(crate) type InstanceId = Arc<str>;

fn default_true() -> bool {
    true
}

/// Static startup description of one DCS server instance.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct InstanceCfg {
    /// Stable URL-safe key (`?instance=<id>`). Do not rename after campaigns run.
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    /// `netidx_base` from this instance's engine CFG. Must be unique per instance.
    #[serde(default)]
    pub base: Option<NetidxPath>,
    /// Optional netidx client config when this instance uses a different resolver.
    #[serde(default)]
    pub netidx_config: Option<PathBuf>,
    /// Hint for the sortie path segment until the engine reports in. Prefer unset.
    #[serde(default)]
    pub sortie: Option<String>,
    #[serde(default)]
    pub stats_jsonl: Option<PathBuf>,
    #[serde(default)]
    pub stats_dir: Option<PathBuf>,
    #[serde(default)]
    pub export_port: Option<u16>,
    #[serde(default)]
    pub engine_config: Option<PathBuf>,
    #[serde(default)]
    pub srs_url: Option<String>,
    #[serde(default)]
    pub dcs_server_name: Option<String>,
    /// When false: omit from public `GET /api/instances` for non-admins.
    #[serde(default = "default_true")]
    pub public: bool,
}

impl InstanceCfg {
    pub(crate) fn label(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.id)
    }

    /// Whether this instance's activity counts toward all-time pilot totals.
    pub(crate) fn counts_toward_totals(&self) -> bool {
        self.public
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct InstancesFile {
    #[serde(default)]
    pub default: Option<String>,
    pub instances: Vec<InstanceCfg>,
}

#[derive(Debug, Clone)]
pub(crate) struct Registry {
    instances: Vec<Arc<InstanceCfg>>,
    default: InstanceId,
}

impl Registry {
    pub(crate) fn new(file: InstancesFile) -> Result<Self> {
        if file.instances.is_empty() {
            bail!("instances file lists no instances");
        }
        let mut ids: HashSet<String> = HashSet::new();
        let mut bases: HashSet<String> = HashSet::new();
        let mut ports: HashSet<u16> = HashSet::new();
        let mut jsonls: HashSet<PathBuf> = HashSet::new();
        for i in &file.instances {
            if i.id.is_empty() {
                bail!("an instance has an empty id");
            }
            if !i
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                bail!(
                    "instance id {:?} must be ASCII alphanumeric, '-' or '_' (URLs and DB keys)",
                    i.id
                );
            }
            if !ids.insert(i.id.clone()) {
                bail!("duplicate instance id {:?}", i.id);
            }
            if let Some(b) = &i.base {
                if !bases.insert(format!("{b}")) {
                    bail!(
                        "instance {:?} reuses netidx base {b} -- each DCS instance needs its own \
                         `netidx_base` in its engine CFG",
                        i.id
                    );
                }
            }
            if let Some(p) = i.export_port {
                if !ports.insert(p) {
                    bail!(
                        "instance {:?} reuses UDP export port {p} -- give each instance its own \
                         port and matching BF_PORT in Export.lua",
                        i.id
                    );
                }
            }
            if let Some(p) = &i.stats_jsonl {
                if !jsonls.insert(p.clone()) {
                    bail!(
                        "instance {:?} reuses stats.jsonl {} -- each instance writes its own",
                        i.id,
                        p.display()
                    );
                }
            }
        }
        let default: InstanceId = match &file.default {
            Some(d) => {
                if !ids.contains(d.as_str()) {
                    bail!("default instance {d:?} is not in the instances list");
                }
                Arc::from(d.as_str())
            }
            None => Arc::from(file.instances[0].id.as_str()),
        };
        Ok(Self {
            instances: file.instances.into_iter().map(Arc::new).collect(),
            default,
        })
    }

    pub(crate) fn load(path: &Path) -> Result<Self> {
        let txt = std::fs::read_to_string(path)
            .with_context(|| format!("reading instances file {}", path.display()))?;
        let file: InstancesFile = serde_json::from_str(&txt)
            .with_context(|| format!("parsing instances file {}", path.display()))?;
        Self::new(file)
    }

    /// Legacy single-server CLI → one instance.
    pub(crate) fn single(cfg: InstanceCfg) -> Self {
        let default: InstanceId = Arc::from(cfg.id.as_str());
        Self {
            instances: vec![Arc::new(cfg)],
            default,
        }
    }

    pub(crate) fn all(&self) -> &[Arc<InstanceCfg>] {
        &self.instances
    }

    pub(crate) fn has_private(&self) -> bool {
        self.instances.iter().any(|i| !i.counts_toward_totals())
    }

    pub(crate) fn is_single(&self) -> bool {
        self.instances.len() == 1
    }

    pub(crate) fn default_id(&self) -> &InstanceId {
        &self.default
    }

    pub(crate) fn get(&self, id: &str) -> Option<&Arc<InstanceCfg>> {
        self.instances.iter().find(|i| i.id == id)
    }

    /// Resolve `?instance=`. Empty / missing / `"all"` → default. Unknown id errors.
    pub(crate) fn resolve(&self, requested: Option<&str>) -> Result<&Arc<InstanceCfg>> {
        match requested
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "all")
        {
            None => self
                .get(&self.default)
                .ok_or_else(|| anyhow::anyhow!("default instance missing")),
            Some(id) => self
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("unknown instance {id:?}")),
        }
    }

    pub(crate) fn by_dcs_server_name(&self, name: &str) -> Option<&Arc<InstanceCfg>> {
        self.instances
            .iter()
            .find(|i| i.dcs_server_name.as_deref() == Some(name))
    }
}
