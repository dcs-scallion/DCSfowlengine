use super::{csar_spawn_names, delete_csar_marks, life_type_display_label};
use crate::db::{
    cargo::InternalPilot,
    group::{DeployKind, SpawnedGroup, SpawnedUnit},
    objective::ObjGroupClass,
    Db, SetS,
};
use anyhow::{anyhow, bail, Result};
use bfprotocols::{
    cfg::{LifeType, Vehicle},
    db::group::{GroupId, UnitId},
    stats::Stat,
};
use chrono::prelude::*;
use compact_str::{format_compact, CompactString};
use dcso3::{
    coalition::Side,
    controller::{ActionTyp, AltType, MissionPoint, PointType, Task, VehicleFormation},
    group::{Group, GroupCategory},
    land::Land,
    net::{SlotId, Ucid},
    object::{DcsObject, DcsOid},
    trigger::{SmokeColor, Trigger},
    unit::{ClassUnit, Unit},
    LuaVec2, LuaVec3, MizLua, String, Time, Vector2,
};
use log::{info, warn};
use smallvec::SmallVec;

impl Db {
    pub fn on_csar_group_destroyed(&mut self, gid: &GroupId) {
        self.ephemeral.csar_extracting.remove(gid);
        self.ephemeral.csar_capture_walk.retain(|_, p| p != gid);
        let Some(ucid) = self.csar_owner_of_group(gid) else {
            return;
        };
        let Some(player) = self.persisted.players.get_mut_cow(&ucid) else {
            return;
        };
        if let Some(idx) = player
            .csar_downed
            .iter()
            .position(|c| c.group_id.as_ref() == Some(gid))
        {
            let mut csar = player.csar_downed.remove(idx);
            delete_csar_marks(self.ephemeral.msgs(), &mut csar);
            self.ephemeral.csar_pilot_unit.remove(&csar.pilot_unit);
            self.ephemeral.dirty();
            info!("csar: downed group {gid} destroyed for {ucid:?}");
        }
    }

    pub fn on_csar_capture_squad_destroyed(&mut self, troop_gid: &GroupId) {
        self.ephemeral.csar_capture_walk.remove(troop_gid);
        self.ephemeral.csar_capture_ordered.remove(troop_gid);
        let owners = self.csar_active_ucids();
        for ucid in owners {
            let mut gids: SmallVec<[GroupId; 2]> = SmallVec::new();
            if let Some(player) = self.persisted.players.get_mut_cow(&ucid) {
                for csar in &mut player.csar_downed {
                    if csar.captured_by.as_ref() == Some(troop_gid) {
                        csar.captured = false;
                        csar.captured_by = None;
                        if let Some(pgid) = csar.group_id {
                            gids.push(pgid);
                        }
                    }
                }
            }
            for pgid in gids {
                if let Some(g) = self.persisted.groups.get_mut_cow(&pgid) {
                    if let DeployKind::CsarPilot {
                        captured,
                        captured_by,
                        ..
                    } = &mut g.origin
                    {
                        *captured = false;
                        *captured_by = None;
                    }
                }
            }
        }
    }

    pub fn try_start_csar_capture(
        &mut self,
        lua: MizLua,
        troop_gid: GroupId,
        side: Side,
        pos: Vector2,
    ) {
        if !self.csar_enabled() {
            return;
        }
        let can = matches!(
            self.persisted.groups.get(&troop_gid).map(|g| &g.origin),
            Some(DeployKind::Troop { spec: tr, .. }) if tr.can_capture_csar
        );
        if !can {
            return;
        }
        let start_r2 = (self.ephemeral.cfg.csar.capture_start_distance_m as f64).powi(2);
        let Some((_pilot_ucid, idx, pilot_gid, dist2)) =
            self.nearest_enemy_csar(side, pos, start_r2)
        else {
            return;
        };
        if dist2 <= (self.ephemeral.cfg.csar.board_distance_m as f64).powi(2) {
            self.mark_csar_captured(_pilot_ucid, idx, troop_gid);
            return;
        }
        let Ok(target) = self.group_center(&pilot_gid) else {
            return;
        };
        if self.csar_order_walk(lua, &troop_gid, target).is_ok() {
            self.ephemeral.csar_capture_ordered.insert(troop_gid);
        }
        self.ephemeral
            .csar_capture_walk
            .insert(troop_gid, pilot_gid);
        if let Some(DeployKind::Troop { player, .. }) =
            self.persisted.groups.get(&troop_gid).map(|g| &g.origin)
        {
            let msg = format_compact!(
                "squad moving to detain enemy downed pilot ({:.0}m)",
                dist2.sqrt()
            );
            self.ephemeral
                .panel_to_player(&self.persisted, 12, player, msg);
        }
    }

