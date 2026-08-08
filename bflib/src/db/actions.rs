use super::{Db, MapM, ai_air, objective::Objective};
use crate::{
    admin,
    db::campaign_stats::action_invest_bucket,
    db::{cargo::Oldest, group::DeployKind},
    group, group_mut,
    jtac::{JtId, Jtacs},
    objective,
    spawnctx::{SpawnCtx, SpawnLoc},
    unit,
};
use anyhow::{Context, Ok, Result, anyhow, bail};
use bfprotocols::{
    cfg::{
        Action, ActionGeoLimit, ActionKind, AiPlaneCfg, AiPlaneKind, AwacsCfg, BomberCfg,
        Deployable, DeployableCfg, DeployableKind, DroneCfg, LimitEnforceTyp, MoveCfg, NukeCfg,
        UnitTag, Vehicle,
    },
    db::{
        group::GroupId,
        objective::{ObjectiveId, ObjectiveKind},
    },
    perf::PerfInner,
    stats::Stat,
};
use chrono::{Duration, prelude::*};
use compact_str::format_compact;
use dcso3::{
    LuaVec2, LuaVec3, MizLua, String, Time, Vector2, Vector3,
    attribute::Attribute,
    centroid2d, change_heading,
    coalition::Side,
    controller::{
        ActionTyp, AiOption, AlarmState, AltType, Command, GroundOption, MissionPoint,
        OrbitPattern, PointType, Task, TurnMethod, VehicleFormation,
    },
    env::miz::{GroupKind, MizIndex},
    group::Group,
    land::Land,
    net::Ucid,
    pointing_towards2,
    trigger::{MarkId, Modulation, Trigger},
    world::World,
};
use enumflags2::BitFlags;
use fxhash::FxHashSet;
use log::error;
use rand::{Rng, thread_rng};
use smallvec::{SmallVec, smallvec};
use std::{cmp::max, f64, vec};

#[derive(Debug, Clone)]
pub struct WithPos<T> {
    pub cfg: T,
    pub pos: Vector2,
}

#[derive(Debug, Clone)]
pub struct WithObj<T> {
    pub cfg: T,
    pub oid: ObjectiveId,
}

#[derive(Debug, Clone)]
pub struct WithFromTo<T> {
    pub cfg: T,
    pub from: ObjectiveId,
    pub to: ObjectiveId,
}

#[derive(Debug, Clone)]
pub struct WithPosAndGroup<T> {
    pub cfg: T,
    pub pos: Vector2,
    pub group: GroupId,
}

#[derive(Debug, Clone)]
pub struct WithJtac<T> {
    pub cfg: T,
    pub jtac: JtId,
}

#[derive(Debug, Clone)]
pub struct WithGroup {
    pub group: GroupId,
}

#[derive(Debug, Clone)]
pub struct AiRtbArgs {
    pub group: GroupId,
    pub hub: Option<ObjectiveId>,
    pub hold: bool,
}

#[derive(Debug, Clone)]
pub enum ActionArgs {
    Tanker(WithPos<AiPlaneCfg>),
    Awacs(WithPos<AwacsCfg>),
    Bomber(WithJtac<BomberCfg>),
    CruiseMissileSpawn(WithPos<AiPlaneCfg>),
    Fighters(WithPos<AiPlaneCfg>),
    FightersWaypoint(WithPosAndGroup<()>),
    Attackers(WithPos<AiPlaneCfg>),
    AttackersWaypoint(WithPosAndGroup<()>),
    Sead(WithPos<AiPlaneCfg>),
    SeadWaypoint(WithPosAndGroup<()>),
    Drone(WithPos<DroneCfg>),
    DroneWaypoint(WithPosAndGroup<()>),
    Nuke(WithPos<NukeCfg>),
    TankerWaypoint(WithPosAndGroup<()>),
    AwacsWaypoint(WithPosAndGroup<()>),
    CruiseMissileWaypoint(WithPosAndGroup<()>),
    Paratrooper(WithPos<DeployableCfg>),
    Deployable(WithPos<DeployableCfg>),
    LogisticsRepair(WithObj<AiPlaneCfg>),
    LogisticsTransfer(WithFromTo<AiPlaneCfg>),
    Move(WithPosAndGroup<MoveCfg>),
    Rtb(AiRtbArgs),
    Start(WithGroup),
    Status(WithGroup),
    Rearm(WithGroup),
}

impl ActionArgs {
    pub fn parse(
        db: &mut Db,
        action: &ActionKind,
        lua: MizLua,
        side: Side,
        s: &str,
    ) -> Result<Self> {
        fn get_key_pos(db: &mut Db, lua: MizLua, side: Side, key: &str) -> Result<Vector2> {
            let mut found: SmallVec<[(MarkId, Vector2); 4]> = smallvec![];
            for mk in World::singleton(lua)?.get_mark_panels()? {
                let mk = mk?;
                if mk.side.is_match(&side) && mk.text.as_str() == key {
                    let pos = Vector2::new(mk.pos.0.x, mk.pos.0.z);
                    found.push((mk.id, pos));
                }
            }
            if found.len() == 0 {
                Err(anyhow!("key {key} was not found"))
            } else if found.len() > 1 {
                Err(anyhow!(
                    "key {key} was found {} times, make sure to choose a unique key",
                    found.len()
                ))
            } else {
                db.ephemeral.msgs().delete_mark(found[0].0);
                Ok(found[0].1)
            }
        }
        fn get_closest_base(db: &mut Db, lua: MizLua, side: Side, key: &str) -> Result<Vector2> {
            let mut found: SmallVec<[(MarkId, Vector2); 4]> = smallvec![];
            for mk in World::singleton(lua)?.get_mark_panels()? {
                let mk = mk?;
                if mk.side.is_match(&side) && mk.text.as_str() == key {
                    let pos = Vector2::new(mk.pos.0.x, mk.pos.0.z);
                    found.push((mk.id, pos));
                }
            }
            if found.len() == 0 {
                Err(anyhow!("key {key} was not found"))
            } else if found.len() > 1 {
                Err(anyhow!(
                    "key {key} was found {} times, make sure to choose a unique key",
                    found.len()
                ))
            } else {
                db.ephemeral.msgs().delete_mark(found[0].0);
                let key_pos = found[0].1;
                let mut min_dist = f64::MAX;
                {
                    let mut closest_base = None;
                    for (_id, obj) in db.objectives() {
                        if obj.is_airbase() {
                            let obj_pos = obj.zone.pos();
                            let dist = na::distance_squared(&obj_pos.into(), &key_pos.into());
                            if dist < min_dist {
                                min_dist = dist;
                                closest_base = Some(obj);
                            };
                        }
                    }
                    match closest_base {
                        Some(o) => Ok(o.zone.pos()),
                        None => bail!("no bases to rtb!"),
                    }
                }
            }
        }
        fn pos_group<T>(
            db: &mut Db,
            lua: MizLua,
            side: Side,
            c: T,
            s: &str,
        ) -> Result<WithPosAndGroup<T>> {
            match s.split_once(" ") {
                None => Err(anyhow!("expected <gid> <key>")),
                Some((gid, key)) => Ok(WithPosAndGroup {
                    cfg: c,
                    pos: get_key_pos(db, lua, side, key)?,
                    group: gid.parse()?,
                }),
            }
        }
        fn pos_closest_base<T>(
            db: &mut Db,
            lua: MizLua,
            side: Side,
            c: T,
            s: &str,
        ) -> Result<WithPosAndGroup<T>> {
            match s.split_once(" ") {
                None => Err(anyhow!("expected <gid> <key>")),
                Some((gid, key)) => Ok(WithPosAndGroup {
                    cfg: c,
                    pos: get_closest_base(db, lua, side, key)?,
                    group: gid.parse()?,
                }),
            }
        }
        fn pos<T>(db: &mut Db, lua: MizLua, side: Side, cfg: T, s: &str) -> Result<WithPos<T>> {
            let pos = get_key_pos(db, lua, side, s)?;
            Ok(WithPos { cfg, pos })
        }
        fn jtac<T>(cfg: T, s: &str) -> Result<WithJtac<T>> {
            Ok(WithJtac {
                cfg,
                jtac: s.parse()?,
            })
        }
        fn obj<T>(db: &Db, cfg: T, s: &str) -> Result<WithObj<T>> {
            Ok(WithObj {
                cfg,
                oid: admin::get_airbase(db, s)?,
            })
        }
        fn from_to<T>(db: &Db, cfg: T, s: &str) -> Result<WithFromTo<T>> {
            match s.split_once(" ") {
                None => Err(anyhow!("expected two objectives <from> <to>")),
                Some((from, to)) => Ok(WithFromTo {
                    cfg,
                    from: admin::get_airbase(db, from).context("getting from airbase")?,
                    to: admin::get_airbase(db, to).context("getting to airbase")?,
                }),
            }
        }
        match action.clone() {
            ActionKind::Tanker(c) => Ok(Self::Tanker(pos(db, lua, side, c, s)?)),
            ActionKind::Awacs(c) => Ok(Self::Awacs(pos(db, lua, side, c, s)?)),
            ActionKind::Fighters(c) => Ok(Self::Fighters(pos(db, lua, side, c, s)?)),
            ActionKind::FighersWaypoint => {
                Ok(Self::FightersWaypoint(pos_group(db, lua, side, (), s)?))
            }
            ActionKind::Attackers(c) => Ok(Self::Attackers(pos(db, lua, side, c, s)?)),
            ActionKind::AttackersWaypoint => {
                Ok(Self::AttackersWaypoint(pos_group(db, lua, side, (), s)?))
            }
            ActionKind::Sead(c) => Ok(Self::Sead(pos(db, lua, side, c, s)?)),
            ActionKind::SeadWaypoint => {
                Ok(Self::SeadWaypoint(pos_group(db, lua, side, (), s)?))
            }
            ActionKind::Drone(c) => Ok(Self::Drone(pos(db, lua, side, c, s)?)),
            ActionKind::DroneWaypoint => Ok(Self::DroneWaypoint(pos_group(db, lua, side, (), s)?)),
            ActionKind::Nuke(c) => Ok(Self::Nuke(pos(db, lua, side, c, s)?)),
            ActionKind::Paratrooper(c) => Ok(Self::Paratrooper(pos(db, lua, side, c, s)?)),
            ActionKind::Deployable(c) => Ok(Self::Deployable(pos(db, lua, side, c, s)?)),
            ActionKind::LogisticsRepair(c) => Ok(Self::LogisticsRepair(obj(db, c, s)?)),
            ActionKind::LogisticsTransfer(c) => Ok(Self::LogisticsTransfer(from_to(db, c, s)?)),
            ActionKind::AwacsWaypoint => Ok(Self::AwacsWaypoint(pos_group(db, lua, side, (), s)?)),
            ActionKind::TankerWaypoint => {
                Ok(Self::TankerWaypoint(pos_group(db, lua, side, (), s)?))
            }
            ActionKind::Bomber(c) => Ok(Self::Bomber(jtac(c, s)?)),
            ActionKind::Move(c) => Ok(Self::Move(pos_group(db, lua, side, c, s)?)),
            ActionKind::CruiseMissileSpawn(c) => {
                Ok(Self::CruiseMissileSpawn(pos(db, lua, side, c, s)?))
            }
            ActionKind::Rtb => {
                let parts: Vec<&str> = s.split_whitespace().collect();
                let gid: GroupId = parts
                    .first()
                    .ok_or_else(|| anyhow!("expected <gid> [base] [hold]"))?
                    .parse()?;
                let mut hold = false;
                let mut hub = None;
                for p in parts.iter().skip(1) {
                    if p.eq_ignore_ascii_case("hold") {
                        hold = true;
                    } else {
                        hub = Some(crate::admin::get_airbase(db, p)?);
                    }
                }
                Ok(Self::Rtb(AiRtbArgs { group: gid, hub, hold }))
            }
            ActionKind::Start => Ok(Self::Start(WithGroup {
                group: s.trim().parse()?,
            })),
            ActionKind::Status => Ok(Self::Status(WithGroup {
                group: s.trim().parse()?,
            })),
            ActionKind::Rearm => Ok(Self::Rearm(WithGroup {
                group: s.trim().parse()?,
            })),
            ActionKind::CruiseMissileWaypoint => Ok(Self::CruiseMissileWaypoint(pos_group(
                db,
                lua,
                side,
                (),
                s,
            )?)),
        }
    }

