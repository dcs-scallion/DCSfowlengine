use super::Db;
use crate::spawnctx::SpawnCtx;
use anyhow::{bail, Context, Result};
use compact_str::format_compact;
use bfprotocols::{
    cfg::Vehicle,
    db::objective::ObjectiveId,
    fowl_miz_export::{DepFarpStaticSideBlueprint, DepFarpStaticSlotEntry},
};
use dcso3::{
    coalition::Side,
    env::miz::{GroupKind, MizIndex, Skill},
    group::Group as DcsGroup,
    String, Vector2,
};
use log::info;
use std::sync::Arc;

fn pad_template_heading(
    spctx: &SpawnCtx,
    idx: &MizIndex,
    side: Side,
    pad_template: &str,
) -> Result<f64> {
    let pad = spctx.get_template_ref(idx, GroupKind::Any, side, pad_template)?;
    for unit in pad.group.units()? {
        let unit = unit?;
        if unit.name()?.as_str() == pad_template {
            return unit.heading();
        }
    }
    bail!("pad template {pad_template} has no anchor unit")
}

fn side_blueprint(
    export: &bfprotocols::fowl_miz_export::FowlMizExport,
    side: Side,
) -> Option<&DepFarpStaticSideBlueprint> {
    if !match side {
        Side::Blue => export.dep_farp_static_slots.enabled.blue,
        Side::Red => export.dep_farp_static_slots.enabled.red,
        Side::Neutral => return None,
    } {
        return None;
    }
    match side {
        Side::Blue => export.dep_farp_static_slots.blue.as_ref(),
        Side::Red => export.dep_farp_static_slots.red.as_ref(),
        Side::Neutral => None,
    }
}

fn pool_group_names(entry: &DepFarpStaticSlotEntry) -> Vec<&str> {
    if entry.pool_groups.is_empty() {
        vec![entry.template_group.as_str()]
    } else {
        entry.pool_groups.iter().map(|s| s.as_str()).collect()
    }
}

impl Db {
    pub(super) fn cache_dep_farp_static_slot_homes(
        &mut self,
        spctx: &SpawnCtx,
        idx: &MizIndex,
    ) -> Result<()> {
        self.ephemeral.dep_farp_static_slot_home.clear();
        self.ephemeral.dep_farp_pool_slot_ids.clear();
        let export = Arc::clone(&self.ephemeral.fowl_miz_export);
        for side in [Side::Blue, Side::Red] {
            let Some(blueprint) = side_blueprint(export.as_ref(), side) else {
                continue;
            };
            for entry in &blueprint.slots {
                for name in pool_group_names(entry) {
                    if self.ephemeral.dep_farp_static_slot_home.contains_key(name) {
                        continue;
                    }
                    let tmpl = spctx.get_template_ref(idx, GroupKind::Any, side, name)?;
                    let unit = tmpl
                        .group
                        .units()?
                        .first()
                        .context("DEP FARP static slot pool group has no units")?;
                    self.ephemeral
                        .dep_farp_static_slot_home
                        .insert(String::from(name), (unit.pos()?, unit.heading()?));
                    for unit in tmpl.group.units()? {
                        let unit = unit?;
                        if unit.skill()? != Skill::Client {
                            continue;
                        }
                        self.ephemeral
                            .dep_farp_pool_slot_ids
                            .insert(unit.slot()?);
                    }
                }
            }
        }
        Ok(())
    }

    fn take_dep_farp_pool_group(
        &mut self,
        entry: &DepFarpStaticSlotEntry,
    ) -> Result<String> {
        for name in pool_group_names(entry) {
            let key = String::from(name);
            if self
                .ephemeral
                .used_dep_farp_static_slot_groups
                .insert(key.clone())
            {
                return Ok(key);
            }
        }
        bail!(
            "no free DEP FARP static slot pool group for {}",
            entry.unit_type
        )
    }

