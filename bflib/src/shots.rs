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

//! Lets not bicker and argue about oo killed oo
use crate::db::{Db, group::DeployKind};
use anyhow::Result;
use bfprotocols::{
    cfg::UnitTag,
    db::group::GroupId,
    shots::{Dead, Shot, Who},
};
use chrono::{Duration, prelude::*};
use dcso3::{
    String,
    event::Shot as ShotEvent,
    net::SlotId,
    object::{DcsObject, DcsOid, Object, ObjectCategory},
    unit::{ClassUnit, Unit, UnitCategory},
};
use fxhash::FxHashMap;
use std::collections::hash_map::Entry;

#[derive(Debug, Clone, Default)]
pub struct ShotDb {
    by_target: FxHashMap<DcsOid<ClassUnit>, Vec<Shot>>,
    dead: FxHashMap<DcsOid<ClassUnit>, DateTime<Utc>>,
    recently_dead: FxHashMap<DcsOid<ClassUnit>, DateTime<Utc>>,
    last_gc: DateTime<Utc>,
}

macro_rules! ok {
    ($r:expr) => {
        match $r {
            Ok(u) => u,
            Err(_) => return Ok(()),
        }
    };
}

macro_rules! some {
    ($o:expr) => {
        match $o {
            Some(u) => u,
            None => return Ok(()),
        }
    };
}

pub(crate) fn who(db: &Db, id: DcsOid<ClassUnit>) -> Option<Who> {
    if let Some(ucid) = db.ca_controller(&id) {
        return db.player(&ucid).map(|p| {
            let slot = p
                .current_slot
                .as_ref()
                .map(|(s, _)| *s)
                .unwrap_or(SlotId::Spectator);
            Who::Player {
                side: p.side,
                slot,
                ucid,
                unit: id.clone(),
            }
        });
    }
    match db.ephemeral.get_uid_by_object_id(&id) {
        Some(uid) => db.unit(uid).ok().map(|u| Who::AI {
            side: u.side,
            gid: u.group,
            uid: *uid,
            unit: id,
            ucid: db.group(&u.group).ok().and_then(|g| match &g.origin {
                DeployKind::Action { player, .. } => *player,
                DeployKind::Deployed { player, .. } => Some(*player),
                DeployKind::Troop { player, .. } => Some(*player),
                DeployKind::Crate { .. }
                | DeployKind::Objective { .. }
                | DeployKind::ObjectiveDeprecated
                | DeployKind::CsarPilot { .. } => None,
            }),
        }),
        None => db
            .ephemeral
            .get_slot_by_object_id(&id)
            .and_then(|sl| db.ephemeral.player_in_slot(sl).map(|ucid| (sl, ucid)))
            .and_then(|(sl, ucid)| db.player(ucid).map(|p| (sl, ucid, p)))
            .map(|(sl, ucid, p)| Who::Player {
                side: p.side,
                slot: *sl,
                ucid: *ucid,
                unit: id,
            }),
    }
}

pub(crate) fn who_from_initiator(db: &Db, initiator: Option<&Object>) -> Option<Who> {
    initiator
        .filter(|o| matches!(o.get_category().ok(), Some(ObjectCategory::Unit)))
        .and_then(|i| i.as_unit().ok())
        .and_then(|u| u.object_id().ok())
        .and_then(|id| who(db, id))
        .or_else(|| {
            initiator
                .filter(|o| matches!(o.get_category().ok(), Some(ObjectCategory::Weapon)))
                .and_then(|i| i.as_weapon().ok())
                .and_then(|w| w.get_launcher().ok())
                .and_then(|u| u.object_id().ok())
                .and_then(|id| who(db, id))
        })
}