    pub fn csar_list_nearby(&self, lua: MizLua, slot: &SlotId) -> Result<String> {
        let (side, point) = self.csar_slot_side_pos(lua, slot)?;
        let range = self.ephemeral.cfg.csar.list_smoke_range_m as f64;
        let range2 = range.powi(2);
        let mut lines: SmallVec<[(u32, CompactString); 8]> = SmallVec::new();
        for (_, p) in self.persisted.players.into_iter() {
            if p.side != side {
                continue;
            }
            for csar in &p.csar_downed {
                if !csar.landed {
                    continue;
                }
                let pos = self.csar_pilot_world_pos(csar);
                let dist2 = na::distance_squared(&pos.into(), &point.into());
                if dist2 > range2 {
                    continue;
                }
                let dist = dist2.sqrt() as u32;
                let cap = if csar.captured { " captured" } else { "" };
                lines.push((
                    dist,
                    format_compact!(
                        "{} ({}){} — {dist}m",
                        p.name,
                        life_type_display_label(csar.life_type),
                        cap
                    ),
                ));
            }
        }
        lines.sort_by_key(|(d, _)| *d);
        if lines.is_empty() {
            Ok(format!("No friendly downed pilots within {range:.0}m").into())
        } else {
            let mut msg = String::from("Downed pilots\n----------------------------\n");
            for (_, line) in lines {
                msg.push_str(line.as_str());
                msg.push('\n');
            }
            Ok(msg)
        }
    }

    pub fn csar_request_smoke(&mut self, lua: MizLua, slot: &SlotId) -> Result<String> {
        let ucid = self
            .ephemeral
            .player_in_slot(slot)
            .cloned()
            .ok_or_else(|| anyhow!("no player in slot"))?;
        let cooldown = self.ephemeral.cfg.csar.smoke_cooldown as i64;
        let now = Utc::now();
        if let Some(last) = self.ephemeral.csar_smoke_at.get(&ucid).copied() {
            let wait = cooldown - (now - last).num_seconds();
            if wait > 0 {
                bail!("smoke on cooldown — {wait}s remaining")
            }
        }
        let (side, point) = self.csar_slot_side_pos(lua, slot)?;
        let range2 = (self.ephemeral.cfg.csar.list_smoke_range_m as f64).powi(2);
        let Some((_, _, pos, dist2, name)) = self.nearest_friendly_csar(side, point, range2, false)
        else {
            bail!("no friendly downed pilots within range")
        };
        let alt = Land::singleton(lua)?
            .get_height(LuaVec2(pos))
            .unwrap_or(0.);
        Trigger::singleton(lua)?.action()?.smoke(
            LuaVec3(na::Vector3::new(pos.x, alt + 1., pos.y)),
            SmokeColor::Green,
        )?;
        self.ephemeral.csar_smoke_at.insert(ucid, now);
        Ok(format_compact!("{name} popped green smoke — {:.0}m", dist2.sqrt()).into())
    }

    pub fn csar_extract_friendly(&mut self, lua: MizLua, slot: &SlotId) -> Result<String> {
        self.csar_check_helo_ready(lua, slot)?;
        let (side, point) = self.csar_slot_side_pos(lua, slot)?;
        let pick2 = (self.ephemeral.cfg.csar.pickup_distance_m as f64).powi(2);
        let Some((pucid, idx, _pos, dist2, name)) =
            self.nearest_friendly_csar(side, point, pick2, true)
        else {
            bail!(
                "no friendly downed pilot within {}m",
                self.ephemeral.cfg.csar.pickup_distance_m
            )
        };
        self.csar_ensure_pilot_room(lua, slot)?;
        let board2 = (self.ephemeral.cfg.csar.board_distance_m as f64).powi(2);
        if dist2 <= board2 {
            return self.csar_board_friendly(lua, slot, pucid, idx);
        }
        let Some(gid) = self
            .persisted
            .players
            .get(&pucid)
            .and_then(|p| p.csar_downed.get(idx))
            .and_then(|c| c.group_id)
        else {
            bail!("downed pilot is not on the ground yet")
        };
        self.csar_order_walk(lua, &gid, point)?;
        self.ephemeral.csar_extracting.insert(gid, *slot);
        Ok(format_compact!("{name} moving to your helicopter ({:.0}m)", dist2.sqrt()).into())
    }

