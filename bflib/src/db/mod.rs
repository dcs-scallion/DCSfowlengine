/*
Copyright 2024 Eric Stokes.

This file is part of bflib.

bflib is free software: you can redistribute it and/or modify it under
the terms of the GNU Affero Public License as published by the Free
Software Foundation, either version 3 of the License, or (at your
option) any later version.

bflib is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE. See the GNU Affero Public License
for more details.
*/

extern crate nalgebra as na;
use self::{group::DeployKind, persisted::Persisted};
use crate::{bg::Task, db::ephemeral::Ephemeral, jtac::JtId};
use anyhow::{Context, Result, anyhow};
use bfprotocols::{
    cfg::{
        Action, ActionKind, AwacsCfg, Cfg, Deployable, DeployableEwr, DeployableJtac, DroneCfg,
        Troop,
    },
    db::{
        group::{GroupId, UnitId},
        objective::ObjectiveId,
    },
    fowl_miz_export::FowlMizExport,
};
use dcso3::{
    Vector3, centroid3d,
    coalition::Side,
    env::miz::{Miz, MizIndex},
    MizLua,
};
use std::{cmp::max, fs::File, path::Path, sync::Arc};
use tokio::sync::mpsc::UnboundedSender;

pub mod actions;
pub mod ai_air;
pub mod aliases;
pub mod cargo;
pub mod csar;
pub mod discord_map;
pub mod ephemeral;
pub mod front_line;
pub mod group;
pub mod logistics;
pub mod markup;
pub mod mizinit;
pub mod server_settings;
pub mod tisp_init;
pub mod objective;
pub mod persisted;
pub mod player;

pub type Map<K, V> = immutable_chunkmap::map::Map<K, V, 256>;
pub type MapM<K, V> = immutable_chunkmap::map::Map<K, V, 64>;
pub type MapS<K, V> = immutable_chunkmap::map::Map<K, V, 16>;

pub type Set<K> = immutable_chunkmap::set::Set<K, 256>;
pub type SetM<K> = immutable_chunkmap::set::Set<K, 64>;
pub type SetS<K> = immutable_chunkmap::set::Set<K, 16>;

pub struct JtDesc {
    pub pos: Vector3,
    pub id: JtId,
    pub side: Side,
    pub spec: DeployableJtac,
    pub air: bool,
}

#[macro_export]
macro_rules! maybe {
    ($t:expr, $id:expr, $name:expr) => {
        $t.get(&$id)
            .ok_or_else(|| anyhow!("no such {} {:?}", $name, $id))
    };
}

#[macro_export]
macro_rules! maybe_mut {
    ($t:expr, $id:expr, $name:expr) => {
        $t.get_mut_cow(&$id)
            .ok_or_else(|| anyhow!("no such {} {:?}", $name, $id))
    };
}

#[macro_export]
macro_rules! unit {
    ($t:expr, $id:expr) => {
        $t.persisted
            .units
            .get(&$id)
            .ok_or_else(|| anyhow!("no such unit {:?}", $id))
    };
}

#[macro_export]
macro_rules! unit_mut {
    ($t:expr, $id:expr) => {
        $t.persisted
            .units
            .get_mut_cow(&$id)
            .ok_or_else(|| anyhow!("no such unit {:?}", $id))
    };
}

#[macro_export]
macro_rules! unit_by_name {
    ($t:expr, $name:expr) => {
        $t.persisted
            .units_by_name
            .get($name)
            .and_then(|id| $t.persisted.units.get(id))
            .ok_or_else(|| anyhow!("no such unit {}", $name))
    };
}

#[macro_export]
macro_rules! group {
    ($t:expr, $id:expr) => {
        $t.persisted
            .groups
            .get(&$id)
            .ok_or_else(|| anyhow!("no such group {:?}", $id))
    };
}

#[macro_export]
macro_rules! group_mut {
    ($t:expr, $id:expr) => {
        $t.persisted
            .groups
            .get_mut_cow(&$id)
            .ok_or_else(|| anyhow!("no such group {:?}", $id))
    };
}