/// Unit type of shooter; Hit/Kill often has Weapon as initiator — use launcher.
pub(crate) fn shooter_typ_from_initiator(initiator: Option<&Object>) -> Option<String> {
    initiator
        .filter(|o| matches!(o.get_category().ok(), Some(ObjectCategory::Unit)))
        .and_then(|o| o.as_unit().ok())
        .and_then(|u| u.get_type_name().ok())
        .or_else(|| {
            initiator
                .filter(|o| matches!(o.get_category().ok(), Some(ObjectCategory::Weapon)))
                .and_then(|o| o.as_weapon().ok())
                .and_then(|w| w.get_launcher().ok())
                .and_then(|u| u.get_type_name().ok())
        })
        .map(|s| String::from(s.as_str()))
}

impl ShotDb {
    pub fn unit_recently_engaged(
        &self,
        target: &DcsOid<ClassUnit>,
        now: DateTime<Utc>,
        within: Duration,
    ) -> bool {
        self.by_target
            .get(target)
            .is_some_and(|shots| shots.iter().any(|s| now - s.time <= within))
    }

    pub fn group_recently_engaged(
        &self,
        gid: GroupId,
        now: DateTime<Utc>,
        within: Duration,
    ) -> bool {
        self.by_target.values().any(|shots| {
            shots
                .iter()
                .any(|s| now - s.time <= within && s.target.gid() == Some(gid))
        })
    }

    pub fn dead(&mut self, target: DcsOid<ClassUnit>, time: DateTime<Utc>) {
        if let Entry::Vacant(e) = self.dead.entry(target) {
            e.insert(time);
        }
    }

    /// Bail mid-fight: credit nearest enemy if no shot was already recorded.
    pub fn abandoned_under_threat(
        &mut self,
        target_oid: DcsOid<ClassUnit>,
        shooter: Who,
        target: Who,
        shooter_typ: Option<String>,
        target_typ: String,
        time: DateTime<Utc>,
    ) {
        if self.recently_dead.contains_key(&target_oid) {
            return;
        }
        let entry = self.by_target.entry(target_oid.clone()).or_default();
        if entry.is_empty() {
            entry.push(Shot {
                weapon_name: Some(String::from("left slot under threat")),
                weapon: None,
                shooter,
                shooter_typ,
                target,
                target_typ,
                time,
                hit: true,
            });
        }
        self.dead.entry(target_oid).or_insert(time);
    }

    pub fn shot(&mut self, db: &Db, now: DateTime<Utc>, e: &ShotEvent) -> Result<()> {
        if let Some(name) = e.weapon_name.as_ref() {
            if db.ephemeral.cfg.weapon_target_exclusions.contains(name) {
                return Ok(());
            }
        }
        // weapon.get_target() on ground-point weapons hard-crashes DCS
        // (wAmmunitionGuided::Target_ID). Allow-list air only; skip before call.
        let category = ok!(e.initiator.get_category_ex());
        if category != UnitCategory::Airplane && category != UnitCategory::Helicopter {
            return Ok(());
        }
        let initiator_oid = ok!(e.initiator.object_id());
        // CAP/modded SSM sometimes report Airplane; tags catch Artillery / non-SAM Launcher.
        // Player slots often have no persisted uid — category gate is enough for them.
        if let Some(uid) = db.ephemeral.get_uid_by_object_id(&initiator_oid) {
            let initiator_unit = ok!(db.unit(uid));
            let itags = &initiator_unit.tags.0;
            if itags.contains(UnitTag::Artillery)
                || (itags.contains(UnitTag::Launcher) && !itags.contains(UnitTag::SAM))
            {
                return Ok(());
            }
        }
        let target = ok!(some!(e.weapon.get_target()?).as_unit());
        let target_oid = target.object_id()?;
        if self.dead.contains_key(&target_oid) || self.recently_dead.contains_key(&target_oid) {
            return Ok(());
        }
        let shooter = some!(who(db, e.initiator.object_id()?));
        let target_typ = target.get_type_name()?;
        let target = some!(who(db, target_oid.clone()));
        let shooter_typ = e
            .initiator
            .get_type_name()
            .ok()
            .map(|s| dcso3::String::from(s.as_str()));
        self.by_target.entry(target_oid).or_default().push(Shot {
            weapon_name: e.weapon_name.clone(),
            weapon: Some(e.weapon.object_id()?),
            shooter,
            shooter_typ,
            target,
            target_typ,
            time: now,
            hit: false,
        });
        Ok(())
    }