    pub fn csar_extract_captured_enemy(&mut self, lua: MizLua, slot: &SlotId) -> Result<String> {
        self.csar_check_helo_ready(lua, slot)?;
        let (side, point) = self.csar_slot_side_pos(lua, slot)?;
        let pick2 = (self.ephemeral.cfg.csar.pickup_distance_m as f64).powi(2);
        let Some((pucid, idx, _gid, _dist2, name)) =
            self.nearest_captured_enemy_csar(side, point, pick2)
        else {
            bail!(
                "no captured enemy pilot within {}m",
                self.ephemeral.cfg.csar.pickup_distance_m
            )
        };
        self.csar_ensure_pilot_room(lua, slot)?;
        self.csar_board_enemy(lua, slot, pucid, idx, name)
    }

    pub fn csar_deliver(&mut self, lua: MizLua, slot: &SlotId) -> Result<String> {
        let unit = self.ephemeral.slot_instance_unit(lua, slot)?;
        if unit.in_air()? {
            bail!("you must land to deliver pilots")
        }
        let ucid = self
            .ephemeral
            .player_in_slot(slot)
            .cloned()
            .ok_or_else(|| anyhow!("no player in slot"))?;
        let player = self
            .persisted
            .players
            .get(&ucid)
            .ok_or_else(|| anyhow!("no player"))?;
        let inst = player
            .current_slot
            .as_ref()
            .and_then(|(_, i)| i.as_ref())
            .ok_or_else(|| anyhow!("not instanced"))?;
        if inst.landed_at_objective.is_none() {
            bail!("you must be at a friendly objective")
        }
        let unit_name = unit.get_name()?;
        let pilots: Vec<InternalPilot> = {
            let cargo = self
                .ephemeral
                .cargo
                .get_mut(slot)
                .ok_or_else(|| anyhow!("no cargo"))?;
            if cargo.pilots.is_empty() {
                bail!("no downed pilots onboard")
            }
            cargo.pilots.drain(..).collect()
        };
        let weight = self
            .ephemeral
            .cargo
            .get(slot)
            .map(|c| c.weight())
            .unwrap_or(0);
        Trigger::singleton(lua)?
            .action()?
            .set_unit_internal_cargo(unit_name, weight)?;
        let (pts_f, pts_e) = self
            .ephemeral
            .cfg
            .points
            .as_ref()
            .map(|p| {
                (
                    p.csar_delivery_coalition_pilot,
                    p.csar_delivery_enemy_pilot,
                )
            })
            .unwrap_or((0, 0));
        let restore = self.ephemeral.cfg.csar.restore_life_on_rescue;
        let cap = self.ephemeral.cfg.csar.restore_life_cap_at_default;
        let mut n_friendly = 0u32;
        let mut n_enemy = 0u32;
        for pilot in &pilots {
            if pilot.enemy {
                n_enemy += 1;
                self.csar_deduct_pow_life(&pilot.ucid, pilot.life_type);
                if pts_e > 0 {
                    self.adjust_points(
                        &ucid,
                        pts_e as i32,
                        &format_compact!("CSAR POW delivery of {}", pilot.name),
                    );
                }
            } else {
                n_friendly += 1;
                if restore {
                    let _ = self.csar_restore_life(&pilot.ucid, pilot.life_type, cap);
                }
                if pts_f > 0 {
                    self.adjust_points(
                        &ucid,
                        pts_f as i32,
                        &format_compact!("CSAR delivery of {}", pilot.name),
                    );
                }
            }
        }
        Ok(format_compact!(
            "delivered {n_friendly} coalition and {n_enemy} enemy pilot(s)"
        )
        .into())
    }

    pub fn cancel_csar_extract_for_slot(&mut self, slot: &SlotId) {
        self.ephemeral.csar_extracting.retain(|_, s| s != slot);
    }

    pub(super) fn maybe_adopt_landed_csar_groups(&mut self, lua: MizLua, ucid: &Ucid) {
        let Some(side) = self.persisted.players.get(ucid).map(|p| p.side) else {
            return;
        };
        let len = self
            .persisted
            .players
            .get(ucid)
            .map(|p| p.csar_downed.len())
            .unwrap_or(0);
        for idx in 0..len {
            let (landed, has_group, unit) = {
                let Some(c) = self
                    .persisted
                    .players
                    .get(ucid)
                    .and_then(|p| p.csar_downed.get(idx))
                else {
                    continue;
                };
                (
                    c.landed,
                    c.group_id
                        .and_then(|g| self.persisted.groups.get(&g))
                        .is_some(),
                    c.pilot_unit.clone(),
                )
            };
            if landed && !has_group {
                if let Err(e) = self.adopt_landed_csar_group(lua, ucid, side, idx, &unit) {
                    warn!("csar: adopt group failed for {ucid:?}: {e:?}");
                }
            }
        }
    }