#[macro_export]
macro_rules! group_by_name {
    ($t:expr, $name:expr) => {
        $t.persisted
            .groups_by_name
            .get($name)
            .and_then(|id| $t.persisted.groups.get(id))
            .ok_or_else(|| anyhow!("no such group {}", $name))
    };
}

#[macro_export]
macro_rules! objective {
    ($t:expr, $id:expr) => {
        $t.persisted
            .objectives
            .get(&$id)
            .ok_or_else(|| anyhow!("no such objective {:?}", $id))
    };
}

#[macro_export]
macro_rules! objective_mut {
    ($t:expr, $id:expr) => {
        $t.persisted
            .objectives
            .get_mut_cow(&$id)
            .ok_or_else(|| anyhow!("no such objective {:?}", $id))
    };
}

#[macro_export]
macro_rules! group_health {
    ($t:expr, $gid:expr) => {{
        let group = group!($t, $gid)?;
        let mut alive = 0;
        for uid in &group.units {
            if !unit!($t, uid)?.dead {
                alive += 1;
            }
        }
        Ok::<_, anyhow::Error>((alive, group.units.len()))
    }};
}

#[derive(Debug, Default)]
pub struct Db {
    pub persisted: Persisted,
    pub ephemeral: Ephemeral,
}

impl Db {
    /// Map label from `SETTINGS-aliases` when keyed by `obj.name` or naval `pad_template`.
    pub fn objective_display_name(&self, obj: &objective::Objective) -> String {
        aliases::resolve_objective_display_name(
            &self.ephemeral.objective_display_aliases,
            obj,
        )
    }

    /// Alias plus F10 map kind suffix (`Senaki ⌂ HUB`, …).
    pub fn objective_f10_map_label(&self, obj: &objective::Objective) -> String {
        aliases::resolve_objective_f10_map_label(
            &self.ephemeral.objective_display_aliases,
            obj,
        )
    }

    pub fn settings_display_name(&self, key: &str) -> String {
        aliases::resolve_settings_alias(&self.ephemeral.objective_display_aliases, key)
    }

    pub fn action_dcs_group_names(
        &self,
        gid: bfprotocols::db::group::GroupId,
    ) -> Result<Vec<dcso3::String>> {
        ai_air::dcs_spawn_names_for(self, gid)
    }

    pub fn life_type_panel_label(&self, typ: bfprotocols::cfg::LifeType) -> String {
        aliases::resolve_life_type_panel_label(
            &self.ephemeral.objective_display_aliases,
            csar::life_type_display_label(typ),
        )
    }

    pub fn objective_matches_chat_name(&self, obj: &objective::Objective, query: &str) -> bool {
        aliases::objective_matches_chat_name(
            obj,
            &self.ephemeral.objective_display_aliases,
            query,
        )
    }

    pub fn load(
        miz: &Miz,
        idx: &MizIndex,
        to_bg: UnboundedSender<Task>,
        cfg: Arc<Cfg>,
        path: &Path,
        fowl_miz_export: Arc<FowlMizExport>,
    ) -> Result<Self> {
        let file = File::open(&path)
            .map_err(|e| anyhow!("failed to open save file {:?}, {:?}", path, e))?;
        let file = zstd::stream::Decoder::new(file)?;
        let persisted: Persisted = serde_json::from_reader(file)
            .map_err(|e| anyhow!("failed to decode save file {:?}, {:?}", path, e))?;
        let mut db = Db {
            persisted,
            ephemeral: Ephemeral::default(),
        };
        ObjectiveId::setseq(max(db.persisted.oid, ObjectiveId::seq()));
        GroupId::setseq(max(db.persisted.gid, GroupId::seq()));
        UnitId::setseq(max(db.persisted.uid, UnitId::seq()));
        db.ephemeral.set_cfg(miz, idx, cfg, to_bg, fowl_miz_export)?;
        db.apply_nominal_owners_from_miz(miz)
            .context("applying nominal owners from ME zone names")?;
        Ok(db)
    }

    pub fn maybe_snapshot(&mut self) -> Option<Persisted> {
        if self.ephemeral.take_dirty() {
            self.persisted.oid = ObjectiveId::seq();
            self.persisted.gid = GroupId::seq();
            self.persisted.uid = UnitId::seq();
            Some(self.persisted.clone())
        } else {
            None
        }
    }