    pub fn hit(
        &mut self,
        db: &Db,
        now: DateTime<Utc>,
        dead: bool,
        target: &Unit,
        shooter: &Unit,
        weapon_name: Option<String>,
    ) -> Result<()> {
        let shooter_typ = shooter
            .get_type_name()
            .ok()
            .map(|s| dcso3::String::from(s.as_str()));
        let shooter = some!(who(db, shooter.object_id()?));
        self.record_hit(db, now, dead, target, shooter, shooter_typ, weapon_name)
    }

    pub fn hit_by_who(
        &mut self,
        db: &Db,
        now: DateTime<Utc>,
        dead: bool,
        target: &Unit,
        shooter: Who,
        shooter_typ: Option<String>,
        weapon_name: Option<String>,
    ) -> Result<()> {
        self.record_hit(db, now, dead, target, shooter, shooter_typ, weapon_name)
    }

    fn record_hit(
        &mut self,
        db: &Db,
        now: DateTime<Utc>,
        dead: bool,
        target: &Unit,
        shooter: Who,
        shooter_typ: Option<dcso3::String>,
        weapon_name: Option<String>,
    ) -> Result<()> {
        let target_oid = target.object_id()?;
        if self.dead.contains_key(&target_oid) || self.recently_dead.contains_key(&target_oid) {
            return Ok(());
        }
        let target_typ = target.get_type_name()?;
        let target = some!(who(db, target_oid.clone()));
        // DCS IR/proximity self-hit (initiator == target); keep death mark, drop shot credit
        if shooter.unit() == target.unit() {
            if dead {
                self.dead.insert(target_oid, now);
            }
            return Ok(());
        }
        let shooter_typ = shooter_typ.or_else(|| {
            self.by_target.get(&target_oid).and_then(|shots| {
                shots.iter().rev().find_map(|s| {
                    if s.shooter.unit() == shooter.unit() {
                        s.shooter_typ.clone()
                    } else {
                        None
                    }
                })
            })
        });
        self.by_target
            .entry(target_oid.clone())
            .or_default()
            .push(Shot {
                weapon_name,
                weapon: None,
                shooter,
                shooter_typ,
                target,
                target_typ,
                time: now,
                hit: true,
            });
        if dead {
            self.dead.insert(target_oid, now);
        }
        Ok(())
    }

    pub fn bring_out_your_dead(&mut self, db: &Db, now: DateTime<Utc>) -> Vec<Dead> {
        let mut dead = Vec::with_capacity(self.dead.len());
        for (target, time) in self.dead.drain() {
            let entry = if let Some(shots) = self.by_target.remove(&target) {
                if !shots.is_empty() {
                    Some(Dead {
                        victim: shots[0].target.clone(),
                        time,
                        shots,
                    })
                } else {
                    who(db, target.clone()).map(|victim| Dead {
                        victim,
                        time,
                        shots: Vec::new(),
                    })
                }
            } else {
                who(db, target.clone()).map(|victim| Dead {
                    victim,
                    time,
                    shots: Vec::new(),
                })
            };
            if let Some(d) = entry {
                dead.push(d);
            }
            self.recently_dead.insert(target, time);
        }
        const FIVE_MIN: Duration = Duration::minutes(5);
        const THIRTY_MIN: Duration = Duration::minutes(30);
        self.recently_dead.retain(|_, t| now - *t <= FIVE_MIN);
        if now - self.last_gc >= THIRTY_MIN {
            self.last_gc = now;
            self.by_target.retain(|_, shots| {
                shots.retain(|shot| now - shot.time <= THIRTY_MIN);
                !shots.is_empty()
            });
        }
        dead
    }
}