    pub(super) fn expire_csar_capture_timer(&mut self, now: DateTime<Utc>) {
        let mins = self.ephemeral.cfg.csar.capture_timer;
        if mins == 0 {
            return;
        }
        let limit = chrono::Duration::minutes(mins as i64);
        for ucid in self.csar_active_ucids() {
            loop {
                let doomed = self.persisted.players.get(&ucid).and_then(|p| {
                    p.csar_downed
                        .iter()
                        .position(|c| now - c.ejected_at >= limit)
                });
                let Some(idx) = doomed else {
                    break;
                };
                let Some(csar) = self
                    .persisted
                    .players
                    .get_mut_cow(&ucid)
                    .and_then(|p| {
                        if idx < p.csar_downed.len() {
                            Some(p.csar_downed.remove(idx))
                        } else {
                            None
                        }
                    })
                else {
                    break;
                };
                self.remove_csar_entry(&csar);
                info!("csar: capture timer expired for {ucid:?}");
            }
        }
    }

    pub(super) fn tick_csar_walks(&mut self, lua: MizLua) {
        let board2 = (self.ephemeral.cfg.csar.board_distance_m as f64).powi(2);
        let pick2 = (self.ephemeral.cfg.csar.pickup_distance_m as f64).powi(2);
        let extracting: Vec<(GroupId, SlotId)> = self
            .ephemeral
            .csar_extracting
            .iter()
            .map(|(g, s)| (*g, *s))
            .collect();
        for (gid, slot) in extracting {
            let Some(helo) = self.csar_slot_point(&slot) else {
                self.ephemeral.csar_extracting.remove(&gid);
                continue;
            };
            let inst_air = self.csar_slot_in_air(&slot);
            let Ok(ppos) = self.group_center(&gid) else {
                self.ephemeral.csar_extracting.remove(&gid);
                continue;
            };
            let dist2 = na::distance_squared(&ppos.into(), &helo.into());
            if inst_air || dist2 > pick2 {
                self.ephemeral.csar_extracting.remove(&gid);
                continue;
            }
            if dist2 <= board2 {
                self.ephemeral.csar_extracting.remove(&gid);
                if let Some((ucid, idx)) = self.csar_owner_idx_of_group(&gid) {
                    if let Err(e) = self.csar_board_friendly(lua, &slot, ucid, idx) {
                        warn!("csar: auto-board failed: {e:?}");
                    }
                }
            }
        }
        let walks: Vec<(GroupId, GroupId)> = self
            .ephemeral
            .csar_capture_walk
            .iter()
            .map(|(t, p)| (*t, *p))
            .collect();
        for (troop_gid, pilot_gid) in walks {
            if !self.ephemeral.csar_capture_ordered.contains(&troop_gid) {
                if let Ok(target) = self.group_center(&pilot_gid) {
                    if self.csar_order_walk(lua, &troop_gid, target).is_ok() {
                        self.ephemeral.csar_capture_ordered.insert(troop_gid);
                    }
                }
            }
            let (Ok(tp), Ok(pp)) = (self.group_center(&troop_gid), self.group_center(&pilot_gid))
            else {
                continue;
            };
            if na::distance_squared(&tp.into(), &pp.into()) <= board2 {
                self.ephemeral.csar_capture_walk.remove(&troop_gid);
                self.ephemeral.csar_capture_ordered.remove(&troop_gid);
                if let Some((ucid, idx)) = self.csar_owner_idx_of_group(&pilot_gid) {
                    self.mark_csar_captured(ucid, idx, troop_gid);
                }
            }
        }
    }