    fn register_dep_farp_static_slot_group(
        &mut self,
        idx: &MizIndex,
        spctx: &SpawnCtx,
        side: Side,
        group_name: &str,
        oid: ObjectiveId,
        spawned_gid: dcso3::env::miz::GroupId,
    ) -> Result<()> {
        let slot = spctx.get_template_ref(idx, GroupKind::Any, side, group_name)?;
        let tpl_gid = slot.group.id()?;
        if tpl_gid != spawned_gid {
            self.ephemeral.remap_slot_miz_gid(tpl_gid, spawned_gid);
        }
        let mut template_slot_ids = Vec::new();
        let mut client_unit_names = Vec::new();
        for unit in slot.group.units()? {
            let unit = unit?;
            if unit.skill()? != Skill::Client {
                continue;
            }
            template_slot_ids.push(unit.slot()?);
            client_unit_names.push(unit.name()?);
        }
        for id in &template_slot_ids {
            self.ephemeral.dep_farp_pool_slot_ids.remove(id);
        }
        let world = DcsGroup::get_by_name(spctx.lua(), group_name).with_context(|| {
            format_compact!("DEP FARP static slot {group_name} missing after activateGroup")
        })?;
        let mut registered = 0usize;
        for unit in world.get_units()? {
            let unit = unit?;
            let unit_name = unit.get_name()?;
            if !client_unit_names.iter().any(|n| n.as_str() == unit_name.as_str()) {
                continue;
            }
            let vehicle = Vehicle::from(unit.get_type_name()?);
            self.ephemeral
                .cfg
                .check_vehicle_has_threat_distance(&vehicle)?;
            self.ephemeral.cfg.check_vehicle_has_life_type(&vehicle)?;
            let id = unit.slot()?;
            self.ephemeral.dep_farp_pool_slot_ids.remove(&id);
            self.ephemeral.slot_info.insert(
                id.clone(),
                super::ephemeral::SlotInfo {
                    typ: vehicle,
                    unit_name,
                    objective: oid,
                    ground_start: true,
                    miz_gid: spawned_gid,
                    side,
                },
            );
            self.ephemeral.slot_by_miz_gid.insert(spawned_gid, id);
            registered += 1;
            info!(
                "DEP FARP unlocked slot {id:?} on {group_name} for objective {oid:?} (TakeOffParking)"
            );
        }
        if registered == 0 {
            bail!("DEP FARP static slot {group_name} has no client units after activateGroup");
        }
        Ok(())
    }

    fn unregister_dep_farp_static_slot_group(
        &mut self,
        idx: &MizIndex,
        spctx: &SpawnCtx,
        side: Side,
        group_name: &str,
    ) -> Result<()> {
        let slot = spctx.get_template_ref(idx, GroupKind::Any, side, group_name)?;
        let tpl_gid = slot.group.id()?;
        self.ephemeral.slot_by_miz_gid.remove(&tpl_gid);
        for unit in slot.group.units()? {
            let unit = unit?;
            if unit.skill()? != Skill::Client {
                continue;
            }
            let id = unit.slot()?;
            self.ephemeral.slot_info.remove(&id);
            self.ephemeral.dep_farp_pool_slot_ids.insert(id);
        }
        let _ = idx;
        let _ = side;
        Ok(())
    }

    fn release_dep_farp_static_slot_group(
        &mut self,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        group_name: &str,
    ) -> Result<()> {
        self.unregister_dep_farp_static_slot_group(idx, spctx, side, group_name)?;
        let Some((home_pos, home_heading)) = self
            .ephemeral
            .dep_farp_static_slot_home
            .get(group_name)
            .copied()
        else {
            self.ephemeral
                .used_dep_farp_static_slot_groups
                .remove(group_name);
            return Ok(());
        };
        let slot = spctx.get_template_ref(idx, GroupKind::Any, side, group_name)?;
        slot.group.set("hidden", true)?;
        let units = slot.group.units()?;
        let anchor = units
            .get(1)
            .context("slot pool group has no units")?
            .pos()?;
        let delta = home_pos - anchor;
        for i in 1..=units.len() as i64 {
            let u = units.get(i)?;
            let p = u.pos()?;
            u.set_pos(Vector2::new(p.x + delta.x, p.y + delta.y))?;
            u.set_heading(home_heading)?;
        }
        spctx
            .deactivate_dep_farp_static_slot_pool(idx, side, group_name)
            .with_context(|| format!("returning DEP FARP slot {group_name} to pool home"))?;
        self.ephemeral
            .used_dep_farp_static_slot_groups
            .remove(group_name);
        Ok(())
    }

