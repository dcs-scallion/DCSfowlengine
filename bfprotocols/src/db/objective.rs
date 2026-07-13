use crate::cfg::Deployable;
use dcso3::{atomic_id, String};
use serde_derive::{Deserialize, Serialize};

atomic_id!(ObjectiveId);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ObjectiveKind {
    Airbase,
    Fob,
    Logistics,
    Production,
    Farp {
        spec: Deployable,
        pad_template: String,
        #[serde(default)]
        mobile: bool,
        /// ME names of pre-placed client slot groups assigned to this deploy FARP.
        #[serde(default)]
        dep_static_slot_groups: Vec<String>,
    },
}

impl ObjectiveKind {
    pub fn is_airbase(&self) -> bool {
        match self {
            Self::Airbase => true,
            Self::Farp { .. } | Self::Fob | Self::Logistics | Self::Production => false,
        }
    }

    pub fn is_farp(&self) -> bool {
        match self {
            Self::Farp { .. } => true,
            Self::Airbase | Self::Fob | Self::Logistics | Self::Production => false,
        }
    }

    pub fn is_hub(&self) -> bool {
        match self {
            Self::Logistics => true,
            Self::Airbase | Self::Farp { .. } | Self::Fob | Self::Production => false,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Airbase => "Airbase",
            Self::Fob => "FOB",
            Self::Farp { .. } => "FARP",
            Self::Logistics => "Logistics Hub",
            Self::Production => "Production",
        }
    }
}