    fn adopt_landed_csar_group(
        &mut self,
        lua: MizLua,
        ucid: &Ucid,
        side: Side,
        idx: usize,
        unit_id: &DcsOid<ClassUnit>,
    ) -> Result<()> {
        let unit = Unit::get_instance(lua, unit_id)?;
        if !unit.is_exist().unwrap_or(false) {
            bail!("pilot unit gone")
        }
        let csar = self
            .persisted
            .players
            .get(ucid)
            .and_then(|p| p.csar_downed.get(idx))
            .cloned()
            .ok_or_else(|| anyhow!("no csar entry"))?;
        let (group_name, unit_name) = csar_spawn_names(ucid, csar.ejected_at);
        let _ = unit.set_name(unit_name.clone());
        let typ = unit
            .get_type_name()
            .unwrap_or_else(|_| String::from("Soldier M4"));
        let pos3 = unit.get_position().unwrap_or(csar.inst.position);
        let pos2 = Vector2::new(pos3.p.x, pos3.p.z);
        let gid = GroupId::new();
        let uid = UnitId::new();
        let tags = self
            .ephemeral
            .cfg
            .unit_classification
            .get(typ.as_str())
            .copied()
            .unwrap_or_default();
        let mut units = SetS::new();
        units.insert_cow(uid);
        let group = SpawnedGroup {
            id: gid,
            name: group_name.clone(),
            template_name: typ.clone(),
            side,
            template_side: None,
            kind: Some(GroupCategory::Ground),
            class: ObjGroupClass::Other,
            origin: DeployKind::CsarPilot {
                ucid: *ucid,
                life_type: csar.life_type,
                captured: false,
                captured_by: None,
            },
            units,
            tags,
        };
        let spawned_unit = SpawnedUnit {
            name: unit_name.clone(),
            id: uid,
            group: gid,
            side,
            typ: Vehicle(typ),
            tags,
            template_name: group_name.clone(),
            spawn_pos: pos2,
            spawn_heading: 0.,
            spawn_position: pos3,
            pos: pos2,
            heading: 0.,
            position: pos3,
            dead: false,
            hp_percent: 100,
            static_max_life: 0,
            moved: None,
            airborne_velocity: None,
            fuel_fraction: None,
        };
        self.persisted.groups.insert_cow(gid, group);
        self.persisted.groups_by_name.insert_cow(group_name, gid);
        self.persisted
            .groups_by_side
            .get_or_default_cow(side)
            .insert_cow(gid);
        self.persisted.units.insert_cow(uid, spawned_unit);
        self.persisted.units_by_name.insert_cow(unit_name, uid);
        self.persisted.csar_pilots.insert_cow(gid);
        self.ephemeral.uid_by_object_id.insert(unit_id.clone(), uid);
        self.ephemeral.object_id_by_uid.insert(uid, unit_id.clone());
        self.ephemeral.units_able_to_move.insert(uid);
        if let Some(player) = self.persisted.players.get_mut_cow(ucid) {
            if let Some(c) = player.csar_downed.get_mut(idx) {
                c.group_id = Some(gid);
            }
        }
        self.ephemeral.dirty();
        info!("csar: adopted group {gid} for {ucid:?}");
        Ok(())
    }

    fn csar_order_walk(&self, lua: MizLua, gid: &GroupId, target: Vector2) -> Result<()> {
        let group = self
            .persisted
            .groups
            .get(gid)
            .ok_or_else(|| anyhow!("no group {gid}"))?;
        let from = self.group_center(gid)?;
        let dcs = Group::get_by_name(lua, group.name.as_str())?;
        let controller = dcs.get_controller()?;
        let land = Land::singleton(lua)?;
        let alt0 = land.get_height(LuaVec2(from)).unwrap_or(0.);
        let alt1 = land.get_height(LuaVec2(target)).unwrap_or(0.);
        controller.set_task(Task::Mission {
            airborne: Some(false),
            route: vec![
                MissionPoint {
                    action: Some(ActionTyp::Ground(VehicleFormation::OffRoad)),
                    airdrome_id: None,
                    helipad: None,
                    typ: PointType::TurningPoint,
                    link_unit: None,
                    pos: LuaVec2(from),
                    alt: alt0,
                    alt_typ: Some(AltType::BARO),
                    time_re_fu_ar: None,
                    eta: Some(Time(0.)),
                    eta_locked: Some(true),
                    speed: 3.5,
                    speed_locked: Some(true),
                    name: None,
                    parking: None,
                    task: Box::new(Task::Hold),
                },
                MissionPoint {
                    action: Some(ActionTyp::Ground(VehicleFormation::OffRoad)),
                    airdrome_id: None,
                    helipad: None,
                    typ: PointType::TurningPoint,
                    time_re_fu_ar: None,
                    link_unit: None,
                    pos: LuaVec2(target),
                    alt: alt1,
                    alt_typ: Some(AltType::BARO),
                    speed: 3.5,
                    speed_locked: Some(true),
                    eta: None,
                    eta_locked: None,
                    name: Some(String::from("csar")),
                    parking: None,
                    task: Box::new(Task::Hold),
                },
            ],
        })?;
        Ok(())
    }