    pub fn drain_pending_dep_farp_static_slot_releases(
        &mut self,
        spctx: &SpawnCtx,
        idx: &MizIndex,
    ) -> Result<()> {
        if self.ephemeral.pending_dep_farp_static_slot_release.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.ephemeral.pending_dep_farp_static_slot_release);
        for (side, name) in pending {
            if let Err(e) =
                self.release_dep_farp_static_slot_group(spctx, idx, side, name.as_str())
            {
                log::warn!("failed to release DEP FARP static slot {name}: {e:?}");
            }
        }
        Ok(())
    }

    pub(super) fn release_ground_dep_farp_static_slots(
        &mut self,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        groups: &[String],
    ) -> Result<()> {
        for name in groups {
            self.release_dep_farp_static_slot_group(spctx, idx, side, name.as_str())?;
        }
        Ok(())
    }

    pub(super) fn spawn_ground_dep_farp_static_slots(
        &mut self,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        oid: ObjectiveId,
        _deploy_pos: Vector2,
        pad_template: &str,
    ) -> Result<Vec<String>> {
        let export = Arc::clone(&self.ephemeral.fowl_miz_export);
        let Some(blueprint) = side_blueprint(export.as_ref(), side) else {
            return Ok(vec![]);
        };
        if blueprint.slots.is_empty() {
            return Ok(vec![]);
        }
        let pad_heading = pad_template_heading(spctx, idx, side, pad_template)?;
        let mut assigned = Vec::with_capacity(blueprint.slots.len());
        for (slot_index, entry) in blueprint.slots.iter().enumerate() {
            let group_name = self.take_dep_farp_pool_group(entry)?;
            let helipad_name =
                crate::spawnctx::dep_farp_helipad_unit_name(pad_template, slot_index);
            let helipad_id_hint = self
                .ephemeral
                .dep_farp_pad_helipad_ids
                .get(&helipad_name)
                .copied();
            let spawned = spctx
                .activate_dep_farp_static_slot(
                    idx,
                    side,
                    group_name.as_str(),
                    pad_template,
                    slot_index,
                    pad_heading,
                    helipad_id_hint,
                )
                .with_context(|| {
                    format!(
                        "activating DEP FARP static slot {group_name} for objective {oid:?}"
                    )
                })?;
            let spawned_gid = match spawned {
                crate::spawnctx::Spawned::Group(g) => g.id()?,
                crate::spawnctx::Spawned::Static => {
                    bail!("DEP FARP static slot {group_name} spawned as static")
                }
            };
            self.register_dep_farp_static_slot_group(
                idx,
                spctx,
                side,
                group_name.as_str(),
                oid,
                spawned_gid,
            )?;
            assigned.push(group_name);
        }
        info!(
            "linked {} DEP FARP static slot(s) to helipads for objective {oid:?}",
            assigned.len()
        );
        Ok(assigned)
    }

    pub(super) fn reactivate_ground_dep_farp_static_slots(
        &mut self,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        oid: ObjectiveId,
        side: Side,
        _deploy_pos: Vector2,
        pad_template: &str,
        groups: &[String],
    ) -> Result<()> {
        let export = Arc::clone(&self.ephemeral.fowl_miz_export);
        let Some(blueprint) = side_blueprint(export.as_ref(), side) else {
            return Ok(());
        };
        let pad_heading = pad_template_heading(spctx, idx, side, pad_template)?;
        for name in groups {
            self.ephemeral
                .used_dep_farp_static_slot_groups
                .insert(name.clone());
            let (slot_index, _entry) = blueprint
                .slots
                .iter()
                .enumerate()
                .find(|(_, e)| pool_group_names(e).contains(&name.as_str()))
                .with_context(|| format!("missing export blueprint for DEP FARP slot {name}"))?;
            let helipad_name =
                crate::spawnctx::dep_farp_helipad_unit_name(pad_template, slot_index);
            let helipad_id_hint = self
                .ephemeral
                .dep_farp_pad_helipad_ids
                .get(&helipad_name)
                .copied();
            let spawned = spctx.activate_dep_farp_static_slot(
                idx,
                side,
                name.as_str(),
                pad_template,
                slot_index,
                pad_heading,
                helipad_id_hint,
            )?;
            let spawned_gid = match spawned {
                crate::spawnctx::Spawned::Group(g) => g.id()?,
                crate::spawnctx::Spawned::Static => {
                    bail!("DEP FARP static slot {name} respawned as static")
                }
            };
            self.register_dep_farp_static_slot_group(
                idx,
                spctx,
                side,
                name.as_str(),
                oid,
                spawned_gid,
            )?;
        }
        Ok(())
    }
}