    fn pos(&self) -> Option<Vector2> {
        match self {
            Self::Attackers(c) => Some(c.pos),
            Self::AttackersWaypoint(c) => Some(c.pos),
            Self::Sead(c) => Some(c.pos),
            Self::SeadWaypoint(c) => Some(c.pos),
            Self::Awacs(c) => Some(c.pos),
            Self::AwacsWaypoint(c) => Some(c.pos),
            Self::CruiseMissileSpawn(c) => Some(c.pos),
            Self::CruiseMissileWaypoint(c) => Some(c.pos),
            Self::Bomber(_) => None,
            Self::Rtb(_) => None,
            Self::Start(_) => None,
            Self::Status(_) => None,
            Self::Rearm(_) => None,
            Self::Deployable(c) => Some(c.pos),
            Self::Drone(c) => Some(c.pos),
            Self::DroneWaypoint(c) => Some(c.pos),
            Self::Fighters(c) => Some(c.pos),
            Self::FightersWaypoint(c) => Some(c.pos),
            Self::LogisticsRepair(_) => None,
            Self::LogisticsTransfer(_) => None,
            Self::Move(c) => Some(c.pos),
            Self::Nuke(c) => Some(c.pos),
            Self::Paratrooper(c) => Some(c.pos),
            Self::Tanker(c) => Some(c.pos),
            Self::TankerWaypoint(c) => Some(c.pos),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActionCmd {
    pub name: String,
    pub action: Action,
    pub args: ActionArgs,
}

impl ActionCmd {
    pub fn parse(db: &mut Db, lua: MizLua, side: Side, s: &str) -> Result<Self> {
        match s.split_once(" ") {
            None => Err(anyhow!("expected <action> <args>")),
            Some((name, args)) => {
                let action = db
                    .ephemeral
                    .cfg
                    .actions
                    .get(&side)
                    .and_then(|actions| actions.get(name))
                    .ok_or_else(|| anyhow!("no such action {name}"))?
                    .clone();
                if !db.ephemeral.cfg.calcm_mission && action.kind.is_calcm_deploy() {
                    bail!("calcm missions are disabled");
                }
                let args = ActionArgs::parse(db, &action.kind, lua, side, args)?;
                Ok(Self {
                    name: name.into(),
                    action,
                    args,
                })
            }
        }
    }
}

// setup the awacs race track 90 degrees offset from the heading
// to the nearest enemy objective
fn racetrack_dist_and_heading(
    obj: &MapM<ObjectiveId, Objective>,
    pos: Vector2,
    enemy: Side,
) -> (f64, f64) {
    match Db::objective_near_point(obj, pos, |o| o.owner == enemy) {
        None => (9999999., 0.),
        Some((dist, hd, _)) => (dist, change_heading(hd, f64::consts::FRAC_PI_2)),
    }
}

fn action_group_position(db: &Db, lua: MizLua, gid: GroupId) -> Result<Vector2> {
    let names = ai_air::dcs_spawn_names_for(db, gid)?;
    ai_air::flight_center_pos(lua, &names).or_else(|_| db.group_center(&gid))
}

impl Db {
    pub fn start_action(
        &mut self,
        lua: MizLua,
        perf: &mut PerfInner,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        jtacs: &Jtacs,
        side: Side,
        ucid: Option<Ucid>,
        cmd: ActionCmd,
    ) -> Result<()> {
        self.check_actions_allowed()?;
        let cost = match &cmd.action.kind {
            ActionKind::Nuke(nc) => {
                let div = max(1, self.persisted.nukes_used * nc.cost_scale as u32);
                max(1, cmd.action.cost / div)
            }
            ActionKind::Paratrooper(p) => {
                let sq = self
                    .ephemeral
                    .deployable_idx
                    .get(&side)
                    .and_then(|idx| idx.squads_by_name.get(&p.name))
                    .ok_or_else(|| anyhow!("missin squad"))?;
                sq.cost + cmd.action.cost
            }
            ActionKind::Deployable(d) => {
                let dp = self
                    .ephemeral
                    .deployable_idx
                    .get(&side)
                    .and_then(|idx| idx.deployables_by_name.get(&d.name))
                    .ok_or_else(|| anyhow!("missing deployable"))?;
                dp.cost + cmd.action.cost
            }
            ActionKind::Move(_) => match &cmd.args {
                ActionArgs::Move(a) => {
                    let pos = self.group_center(&a.group)?;
                    let dist = na::distance(&pos.into(), &a.pos.into());
                    let group = group!(self, a.group)?;
                    let step = match &group.origin {
                        DeployKind::Deployed { .. } => a.cfg.deployable,
                        DeployKind::Objective { origin } => {
                            let obj = objective!(self, origin)?;
                            match obj.kind {
                                ObjectiveKind::Farp { mobile: true, .. } => a.cfg.deployable,
                                _ => bail!("can't move this unit type"),
                            }
                        }
                        DeployKind::Troop { .. } => a.cfg.troop,
                        _ => bail!("can't move this unit type"),
                    };
                    let steps = dist / (step as f64);
                    steps as u32 * cmd.action.cost
                }
                _ => cmd.action.cost,
            },
            _ => cmd.action.cost,
        };
        if let Some(ucid) = ucid.as_ref() {
            if !self.ephemeral.cfg.rules.actions.check(ucid) {
                bail!("you are not authorized for actions")
            }
            match self.persisted.players.get(ucid) {
                None => bail!("unknown player {ucid}"),
                Some(player) => {
                    if cost > 0 && player.points < cost as i32 {
                        bail!(
                            "{ucid}({}) this action costs {} points and you have {} points",
                            player.name,
                            cost,
                            player.points
                        )
                    }
                    if side != player.side {
                        bail!(
                            "mismatched action side {side} vs player side {}",
                            player.side
                        )
                    }
                }
            }
        }
        let n = self
            .ephemeral
            .actions_taken
            .entry(side)
            .or_default()
            .entry(cmd.name.clone())
            .or_default();
        if let Some(limit) = cmd.action.limit {
            if *n >= limit {
                bail!("{side} is out of {} actions", cmd.name)
            }
        }
        match cmd.action.geo_limit {
            ActionGeoLimit::Unlimited => (),
            ActionGeoLimit::NearFriendlyObjective { max } => {
                if let Some(pos) = cmd.args.pos()
                    && let Some((dist, _, _)) =
                        Db::objective_near_point(&self.persisted.objectives, pos, |obj| {
                            obj.owner == side
                        })
                    && dist > max as f64
                {
                    bail!(
                        "this action point is {dist} meters but can only be targeted within {max} meters of a friendly objective"
                    )
                }
            }
        }
        let name = cmd.name.clone();
        let action_kind = cmd.action.kind.clone();
        let gid = match cmd.args {
            ActionArgs::Awacs(args) => self
                .awacs(perf, spctx, idx, side, ucid.clone(), name, cmd.action, args)
                .context("calling awacs")?,
            ActionArgs::AwacsWaypoint(args) => self
                .move_awacs(spctx, side, ucid.clone(), args)
                .context("moving awacs")?,
            ActionArgs::Bomber(args) => self
                .bomber_strike(
                    perf,
                    jtacs,
                    spctx,
                    idx,
                    side,
                    ucid.clone(),
                    name,
                    cmd.action,
                    args,
                )
                .context("calling bomber strike")?,
            ActionArgs::CruiseMissileWaypoint(args) => self
                .move_cruise_missile(spctx, side, ucid.clone(), args)
                .context("moving tanker")?,
            ActionArgs::CruiseMissileSpawn(args) => self
                .cruise_missile(perf, spctx, idx, side, ucid.clone(), name, cmd.action, args)
                .context("calling cruise missile bomber")?,
            ActionArgs::Deployable(args) => self
                .ai_deploy(
                    lua,
                    perf,
                    spctx,
                    idx,
                    side,
                    ucid.clone(),
                    name,
                    cmd.action,
                    args,
                )
                .context("calling ai deployment")?,
            ActionArgs::Fighters(args) => self
                .ai_fighters(perf, spctx, idx, side, ucid.clone(), name, cmd.action, args)
                .context("calling ai fighters")?,
            ActionArgs::FightersWaypoint(args) => self
                .move_ai_fighters(spctx, side, ucid.clone(), args)
                .context("moving ai fighters")?,
            ActionArgs::Attackers(args) => {
                self.ai_attackers(perf, spctx, idx, side, ucid.clone(), name, cmd.action, args)?
            }
            ActionArgs::AttackersWaypoint(args) => {
                self.move_ai_attackers(spctx, side, ucid.clone(), args)?
            }
            ActionArgs::Sead(args) => {
                self.ai_sead(perf, spctx, idx, side, ucid.clone(), name, cmd.action, args)?
            }
            ActionArgs::SeadWaypoint(args) => {
                self.move_ai_sead(spctx, side, ucid.clone(), args)?
            }
            ActionArgs::Rtb(args) => self
                .ai_rtb(lua, spctx, idx, side, args)
                .context("rtbing unit")?,
            ActionArgs::Start(args) => {
                let ucid = ucid.ok_or_else(|| anyhow!("ucid is required for start"))?;
                self.ai_start(spctx, idx, side, &ucid, args)
                    .context("starting ai air unit")?
            }
            ActionArgs::Status(args) => {
                let ucid = ucid.ok_or_else(|| anyhow!("ucid is required for status"))?;
                self.ai_status(lua, side, &ucid, args)
                    .context("action group status")?;
                None
            }
            ActionArgs::Rearm(args) => {
                let ucid = ucid.ok_or_else(|| anyhow!("ucid is required for rearm"))?;
                self.ai_rearm(spctx, idx, side, &ucid, args)
                    .context("action group rearm")?;
                None
            }
            ActionArgs::Drone(args) => self
                .drone(perf, spctx, idx, side, ucid.clone(), name, cmd.action, args)
                .context("calling drone")?,
            ActionArgs::DroneWaypoint(args) => self
                .move_drone(spctx, side, ucid.clone(), args)
                .context("moving drone")?,
            ActionArgs::LogisticsRepair(args) => self
                .ai_logistics_repair(perf, spctx, idx, side, ucid.clone(), name, cmd.action, args)
                .context("calling ai logi repair")?,
            ActionArgs::LogisticsTransfer(args) => self
                .ai_logistics_transfer(perf, spctx, idx, side, ucid.clone(), name, cmd.action, args)
                .context("calling ai log transfer")?,
            ActionArgs::Nuke(args) => self.nuke(spctx, args).context("calling nuke")?,
            ActionArgs::Paratrooper(args) => self
                .paratroops(perf, spctx, idx, side, ucid.clone(), name, cmd.action, args)
                .context("calling paratroops")?,
            ActionArgs::Tanker(args) => self
                .tanker(perf, spctx, idx, side, ucid.clone(), name, cmd.action, args)
                .context("calling tanker")?,
            ActionArgs::TankerWaypoint(args) => self
                .move_tanker(spctx, side, ucid.clone(), args)
                .context("moving tanker")?,
            ActionArgs::Move(args) => match &ucid {
                None => bail!("ucid is required for move"),
                Some(ucid) => self
                    .move_group(spctx, side, ucid, cmd.action.penalty.unwrap_or(0), args)
                    .context("moving unit")?,
            },
        };
        if let Some(ucid) = ucid.as_ref() {
            self.ephemeral.stat(Stat::Action {
                by: *ucid,
                action: cmd.name.clone(),
                gid,
            });
            let invest = action_invest_bucket(&action_kind);
            let cost_i = cost as i32;
            self.campaign_on_invested(
                side,
                invest,
                cost,
                0,
            );
            self.adjust_points(
                ucid,
                -cost_i,
                &format!("perform action {}", cmd.name),
            );
        }
        *self
            .ephemeral
            .actions_taken
            .entry(side)
            .or_default()
            .entry(cmd.name.clone())
            .or_default() += 1;
        Ok(())
    }

    pub(super) fn respawn_action(
        &mut self,
        perf: &mut PerfInner,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        gid: GroupId,
    ) -> Result<()> {
        let now = Utc::now();
        let spawn_pos = self.group_center(&gid)?;
        let lua = spctx.lua();
        let ai_resume = {
            let group = group!(self, gid)?;
            match &group.origin {
                DeployKind::Action { ai_air, .. }
                    if ai_air.hub.is_some() && !ai_air.hub_slots.is_empty() =>
                {
                    Some((
                        group.side,
                        ai_air.phase,
                        ai_air.hub,
                        ai_air.hub_slots.clone(),
                        ai_air.active_mission.clone(),
                    ))
                }
                _ => None,
            }
        };
        if let Some((_side, raw_phase, hub_opt, hub_slots, active_snap)) = ai_resume {
            let hub_oid = hub_opt.ok_or_else(|| anyhow!("ai air missing hub on respawn"))?;
            let hub_pick = ai_air::finish_hub_pick(
                lua,
                self,
                hub_oid,
                hub_slots,
                ai_air::hub_airbase_id(self, lua, hub_oid)?,
            )?;
            let resume_air =
                ai_air::should_resume_airborne(lua, self, gid, raw_phase).unwrap_or(false);
            let phase = if resume_air {
                match raw_phase {
                    ai_air::AiAirPhase::Departing => ai_air::AiAirPhase::OnMission,
                    p => p,
                }
            } else {
                match raw_phase {
                    ai_air::AiAirPhase::Legacy
                    | ai_air::AiAirPhase::OnMission
                    | ai_air::AiAirPhase::Departing => ai_air::AiAirPhase::Bootstrap,
                    p => p,
                }
            };
            let spawn_mode = if resume_air {
                ai_air::AiAirPersistSpawn::PersistInAir
            } else {
                ai_air::AiAirPersistSpawn::PersistGround
            };
            let mission = match phase {
                ai_air::AiAirPhase::Bootstrap | ai_air::AiAirPhase::Departing => vec![],
                ai_air::AiAirPhase::RtbInbound => {
                    let slot = hub_pick
                        .slots
                        .first()
                        .ok_or_else(|| anyhow!("hub has no landing slot"))?;
                    let duration_shutdown = {
                        let group = group!(self, gid)?;
                        match &group.origin {
                            DeployKind::Action { ai_air, .. } => ai_air.duration_shutdown,
                            _ => false,
                        }
                    };
                    let mode = if duration_shutdown {
                        ai_air::HubLandMode::DurationShutdown
                    } else {
                        ai_air::HubLandMode::CycleRtb
                    };
                    ai_air::rtb_inbound_route(
                        lua,
                        &hub_pick,
                        slot,
                        active_snap.alt,
                        active_snap.alt_typ,
                        mode,
                        None,
                    )?
                }
                ai_air::AiAirPhase::OnMission => {
                    // Ingress from live position; orbit center stays active_mission.pos.
                    self.regenerate_ai_air_mission(lua, spctx, idx, gid, false)?
                }
                ai_air::AiAirPhase::AwaitingLaunch
                | ai_air::AiAirPhase::Servicing
                | ai_air::AiAirPhase::Refueling
                | ai_air::AiAirPhase::TaxiToParking
                | ai_air::AiAirPhase::ShutdownParked => vec![],
                ai_air::AiAirPhase::Legacy => vec![],
            };
            ai_air::spawn_ai_air_group(
                Some(perf),
                self,
                spctx,
                idx,
                gid,
                &hub_pick,
                spawn_mode,
            )?;
            if let DeployKind::Action { ai_air, .. } = &mut group_mut!(self, gid)?.origin {
                ai_air::set_phase(ai_air, phase);
                if resume_air {
                    ai_air.bootstrap_grounded = false;
                    ai_air.bootstrap_mission_pushed = true;
                } else if matches!(
                    phase,
                    ai_air::AiAirPhase::Bootstrap | ai_air::AiAirPhase::Departing
                ) {
                    ai_air.bootstrap_grounded = true;
                    ai_air.bootstrap_mission_pushed = false;
                }
            }
            if !mission.is_empty() {
                let airborne = resume_air
                    || !matches!(
                        phase,
                        ai_air::AiAirPhase::Bootstrap | ai_air::AiAirPhase::Departing
                    );
                self.ai_air_push_mission(spctx, gid, mission, airborne)?;
            } else if !resume_air
                && matches!(
                    phase,
                    ai_air::AiAirPhase::Bootstrap | ai_air::AiAirPhase::Departing
                )
            {
                let dcs_names = ai_air::dcs_spawn_names_for(self, gid)?;
                for (i, dcs_name) in dcs_names.iter().enumerate() {
                    let slot = hub_pick
                        .slots
                        .get(i)
                        .or(hub_pick.slots.first())
                        .ok_or_else(|| anyhow!("no hub slot"))?;
                    let route = ai_air::bootstrap_route_for_group(
                        lua,
                        self,
                        gid,
                        &hub_pick,
                        slot,
                        ai_air::BootstrapMode::ColdSpawn,
                        false,
                    )?;
                    self.ai_air_push_mission_to_name(spctx, dcs_name, route, false)?;
                }
                if let DeployKind::Action { ai_air, .. } = &mut group_mut!(self, gid)?.origin {
                    ai_air.bootstrap_mission_pushed = true;
                    ai_air.bootstrap_grounded = true;
                }
            }
            return Ok(());
        }
        let group = group!(self, gid)?;
        let side = group.side;
        if let DeployKind::Action {
            loc,
            player,
            spec,
            time,
            ..
        } = &group.origin
        {
            if let SpawnLoc::InAir { pos, .. } = loc {
                let args = WithPosAndGroup {
                    pos: *pos,
                    group: gid,
                    cfg: (),
                };
                if let ActionKind::Awacs(ai) = &spec.kind {
                    let player = *player;
                    let mission = self
                        .awacs_mission(side, player, spawn_pos, args)
                        .context("generating awacs mission")?;
                    let group = group!(self, gid)?;
                    self.ephemeral.spawn_group(
                        perf,
                        &self.persisted,
                        idx,
                        spctx,
                        group,
                        mission,
                    )?;
                    return Ok(());
                }
                if let ActionKind::Tanker(ai) = &spec.kind {
                    let player = *player;
                    let mission = self
                        .tanker_mission(side, player, spawn_pos, args)
                        .context("generate tanker mission")?;
                    let group = group!(self, gid)?;
                    self.ephemeral.spawn_group(
                        perf,
                        &self.persisted,
                        idx,
                        spctx,
                        group,
                        mission,
                    )?;
                    return Ok(());
                }
                if let ActionKind::CruiseMissileSpawn(ai) = &spec.kind {
                    let player = *player;
                    let mission = self
                        .cruise_missile_mission(side, player, spawn_pos, args)
                        .context("generate calcm mission")?;
                    let group = group!(self, gid)?;
                    self.ephemeral.spawn_group(
                        perf,
                        &self.persisted,
                        idx,
                        spctx,
                        group,
                        mission,
                    )?;
                    return Ok(());
                }
                if let ActionKind::Drone(_ai) = &spec.kind {
                    let player = *player;
                    let mission = self
                        .drone_mission_with_climb(spctx.lua(), side, player, spawn_pos, args)
                        .context("generate drone mission")?;
                    let group = group!(self, gid)?;
                    self.ephemeral.spawn_group(
                        perf,
                        &self.persisted,
                        idx,
                        spctx,
                        group,
                        mission,
                    )?;
                    return Ok(());
                }
                if let ActionKind::Fighters(ai) = &spec.kind {
                    let player = *player;
                    let mission = self
                        .ai_fighters_mission(side, player, spawn_pos, args)
                        .context("generate fighters mission")?;
                    let group = group!(self, gid)?;
                    self.ephemeral.spawn_group(
                        perf,
                        &self.persisted,
                        idx,
                        spctx,
                        group,
                        mission,
                    )?;
                    return Ok(());
                }
                if let ActionKind::Attackers(ai) = &spec.kind {
                    let player = *player;
                    let mission = self
                        .ai_attackers_mission(side, player, spawn_pos, args)
                        .context("generate ai attackers mission")?;
                    let group = group!(self, gid)?;
                    self.ephemeral.spawn_group(
                        perf,
                        &self.persisted,
                        idx,
                        spctx,
                        group,
                        mission,
                    )?;
                    return Ok(());
                }
                if let ActionKind::Sead(ai) = &spec.kind {
                    let player = *player;
                    let mission = self
                        .ai_sead_mission(side, player, spawn_pos, args)
                        .context("generate ai sead mission")?;
                    let group = group!(self, gid)?;
                    self.ephemeral.spawn_group(
                        perf,
                        &self.persisted,
                        idx,
                        spctx,
                        group,
                        mission,
                    )?;
                    return Ok(());
                }
            }
        }
        if let DeployKind::Action { ai_air, .. } = &group!(self, gid)?.origin {
            if ai_air.hub.is_some() {
                bail!("ai air {gid} could not resume from persisted state");
            }
        }
        self.delete_group(&gid)
    }

    fn drone_mission<'lua>(
        &mut self,
        side: Side,
        ucid: Option<Ucid>,
        spawn_point: Vector2,
        args: WithPosAndGroup<()>,
    ) -> Result<Vec<MissionPoint<'lua>>> {
        self.ai_loiter_point_mission(
            side,
            ucid,
            args,
            OrbitPattern::Circle,
            spawn_point,
            |k| match k {
                ActionKind::Drone(_) => true,
                _ => false,
            },
            || Task::ComboTask(vec![]),
            || vec![],
        )
    }

    fn drone_mission_with_climb<'lua>(
        &mut self,
        lua: MizLua<'lua>,
        side: Side,
        ucid: Option<Ucid>,
        spawn_point: Vector2,
        args: WithPosAndGroup<()>,
    ) -> Result<Vec<MissionPoint<'lua>>> {
        let mut route = self.drone_mission(side, ucid, spawn_point, args.clone())?;
        ai_air::insert_drone_climb_waypoints(lua, self, args.group, &mut route)?;
        Ok(route)
    }

    fn move_drone(
        &mut self,
        spctx: &SpawnCtx,
        side: Side,
        ucid: Option<Ucid>,
        args: WithPosAndGroup<()>,
    ) -> Result<Option<GroupId>> {
        let gid = args.group;
        let lua = spctx.lua();
        let pos = action_group_position(self, lua, gid).context("getting pos")?;
        let mission = self
            .drone_mission_with_climb(lua, side, ucid, pos, args)
            .context("generate drone mission")?;
        self.set_ai_mission(spctx, gid, mission, true)
            .context("setting ai mission")?;
        Ok(None)
    }

    fn drone(
        &mut self,
        perf: &mut PerfInner,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        ucid: Option<Ucid>,
        name: String,
        action: Action,
        args: WithPos<DroneCfg>,
    ) -> Result<Option<GroupId>> {
        Ok(Some(self.add_and_spawn_ai_air(
            perf,
            spctx,
            idx,
            side,
            &ucid,
            name,
            action,
            0.,
            &WithPos {
                pos: args.pos,
                cfg: args.cfg.plane,
            },
            None,
            BitFlags::empty(),
            move |db, gid, pos| {
                db.drone_mission(
                    side,
                    ucid,
                    pos,
                    WithPosAndGroup {
                        group: gid,
                        pos: args.pos,
                        cfg: (),
                    },
                )
            },
        )?))
    }

    fn ai_fighters_mission<'lua>(
        &mut self,
        side: Side,
        ucid: Option<Ucid>,
        spawn_pos: Vector2,
        args: WithPosAndGroup<()>,
    ) -> Result<Vec<MissionPoint<'lua>>> {
        let main_task = Task::EngageTargets {
            target_types: vec![
                Attribute::Fighters,
                Attribute::MultiroleFighters,
                Attribute::BattleAirplanes,
                Attribute::Battleplanes,
                Attribute::Helicopters,
                Attribute::AttackHelicopters,
            ],
            max_dist: Some(30_000.),
            max_dist_enabled: None,
            no_target_types: None,
            priority: None,
            preset_key: None,
        };
        let init_task = Task::ComboTask(vec![main_task.clone()]);
        self.ai_loiter_point_mission(
            side,
            ucid,
            args,
            OrbitPattern::Circle,
            spawn_pos,
            |k| match k {
                ActionKind::Fighters(_) => true,
                _ => false,
            },
            move || init_task.clone(),
            move || vec![main_task.clone()],
        )
    }

    fn move_ai_fighters(
        &mut self,
        spctx: &SpawnCtx,
        side: Side,
        ucid: Option<Ucid>,
        args: WithPosAndGroup<()>,
    ) -> Result<Option<GroupId>> {
        let gid = args.group;
        let pos = action_group_position(self, spctx.lua(), gid)?;
        let mission = self
            .ai_fighters_mission(side, ucid, pos, args)
            .context("generate fighters mission")?;
        self.set_ai_mission(spctx, gid, mission, true)
            .context("setting fighters mission")?;
        Ok(None)
    }

    fn ai_fighters(
        &mut self,
        perf: &mut PerfInner,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        ucid: Option<Ucid>,
        name: String,
        action: Action,
        args: WithPos<AiPlaneCfg>,
    ) -> Result<Option<GroupId>> {
        Ok(Some(self.add_and_spawn_ai_air(
            perf,
            spctx,
            idx,
            side,
            &ucid,
            name,
            action,
            0.,
            &args,
            None,
            BitFlags::empty(),
            move |db, gid, pos| {
                db.ai_fighters_mission(
                    side,
                    ucid,
                    pos,
                    WithPosAndGroup {
                        cfg: (),
                        pos: args.pos,
                        group: gid,
                    },
                )
            },
        )?))
    }

    fn ai_attackers_mission<'lua>(
        &mut self,
        side: Side,
        ucid: Option<Ucid>,
        spawn_pos: Vector2,
        args: WithPosAndGroup<()>,
    ) -> Result<Vec<MissionPoint<'lua>>> {
        ai_air::ensure_player_may_control_ai_air(self, args.group, ucid.as_ref())?;
        let group = group_mut!(self, args.group)?;
        if group.side != side {
            bail!("can't move the other team's awacs")
        }
        let (altitude, alt_typ, speed, marks, player, plane_kind) = match &mut group.origin {
            DeployKind::Action {
                marks,
                spec,
                player,
                ai_air,
                ..
            } => {
                if !matches!(spec.kind, ActionKind::Attackers(_)) {
                    bail!("this move action is not compatible with the selected group")
                }
                let ActionKind::Attackers(a) = &spec.kind else {
                    bail!("not attackers");
                };
                let kind = ai_air
                    .plane_cfg
                    .as_ref()
                    .map(|c| c.kind)
                    .unwrap_or(a.kind);
                if ai_air.phase != ai_air::AiAirPhase::Legacy {
                    for id in marks.drain() {
                        self.ephemeral.msgs().delete_mark(id);
                    }
                    ai_air.active_mission = ai_air::snapshot_from_loiter(
                        args.pos,
                        a.altitude,
                        a.altitude_typ.clone(),
                        a.speed,
                        false,
                    );
                }
                (a.altitude, a.altitude_typ.clone(), a.speed, marks, player, kind)
            }
            _ => bail!("not an action aircraft"),
        };
        let responsible = player
            .as_ref()
            .and_then(|u| self.persisted.players.get(u))
            .map(|p| p.name.clone())
            .unwrap_or(String::from(""));
        marks.insert(self.ephemeral.msgs().mark_to_side(
            side,
            args.pos,
            true,
            format_compact!(
                "{} CAS zone\nresponsible party: {}",
                args.group,
                responsible
            ),
        ));
        self.ephemeral.dirty();
        let route = ai_air::cas_patrol_mission(
            args.pos,
            spawn_pos,
            altitude,
            alt_typ,
            speed,
            plane_kind,
        );
        log::info!(
            "ai air {}: CAS route to mark [{:.0},{:.0}] ({} wpts)",
            args.group,
            args.pos.x,
            args.pos.y,
            route.len()
        );
        Ok(route)
    }

    fn ai_sead_mission<'lua>(
        &mut self,
        side: Side,
        ucid: Option<Ucid>,
        spawn_pos: Vector2,
        args: WithPosAndGroup<()>,
    ) -> Result<Vec<MissionPoint<'lua>>> {
        let main_task = Task::EngageTargets {
            target_types: vec![
                // Radar-guided SAM systems
                Attribute::SAM_SR,      // SAM Search Radar
                Attribute::SAM_TR,      // SAM Tracking Radar
                Attribute::SAM_LL,      // SAM Launcher
                Attribute::SAM_CC,      // SAM Command Center
                Attribute::SR_SAM,      // Short Range SAM
                Attribute::MR_SAM,      // Medium Range SAM
                Attribute::LR_SAM,      // Long Range SAM
                Attribute::SAMElements, // SAM elements
                Attribute::SAM,         // General SAM
                Attribute::SAMRelated,  // SAM related
                Attribute::AirDefence,  // Air Defence
                Attribute::ArmedAirDefence, // Armed Air Defence
                Attribute::AirDefenceVehicles, // Air Defence vehicles
                // EWR (Early Warning Radar) systems
                Attribute::EWR,         // Early Warning Radar
                // Static and Mobile AAA that might have radar
                Attribute::StaticAAA,   // Static AAA
                Attribute::MobileAAA,   // Mobile AAA
            ],
            max_dist: Some(15_000.), // Same range as Attackers
            max_dist_enabled: None,
            no_target_types: None,
            priority: None,
            preset_key: None,
        };
        let init_task = Task::ComboTask(vec![main_task.clone()]);
        self.ai_loiter_point_mission(
            side,
            ucid,
            args,
            OrbitPattern::Circle,
            spawn_pos,
            |k| match k {
                ActionKind::Sead(_) => true,
                _ => false,
            },
            move || init_task.clone(),
            move || vec![main_task.clone()],
        )
    }

    fn move_ai_attackers(
        &mut self,
        spctx: &SpawnCtx,
        side: Side,
        ucid: Option<Ucid>,
        args: WithPosAndGroup<()>,
    ) -> Result<Option<GroupId>> {
        let gid = args.group;
        let lua = spctx.lua();
        let spawn_pos = action_group_position(self, lua, gid)?;
        let mission = self
            .ai_attackers_mission(side, ucid, spawn_pos, args)
            .context("generate attackers mission")?;
        let in_air = ai_air::flight_any_in_air(lua, &ai_air::dcs_spawn_names_for(self, gid)?)
            .unwrap_or(false);
        if in_air {
            self.set_ai_mission(spctx, gid, mission, true)
                .context("setting ai mission")?;
        }
        Ok(None)
    }

    fn move_ai_sead(
        &mut self,
        spctx: &SpawnCtx,
        side: Side,
        ucid: Option<Ucid>,
        args: WithPosAndGroup<()>,
    ) -> Result<Option<GroupId>> {
        let gid = args.group;
        let pos = action_group_position(self, spctx.lua(), gid)?;
        let mission = self
            .ai_sead_mission(side, ucid, pos, args)
            .context("generate sead mission")?;
        self.set_ai_mission(spctx, gid, mission, true)
            .context("setting ai mission")?;
        Ok(None)
    }

    fn ai_attackers(
        &mut self,
        perf: &mut PerfInner,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        ucid: Option<Ucid>,
        name: String,
        action: Action,
        args: WithPos<AiPlaneCfg>,
    ) -> Result<Option<GroupId>> {
        Ok(Some(self.add_and_spawn_ai_air(
            perf,
            spctx,
            idx,
            side,
            &ucid,
            name,
            action,
            0.,
            &args,
            None,
            BitFlags::empty(),
            move |db, group, pos| {
                db.ai_attackers_mission(
                    side,
                    ucid,
                    pos,
                    WithPosAndGroup {
                        cfg: (),
                        pos: args.pos,
                        group,
                    },
                )
            },
        )?))
    }

    fn ai_sead(
        &mut self,
        perf: &mut PerfInner,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        ucid: Option<Ucid>,
        name: String,
        action: Action,
        args: WithPos<AiPlaneCfg>,
    ) -> Result<Option<GroupId>> {
        Ok(Some(self.add_and_spawn_ai_air(
            perf,
            spctx,
            idx,
            side,
            &ucid,
            name,
            action,
            0.,
            &args,
            None,
            BitFlags::empty(),
            move |db, group, pos| {
                db.ai_sead_mission(
                    side,
                    ucid,
                    pos,
                    WithPosAndGroup {
                        cfg: (),
                        pos: args.pos,
                        group,
                    },
                )
            },
        )?))
    }

    fn move_group(
        &mut self,
        spctx: &SpawnCtx,
        side: Side,
        ucid: &Ucid,
        penalty: u32,
        args: WithPosAndGroup<MoveCfg>,
    ) -> Result<Option<GroupId>> {
        let pos = self.group_center(&args.group)?;
        let group = group_mut!(self, args.group)?;
        if group.side != side {
            bail!("can't move an enemy unit")
        }
        self.ephemeral
            .groups_with_move_missions
            .insert(args.group, args.pos);
        for uid in &group.units {
            self.ephemeral.units_able_to_move.insert(*uid);
        }
        if penalty > 0 {
            match &mut group.origin {
                DeployKind::Deployed {
                    player, moved_by, ..
                }
                | DeployKind::Troop {
                    player, moved_by, ..
                } if ucid != player => *moved_by = Some((ucid.clone(), penalty)),
                DeployKind::Action { .. }
                | DeployKind::Crate { .. }
                | DeployKind::Objective { .. }
                | DeployKind::ObjectiveDeprecated
                | DeployKind::Troop { .. }
                | DeployKind::Deployed { .. } => (),
            }
        }
        let land = Land::singleton(spctx.lua())?;
        let alt0 = land.get_height(LuaVec2(pos))?;
        let alt1 = land.get_height(LuaVec2(args.pos))?;
        let group = Group::get_by_name(spctx.lua(), &group.name).context("getting group")?;
        let con = group.get_controller()?;
        let att = Task::EngageTargets {
            target_types: vec![
                Attribute::Fighters,
                Attribute::MultiroleFighters,
                Attribute::BattleAirplanes,
                Attribute::Battleplanes,
                Attribute::Helicopters,
                Attribute::AttackHelicopters,
                Attribute::GroundUnits,
                Attribute::GroundVehicles,
                Attribute::ArmedGroundUnits,
            ],
            max_dist: Some(2_000.),
            max_dist_enabled: None,
            no_target_types: None,
            priority: None,
            preset_key: None,
        };
        con.set_task(Task::Mission {
            airborne: Some(false),
            route: vec![
                MissionPoint {
                    action: Some(ActionTyp::Ground(VehicleFormation::OffRoad)),
                    airdrome_id: None,
                    helipad: None,
                    typ: PointType::TurningPoint,
                    link_unit: None,
                    pos: LuaVec2(pos),
                    alt: alt0,
                    alt_typ: Some(AltType::BARO),
                    time_re_fu_ar: None,
                    eta: Some(Time(0.)),
                    eta_locked: Some(true),
                    speed: 20.,
                    speed_locked: Some(true),
                    name: None,
                    parking: None,
                    task: Box::new(Task::ComboTask(vec![
                        Task::WrappedOption(AiOption::Ground(GroundOption::AlarmState(
                            AlarmState::Green,
                        ))),
                        Task::WrappedOption(AiOption::Ground(GroundOption::AlarmState(
                            AlarmState::Auto,
                        ))),
                        att.clone(),
                    ])),
                },
                MissionPoint {
                    action: Some(ActionTyp::Ground(VehicleFormation::OffRoad)),
                    airdrome_id: None,
                    helipad: None,
                    typ: PointType::TurningPoint,
                    time_re_fu_ar: None,
                    link_unit: None,
                    pos: LuaVec2(args.pos),
                    alt: alt1,
                    alt_typ: Some(AltType::BARO),
                    speed: 20.,
                    speed_locked: None,
                    eta: None,
                    eta_locked: None,
                    name: Some(String::from("move")),
                    parking: None,
                    task: Box::new(Task::ComboTask(vec![
                        Task::WrappedOption(AiOption::Ground(GroundOption::AlarmState(
                            AlarmState::Red,
                        ))),
                        att,
                    ])),
                },
            ],
        })?;
        Ok(None)
    }

    fn rtb(&mut self, spctx: &SpawnCtx, mut args: WithPosAndGroup<()>) -> Result<Option<GroupId>> {
        let gid = args.group;
        let mission = self
            .ai_rtb_mission(&mut args, || Task::ComboTask(vec![]))
            .context("generate rtb mission")?;
        self.set_ai_mission(spctx, gid, mission, true)?;
        Ok(Some(gid))
    }

    fn tanker_mission<'lua>(
        &mut self,
        side: Side,
        ucid: Option<Ucid>,
        spawn_pos: Vector2,
        args: WithPosAndGroup<()>,
    ) -> Result<Vec<MissionPoint<'lua>>> {
        let freq = match &group!(self, args.group)?.origin {
            DeployKind::Action { spec, .. } => match &spec.kind {
                ActionKind::Tanker(pl) => pl.freq,
                _ => None,
            },
            _ => None,
        };
        self.ai_loiter_point_mission(
            side,
            ucid,
            args,
            OrbitPattern::RaceTrack,
            spawn_pos,
            |k| match k {
                ActionKind::Tanker(_) => true,
                _ => false,
            },
            move || {
                Task::ComboTask(vec![
                    Task::Tanker,
                    Task::WrappedCommand(Command::SetFrequency {
                        frequency: freq.unwrap_or(264000000),
                        modulation: Modulation::AM,
                        power: 25,
                    }),
                ])
            },
            || vec![Task::Tanker],
        )
    }

    fn move_tanker(
        &mut self,
        spctx: &SpawnCtx,
        side: Side,
        ucid: Option<Ucid>,
        args: WithPosAndGroup<()>,
    ) -> Result<Option<GroupId>> {
        let gid = args.group;
        let pos = action_group_position(self, spctx.lua(), gid)?;
        let mission = self
            .tanker_mission(side, ucid, pos, args)
            .context("generate tanker mission")?;
        self.set_ai_mission(spctx, gid, mission, true)?;
        Ok(None)
    }

    fn tanker(
        &mut self,
        perf: &mut PerfInner,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        ucid: Option<Ucid>,
        name: String,
        action: Action,
        args: WithPos<AiPlaneCfg>,
    ) -> Result<Option<GroupId>> {
        Ok(Some(self.add_and_spawn_ai_air(
            perf,
            spctx,
            idx,
            side,
            &ucid,
            name,
            action,
            0.,
            &args,
            None,
            BitFlags::empty(),
            move |db, gid, pos| {
                db.tanker_mission(
                    side,
                    ucid,
                    pos,
                    WithPosAndGroup {
                        cfg: (),
                        pos: args.pos,
                        group: gid,
                    },
                )
            },
        )?))
    }

    fn move_cruise_missile<'lua>(
        &mut self,
        spctx: &SpawnCtx<'lua>,
        side: Side,
        ucid: Option<Ucid>,
        args: WithPosAndGroup<()>,
    ) -> Result<Option<GroupId>> {
        let gid = args.group;
        let pos = action_group_position(self, spctx.lua(), gid)?;
        let mission = self
            .cruise_missile_mission(side, ucid, pos, args)
            .context("generating CruiseMissile mission")?;
        self.set_ai_mission(spctx, gid, mission, true)
            .context("setting ai mission")?;
        Ok(Some(gid))
    }

    fn cruise_missile(
        &mut self,
        perf: &mut PerfInner,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        ucid: Option<Ucid>,
        name: String,
        action: Action,
        args: WithPos<AiPlaneCfg>,
    ) -> Result<Option<GroupId>> {
        let gid = self.add_and_spawn_ai_air(
            perf,
            spctx,
            idx,
            side,
            &ucid,
            name,
            action,
            0.,
            &args,
            None,
            BitFlags::empty(),
            move |db, gid, pos| {
                db.cruise_missile_mission(
                    side,
                    ucid,
                    pos,
                    WithPosAndGroup {
                        cfg: (),
                        pos: args.pos,
                        group: gid,
                    },
                )
            },
        )?;
        self.persisted.actions.insert_cow(gid);
        Ok(Some(gid))
    }

    fn paratroops(
        &mut self,
        perf: &mut PerfInner,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        ucid: Option<Ucid>,
        name: String,
        action: Action,
        args: WithPos<DeployableCfg>,
    ) -> Result<Option<GroupId>> {
        Ok(Some(
            self.add_and_spawn_ai_air(
                perf,
                spctx,
                idx,
                side,
                &ucid,
                name,
                action,
                0.,
                &WithPos {
                    pos: args.pos,
                    cfg: args
                        .cfg
                        .plane
                        .clone()
                        .ok_or_else(|| anyhow!("paratrooper missing plane config"))?,
                },
                Some(args.pos),
                BitFlags::empty(),
                |db, gid, _pos| db.ai_point_to_point_mission(gid),
            )?,
        ))
    }

    fn nuke(&mut self, spctx: &SpawnCtx, args: WithPos<NukeCfg>) -> Result<Option<GroupId>> {
        let land = Land::singleton(spctx.lua())?;
        let act = Trigger::singleton(spctx.lua())?.action()?;
        let alt = land.get_height(LuaVec2(args.pos))? + 500.;
        let pos = Vector3::new(args.pos.x, alt, args.pos.y);
        act.explosion(LuaVec3(pos), args.cfg.power as f32)?;
        self.persisted.nukes_used += 1;
        self.ephemeral.dirty();
        Ok(None)
    }

    fn ai_logistics_transfer(
        &mut self,
        perf: &mut PerfInner,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        ucid: Option<Ucid>,
        name: String,
        action: Action,
        args: WithFromTo<AiPlaneCfg>,
    ) -> Result<Option<GroupId>> {
        let from = objective!(self, args.from)?.zone.pos();
        let to = objective!(self, args.to)?.zone.pos();
        Ok(Some(self.add_and_spawn_ai_air(
            perf,
            spctx,
            idx,
            side,
            &ucid,
            name,
            action,
            0.,
            &WithPos {
                cfg: args.cfg.clone(),
                pos: from,
            },
            Some(to),
            BitFlags::empty(),
            |db, gid, _pos| db.ai_point_to_point_mission(gid),
        )?))
    }

    fn ai_logistics_repair(
        &mut self,
        perf: &mut PerfInner,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        ucid: Option<Ucid>,
        name: String,
        action: Action,
        args: WithObj<AiPlaneCfg>,
    ) -> Result<Option<GroupId>> {
        let pos = objective!(self, args.oid)?.zone.pos();
        Ok(Some(self.add_and_spawn_ai_air(
            perf,
            spctx,
            idx,
            side,
            &ucid,
            name,
            action,
            0.,
            &WithPos {
                pos,
                cfg: args.cfg.clone(),
            },
            Some(pos),
            BitFlags::empty(),
            |db, gid, _pos| db.ai_point_to_point_mission(gid),
        )?))
    }

    fn ai_deploy(
        &mut self,
        lua: MizLua,
        perf: &mut PerfInner,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        ucid: Option<Ucid>,
        name: String,
        action: Action,
        args: WithPos<DeployableCfg>,
    ) -> Result<Option<GroupId>> {
        match args.cfg.plane.as_ref() {
            Some(plane) => Ok(Some(self.add_and_spawn_ai_air(
                perf,
                spctx,
                idx,
                side,
                &ucid,
                name,
                action,
                0.,
                &WithPos {
                    cfg: plane.clone(),
                    pos: args.pos,
                },
                Some(args.pos),
                BitFlags::empty(),
                |db, gid, _pos| db.ai_point_to_point_mission(gid),
            )?)),
            None => {
                self.deployable_to_point(
                    lua,
                    idx,
                    args.pos,
                    args.cfg.name,
                    side,
                    ucid.unwrap_or_default(),
                )?;
                Ok(None)
            }
        }
    }

    fn ai_point_to_point_mission<'lua>(
        &mut self,
        gid: GroupId,
    ) -> Result<Vec<MissionPoint<'lua>>> {
        let group = group!(self, gid)?;
        let (_src, tgt, alt, alt_typ, speed) = match &group.origin {
            DeployKind::Action {
                spec,
                destination: Some(tgt),
                rtb: Some(src),
                ..
            } => match &spec.kind {
                ActionKind::Bomber(b) => (
                    *src,
                    *tgt,
                    b.plane.altitude,
                    b.plane.altitude_typ.clone(),
                    b.plane.speed,
                ),
                ActionKind::LogisticsRepair(p)
                | ActionKind::LogisticsTransfer(p)
                | ActionKind::Paratrooper(DeployableCfg {
                    name: _,
                    plane: Some(p),
                })
                | ActionKind::Deployable(DeployableCfg {
                    name: _,
                    plane: Some(p),
                }) => (*src, *tgt, p.altitude, p.altitude_typ.clone(), p.speed),
                _ => bail!("expected a point to point action"),
            },
            _ => bail!("expected action group with rtb and destination"),
        };
        let hold_orbit = Task::ComboTask(vec![Task::Orbit {
            pattern: OrbitPattern::Circle,
            point: Some(LuaVec2(tgt)),
            point2: None,
            speed: Some(speed),
            altitude: Some(alt),
        }]);
        macro_rules! wpt {
            ($name:expr, $pos:expr, $task:expr) => {
                MissionPoint {
                    action: Some(ActionTyp::Air(TurnMethod::FlyOverPoint)),
                    typ: PointType::TurningPoint,
                    airdrome_id: None,
                    helipad: None,
                    time_re_fu_ar: None,
                    link_unit: None,
                    pos: LuaVec2($pos),
                    alt,
                    alt_typ: Some(alt_typ.clone()),
                    speed,
                    eta: None,
                    speed_locked: None,
                    eta_locked: None,
                    name: Some($name.into()),
                    parking: None,
                    task: Box::new($task),
                }
            };
        }
        Ok(vec![
            wpt!("ingress", tgt, Task::ComboTask(vec![])),
            wpt!("hold", tgt, hold_orbit),
        ])
    }

    fn ai_rtb_mission<'lua>(
        &mut self,
        args: &mut WithPosAndGroup<()>,
        task: impl Fn() -> Task<'lua> + 'static,
    ) -> Result<Vec<MissionPoint<'lua>>> {
        let gid = args.group;
        let group = group!(self, gid)?;
        let mut min_dist = f64::MAX;
        let rtb_pos = {
            let mut closest_base = None;
            for (_id, obj) in self.objectives() {
                if obj.is_airbase() {
                    let obj_pos = obj.zone.pos();
                    let dist = na::distance_squared(&obj_pos.into(), &args.pos.into());
                    if dist < min_dist {
                        min_dist = dist;
                        closest_base = Some(obj);
                    };
                }
            }
            match closest_base {
                Some(o) => o.zone.pos(),
                None => bail!("no bases to rtb!"),
            }
        };

        let (alt, alt_typ, speed) = match &group.origin {
            DeployKind::Action {
                marks: _,
                loc: _,
                player: _,
                name: _,
                spec,
                time: _,
                destination: _,
                rtb: _,
                origin: _,
                ammo: _,
                ai_air: _,
                owner_lock_released: _,
            } => match &spec.kind {
                ActionKind::Tanker(ai_plane_cfg) => (
                    ai_plane_cfg.altitude,
                    ai_plane_cfg.altitude_typ.clone(),
                    ai_plane_cfg.speed,
                ),
                ActionKind::Awacs(awacs_cfg) => (
                    awacs_cfg.plane.altitude,
                    awacs_cfg.plane.altitude_typ.clone(),
                    awacs_cfg.plane.speed,
                ),
                ActionKind::Bomber(bomber_cfg) => (
                    bomber_cfg.plane.altitude,
                    bomber_cfg.plane.altitude_typ.clone(),
                    bomber_cfg.plane.speed,
                ),
                ActionKind::CruiseMissileSpawn(ai_plane_cfg) => (
                    ai_plane_cfg.altitude,
                    ai_plane_cfg.altitude_typ.clone(),
                    ai_plane_cfg.speed,
                ),
                ActionKind::Drone(drone_cfg) => (
                    drone_cfg.plane.altitude,
                    drone_cfg.plane.altitude_typ.clone(),
                    drone_cfg.plane.speed,
                ),
                ActionKind::Sead(ai_plane_cfg) => (
                    ai_plane_cfg.altitude,
                    ai_plane_cfg.altitude_typ.clone(),
                    ai_plane_cfg.speed,
                ),
                ActionKind::Fighters(ai_plane_cfg) => (
                    ai_plane_cfg.altitude,
                    ai_plane_cfg.altitude_typ.clone(),
                    ai_plane_cfg.speed,
                ),
                ActionKind::Attackers(ai_plane_cfg) => (
                    ai_plane_cfg.altitude,
                    ai_plane_cfg.altitude_typ.clone(),
                    ai_plane_cfg.speed,
                ),
                _ => bail!("not a valid type"),
            },
            _ => bail!("not the right action kind"),
        };

        let group = group_mut!(self, gid)?;

        match &mut group.origin {
            DeployKind::Action {
                marks, rtb, spec, ..
            } => {
                *rtb = Some(rtb_pos);
                (*spec).kind = ActionKind::Rtb;
                for id in marks.drain() {
                    self.ephemeral.msgs().delete_mark(id);
                }
            }
            _ => bail!("only works with some action deployed groups."),
        }

        Ok(vec![MissionPoint {
            action: Some(ActionTyp::Air(TurnMethod::FlyOverPoint)),
            typ: PointType::TurningPoint,
            airdrome_id: None,
            helipad: None,
            time_re_fu_ar: None,
            link_unit: None,
            pos: LuaVec2(rtb_pos),
            alt,
            alt_typ: Some(alt_typ.clone()),
            speed,
            eta: None,
            speed_locked: None,
            eta_locked: None,
            name: Some("rtb".to_owned().into()),
            parking: None,
            task: Box::new(task()),
        }])
    }

    fn bomber_strike(
        &mut self,
        perf: &mut PerfInner,
        jtacs: &Jtacs,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        ucid: Option<Ucid>,
        name: String,
        action: Action,
        args: WithJtac<BomberCfg>,
    ) -> Result<Option<GroupId>> {
        let jt = jtacs.get(&args.jtac)?;
        let tgt = jt
            .target()
            .as_ref()
            .map(|t| Vector2::new(t.pos.x, t.pos.z))
            .unwrap_or(jt.location().pos);
        Ok(Some(self.add_and_spawn_ai_air(
            perf,
            spctx,
            idx,
            side,
            &ucid,
            name,
            action,
            0.,
            &WithPos {
                cfg: args.cfg.plane,
                pos: tgt,
            },
            Some(tgt),
            BitFlags::empty(),
            |db, gid, _pos| db.ai_point_to_point_mission(gid),
        )?))
    }

    fn add_and_spawn_ai_air<'lua>(
        &mut self,
        perf: &mut PerfInner,
        spctx: &SpawnCtx<'lua>,
        idx: &MizIndex,
        side: Side,
        ucid: &Option<Ucid>,
        name: String,
        action: Action,
        heading: f64,
        args: &WithPos<AiPlaneCfg>,
        destination: Option<Vector2>,
        tags: BitFlags<UnitTag>,
        gen_mission: impl FnOnce(&mut Db, GroupId, Vector2) -> Result<Vec<MissionPoint<'lua>>> + 'static,
    ) -> Result<GroupId> {
        let lua = spctx.lua();
        let unit_count = {
            let tpl = spctx.get_template_ref(idx, GroupKind::Any, side, &args.cfg.template)?;
            tpl.group.units()?.len()
        };
        let hub = ai_air::select_hub_for_ai(
            lua,
            self,
            spctx,
            idx,
            side,
            args.pos,
            &args.cfg,
            unit_count,
            ai_air::HubSelectMode::Spawn,
        )?;
        let hub_pos = ai_air::hub_zone_pos(self, hub.oid)?;
        let slot0 = hub
            .slots
            .first()
            .ok_or_else(|| anyhow!("hub has no slots"))?;
        let sloc = SpawnLoc::AtPos {
            pos: slot0.pos,
            offset_direction: Vector2::new(1., 0.),
            group_heading: slot0.heading,
        };
        let snap = ai_air::snapshot_from_loiter(
            args.pos,
            args.cfg.altitude,
            args.cfg.altitude_typ.clone(),
            args.cfg.speed,
            false,
        );
        let mission_kind = ai_air::mission_kind_from_action(&action.kind);
        // Always bootstrap so units fly immediately; DCS `timeReFuAr` handles refuel/rearm.
        let spawn_phase = ai_air::AiAirPhase::Bootstrap;
        let origin = DeployKind::Action {
            marks: FxHashSet::default(),
            loc: sloc.clone(),
            player: ucid.clone(),
            name,
            spec: action,
            time: Utc::now(),
            destination,
            rtb: Some(hub_pos),
            origin: Some(hub.oid),
            ammo: 0,
            ai_air: ai_air::AiAirState {
                phase: spawn_phase,
                hub: Some(hub.oid),
                hub_slots: hub.slots.clone(),
                active_mission: snap,
                rtb_hold: false,
                bootstrap_retries: 0,
                bootstrap_grounded: false,
                bootstrap_mission_pushed: false,
                refuel_mission_pushed: false,
                dcs_spawn_names: vec![],
                plane_cfg: Some(args.cfg.clone()),
                mission_kind,
                phase_since: Utc::now(),
                duration_shutdown: false,
                attrition: false,
                aar_tanker: None,
                aar_since: None,
                servicing_handoff: false,
                last_airborne_task_push: None,
                calcm_rack_empty_since: None,
                drone_climb_pos: None,
                drone_climb_hub: None,
                drone_climb_orbit: None,
            },
            owner_lock_released: false,
        };
        let gid = self
            .add_group(
                spctx,
                idx,
                side,
                sloc,
                &args.cfg.template,
                origin,
                tags | UnitTag::Driveable,
                None,
                None,
            )
            .context("creating group")?;
        let _ = gen_mission(self, gid, hub_pos).context("generating mission for new unit")?;
        if let Err(e) = ai_air::spawn_ai_air_group(
            Some(perf),
            self,
            spctx,
            idx,
            gid,
            &hub,
            ai_air::AiAirPersistSpawn::NewDeploy,
        )
            .context("spawning ai air group")
        {
            if self.ephemeral.airbase_by_oid.get(&hub.oid).is_some() {
                let _ = self.sync_warehouse_to_objective(lua, hub.oid);
            }
            let _ = self.delete_group(&gid);
            return Err(e);
        }
        if self.ephemeral.airbase_by_oid.get(&hub.oid).is_some() {
            self.sync_warehouse_to_objective(lua, hub.oid)
                .context("syncing warehouse after ai air spawn")?;
        }
        Ok(gid)
    }

    fn awacs_mission<'lua>(
        &mut self,
        side: Side,
        ucid: Option<Ucid>,
        spawn_pos: Vector2,
        args: WithPosAndGroup<()>,
    ) -> Result<Vec<MissionPoint<'lua>>> {
        let group = group!(self, args.group)?;
        let freq = match &group.origin {
            DeployKind::Action { spec, .. } => match &spec.kind {
                ActionKind::Awacs(aw) => aw.plane.freq,
                _ => None,
            },
            _ => None,
        };
        let init_task = if group.tags.contains(UnitTag::Link16) {
            Task::ComboTask(vec![
                Task::AWACS,
                Task::WrappedCommand(Command::SetFrequency {
                    frequency: freq.unwrap_or(264000000),
                    modulation: Modulation::AM,
                    power: 25,
                }),
                Task::WrappedCommand(Command::EPLRS {
                    enable: true,
                    group: Some(dcso3::env::miz::GroupId::from(1)),
                }),
            ])
        } else {
            Task::ComboTask(vec![
                Task::AWACS,
                Task::WrappedCommand(Command::SetFrequency {
                    frequency: freq.unwrap_or(125000000),
                    modulation: Modulation::AM,
                    power: 25,
                }),
            ])
        };
        let main_task = vec![Task::AWACS];
        self.ai_loiter_point_mission(
            side,
            ucid,
            args,
            OrbitPattern::RaceTrack,
            spawn_pos,
            |k| match k {
                ActionKind::Awacs(_) => true,
                _ => false,
            },
            move || init_task.clone(),
            move || main_task.clone(),
        )
    }

    fn move_awacs<'lua>(
        &mut self,
        spctx: &SpawnCtx<'lua>,
        side: Side,
        ucid: Option<Ucid>,
        args: WithPosAndGroup<()>,
    ) -> Result<Option<GroupId>> {
        let gid = args.group;
        let pos = action_group_position(self, spctx.lua(), gid)?;
        let mission = self
            .awacs_mission(side, ucid, pos, args)
            .context("generating awacs mission")?;
        self.set_ai_mission(spctx, gid, mission, true)
            .context("setting ai mission")?;
        Ok(None)
    }

    fn awacs(
        &mut self,
        perf: &mut PerfInner,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        ucid: Option<Ucid>,
        name: String,
        action: Action,
        args: WithPos<AwacsCfg>,
    ) -> Result<Option<GroupId>> {
        Ok(Some(self.add_and_spawn_ai_air(
            perf,
            spctx,
            idx,
            side,
            &ucid,
            name,
            action,
            0.,
            &WithPos {
                cfg: args.cfg.plane,
                pos: args.pos,
            },
            None,
            UnitTag::AWACS.into(),
            move |db, gid, pos| {
                db.awacs_mission(
                    side,
                    ucid,
                    pos,
                    WithPosAndGroup {
                        cfg: (),
                        pos: args.pos,
                        group: gid,
                    },
                )
            },
        )?))
    }

    fn ai_loiter_point_mission<'lua>(
        &mut self,
        side: Side,
        ucid: Option<Ucid>,
        args: WithPosAndGroup<()>,
        pattern: OrbitPattern,
        spawn_point: Vector2,
        validator: impl Fn(&ActionKind) -> bool,
        init_task: impl Fn() -> Task<'lua> + 'static,
        main_task: impl Fn() -> Vec<Task<'lua>> + 'static,
    ) -> Result<Vec<MissionPoint<'lua>>> {
        let enemy = side.opposite();
        let heading = match pattern {
            OrbitPattern::Circle => 0.,
            OrbitPattern::RaceTrack => {
                racetrack_dist_and_heading(&self.persisted.objectives, args.pos, enemy).1
            }
            OrbitPattern::Custom(x) => bail!("invalid orbit pattern {x}"),
        };
        ai_air::ensure_player_may_control_ai_air(self, args.group, ucid.as_ref())?;
        let group = group_mut!(self, args.group)?;
        if group.side != side {
            bail!("can't move the other team's awacs")
        }
        let (altitude, alt_typ, speed, marks, player) = match &mut group.origin {
            DeployKind::Action {
                marks,
                spec,
                loc,
                player,
                ai_air,
                ..
            } => {
                if !validator(&spec.kind) {
                    bail!("this move action is not compatible with the selected group")
                }
                match &mut spec.kind {
                    ActionKind::Awacs(AwacsCfg { plane: a, .. })
                    | ActionKind::Tanker(a)
                    | ActionKind::Drone(DroneCfg { plane: a, .. })
                    | ActionKind::CruiseMissileSpawn(a)
                    | ActionKind::Fighters(a)
                    | ActionKind::Attackers(a)
                    | ActionKind::Sead(a) => {
                        match loc {
                            SpawnLoc::InAir { pos: oldpos, .. } => {
                                let dir = *oldpos - args.pos;
                                let step = dir.magnitude() / 4.;
                                let dir = dir.normalize();
                                let (old_dist, _) = racetrack_dist_and_heading(
                                    &self.persisted.objectives,
                                    *oldpos,
                                    enemy,
                                );
                                for i in 1..4 {
                                    let pos = *oldpos + dir * (step * i as f64);
                                    let (dist, _) = racetrack_dist_and_heading(
                                        &self.persisted.objectives,
                                        pos,
                                        enemy,
                                    );
                                    if old_dist < dist && dist - old_dist >= 500. {
                                        *player = ucid.clone();
                                    }
                                }
                                *oldpos = args.pos;
                                for id in marks.drain() {
                                    self.ephemeral.msgs().delete_mark(id)
                                }
                            }
                            SpawnLoc::AtPos { .. }
                            | SpawnLoc::AtPosOnShip { .. }
                            | SpawnLoc::AtPosWithCenter { .. }
                            | SpawnLoc::AtPosWithComponents { .. }
                            | SpawnLoc::AtTrigger { .. } => (),
                        }
                        if ai_air.phase != ai_air::AiAirPhase::Legacy {
                            for id in marks.drain() {
                                self.ephemeral.msgs().delete_mark(id);
                            }
                            let mark_moved = (ai_air.active_mission.pos - args.pos).magnitude()
                                > ai_air::DRONE_CLIMB_MARK_EPS_M;
                            let is_drone = ai_air.mission_kind == ai_air::AiAirMissionKind::Drone;
                            ai_air.active_mission = ai_air::snapshot_from_loiter(
                                args.pos,
                                a.altitude,
                                a.altitude_typ.clone(),
                                a.speed,
                                matches!(pattern, OrbitPattern::RaceTrack),
                            );
                            if is_drone && mark_moved {
                                ai_air::clear_drone_climb_cache(ai_air);
                            }
                        }
                        (a.altitude, a.altitude_typ.clone(), a.speed, marks, player)
                    }
                    ActionKind::AttackersWaypoint
                    | ActionKind::SeadWaypoint
                    | ActionKind::AwacsWaypoint
                    | ActionKind::DroneWaypoint
                    | ActionKind::CruiseMissileWaypoint
                    | ActionKind::TankerWaypoint
                    | ActionKind::FighersWaypoint
                    | ActionKind::Move(_)
                    | ActionKind::Rtb
                    | ActionKind::Start
                    | ActionKind::Status
                    | ActionKind::Rearm
                    | ActionKind::Deployable(_)
                    | ActionKind::Paratrooper(_)
                    | ActionKind::Bomber(_)
                    | ActionKind::Nuke(_)
                    | ActionKind::LogisticsRepair(_)
                    | ActionKind::LogisticsTransfer(_) => bail!("not a race tracker"),
                }
            }
            DeployKind::Crate { .. }
            | DeployKind::Deployed { .. }
            | DeployKind::Objective { .. }
            | DeployKind::ObjectiveDeprecated
            | DeployKind::Troop { .. } => bail!("not a race tracker"),
        };
        let responsible = player
            .as_ref()
            .and_then(|u| self.persisted.players.get(u))
            .map(|p| p.name.clone())
            .unwrap_or(String::from(""));
        let (point1, point2) = match pattern {
            OrbitPattern::Circle => {
                marks.insert(self.ephemeral.msgs().mark_to_side(
                    side,
                    args.pos,
                    true,
                    format_compact!(
                        "{} orbit point 1\nresponsible party: {}",
                        args.group,
                        responsible
                    ),
                ));
                (args.pos, None)
            }
            OrbitPattern::RaceTrack => {
                let point1 = args.pos
                    + pointing_towards2(change_heading(heading, -f64::consts::PI)) * 30_000.;
                let point2 = args.pos + pointing_towards2(heading) * 30_000.;
                marks.insert(self.ephemeral.msgs().mark_to_side(
                    side,
                    point1,
                    true,
                    format_compact!(
                        "{} race point 1\nresponsible party: {}",
                        args.group,
                        responsible
                    ),
                ));
                marks.insert(self.ephemeral.msgs().mark_to_side(
                    side,
                    point2,
                    true,
                    format_compact!(
                        "{} race point 2\nresponsible party: {}",
                        args.group,
                        responsible
                    ),
                ));
                (point1, Some(point2))
            }
            OrbitPattern::Custom(x) => bail!("invalid orbit pattern {x}"),
        };
        self.ephemeral.dirty();
        macro_rules! wpt {
            ($name:expr, $pos:expr, $task:expr) => {
                MissionPoint {
                    action: Some(ActionTyp::Air(TurnMethod::FlyOverPoint)),
                    typ: PointType::TurningPoint,
                    airdrome_id: None,
                    helipad: None,
                    time_re_fu_ar: None,
                    link_unit: None,
                    pos: LuaVec2($pos),
                    alt: altitude,
                    alt_typ: Some(alt_typ.clone()),
                    speed,
                    eta: None,
                    speed_locked: None,
                    eta_locked: None,
                    name: Some($name.into()),
                    parking: None,
                    task: Box::new($task),
                }
            };
        }
        match &pattern {
            OrbitPattern::Circle => {
                let mut tlist = vec![Task::Orbit {
                    pattern: OrbitPattern::Circle,
                    point: Some(LuaVec2(point1)),
                    point2: None,
                    speed: Some(speed),
                    altitude: Some(altitude),
                }];
                for t in main_task() {
                    tlist.push(t);
                }
                Ok(vec![
                    wpt!("ip", spawn_point, init_task()),
                    wpt!("orbit", point1, Task::ComboTask(tlist)),
                ])
            }
            OrbitPattern::RaceTrack => {
                let mut tlist = vec![Task::Orbit {
                    pattern: OrbitPattern::RaceTrack,
                    point: Some(LuaVec2(point1)),
                    point2: Some(LuaVec2(point2.unwrap())),
                    speed: Some(speed),
                    altitude: Some(altitude),
                }];
                for t in main_task() {
                    tlist.push(t);
                }
                Ok(vec![
                    wpt!("ip", spawn_point, init_task()),
                    wpt!("point1", point1, Task::ComboTask(tlist.clone())),
                    wpt!("point2", point2.unwrap(), Task::ComboTask(tlist)),
                ])
            }
            OrbitPattern::Custom(x) => bail!("invalid orbit pattern {x}"),
        }
    }

    fn cruise_missile_mission<'lua>(
        &mut self,
        side: Side,
        ucid: Option<Ucid>,
        spawn_pos: Vector2,
        args: WithPosAndGroup<()>,
    ) -> Result<Vec<MissionPoint<'lua>>> {
        let group = group!(self, args.group)?;
        let init_task = Task::ComboTask(vec![]);
        let main_task = if group.tags.contains(UnitTag::Link16) {
            vec![]
        } else {
            vec![]
        };
        self.ai_loiter_point_mission(
            side,
            ucid,
            args,
            OrbitPattern::Circle,
            spawn_pos,
            |k| match k {
                ActionKind::CruiseMissileSpawn(_) => true,
                _ => false,
            },
            move || init_task.clone(),
            move || main_task.clone(),
        )
    }

    fn push_mission_to_dcs_group<'lua>(
        spctx: &SpawnCtx<'lua>,
        dcs_name: &str,
        mission: Vec<MissionPoint<'lua>>,
        airborne: bool,
    ) -> Result<()> {
        let group = Group::get_by_name(spctx.lua(), dcs_name)?;
        let unit = group.get_unit(1).context("getting unit 1")?;
        if !unit.is_exist()? {
            return Ok(());
        }
        let con = group.get_controller().context("getting controller")?;
        ai_air::apply_fowl_air_controller_options(&con)?;
        con.set_task(Task::Mission {
            airborne: Some(airborne),
            route: mission,
        })
        .context("setting mission")
    }

    fn set_ai_mission<'lua>(
        &mut self,
        spctx: &SpawnCtx<'lua>,
        gid: GroupId,
        mission: Vec<MissionPoint<'lua>>,
        airborne: bool,
    ) -> Result<()> {
        let names = ai_air::dcs_spawn_names_for(self, gid)?;
        for dcs_name in &names {
            Self::push_mission_to_dcs_group(spctx, dcs_name, mission.clone(), airborne)?;
        }
        Ok(())
    }

    pub(super) fn ai_air_push_mission_to_name<'lua>(
        &mut self,
        spctx: &SpawnCtx<'lua>,
        dcs_name: &str,
        mission: Vec<MissionPoint<'lua>>,
        airborne: bool,
    ) -> Result<()> {
        Self::push_mission_to_dcs_group(spctx, dcs_name, mission, airborne)
    }

    pub(super) fn ai_air_push_mission<'lua>(
        &mut self,
        spctx: &SpawnCtx<'lua>,
        gid: GroupId,
        mission: Vec<MissionPoint<'lua>>,
        airborne: bool,
    ) -> Result<()> {
        self.set_ai_mission(spctx, gid, mission, airborne)
    }

    pub(super) fn regenerate_ai_air_mission<'lua>(
        &mut self,
        lua: MizLua<'lua>,
        spctx: &SpawnCtx<'lua>,
        idx: &MizIndex,
        gid: GroupId,
        resume_airborne: bool,
    ) -> Result<Vec<MissionPoint<'lua>>> {
        let group = group!(self, gid)?;
        let side = group.side;
        let (mission_pos, mission_kind) = match &group.origin {
            DeployKind::Action { ai_air, .. } => (ai_air.active_mission.pos, ai_air.mission_kind),
            _ => bail!("not action"),
        };
        let spawn_pos = if resume_airborne {
            mission_pos
        } else if matches!(
            mission_kind,
            ai_air::AiAirMissionKind::Attackers | ai_air::AiAirMissionKind::Sead
        ) && mission_pos != Vector2::default()
        {
            mission_pos
        } else {
            ai_air::flight_center_pos(lua, &ai_air::dcs_spawn_names_for(self, gid)?)?
        };
        let args = WithPosAndGroup {
            cfg: (),
            pos: mission_pos,
            group: gid,
        };
        let DeployKind::Action { spec, player, .. } = &group.origin else {
            bail!("not action");
        };
        let ucid = player.clone();
        match &spec.kind {
            ActionKind::Fighters(_) => self.ai_fighters_mission(side, ucid, spawn_pos, args),
            ActionKind::Attackers(_) => self.ai_attackers_mission(side, ucid, spawn_pos, args),
            ActionKind::Sead(_) => self.ai_sead_mission(side, ucid, spawn_pos, args),
            ActionKind::Tanker(_) => self.tanker_mission(side, ucid, spawn_pos, args),
            ActionKind::Awacs(_) => self.awacs_mission(side, ucid, spawn_pos, args),
            ActionKind::Drone(_) => self.drone_mission_with_climb(lua, side, ucid, spawn_pos, args),
            ActionKind::CruiseMissileSpawn(_) => {
                self.cruise_missile_mission(side, ucid, spawn_pos, args)
            }
            ActionKind::FighersWaypoint
            | ActionKind::AttackersWaypoint
            | ActionKind::SeadWaypoint
            | ActionKind::AwacsWaypoint
            | ActionKind::DroneWaypoint
            | ActionKind::CruiseMissileWaypoint
            | ActionKind::TankerWaypoint => {
                let args = WithPosAndGroup {
                    cfg: (),
                    pos: mission_pos,
                    group: gid,
                };
                match ai_air::ai_air_mission_kind(self, gid) {
                    ai_air::AiAirMissionKind::Fighters => {
                        self.ai_fighters_mission(side, ucid, spawn_pos, args)
                    }
                    ai_air::AiAirMissionKind::Attackers => {
                        self.ai_attackers_mission(side, ucid, spawn_pos, args)
                    }
                    ai_air::AiAirMissionKind::Sead => self.ai_sead_mission(side, ucid, spawn_pos, args),
                    ai_air::AiAirMissionKind::Tanker => self.tanker_mission(side, ucid, spawn_pos, args),
                    ai_air::AiAirMissionKind::Awacs => self.awacs_mission(side, ucid, spawn_pos, args),
                    ai_air::AiAirMissionKind::Drone => {
                        self.drone_mission_with_climb(lua, side, ucid, spawn_pos, args)
                    }
                    ai_air::AiAirMissionKind::CruiseMissileSpawn => {
                        self.cruise_missile_mission(side, ucid, spawn_pos, args)
                    }
                    ai_air::AiAirMissionKind::PointToPoint => {
                        self.ai_point_to_point_mission(gid)
                    }
                    ai_air::AiAirMissionKind::Unknown => {
                        let DeployKind::Action { ai_air, .. } = &group.origin else {
                            bail!("not action");
                        };
                        Ok(ai_air::cap_orbit_mission(
                            &ai_air.active_mission,
                            spawn_pos,
                            vec![],
                        ))
                    }
                }
            }
            ActionKind::Bomber(_)
            | ActionKind::Nuke(_)
            | ActionKind::Deployable(_)
            | ActionKind::Paratrooper(_)
            | ActionKind::LogisticsRepair(_)
            | ActionKind::LogisticsTransfer(_) => {
                self.ai_point_to_point_mission(gid)
            }
            ActionKind::Rtb | ActionKind::Start | ActionKind::Status | ActionKind::Rearm => {
                let args = WithPosAndGroup {
                    cfg: (),
                    pos: mission_pos,
                    group: gid,
                };
                match ai_air::ai_air_mission_kind(self, gid) {
                    ai_air::AiAirMissionKind::Fighters => {
                        self.ai_fighters_mission(side, ucid, spawn_pos, args)
                    }
                    ai_air::AiAirMissionKind::Attackers => {
                        self.ai_attackers_mission(side, ucid, spawn_pos, args)
                    }
                    ai_air::AiAirMissionKind::Sead => self.ai_sead_mission(side, ucid, spawn_pos, args),
                    ai_air::AiAirMissionKind::Tanker => self.tanker_mission(side, ucid, spawn_pos, args),
                    ai_air::AiAirMissionKind::Awacs => self.awacs_mission(side, ucid, spawn_pos, args),
                    ai_air::AiAirMissionKind::Drone => {
                        self.drone_mission_with_climb(lua, side, ucid, spawn_pos, args)
                    }
                    ai_air::AiAirMissionKind::CruiseMissileSpawn => {
                        self.cruise_missile_mission(side, ucid, spawn_pos, args)
                    }
                    ai_air::AiAirMissionKind::PointToPoint => {
                        self.ai_point_to_point_mission(gid)
                    }
                    ai_air::AiAirMissionKind::Unknown => {
                        let DeployKind::Action { ai_air, .. } = &group.origin else {
                            bail!("not action");
                        };
                        Ok(ai_air::cap_orbit_mission(
                            &ai_air.active_mission,
                            spawn_pos,
                            vec![],
                        ))
                    }
                }
            }
            _ => bail!("no mission regen for this action kind"),
        }
    }

    fn ai_rtb(
        &mut self,
        lua: MizLua,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        args: AiRtbArgs,
    ) -> Result<Option<GroupId>> {
        ai_air::issue_rtb(
            self,
            lua,
            spctx,
            idx,
            side,
            ai_air::RtbRequest {
                group: args.group,
                hub: args.hub,
                hold: args.hold,
                preserve_mission_kind: true,
                duration_shutdown: false,
            },
        )?;
        Ok(Some(args.group))
    }

    fn ai_start(
        &mut self,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        ucid: &Ucid,
        args: WithGroup,
    ) -> Result<Option<GroupId>> {
        let lua = spctx.lua();
        ai_air::issue_start(self, lua, spctx, idx, args.group, side, ucid)?;
        Ok(Some(args.group))
    }

    fn ai_status(
        &mut self,
        lua: MizLua,
        side: Side,
        ucid: &Ucid,
        args: WithGroup,
    ) -> Result<()> {
        ai_air::issue_status(self, lua, args.group, side, ucid)
    }

    fn ai_rearm(
        &mut self,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        ucid: &Ucid,
        args: WithGroup,
    ) -> Result<()> {
        let lua = spctx.lua();
        ai_air::issue_rearm(self, lua, spctx, idx, args.group, side, ucid)
    }

    fn bomb_targets(
        &self,
        lua: MizLua,
        side: Side,
        jtacs: &Jtacs,
        cfg: &BomberCfg,
        target: Vector2,
    ) -> Result<()> {
        let mut rng = thread_rng();
        let land = Land::singleton(lua)?;
        let act = Trigger::singleton(lua)?.action()?;
        for (i, (_, ct)) in jtacs.contacts_near_point(side, target, 15_000.).enumerate() {
            if i < cfg.targets as usize {
                let dir = Vector2::new(rng.gen_range(0. ..1.), rng.gen_range(0. ..1.)).normalize();
                let mag = rng.gen_range(0. ..cfg.accuracy as f64);
                let pos = Vector2::new(ct.pos.x, ct.pos.z) + dir * mag;
                let alt = land.get_height(LuaVec2(pos))?;
                let pos = Vector3::new(pos.x, alt, pos.y);
                act.explosion(LuaVec3(pos), cfg.power as f32)?
            }
        }
        Ok(())
    }

    fn repair_target(&mut self, target: Vector2, ucid: Option<Ucid>, side: Side) -> Result<()> {
        let (dist, _, obj) =
            Self::objective_near_point(&self.persisted.objectives, target, |o| o.owner == side)
                .ok_or_else(|| anyhow!("no friendly objective near drop off point"))?;
        if dist > 5_000. {
            bail!("no friendly objective near drop off point")
        }
        let oid = obj.id;
        if let Some(ucid) = ucid {
            self.ephemeral.stat(Stat::Repair { id: oid, by: ucid });
        }
        self.repair_one_logi_step(side, Utc::now(), oid)?;
        Ok(())
    }

    fn transfer_to_target(
        &mut self,
        lua: MizLua,
        src: Vector2,
        target: Vector2,
        ucid: Option<Ucid>,
        side: Side,
    ) -> Result<()> {
        let (dist, _, src) =
            Self::objective_near_point(&self.persisted.objectives, src, |o| o.owner == side)
                .ok_or_else(|| anyhow!("no friendly objective near source point"))?;
        if dist > 5_000. {
            bail!("no friendly objective near source point")
        }
        let (dist, _, tgt) =
            Self::objective_near_point(&self.persisted.objectives, target, |o| o.owner == side)
                .ok_or_else(|| anyhow!("no friendly objective near target point"))?;
        if dist > 5_000. {
            bail!("no friendly objective near target point")
        }
        let src = src.id;
        let tgt = tgt.id;
        if let Some(ucid) = ucid {
            self.ephemeral.stat(Stat::SupplyTransfer {
                from: src,
                to: tgt,
                by: ucid,
            });
        }
        self.transfer_supplies(lua, src, tgt)
    }

    /// Carrier-style objective deploy (boat pad): `-action` must not delete the oldest instance (teleport exploit).
    fn deployable_action_denies_at_limit(
        &self,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        spec: &Deployable,
    ) -> bool {
        let DeployableKind::Objective(parts) = &spec.kind else {
            return false;
        };
        parts.pad_templates.iter().any(|pad| {
            spctx
                .get_template_ref(idx, GroupKind::Any, side, pad.as_str())
                .ok()
                .and_then(|gifo| gifo.group.units().ok())
                .and_then(|units| units.first().ok())
                .and_then(|u| u.typ().ok())
                .and_then(|typ| self.ephemeral.cfg.unit_classification.get(&Vehicle(typ)))
                .is_some_and(|tags| tags.contains(UnitTag::Boat))
        })
    }

    fn deployable_to_point(
        &mut self,
        lua: MizLua,
        idx: &MizIndex,
        pos: Vector2,
        dep: String,
        side: Side,
        ucid: Ucid,
    ) -> Result<()> {
        let spec = self
            .ephemeral
            .deployable_idx
            .get(&side)
            .ok_or_else(|| anyhow!("no such deployable {dep} for {side}"))?
            .deployables_by_name
            .get(dep.as_str())
            .ok_or_else(|| anyhow!("no such deployable {dep} for {side}"))?
            .clone();
        let dep_key = spec
            .path
            .last()
            .map(|s| s.as_str())
            .unwrap_or(dep.as_str());
        let spctx = SpawnCtx::new(lua)?;
        let (n, oldest) = self.number_deployed(side, dep_key)?;
        if n >= spec.limit as usize {
            if self.deployable_action_denies_at_limit(&spctx, idx, side, &spec) {
                bail!(
                    "the maximum number of {} are already deployed ({}/{})",
                    dep_key,
                    n,
                    spec.limit
                );
            }
            match spec.limit_enforce {
                LimitEnforceTyp::DenyCrate => {
                    bail!(
                        "the maximum number of {} are already deployed ({}/{})",
                        dep_key,
                        n,
                        spec.limit
                    );
                }
                LimitEnforceTyp::DeleteOldest => match oldest {
                    Some(Oldest::Group(gid)) => self.delete_group(&gid)?,
                    Some(Oldest::Objective(oid)) => self.delete_objective(&oid)?,
                    None => (),
                },
            }
        }
        let spawnloc = SpawnLoc::AtPos {
            pos,
            offset_direction: Vector2::new(1., 0.),
            group_heading: 0.,
        };
        match &spec.kind {
            DeployableKind::Objective(parts) => {
                let oid = self.add_farp(lua, &spctx, idx, side, pos, &spec, parts)?;
                self.ephemeral.stat(Stat::DeployFarp {
                    by: ucid,
                    oid,
                    deployable: spec.path.last().unwrap().clone(),
                });
                Ok(())
            }
            DeployableKind::Group { template } => {
                let origin = DeployKind::Deployed {
                    player: ucid,
                    moved_by: None,
                    spec: spec.clone(),
                    cost_fraction: 1.,
                    origin: None,
                };
                let spawn_group_label: Option<std::string::String> = if spec.limit > 1 {
                    let i = n;
                    Some(if i == 0 {
                        template.to_string()
                    } else {
                        format!("{}-{}", template.as_str(), i)
                    })
                } else {
                    None
                };
                let gid = self.add_and_queue_group(
                    &spctx,
                    idx,
                    side,
                    spawnloc,
                    template.as_str(),
                    origin,
                    BitFlags::empty(),
                    None,
                    spawn_group_label.as_deref(),
                    None,
                )?;
                self.ephemeral.stat(Stat::DeployGroup {
                    gid,
                    deployable: dep,
                    by: ucid,
                });
                Ok(())
            }
        }
    }

    fn paratroops_to_point(
        &mut self,
        lua: MizLua,
        idx: &MizIndex,
        pos: Vector2,
        troop: String,
        side: Side,
        ucid: Ucid,
        origin: ObjectiveId,
    ) -> Result<()> {
        let troop_cfg = self
            .ephemeral
            .deployable_idx
            .get(&side)
            .ok_or_else(|| anyhow!("no such troop {troop} for {side}"))?
            .squads_by_name
            .get(troop.as_str())
            .ok_or_else(|| anyhow!("no such troop {troop} for {side}"))?
            .clone();
        let spawnpos = SpawnLoc::AtPos {
            pos,
            offset_direction: Vector2::new(1., 0.),
            group_heading: 0.,
        };
        let dk = DeployKind::Troop {
            player: ucid.clone(),
            moved_by: None,
            spec: troop_cfg.clone(),
            origin: Some(origin),
            cost_fraction: 1.,
            ship_hub: None,
            ship_offsets: None,
        };
        let spctx = SpawnCtx::new(lua)?;
        let (n, oldest) = self.number_troops_deployed(side, troop_cfg.name.as_str())?;
        let to_delete = if n < troop_cfg.limit as usize {
            None
        } else {
            match troop_cfg.limit_enforce {
                LimitEnforceTyp::DeleteOldest => oldest,
                LimitEnforceTyp::DenyCrate => {
                    bail!(
                        "the maximum number of {} troops are already deployed ({}/{})",
                        troop_cfg.name,
                        n,
                        troop_cfg.limit
                    );
                }
            }
        };
        if let Some(gid) = to_delete {
            self.delete_group(&gid)?;
        }
        let gid = self.add_and_queue_group(
            &spctx,
            idx,
            side,
            spawnpos,
            &*troop_cfg.template,
            dk,
            BitFlags::empty(),
            None,
            None,
            None,
        )?;
        self.ephemeral.stat(Stat::DeployTroop {
            troop,
            by: ucid,
            gid,
        });
        Ok(())
    }


    pub fn advance_actions(
        &mut self,
        lua: MizLua,
        idx: &MizIndex,
        jtacs: &Jtacs,
        now: DateTime<Utc>,
        perf: &mut PerfInner,
    ) -> Result<()> {
        let spctx = SpawnCtx::new(lua)?;
        let ai_gids: Vec<GroupId> = self.persisted.actions.into_iter().copied().collect();
        for gid in ai_gids {
            if let Err(e) = ai_air::advance_ai_air(lua, self, &spctx, idx, gid, now, Some(perf)) {
                error!("ai air tick {gid}: {e:?}");
            }
        }

        let mut to_delete: SmallVec<[GroupId; 4]> = smallvec![];
        let mut to_bomb: SmallVec<[(BomberCfg, Vector2, Side); 2]> = smallvec![];
        let mut to_repair: SmallVec<[(Vector2, Option<Ucid>, Side); 2]> = smallvec![];
        let mut to_transfer: SmallVec<[(Vector2, Vector2, Option<Ucid>, Side); 2]> = smallvec![];
        let mut to_deploy: SmallVec<[(Vector2, String, Side, Ucid); 2]> = smallvec![];
        let mut to_paratroop: SmallVec<[(Vector2, String, Side, Ucid, ObjectiveId); 2]> =
            smallvec![];
        macro_rules! at_dest {
            ($group:expr, $dest:expr, $radius:expr) => {{
                let r2 = f64::powi($radius, 2);
                let mut iter = $group.units.into_iter();
                loop {
                    match iter.next() {
                        None => break false,
                        Some(uid) => {
                            let unit = unit!(self, uid)?;
                            if na::distance_squared(&unit.pos.into(), &$dest.into()) <= r2 {
                                break true;
                            }
                        }
                    }
                }
            }};
        }


        for gid in &self.persisted.actions {
            let group = group_mut!(self, gid)?;

            if let DeployKind::Action {
                spec,
                time,
                destination,
                rtb,
                player,
                origin,
                ..
            } = &mut group.origin
            {
                match &spec.kind {
                    ActionKind::Awacs(AwacsCfg { .. })
                    | ActionKind::Fighters(_)
                    | ActionKind::Attackers(_)
                    | ActionKind::CruiseMissileSpawn(_)
                    | ActionKind::Drone(DroneCfg { .. })
                    | ActionKind::Tanker(_) => (),
                    ActionKind::Sead(_) => (),
                    ActionKind::Bomber(b) => {
                        if let Some(target) = *destination {
                            if at_dest!(group, target, 10_000.) {
                                destination.take();
                                to_bomb.push((b.clone(), target, group.side));
                            }
                        }
                        if destination.is_none() {
                            if let Some(target) = *rtb {
                                if at_dest!(group, target, 10_000.) {
                                    to_delete.push(*gid);
                                }
                            }
                        }
                    }
                    ActionKind::Rtb | ActionKind::Start | ActionKind::Status | ActionKind::Rearm => (),
                    ActionKind::LogisticsRepair(_) => {
                        if let Some(target) = *destination {
                            if at_dest!(group, target, 800.) {
                                destination.take();
                                to_repair.push((target, *player, group.side));
                                to_delete.push(group.id);
                            }
                        }
                    }
                    ActionKind::LogisticsTransfer(_) => {
                        if let Some(target) = *destination {
                            if at_dest!(group, target, 800.) {
                                destination.take();
                                if let Some(rtb) = *rtb {
                                    to_transfer.push((rtb, target, *player, group.side));
                                    to_delete.push(group.id);
                                }
                            }
                        }
                    }
                    ActionKind::Paratrooper(t) => {
                        if let Some(target) = *destination {
                            if at_dest!(group, target, 800.) {
                                destination.take();
                                let ucid = player
                                    .ok_or_else(|| anyhow!("paratroop missions require a ucid"))?;
                                let origin = (*origin).ok_or_else(|| {
                                    anyhow!("objective origin is required for paratroops")
                                })?;
                                to_paratroop.push((
                                    target,
                                    t.name.clone(),
                                    group.side,
                                    ucid,
                                    origin,
                                ));
                            }
                        }
                    }
                    ActionKind::Deployable(d) => {
                        if let Some(target) = *destination {
                            if at_dest!(group, target, 800.) {
                                destination.take();
                                let ucid = player.as_ref().map(|u| u.clone()).ok_or_else(|| {
                                    anyhow!("deployables missions require a ucid")
                                })?;
                                to_deploy.push((target, d.name.clone(), group.side, ucid));
                            }
                        }
                    }
                    ActionKind::Move(_) => {
                        self.ephemeral.groups_with_move_missions.retain(|gid, dst| {
                            match self.persisted.groups.get(gid) {
                                None => false,
                                Some(group) => {
                                    let pos = centroid2d(
                                        group
                                            .units
                                            .into_iter()
                                            .filter_map(|uid| self.persisted.units.get(uid))
                                            .map(|u| u.pos),
                                    );
                                    if (pos - *dst).magnitude() > 100. {
                                        true
                                    } else {
                                        for uid in &group.units {
                                            match self.persisted.units.get(uid) {
                                                None => {
                                                    self.ephemeral
                                                        .units_able_to_move
                                                        .swap_remove(uid);
                                                }
                                                Some(unit) => {
                                                    if !unit.tags.contains(UnitTag::Driveable) {
                                                        self.ephemeral
                                                            .units_able_to_move
                                                            .swap_remove(uid);
                                                    }
                                                }
                                            }
                                        }
                                        false
                                    }
                                }
                            }
                        });
                    }
                    ActionKind::AwacsWaypoint
                    | ActionKind::FighersWaypoint
                    | ActionKind::AttackersWaypoint
                    | ActionKind::SeadWaypoint
                    | ActionKind::CruiseMissileWaypoint
                    | ActionKind::TankerWaypoint
                    | ActionKind::DroneWaypoint
                    | ActionKind::Nuke(_) => {
                        bail!("should not be a group")
                    }
                }
            }
        }

        for gid in to_delete {
            if let Err(e) = self.delete_group(&gid) {
                error!("delete action group failed {e:?}")
            }
        }
        for (cfg, target, side) in to_bomb {
            if let Err(e) = self.bomb_targets(lua, side, jtacs, &cfg, target) {
                error!("bomb targets failed {e:?}")
            }
        }
        for (target, ucid, side) in to_repair {
            if let Err(e) = self.repair_target(target, ucid, side) {
                self.ephemeral.msgs().panel_to_side(
                    10,
                    false,
                    side,
                    format_compact!("repair mission failed {e:?}"),
                );
            }
        }
        for (src, target, ucid, side) in to_transfer {
            if let Err(e) = self.transfer_to_target(lua, src, target, ucid, side) {
                self.ephemeral.msgs().panel_to_side(
                    10,
                    false,
                    side,
                    format_compact!("transfer mission failed {e:?}"),
                );
            }
        }
        for (dst, troop, side, ucid, origin) in to_paratroop {
            if let Err(e) =
                self.paratroops_to_point(lua, idx, dst, troop, side, ucid.clone(), origin)
            {
                self.ephemeral.panel_to_player(
                    &self.persisted,
                    10,
                    &ucid,
                    format_compact!("paratroop mission failed {e:?}"),
                )
            }
        }
        for (dst, dep, side, ucid) in to_deploy {
            if let Err(e) = self.deployable_to_point(lua, idx, dst, dep, side, ucid.clone()) {
                self.ephemeral.panel_to_player(
                    &self.persisted,
                    10,
                    &ucid,
                    format_compact!("deploy mission failed {e:?}"),
                )
            }
        }
        Ok(())
    }
}