    fn csar_check_helo_ready(&self, lua: MizLua, slot: &SlotId) -> Result<()> {
        let unit = self.ephemeral.slot_instance_unit(lua, slot)?;
        if self.ephemeral.cfg.csar.pickup_requires_landed && unit.in_air()? {
            bail!("you must land to extract a downed pilot")
        }
        Ok(())
    }

    fn csar_ensure_pilot_room(&self, lua: MizLua, slot: &SlotId) -> Result<()> {
        let (cap, _, _) = self.unit_cargo_cfg(slot)?;
        let ucid = self
            .ephemeral
            .player_in_slot(slot)
            .cloned()
            .ok_or_else(|| anyhow!("no player in slot"))?;
        let (crates, _) = self.fowl_crate_and_troop_slot_usage_with_bay(lua, slot, &ucid);
        let cargo = self.ephemeral.cargo.get(slot).cloned().unwrap_or_default();
        if cargo.troop_half_slots() + 1 > cap.troop_slots as usize * 2 {
            bail!("no troop capacity left for a downed pilot")
        }
        if cargo.troop_half_slots() + crates * 2 + 1 > cap.total_slots as usize * 2 {
            bail!("you already have a full load onboard")
        }
        Ok(())
    }

    fn csar_board_friendly(
        &mut self,
        lua: MizLua,
        slot: &SlotId,
        pucid: Ucid,
        idx: usize,
    ) -> Result<String> {
        let (name, life_type, side, gid, unit) = {
            let p = self
                .persisted
                .players
                .get(&pucid)
                .ok_or_else(|| anyhow!("no player"))?;
            let c = p
                .csar_downed
                .get(idx)
                .ok_or_else(|| anyhow!("no csar entry"))?;
            if c.captured {
                bail!("that pilot has been captured")
            }
            (
                p.name.clone(),
                c.life_type,
                p.side,
                c.group_id,
                c.pilot_unit.clone(),
            )
        };
        let weight = self.ephemeral.cfg.csar.downed_pilot_weight_kg;
        let mass = {
            let cargo = self.ephemeral.cargo.entry(*slot).or_default();
            cargo.pilots.push(InternalPilot {
                ucid: pucid,
                name: name.clone(),
                life_type,
                side,
                enemy: false,
                weight_kg: weight,
            });
            cargo.weight()
        };
        if let Some(unit_name) = self
            .ephemeral
            .slot_info
            .get(slot)
            .map(|s| s.unit_name.clone())
        {
            let _ = Trigger::singleton(lua)?
                .action()?
                .set_unit_internal_cargo(unit_name, mass);
        }
        let mut csar = {
            let player = self
                .persisted
                .players
                .get_mut_cow(&pucid)
                .ok_or_else(|| anyhow!("no player"))?;
            if idx >= player.csar_downed.len() {
                bail!("no csar entry")
            }
            player.csar_downed.remove(idx)
        };
        delete_csar_marks(self.ephemeral.msgs(), &mut csar);
        self.ephemeral.csar_pilot_unit.remove(&unit);
        if let Some(gid) = gid {
            self.ephemeral.csar_extracting.remove(&gid);
            if self.persisted.groups.get(&gid).is_some() {
                let _ = self.delete_group(&gid);
            }
        }
        self.ephemeral.dirty();
        Ok(format_compact!("{name} boarded").into())
    }

    fn csar_board_enemy(
        &mut self,
        lua: MizLua,
        slot: &SlotId,
        pucid: Ucid,
        idx: usize,
        name: String,
    ) -> Result<String> {
        let (life_type, side, gid, unit) = {
            let p = self
                .persisted
                .players
                .get(&pucid)
                .ok_or_else(|| anyhow!("no player"))?;
            let c = p
                .csar_downed
                .get(idx)
                .ok_or_else(|| anyhow!("no csar entry"))?;
            if !c.captured {
                bail!("enemy pilot is not detained yet")
            }
            (c.life_type, p.side, c.group_id, c.pilot_unit.clone())
        };
        let weight = self.ephemeral.cfg.csar.downed_pilot_weight_kg;
        let mass = {
            let cargo = self.ephemeral.cargo.entry(*slot).or_default();
            cargo.pilots.push(InternalPilot {
                ucid: pucid,
                name: name.clone(),
                life_type,
                side,
                enemy: true,
                weight_kg: weight,
            });
            cargo.weight()
        };
        if let Some(unit_name) = self
            .ephemeral
            .slot_info
            .get(slot)
            .map(|s| s.unit_name.clone())
        {
            let _ = Trigger::singleton(lua)?
                .action()?
                .set_unit_internal_cargo(unit_name, mass);
        }
        let mut csar = {
            let player = self
                .persisted
                .players
                .get_mut_cow(&pucid)
                .ok_or_else(|| anyhow!("no player"))?;
            if idx >= player.csar_downed.len() {
                bail!("no csar entry")
            }
            player.csar_downed.remove(idx)
        };
        delete_csar_marks(self.ephemeral.msgs(), &mut csar);
        self.ephemeral.csar_pilot_unit.remove(&unit);
        if let Some(gid) = gid {
            if self.persisted.groups.get(&gid).is_some() {
                let _ = self.delete_group(&gid);
            }
        }
        self.ephemeral.dirty();
        Ok(format_compact!("captured {name} boarded").into())
    }