    pub fn ewrs(&self) -> impl Iterator<Item = (Vector3, Side, &DeployableEwr)> {
        self.persisted.ewrs.into_iter().filter_map(|gid| {
            let group = self.persisted.groups.get(gid)?;
            match &group.origin {
                DeployKind::Crate { .. }
                | DeployKind::Objective { .. }
                | DeployKind::ObjectiveDeprecated
                | DeployKind::Troop { .. } => None,
                DeployKind::Action {
                    spec:
                        Action {
                            kind: ActionKind::Awacs(AwacsCfg { ewr, .. }),
                            ..
                        },
                    ..
                }
                | DeployKind::Deployed {
                    spec: Deployable { ewr: Some(ewr), .. },
                    ..
                } => {
                    let pos = centroid3d(
                        group
                            .units
                            .into_iter()
                            .map(|u| self.persisted.units[u].position.p.0),
                    );
                    Some((pos, group.side, ewr))
                }
                DeployKind::Action { .. } | DeployKind::Deployed { .. } => None,
            }
        })
    }

    pub fn jtacs<'a>(&'a self) -> impl Iterator<Item = JtDesc> + 'a {
        self.persisted
            .jtacs
            .into_iter()
            .filter_map(|gid| {
                let group = self.persisted.groups.get(gid)?;
                let pos = centroid3d(
                    group
                        .units
                        .into_iter()
                        .filter_map(|u| self.persisted.units.get(u).map(|u| u.position.p.0)),
                );
                match &group.origin {
                    DeployKind::Troop {
                        spec:
                            Troop {
                                jtac: Some(jtac), ..
                            },
                        ..
                    }
                    | DeployKind::Deployed {
                        spec:
                            Deployable {
                                jtac: Some(jtac), ..
                            },
                        ..
                    } => Some(JtDesc {
                        pos,
                        id: JtId::Group(*gid),
                        side: group.side,
                        spec: *jtac,
                        air: false,
                    }),
                    DeployKind::Action {
                        spec:
                            Action {
                                kind: ActionKind::Drone(DroneCfg { jtac, .. }),
                                ..
                            },
                        ..
                    } => Some(JtDesc {
                        pos,
                        id: JtId::Group(*gid),
                        side: group.side,
                        spec: *jtac,
                        air: true,
                    }),
                    DeployKind::Crate { .. }
                    | DeployKind::Action { .. }
                    | DeployKind::Objective { .. }
                    | DeployKind::ObjectiveDeprecated
                    | DeployKind::Troop { .. }
                    | DeployKind::Deployed { .. } => None,
                }
            })
            .chain(self.instanced_players().filter_map(|(_, p, inst)| {
                let slot = p.current_slot.as_ref().unwrap().0;
                let pos = inst.position.p.0;
                let id = JtId::Slot(slot);
                match self.ephemeral.cfg.airborne_jtacs.get(&inst.typ) {
                    Some(jt) => Some(JtDesc {
                        pos,
                        id,
                        side: p.side,
                        spec: *jt,
                        air: true,
                    }),
                    None => match self.ephemeral.cargo.get(&slot) {
                        None => None,
                        Some(cargo) => {
                            for it in &cargo.troops {
                                if let Some(jt) = &it.troop.jtac {
                                    return Some(JtDesc {
                                        pos,
                                        id,
                                        side: p.side,
                                        spec: *jt,
                                        air: false,
                                    });
                                }
                            }
                            None
                        }
                    },
                }
            }))
    }

    pub fn flush_markup_messages(&mut self, lua: MizLua) -> Result<()> {
        self.ephemeral
            .prepare_objective_overlay_layer(&self.persisted);
        if self.ephemeral.msgs().len() == 0 {
            return Ok(());
        }
        let net = dcso3::net::Net::singleton(lua).context("net for markup flush")?;
        let act = dcso3::trigger::Trigger::singleton(lua)
            .context("trigger for markup flush")?
            .action()
            .context("action for markup flush")?;
        while self.ephemeral.msgs().len() > 0 {
            self.ephemeral.msgs().process(100, &net, &act);
        }
        Ok(())
    }
}