    fn mark_csar_captured(&mut self, pucid: Ucid, idx: usize, troop_gid: GroupId) {
        let name = self
            .persisted
            .players
            .get(&pucid)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let mut gid = None;
        if let Some(player) = self.persisted.players.get_mut_cow(&pucid) {
            if let Some(c) = player.csar_downed.get_mut(idx) {
                c.captured = true;
                c.captured_by = Some(troop_gid);
                gid = c.group_id;
            }
        }
        if let Some(gid) = gid {
            self.ephemeral.csar_extracting.remove(&gid);
            if let Some(g) = self.persisted.groups.get_mut_cow(&gid) {
                if let DeployKind::CsarPilot {
                    captured,
                    captured_by,
                    ..
                } = &mut g.origin
                {
                    *captured = true;
                    *captured_by = Some(troop_gid);
                }
            }
        }
        if let Some(DeployKind::Troop { player, .. }) =
            self.persisted.groups.get(&troop_gid).map(|g| &g.origin)
        {
            let msg = format_compact!(
                "enemy pilot {name} detained — land within {}m and Extract captured enemy",
                self.ephemeral.cfg.csar.pickup_distance_m
            );
            self.ephemeral
                .panel_to_player(&self.persisted, 15, player, msg);
        }
        self.ephemeral.dirty();
    }

    fn csar_restore_life(
        &mut self,
        ucid: &Ucid,
        life_type: LifeType,
        _cap_at_default: bool,
    ) -> Option<u32> {
        let default = self.ephemeral.cfg.default_lives.get(&life_type)?.0;
        let player = self.persisted.players.get_mut_cow(ucid)?;
        let remaining = player.lives.get(&life_type)?.1.saturating_add(1);
        if remaining >= default {
            player.lives.remove_cow(&life_type);
        } else if let Some((_, n)) = player.lives.get_mut_cow(&life_type) {
            *n = remaining;
        }
        let lives = player.lives.clone();
        self.ephemeral.stat(Stat::Life { id: *ucid, lives });
        self.ephemeral.dirty();
        Some(remaining as u32)
    }

    fn csar_deduct_pow_life(&mut self, ucid: &Ucid, life_type: LifeType) {
        let Some(&(default, _)) = self.ephemeral.cfg.default_lives.get(&life_type) else {
            return;
        };
        {
            let Some(player) = self.persisted.players.get_mut_cow(ucid) else {
                return;
            };
            match player.lives.get(&life_type).map(|(_, n)| *n) {
                None => {
                    if default == 0 {
                        return;
                    }
                    player
                        .lives
                        .insert_cow(life_type, (Utc::now(), default.saturating_sub(1)));
                }
                Some(0) => return,
                Some(n) => {
                    if let Some((_, slot)) = player.lives.get_mut_cow(&life_type) {
                        *slot = n.saturating_sub(1);
                    }
                }
            }
        }
        if let Some(player) = self.persisted.players.get(ucid) {
            self.ephemeral.stat(Stat::Life {
                id: *ucid,
                lives: player.lives.clone(),
            });
        }
        self.ephemeral.dirty();
    }

    fn csar_slot_side_pos(&self, lua: MizLua, slot: &SlotId) -> Result<(Side, Vector2)> {
        let st = crate::db::cargo::SlotStats::get(self, lua, slot)?;
        Ok((st.side, st.point))
    }

    fn csar_slot_point(&self, slot: &SlotId) -> Option<Vector2> {
        let ucid = self.ephemeral.players_by_slot.get(slot)?;
        let p = self.persisted.players.get(ucid)?;
        let inst = p.current_slot.as_ref().and_then(|(_, i)| i.as_ref())?;
        Some(Vector2::new(inst.position.p.x, inst.position.p.z))
    }

    fn csar_slot_in_air(&self, slot: &SlotId) -> bool {
        let Some(ucid) = self.ephemeral.players_by_slot.get(slot) else {
            return true;
        };
        let Some(p) = self.persisted.players.get(ucid) else {
            return true;
        };
        p.current_slot
            .as_ref()
            .and_then(|(_, i)| i.as_ref())
            .map(|i| i.in_air)
            .unwrap_or(true)
    }

    fn csar_owner_of_group(&self, gid: &GroupId) -> Option<Ucid> {
        match &self.persisted.groups.get(gid)?.origin {
            DeployKind::CsarPilot { ucid, .. } => Some(*ucid),
            _ => None,
        }
    }

    fn csar_owner_idx_of_group(&self, gid: &GroupId) -> Option<(Ucid, usize)> {
        let ucid = self.csar_owner_of_group(gid)?;
        let p = self.persisted.players.get(&ucid)?;
        let idx = p
            .csar_downed
            .iter()
            .position(|c| c.group_id.as_ref() == Some(gid))?;
        Some((ucid, idx))
    }

    fn csar_pilot_world_pos(&self, csar: &super::CsarDowned) -> Vector2 {
        if let Some(gid) = csar.group_id {
            if let Ok(p) = self.group_center(&gid) {
                return p;
            }
        }
        Vector2::new(csar.inst.position.p.x, csar.inst.position.p.z)
    }

    fn nearest_friendly_csar(
        &self,
        side: Side,
        point: Vector2,
        max2: f64,
        extractable_only: bool,
    ) -> Option<(Ucid, usize, Vector2, f64, String)> {
        let mut best: Option<(Ucid, usize, Vector2, f64, String)> = None;
        for (ucid, p) in self.persisted.players.into_iter() {
            if p.side != side {
                continue;
            }
            for (idx, csar) in p.csar_downed.iter().enumerate() {
                if !csar.landed {
                    continue;
                }
                if extractable_only && (csar.captured || csar.group_id.is_none()) {
                    continue;
                }
                let pos = self.csar_pilot_world_pos(csar);
                let dist2 = na::distance_squared(&pos.into(), &point.into());
                if dist2 > max2 {
                    continue;
                }
                let better = best.as_ref().map(|b| dist2 < b.3).unwrap_or(true);
                if better {
                    best = Some((*ucid, idx, pos, dist2, p.name.clone()));
                }
            }
        }
        best
    }

    fn nearest_enemy_csar(
        &self,
        side: Side,
        point: Vector2,
        max2: f64,
    ) -> Option<(Ucid, usize, GroupId, f64)> {
        let mut best = None;
        for (ucid, p) in self.persisted.players.into_iter() {
            if p.side == side {
                continue;
            }
            for (idx, csar) in p.csar_downed.iter().enumerate() {
                if !csar.landed || csar.captured {
                    continue;
                }
                let Some(gid) = csar.group_id else {
                    continue;
                };
                let pos = self.csar_pilot_world_pos(csar);
                let dist2 = na::distance_squared(&pos.into(), &point.into());
                if dist2 > max2 {
                    continue;
                }
                let better = best.as_ref().map(|b: &(_, _, _, f64)| dist2 < b.3).unwrap_or(true);
                if better {
                    best = Some((*ucid, idx, gid, dist2));
                }
            }
        }
        best
    }

    fn nearest_captured_enemy_csar(
        &self,
        side: Side,
        point: Vector2,
        max2: f64,
    ) -> Option<(Ucid, usize, GroupId, f64, String)> {
        let mut best = None;
        for (ucid, p) in self.persisted.players.into_iter() {
            if p.side == side {
                continue;
            }
            for (idx, csar) in p.csar_downed.iter().enumerate() {
                if !csar.landed || !csar.captured {
                    continue;
                }
                let Some(gid) = csar.group_id else {
                    continue;
                };
                let pos = self.csar_pilot_world_pos(csar);
                let dist2 = na::distance_squared(&pos.into(), &point.into());
                if dist2 > max2 {
                    continue;
                }
                let better = best
                    .as_ref()
                    .map(|b: &(_, _, _, f64, _)| dist2 < b.3)
                    .unwrap_or(true);
                if better {
                    best = Some((*ucid, idx, gid, dist2, p.name.clone()));
                }
            }
        }
        best
    }
}
