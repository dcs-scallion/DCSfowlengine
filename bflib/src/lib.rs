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

mod admin;
mod bg;
mod chatcmd;
mod db;
mod ewr;
mod jtac;
mod landcache;
mod menu;
mod msgq;
mod shots;
mod spawnctx;

extern crate nalgebra as na;
use crate::db::player::SlotAuth;
use admin::{AdminCommand, AdminResult, run_admin_commands, theatre_slug};
use anyhow::{anyhow, bail, Context as AnyhowContext, Result};
use bfprotocols::{
    cfg::{Cfg, LifeType, UnitTag},
    db::objective::ObjectiveId,
    fowl_miz_export::FowlMizExport,
    perf::{Perf, PerfInner},
    stats::Stat,
};
use bg::Task;
use chatcmd::{run_action_commands, run_jtac_commands};
use chrono::{prelude::*, Duration};
use compact_str::{format_compact, CompactString};
use crossbeam::queue::SegQueue;
use db::{
    group::BirthRes,
    player::{RegErr, TakeoffRes},
    Db,
};
use dcso3::{
    coalition::Side,
    env::{
        self,
        miz::{Miz, UnitId},
        Env,
    },
    event::Event,
    hooks::UserHooks,
    lfs::Lfs,
    net::{DcsLuaEnvironment, Net, PlayerId, SlotId, Ucid},
    object::{DcsObject, DcsOid},
    perf::record_perf,
    timer::Timer,
    trigger::Trigger,
    unit::{ClassUnit, Unit},
    world::{HandlerId, MarkPanel, World},
    HooksLua, LuaEnv, MizLua, String,
};
use ewr::Ewr;
use fxhash::{FxBuildHasher, FxHashMap, FxHashSet};
use indexmap::IndexSet;
use jtac::{JtId, Jtacs};
use landcache::LandCache;
use log::{debug, error, info, warn};
use mlua::prelude::*;
use msgq::MsgTyp;
use netidx::publisher::Value;
use shots::ShotDb;
use smallvec::{smallvec, SmallVec};
use spawnctx::SpawnCtx;
use std::{
    backtrace::Backtrace,
    panic::{catch_unwind, AssertUnwindSafe},
    path::PathBuf,
    sync::Arc,
};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

#[derive(Debug, Clone)]
struct PlayerInfo {
    name: String,
    addr: Option<String>,
    ucid: Ucid,
}

#[derive(Debug, Default)]
struct Connected {
    info_by_player_id: FxHashMap<PlayerId, PlayerInfo>,
    id_by_ucid: FxHashMap<Ucid, PlayerId>,
    id_by_name: FxHashMap<String, PlayerId>,
    id_by_addr: FxHashMap<Option<String>, PlayerId>,
}

impl Connected {
    pub fn len(&self) -> usize {
        self.info_by_player_id.len()
    }

    pub fn get(&self, id: &PlayerId) -> Option<&PlayerInfo> {
        self.info_by_player_id.get(id)
    }

    pub fn get_by_name(&self, name: &str) -> Option<&PlayerInfo> {
        self.id_by_name.get(name).and_then(|id| self.info_by_player_id.get(id))
    }

    fn get_or_lookup_player_info<'a, 'lua, L: LuaEnv<'lua>>(
        &'a mut self,
        lua: L,
        id: PlayerId,
    ) -> Result<&'a PlayerInfo> {
        if self.info_by_player_id.contains_key(&id) {
            Ok(&self.info_by_player_id[&id])
        } else {
            let net = Net::singleton(lua)?;
            let ifo = net.get_player_info(id)?;
            let ucid =
                ifo.ucid()?.ok_or_else(|| anyhow!("player {:?} has no ucid", ifo))?;
            let name = ifo.name()?;
            let addr = ifo.ip()?;
            info!("player name: '{}', id: {:?}, ucid: {:?}", name, id, ucid);
            self.player_connected(id, PlayerInfo { name, addr, ucid })?;
            Ok(&self.info_by_player_id[&id])
        }
    }

    pub fn player_connected(&mut self, id: PlayerId, ifo: PlayerInfo) -> Result<()> {
        if let Some(id) = self.id_by_ucid.remove(&ifo.ucid) {
            self.player_disconnected(id);
        }
        if self.id_by_name.contains_key(&ifo.name) {
            bail!("your callsign is already taken by another player")
        }
        if self.id_by_addr.contains_key(&ifo.addr) {
            bail!("another player is already connected from your ip address")
        }
        self.id_by_ucid.insert(ifo.ucid, id);
        self.id_by_name.insert(ifo.name.clone(), id);
        self.id_by_addr.insert(ifo.addr.clone(), id);
        self.info_by_player_id.insert(id, ifo);
        Ok(())
    }

    pub fn player_disconnected(&mut self, id: PlayerId) -> Option<PlayerInfo> {
        self.info_by_player_id.remove(&id).map(|ifo| {
            self.id_by_name.remove(&ifo.name);
            self.id_by_ucid.remove(&ifo.ucid);
            self.id_by_addr.remove(&ifo.addr);
            ifo
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct AutoShutdown {
    when: DateTime<Utc>,
    thirty_minute_warning: bool,
    ten_minute_warning: bool,
    five_minute_warning: bool,
    one_minute_warning: bool,
}

impl AutoShutdown {
    fn new(ts: DateTime<Utc>) -> Self {
        let mut t = Self::default();
        t.when = ts;
        t
    }
}

#[derive(Debug, Clone, Copy)]
enum LoadState {
    Init,
    MissionLoaded { time: DateTime<Utc> },
    Running,
}

impl Default for LoadState {
    fn default() -> Self {
        Self::Init
    }
}

impl LoadState {
    fn login_ok(&self) -> Option<String> {
        match self {
            Self::Running => None,
            Self::Init => {
                Some(String::from("The server is not finished loading the mission"))
            }
            Self::MissionLoaded { time } => {
                let remains = (Duration::seconds(62) - (Utc::now() - time)).num_seconds();
                Some(format_compact!("The server is initializing ETA {remains}s").into())
            }
        }
    }

    fn init_ok(&self) -> bool {
        match self {
            Self::Init => false,
            Self::MissionLoaded { time } => Utc::now() - *time > Duration::seconds(1),
            Self::Running => true,
        }
    }

    fn step(&mut self) {
        match self {
            Self::Running | Self::Init => (),
            Self::MissionLoaded { time } => {
                if Utc::now() - *time >= Duration::minutes(1) {
                    *self = Self::Running;
                }
            }
        }
    }
}

#[derive(Debug, Default)]
struct JtacSlotIfo {
    subscribed_objectives: FxHashSet<ObjectiveId>,
    pinned: FxHashSet<JtId>,
}

#[derive(Debug, Default)]
struct Context {
    sortie: String,
    event_handler_id: Option<HandlerId>,
    miz_state_path: PathBuf,
    shutdown: Option<AutoShutdown>,
    last_perf_log: DateTime<Utc>,
    load_state: LoadState,
    idx: env::miz::MizIndex,
    db: Db,
    external_admin_commands: Arc<SegQueue<(AdminCommand, oneshot::Sender<Value>)>>,
    admin_commands: Vec<(admin::Caller, AdminCommand)>,
    action_commands: Vec<(PlayerId, String)>,
    jtac_commands: Vec<(PlayerId, JtId, String)>,
    to_background: Option<UnboundedSender<bg::Task>>,
    recently_landed: FxHashMap<DcsOid<ClassUnit>, DateTime<Utc>>,
    recently_born: FxHashMap<DcsOid<ClassUnit>, DateTime<Utc>>,
    airborne: FxHashSet<DcsOid<ClassUnit>>,
    /// Undamaged + fueled at last check; voluntary ejection penalty eligibility.
    airborne_voluntary_eject: FxHashSet<Ucid>,
    /// Deslot penalty deferred so disconnect can cancel before apply.
    pending_airborne_deslot_penalty: FxHashMap<Ucid, DateTime<Utc>>,
    captureable: FxHashMap<ObjectiveId, usize>,
    shots_out: ShotDb,
    menu_init_queue: IndexSet<SlotId, FxBuildHasher>,
    last_frame: Option<DateTime<Utc>>,
    last_slow_timed_events: DateTime<Utc>,
    last_periodic_points: DateTime<Utc>,
    last_unit_position: usize,
    last_player_position: usize,
    subscribed_jtac_menus: FxHashMap<SlotId, JtacSlotIfo>,
    subscribed_action_menus: FxHashSet<SlotId>,
    connected: Connected,
    landcache: LandCache,
    ewr: Ewr,
    jtac: Jtacs,
}

impl Context {
    // this must be used cautiously. Reasons why it's not totally nuts,
    // - the dcs scripting api is single threaded
    // - the event handlers can be triggerred by api calls, making refcells and mutexes error prone
    // - as long as an event handler doesn't step on state in an api call it's ok, since concurrency never happens
    //   that isn't so hard to guarantee
    unsafe fn get_mut() -> &'static mut Self {
        static mut SELF: Option<Context> = None;
        #[allow(static_mut_refs)]
        let t = unsafe { SELF.as_mut() };
        match t {
            Some(ctx) => ctx,
            None => {
                unsafe { SELF = Some(Context::default()) };
                #[allow(static_mut_refs)]
                unsafe {
                    SELF.as_mut().unwrap()
                }
            }
        }
    }

    unsafe fn _get() -> &'static Context {
        unsafe { Context::get_mut() }
    }

    unsafe fn reset() {
        unsafe {
            *Self::get_mut() = Self::default();
        }
    }

    fn do_bg_task(&self, task: bg::Task) {
        if let Some(to_bg) = &self.to_background {
            match to_bg.send(task) {
                Ok(()) => (),
                Err(_) => panic!("background thread is dead"),
            }
        }
    }

    fn init_async_bg(&mut self, lua: &Lua) -> Result<()> {
        if self.to_background.is_none() {
            let write_dir = PathBuf::from(Lfs::singleton(lua)?.writedir()?.as_str());
            self.to_background = Some(bg::init(write_dir));
        }
        Ok(())
    }

    fn respawn_groups(&mut self, lua: MizLua, miz: &Miz) -> Result<()> {
        let spctx = SpawnCtx::new(lua)?;
        let perf = Arc::make_mut(&mut unsafe { Perf::get_mut() }.inner);
        self.db.respawn_after_load(lua, perf, &self.idx, miz, &mut self.landcache, &spctx)
    }

    fn log_perf(&mut self, now: DateTime<Utc>) {
        if now - self.last_perf_log > Duration::seconds(60) {
            self.last_perf_log = now;
            self.do_bg_task(bg::Task::LogPerf {
                players: self.connected.len(),
                perf: unsafe { Perf::get_mut() }.clone(),
                api_perf: unsafe { dcso3::perf::Perf::get_mut() }.clone(),
            });
            info!("landcache {}", self.landcache.stats())
        }
    }
}

fn on_player_try_connect(
    _: HooksLua,
    addr: String,
    name: String,
    ucid: Ucid,
    id: PlayerId,
) -> Result<Option<String>> {
    let ts = Utc::now();
    info!(
        "onPlayerTryConnect addr: {:?}, name: {:?}, ucid: {:?}, id: {:?}",
        addr, name, ucid, id
    );
    let ctx = unsafe { Context::get_mut() };
    if let Some(msg) = ctx.load_state.login_ok() {
        return Ok(Some(msg));
    }
    if let Some(filter) = &ctx.db.ephemeral.cfg.name_filter {
        if !filter.check(&name) {
            let msg = format_compact!("name must match {}", filter.as_str());
            return Ok(Some(msg.into()));
        }
    }
    if let Some((until, _)) = ctx.db.ephemeral.cfg.banned.get(&ucid) {
        match until {
            None => return Ok(Some("you are banned forever".into())),
            Some(until) if until >= &Utc::now() => {
                return Ok(Some(
                    format_compact!("you are banned until {}", until).into(),
                ));
            }
            Some(_) => {
                let path = ctx.miz_state_path.clone();
                {
                    let cfg = Arc::make_mut(&mut ctx.db.ephemeral.cfg);
                    cfg.banned.remove(&ucid);
                }
                let cfg = Arc::clone(&ctx.db.ephemeral.cfg);
                ctx.do_bg_task(bg::Task::SaveConfig(path, cfg))
            }
        }
    }
    if let Err(e) = ctx.connected.player_connected(
        id,
        PlayerInfo { name: name.clone(), addr: Some(addr.clone()), ucid },
    ) {
        return Ok(Some(String::from(format_compact!("{e}"))));
    }
    ctx.db.player_connected(ucid, name.clone());
    ctx.do_bg_task(Task::Stat(Stat::Connect { id: ucid, addr, name }));
    record_perf(&mut Arc::make_mut(&mut unsafe { Perf::get_mut() }.inner).dcs_hooks, ts);
    Ok(None)
}

fn on_player_try_send_chat(
    lua: HooksLua,
    id: PlayerId,
    msg: String,
    all: bool,
) -> Result<Option<String>> {
    let start_ts = Utc::now();
    let ctx = unsafe { Context::get_mut() };
    let perf = &mut Arc::make_mut(&mut unsafe { Perf::get_mut() }.inner).dcs_hooks;
    info!("onPlayerTrySendChat id: {:?}, msg: {:?}, all: {:?}", id, msg, all);
    let r = chatcmd::process(ctx, lua, start_ts, id, msg);
    record_perf(perf, start_ts);
    match r {
        Ok(_) => Ok(None),
        Err(e) => {
            ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), format_compact!("{e}"));
            Ok(Some("".into()))
        }
    }
}

fn process_slot_rejection(ctx: &mut Context, id: PlayerId, ucid: Ucid, rej: SlotAuth) {
    match rej {
        SlotAuth::Denied => {
            ctx.db.ephemeral.msgs().send(
                MsgTyp::Chat(Some(id)),
                format_compact!("access to slot is denied"),
            );
        }
        SlotAuth::NoPoints { vehicle, cost, balance } => {
            ctx.db.ephemeral.msgs().send(
                MsgTyp::Chat(Some(id)),
                format_compact!("{vehicle} costs {cost}, you have {balance}"),
            );
        }
        SlotAuth::NoLives(typ) => {
            let msg = match lives(&mut ctx.db, &ucid, Some(typ), true, LivesFormat::Chat) {
                Ok(s) => s,
                Err(e) => {
                    error!("failed to get lives for {} {:?}", ucid, e);
                    "".into()
                }
            };
            ctx.db.ephemeral.msgs().send(
                MsgTyp::Chat(Some(id)),
                format_compact!("you have no {:?} lives remaining. {}", typ, msg),
            );
        }
        SlotAuth::VehicleNotAvailable(vehicle) => {
            let msg =
                format_compact!("Objective does not have any {} in stock", vehicle.0);
            ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), msg);
        }
        SlotAuth::ObjectiveHasNoLogistics => {
            let msg = format_compact!("Objective is capturable");
            ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), msg);
        }
        SlotAuth::ObjectiveNotOwned(side) => {
            let msg = String::from(format_compact!(
                "{:?} does not own the objective associated with this slot",
                side
            ));
            ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), msg);
        }
        SlotAuth::AirborneDeslotBlocked { remaining_secs } => {
            let msg = if remaining_secs > 0 {
                format_compact!(
                    "slot change blocked: {remaining_secs}s remaining (airborne deslot penalty)"
                )
            } else {
                format_compact!("cannot switch to observer/spectator slots while airborne")
            };
            ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), msg);
        }
        SlotAuth::NotRegistered(_) => warn!("unexpected NotRegistered"),
        SlotAuth::Yes(_) => warn!("slot was not rejected!"),
    }
}

fn try_occupy_slot(
    ctx: &mut Context,
    lua: HooksLua,
    id: PlayerId,
    ifo: PlayerInfo,
    side: Side,
    slot: SlotId,
) -> Result<bool> {
    let miz = MizLua::from_env(lua);
    let now = Utc::now();
    sanitize_airborne_slot_lock(ctx, miz, &ifo.ucid, now);
    if ctx.db.ephemeral.cfg.airborne_deslot_block {
        if let Some(secs) = ctx.db.airborne_observer_penalty_remaining(&ifo.ucid, now) {
            process_slot_rejection(
                ctx,
                id,
                ifo.ucid,
                SlotAuth::AirborneDeslotBlocked { remaining_secs: secs },
            );
            return Ok(false);
        }
    }
    if slot_exits_aircraft(&slot) {
        if let Some(rej) = in_flight_exit_slot_block(ctx, miz, &ifo.ucid) {
            process_slot_rejection(ctx, id, ifo.ucid, rej);
            return Ok(false);
        }
    }
    match ctx.db.try_occupy_slot(miz, now, side, slot, &ifo.ucid) {
        SlotAuth::NotRegistered(side) => {
            let name = ifo.name.clone();
            match ctx.db.register_player(ifo.ucid, name.clone(), side) {
                Ok(()) => {
                    chatcmd::register_success(ctx, id, name, side);
                    try_occupy_slot(ctx, lua, id, ifo, side, slot)
                }
                Err(RegErr::AlreadyRegistered(_, _)) => {
                    warn!(
                        "{:?} try_occupy_slot says NotRegistered but register_player says AlreadyRegistered",
                        ifo.ucid
                    );
                    Ok(false)
                }
                Err(RegErr::AlreadyOn(_)) => {
                    warn!(
                        "{:?} try_occupy_slot says NotRegistered but register_player says AlreadyOn",
                        ifo.ucid
                    );
                    Ok(false)
                }
            }
        }
        SlotAuth::Yes(typ) => {
            ctx.db.ephemeral.cancel_force_to_spectators(&ifo.ucid);
            ctx.subscribed_jtac_menus.remove(&slot);
            ctx.do_bg_task(Task::Stat(Stat::Slot { id: ifo.ucid, slot, typ }));
            Ok(true)
        }
        rej => {
            process_slot_rejection(ctx, id, ifo.ucid, rej);
            Ok(false)
        }
    }
}

fn slot_exits_aircraft(slot: &SlotId) -> bool {
    !matches!(slot, SlotId::Unit(_) | SlotId::MultiCrew(_, _))
}

fn in_flight_exit_slot_block(
    ctx: &mut Context,
    lua: MizLua,
    ucid: &Ucid,
) -> Option<SlotAuth> {
    if !ctx.db.ephemeral.cfg.airborne_deslot_block {
        return None;
    }
    let now = Utc::now();
    if let Some(secs) = ctx.db.airborne_observer_penalty_remaining(ucid, now) {
        return Some(SlotAuth::AirborneDeslotBlocked { remaining_secs: secs });
    }
    if player_unit_in_air(ctx, lua, ucid, None) {
        return Some(SlotAuth::AirborneDeslotBlocked { remaining_secs: 0 });
    }
    None
}

fn player_unit_in_air(
    ctx: &Context,
    lua: MizLua,
    ucid: &Ucid,
    unit_id: Option<&DcsOid<ClassUnit>>,
) -> bool {
    let db = &ctx.db;
    let mut ids: SmallVec<[DcsOid<ClassUnit>; 2]> = smallvec![];
    if let Some(id) = unit_id {
        ids.push(id.clone());
    }
    if let Some(slot) = db.ephemeral.slot_for_ucid(ucid) {
        if let Some(id) = db.ephemeral.get_object_id_by_slot(&slot) {
            if !ids.iter().any(|i| i == id) {
                ids.push(id.clone());
            }
        }
    }
    if let Some((slot, _)) = db
        .persisted
        .players
        .get(ucid)
        .and_then(|p| p.current_slot.as_ref())
    {
        if let Some(id) = db.ephemeral.get_object_id_by_slot(slot) {
            if !ids.iter().any(|i| i == id) {
                ids.push(id.clone());
            }
        }
    }
    for id in ids {
        let Ok(unit) = Unit::get_instance(lua, &id) else {
            continue;
        };
        if unit.is_exist().unwrap_or(false) && unit.in_air().unwrap_or(false) {
            return true;
        }
    }
    false
}

fn sanitize_airborne_slot_lock(ctx: &mut Context, lua: MizLua, ucid: &Ucid, now: DateTime<Utc>) {
    let _ = ctx.db.airborne_observer_penalty_remaining(ucid, now);
    if player_unit_in_air(ctx, lua, ucid, None) {
        return;
    }
    clear_stale_airborne_session(ctx, *ucid);
}

fn clear_stale_airborne_session(ctx: &mut Context, ucid: Ucid) {
    ctx.airborne_voluntary_eject.remove(&ucid);
    ctx.airborne.retain(|id| ctx.db.player_in_unit(false, id) != Some(ucid));
    ctx.db.clear_airborne_session(&ucid);
}

fn sanitize_connected_airborne_locks(ctx: &mut Context, lua: MizLua, now: DateTime<Utc>) {
    let ucids: SmallVec<[Ucid; 16]> = ctx.connected.id_by_ucid.keys().copied().collect();
    for ucid in ucids {
        sanitize_airborne_slot_lock(ctx, lua, &ucid, now);
    }
}

#[derive(Debug, Clone, Copy)]
enum AirbornePenaltyReason {
    Deslot,
    VoluntaryEjection,
}

impl AirbornePenaltyReason {
    fn violation_all(self, name: &str) -> CompactString {
        let _ = self;
        format_compact!("{name} performed forbidden deslot while airborne")
    }
}

fn notify_airborne_observer_penalty(
    ctx: &mut Context,
    ucid: Ucid,
    penalty_secs: u32,
    reason: AirbornePenaltyReason,
) {
    let penalty_mins = penalty_secs.div_ceil(60);
    let penalty_points = ctx.db.ephemeral.cfg.airborne_deslot_penalty_points;
    let name = ctx
        .db
        .player(&ucid)
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "unknown".into());
    ctx.db.ephemeral.msgs().send(
        MsgTyp::Chat(None),
        format_compact!(
            "{}: {penalty_points} points deducted, may not slot for {penalty_mins} min",
            reason.violation_all(&name)
        ),
    );
    info!(
        "airborne observer penalty {penalty_secs}s ({reason:?}) for {ucid:?} ({name}): {penalty_points} points"
    );
}

const AIRBORNE_DESLOT_PENALTY_DEBOUNCE: Duration = Duration::seconds(1);

fn penalize_airborne_exit(
    ctx: &mut Context,
    ucid: Ucid,
    now: DateTime<Utc>,
    reason: AirbornePenaltyReason,
) {
    let penalty_secs = ctx.db.ephemeral.cfg.airborne_deslot_penalty_secs;
    if ctx.db.apply_airborne_observer_penalty(&ucid, now) {
        notify_airborne_observer_penalty(ctx, ucid, penalty_secs, reason);
    }
    ctx.db.ephemeral.cancel_force_to_spectators(&ucid);
}

fn schedule_airborne_deslot_penalty(ctx: &mut Context, ucid: Ucid, when: DateTime<Utc>) {
    ctx.pending_airborne_deslot_penalty.insert(ucid, when);
}

fn cancel_airborne_deslot_penalty(ctx: &mut Context, ucid: &Ucid) {
    ctx.pending_airborne_deslot_penalty.remove(ucid);
}

fn process_pending_airborne_deslot_penalties(ctx: &mut Context, now: DateTime<Utc>) {
    let pending: SmallVec<[(Ucid, DateTime<Utc>); 8]> = ctx
        .pending_airborne_deslot_penalty
        .iter()
        .map(|(&ucid, &when)| (ucid, when))
        .collect();
    for (ucid, scheduled) in pending {
        if now - scheduled < AIRBORNE_DESLOT_PENALTY_DEBOUNCE {
            continue;
        }
        ctx.pending_airborne_deslot_penalty.remove(&ucid);
        if ctx.connected.id_by_ucid.contains_key(&ucid) {
            penalize_airborne_exit(ctx, ucid, now, AirbornePenaltyReason::Deslot);
        } else {
            info!("airborne deslot penalty skipped for {ucid:?} (disconnected)");
        }
    }
}

fn finish_airborne_exit(ctx: &mut Context, ucid: Ucid, unit_id: &DcsOid<ClassUnit>) {
    ctx.airborne.remove(unit_id);
    clear_stale_airborne_session(ctx, ucid);
}

fn mark_airborne_voluntary_eject(ctx: &mut Context, ucid: Ucid) {
    ctx.airborne_voluntary_eject.insert(ucid);
}

fn clear_airborne_voluntary_eject_on_damage(ctx: &mut Context, unit_id: &DcsOid<ClassUnit>) {
    if !ctx.airborne.contains(unit_id) {
        return;
    }
    let Some(ucid) = ctx.db.player_in_unit(false, unit_id) else {
        return;
    };
    ctx.airborne_voluntary_eject.remove(&ucid);
}

fn sync_airborne_voluntary_eject_fuel(ctx: &mut Context, lua: MizLua) {
    let ucids: SmallVec<[Ucid; 8]> = ctx.airborne_voluntary_eject.iter().copied().collect();
    for ucid in ucids {
        let Some(slot) = ctx.db.ephemeral.slot_for_ucid(&ucid) else {
            continue;
        };
        let Some(id) = ctx.db.ephemeral.get_object_id_by_slot(&slot).cloned() else {
            continue;
        };
        let Ok(unit) = Unit::get_instance(lua, &id) else {
            continue;
        };
        if unit.get_fuel().ok().is_some_and(|f| f <= 0.) {
            ctx.airborne_voluntary_eject.remove(&ucid);
        }
    }
}

fn is_aircraft_slot(db: &Db, slot: &SlotId) -> bool {
    let Some(sifo) = db.ephemeral.get_slot_info(slot) else {
        return false;
    };
    if sifo.ground_start {
        return false;
    }
    match db.ephemeral.cfg.unit_classification.get(&sifo.typ) {
        Some(tags) => tags.contains(UnitTag::Aircraft) || tags.contains(UnitTag::Helicopter),
        None => true,
    }
}

fn unit_in_active_flight(
    ctx: &Context,
    lua: MizLua,
    ucid: &Ucid,
    unit_id: Option<&DcsOid<ClassUnit>>,
) -> bool {
    player_unit_in_air(ctx, lua, ucid, unit_id)
}

fn airborne_deslot_block(
    ctx: &Context,
    lua: MizLua,
    ucid: Ucid,
    unit_id: Option<&DcsOid<ClassUnit>>,
) -> Option<(Side, SlotId)> {
    if !ctx.db.ephemeral.cfg.airborne_deslot_block {
        return None;
    }
    if !unit_in_active_flight(ctx, lua, &ucid, unit_id) {
        return None;
    }
    let slot = unit_id
        .and_then(|id| ctx.db.ephemeral.get_slot_by_object_id(id).copied())
        .or_else(|| {
            ctx.db
                .persisted
                .players
                .get(&ucid)
                .and_then(|p| p.current_slot.as_ref().map(|(s, _)| *s))
        })
        .or_else(|| ctx.db.ephemeral.slot_for_ucid(&ucid))?;
    if !is_aircraft_slot(&ctx.db, &slot) {
        return None;
    }
    let side = ctx
        .db
        .ephemeral
        .get_slot_info(&slot)
        .map(|s| s.side)
        .unwrap_or(ctx.db.persisted.players.get(&ucid)?.side);
    Some((side, slot))
}

fn on_player_change_slot(lua: HooksLua, id: PlayerId) -> Result<()> {
    let start_ts = Utc::now();
    let ctx = unsafe { Context::get_mut() };
    let res = (|| {
        let _ifo = ctx
            .connected
            .get_or_lookup_player_info(lua, id)?
            .clone();
        Ok(())
    })();
    record_perf(
        &mut Arc::make_mut(&mut unsafe { Perf::get_mut() }.inner).dcs_hooks,
        start_ts,
    );
    res
}

fn on_player_try_change_slot(
    lua: HooksLua,
    id: PlayerId,
    side: Side,
    slot: SlotId,
) -> Result<Option<bool>> {
    info!("onPlayerTryChangeSlot: {:?} {:?} {:?}", id, side, slot);
    let start_ts = Utc::now();
    let ctx = unsafe { Context::get_mut() };
    let res = match ctx.connected.get_or_lookup_player_info(lua, id) {
        Err(e) => {
            error!("failed to get player info for {:?} {:?}", id, e);
            Ok(Some(false))
        }
        Ok(ifo) => {
            let ifo = ifo.clone();
            match try_occupy_slot(ctx, lua, id, ifo, side, slot.clone()) {
                Err(e) => {
                    error!("error checking slot {:?}", e);
                    Ok(Some(false))
                }
                Ok(false) => Ok(Some(false)),
                Ok(true) => Ok(None),
            }
        }
    };
    record_perf(
        &mut Arc::make_mut(&mut unsafe { Perf::get_mut() }.inner).dcs_hooks,
        start_ts,
    );
    res
}

fn flush_markup_if_pending(ctx: &mut Context, lua: MizLua) {
    if let Err(e) = ctx.db.flush_markup_messages(lua) {
        error!("could not flush markup messages {e:?}");
    }
}

fn unit_killed(
    lua: MizLua,
    ctx: &mut Context,
    id: DcsOid<ClassUnit>,
    now: DateTime<Utc>,
) -> Result<()> {
    if ctx.db.ephemeral.cfg.airborne_deslot_block {
        if let Some(ucid) = ctx.db.player_in_unit(false, &id) {
            if ctx.db.persisted.players.get(&ucid).and_then(|p| p.airborne).is_some()
                || airborne_deslot_block(ctx, lua, ucid, Some(&id)).is_some()
            {
                info!("suppressed campaign unit death after airborne exit for {ucid:?}");
                return Ok(());
            }
        }
    }
    ctx.recently_landed.remove(&id);
    if ctx.db.csar_enabled() {
        if let Some(ucid) = ctx.db.csar_pilot_ucid(&id) {
            ctx.db.on_csar_pilot_killed(&id, &ucid);
            flush_markup_if_pending(ctx, lua);
        }
    }
    ctx.shots_out.dead(id.clone(), now);
    if let Err(e) = ctx.jtac.unit_dead(lua, &mut ctx.db, &id) {
        error!("jtac unit dead failed for {:?} {:?}", id, e)
    }
    if let Err(e) = ctx.db.unit_dead(&id, Utc::now()) {
        error!("unit dead failed for {:?} {:?}", id, e);
    } else {
        flush_markup_if_pending(ctx, lua);
    }
    Ok(())
}

fn on_event(lua: MizLua, ev: Event) -> Result<()> {
    let start_ts = Utc::now();
    let ctx = unsafe { Context::get_mut() };
    let perf = Arc::make_mut(&mut unsafe { Perf::get_mut() }.inner);
    match &ev {
        Event::MarkAdded(e) | Event::MarkChange(e) | Event::MarkRemoved(e)
            if e.initiator.is_none() =>
        {
            ()
        }
        ev => info!("onEvent: {:?}", ev),
    }
    match ev {
        Event::Birth(b) => {
            if let Ok(unit) = b.initiator.as_unit() {
                ctx.recently_born.insert(unit.object_id()?, Utc::now());
                let birth_place = b.place.as_ref();
                match ctx.db.unit_born(lua, &unit, &ctx.connected, birth_place) {
                    Ok(BirthRes::None) => (),
                    Ok(BirthRes::OccupiedSlot(slot)) => {
                        if ctx.db.ephemeral.cfg.limited_lives && ctx.db.ephemeral.cfg.lives_birth {
                            let typ = ctx
                                .db
                                .ephemeral
                                .get_slot_info(&slot)
                                .and_then(|sifo| ctx.db.ephemeral.cfg.life_types.get(&sifo.typ))
                                .copied();
                            if let Err(e) = message_life(ctx, &slot, typ, "life taken\n") {
                                error!("could not display life taken message {:?}", e)
                            }
                        }
                        ctx.menu_init_queue.insert(slot);
                    }
                    Ok(BirthRes::DynamicSlotDenied(ucid, rej)) => {
                        if let Some(id) = ctx.connected.id_by_ucid.get(&ucid) {
                            process_slot_rejection(ctx, *id, ucid, rej)
                        }
                        // just in case destroying the unit didn't work
                        ctx.db.ephemeral.force_player_to_spectators(&ucid);
                    }
                    Err(e) => {
                        error!("unit born failed {:?} {:?}", unit, e);
                    }
                }
            } else if let Ok(st) = b.initiator.as_static() {
                if let Err(e) = ctx.db.static_born(&st) {
                    error!("static born failed {:?} {:?}", st, e);
                }
            }
        }
        Event::PlayerLeaveUnit(e) => {
            if let Some(initiator) = e.initiator {
                if let Some(ucid) = ctx.db.player_in_unit(false, &initiator) {
                    if airborne_deslot_block(ctx, lua, ucid, Some(&initiator)).is_some() {
                        schedule_airborne_deslot_penalty(ctx, ucid, start_ts);
                        finish_airborne_exit(ctx, ucid, &initiator);
                        ctx.db.player_deslot(&ucid);
                        return Ok(());
                    }
                }
                if let Some(ucid) = ctx.db.player_in_unit(false, &initiator) {
                    if let Some(player) = ctx.db.player(&ucid) {
                        if let Some((_, Some(inst))) = player.current_slot.as_ref() {
                            if inst.landed_at_objective.is_none() {
                                ctx.shots_out.dead(initiator.clone(), start_ts)
                            }
                        }
                    }
                }
                match ctx.db.player_left_unit(lua, start_ts, &initiator) {
                    Ok((_, Some((ucid, slot, typ)), deslot)) => {
                        let mut msg = CompactString::new("life returned\n");
                        if let Ok(l) = format_lives_total(&mut ctx.db, &ucid, typ) {
                            msg.push_str(&l);
                        }
                        // Synchronous call while player is still in the DCS group
                        if let Some(miz_gid) =
                            ctx.db.ephemeral.get_slot_info(&slot).map(|sifo| sifo.miz_gid)
                        {
                            if let Ok(trigger) = Trigger::singleton(lua) {
                                if let Ok(action) = trigger.action() {
                                    let _ = action.out_text_for_group(
                                        miz_gid,
                                        msg.clone().into(),
                                        10,
                                        false,
                                    );
                                }
                            }
                        }
                        if let Some((ucid, slot)) = deslot {
                            ctx.db.player_deslot_slot(&ucid, &slot);
                        }
                    }
                    Ok((_, None, deslot)) => {
                        if let Some((ucid, slot)) = deslot {
                            ctx.db.player_deslot_slot(&ucid, &slot);
                        }
                    }
                    Err(e) => error!("player left unit failed {:?}", e),
                }
            } else {
                handle_player_leave_unit_no_initiator(ctx, lua, start_ts);
            }
        }
        Event::Hit(e) | Event::Kill(e) => {
            if let Some(target) = e.target.as_ref().and_then(|t| t.as_unit().ok()) {
                let target_id = target.object_id()?;
                if let (Ok(life), Ok(life0)) = (target.get_life(), target.get_life0()) {
                    if life < life0 {
                        clear_airborne_voluntary_eject_on_damage(ctx, &target_id);
                    }
                }
                let dead = target.get_life()? < 1;
                if let Some(shooter) = e.initiator.and_then(|u| u.as_unit().ok()) {
                    if let Err(e) = ctx.shots_out.hit(
                        &ctx.db,
                        start_ts,
                        dead,
                        &target,
                        &shooter,
                        e.weapon_name,
                    ) {
                        error!("error processing hit event {:?}", e)
                    }
                }
                if dead {
                    if let Err(e) = unit_killed(lua, ctx, target.object_id()?, start_ts) {
                        error!("0 unit killed failed {:?}", e)
                    }
                }
            } else if let Some(target) =
                e.target.as_ref().and_then(|t| t.as_static().ok())
            {
                let static_id = target.object_id()?;
                if let Some(who) =
                    crate::shots::who_from_initiator(&ctx.db, e.initiator.as_ref())
                {
                    ctx.db.note_static_hit(static_id.clone(), who);
                }
                if target.get_life()? < 1 {
                    let killer = crate::shots::who_from_initiator(
                        &ctx.db,
                        e.initiator.as_ref(),
                    )
                    .or_else(|| ctx.db.take_static_hit(&static_id));
                    if let Err(e) = ctx.db.static_dead(
                        lua,
                        &static_id,
                        start_ts,
                        killer.as_ref(),
                    ) {
                        error!("static dead failed {e:?}")
                    } else {
                        flush_markup_if_pending(ctx, lua);
                    }
                } else if let Err(e) =
                    ctx.db.production_static_damaged(lua, &static_id, start_ts)
                {
                    error!("production factory damaged failed {e:?}")
                }
            }
        }
        Event::Shot(e) => {
            if let Err(e) = ctx.shots_out.shot(&ctx.db, start_ts, &e) {
                error!("error processing shot event {:?}", e)
            }
            ()
        }
        Event::Dead(e) | Event::UnitLost(e) | Event::PilotDead(e) => {
            if let Some(unit) = e.initiator.as_ref().and_then(|u| u.as_unit().ok()) {
                let id = unit.object_id()?;
                if let Err(e) = unit_killed(lua, ctx, id, start_ts) {
                    error!("1 unit killed failed {:?}", e)
                }
            } else if let Some(st) = e.initiator.as_ref().and_then(|s| s.as_static().ok())
            {
                let id = st.object_id()?;
                let killer = ctx.db.take_static_hit(&id);
                if let Err(e) = ctx.db.static_dead(lua, &id, start_ts, killer.as_ref()) {
                    error!("static killed failed {e:?}")
                } else {
                    flush_markup_if_pending(ctx, lua);
                }
            }
        }
        Event::Ejection(e) => {
            if let Ok(aircraft) = e.initiator.as_unit() {
                let id = aircraft.object_id()?;
                let ucid = ctx.db.player_in_unit(false, &id);
                if ctx.db.csar_enabled() {
                    if let (Some(ucid), Ok(pilot)) = (ucid, e.target.as_unit()) {
                        if let Err(e) =
                            ctx.db.on_csar_ejection(lua, &aircraft, &pilot, ucid, start_ts)
                        {
                            error!("csar ejection failed {e:?}");
                        } else {
                            flush_markup_if_pending(ctx, lua);
                        }
                    }
                }
                if let Some(ucid) = ucid {
                    let voluntary = ctx.airborne_voluntary_eject.contains(&ucid);
                    let was_in_flight = voluntary
                        || player_unit_in_air(ctx, lua, &ucid, Some(&id))
                        || ctx
                            .db
                            .persisted
                            .players
                            .get(&ucid)
                            .and_then(|p| p.airborne)
                            .is_some();
                    if was_in_flight {
                        if voluntary {
                            penalize_airborne_exit(
                                ctx,
                                ucid,
                                start_ts,
                                AirbornePenaltyReason::VoluntaryEjection,
                            );
                        } else {
                            info!(
                                "ejection bailout: no observer penalty for {ucid:?} (damaged or no fuel)"
                            );
                        }
                    }
                    finish_airborne_exit(ctx, ucid, &id);
                    ctx.db.player_deslot(&ucid);
                }
                if let Err(e) = unit_killed(lua, ctx, id, start_ts) {
                    error!("2 unit killed failed {}", e)
                }
            }
        }
        Event::LandingAfterEjection => {
            if ctx.db.csar_enabled() {
                let ucids = ctx.db.csar_active_ucids();
                for ucid in ucids {
                    if let Err(e) =
                        ctx.db.on_csar_landing_after_ejection(lua, start_ts, &ucid)
                    {
                        warn!("csar landing update failed for {ucid:?}: {e:?}");
                    }
                }
                flush_markup_if_pending(ctx, lua);
            }
        }
        Event::Takeoff(e) | Event::PostponedTakeoff(e) => {
            if let Ok(unit) = e.initiator.as_unit() {
                let id = unit.object_id()?;
                if !ctx.recently_born.contains_key(&id)
                    && ctx.airborne.insert(id.clone())
                    && ctx.recently_landed.remove(&id).is_none()
                {
                    let slot = unit.slot()?;
                    let position = unit.get_ground_position()?.0;
                    match ctx.db.takeoff(Utc::now(), slot, &unit, position) {
                        Err(e) => error!("could not process takeoff, {:?}", e),
                        Ok(TakeoffRes::NoLifeTaken) => {
                            if let Some(ucid) = ctx.db.player_in_unit(false, &id) {
                                mark_airborne_voluntary_eject(ctx, ucid);
                            }
                        }
                        Ok(TakeoffRes::TookLife(typ)) => {
                            if let Some(ucid) = ctx.db.player_in_unit(false, &id) {
                                mark_airborne_voluntary_eject(ctx, ucid);
                            }
                            if let Err(e) =
                                message_life(ctx, &slot, Some(typ), "life taken\n")
                            {
                                error!("could not display life taken message {:?}", e)
                            }
                            let _ = menu::cargo::list_cargo_for_slot(ctx, &slot);
                        }
                        Ok(TakeoffRes::OutOfLives | TakeoffRes::OutOfPoints) => {
                            if let Err(e) = unit.destroy() {
                                error!(
                                    "failed to destroy unit that took off without lives or points {e:?}"
                                )
                            }
                        }
                    }
                }
            }
        }
        Event::Land(e) | Event::PostponedLand(e) => {
            if let Ok(unit) = e.initiator.as_unit() {
                let id = unit.object_id()?;
                if let Some(ucid) = ctx.db.player_in_unit(false, &id) {
                    ctx.airborne_voluntary_eject.remove(&ucid);
                }
                if !ctx.recently_born.contains_key(&id) && ctx.airborne.remove(&id) {
                    ctx.recently_landed.insert(id, Utc::now());
                }
            }
        }
        Event::MarkAdded(MarkPanel { initiator: Some(unit), .. }) => {
            let oid = unit.object_id()?;
            if let Some(slot) = ctx.db.ephemeral.get_slot_by_object_id(&oid) {
                let slot = *slot;
                if let Some(ucid) = ctx.db.ephemeral.player_in_slot(&slot) {
                    let ucid = *ucid;
                    if ctx.subscribed_action_menus.contains(&slot) {
                        if let Err(e) = menu::action::init_action_menu_for_slot(
                            ctx, lua, &slot, &ucid,
                        ) {
                            error!("failed to init action menu for {ucid} {slot} {e:?}")
                        }
                    }
                }
            }
        }
        Event::MissionEnd => unsafe {
            Context::reset();
            Perf::reset();
            Context::get_mut().init_async_bg(lua.inner())?;
            return Ok(()); // avoid record perf with a reset perf context
        },
        _ => (),
    }
    record_perf(&mut perf.dcs_events, start_ts);
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LivesFormat {
    Chat,
    Panel,
}

fn life_type_user_label(db: &Db, typ: LifeType, format: LivesFormat) -> CompactString {
    match format {
        LivesFormat::Panel => db.life_type_panel_label(typ).into(),
        LivesFormat::Chat => db::csar::life_type_display_label(typ).into(),
    }
}

/// After a life is returned: always show banked total (`cur/n`), never `1+cur/n`.
fn format_lives_total(db: &mut Db, ucid: &Ucid, typ: LifeType) -> Result<CompactString> {
    db.maybe_reset_lives(ucid, Utc::now())?;
    let player = db.player(ucid).ok_or_else(|| anyhow!("no such player {:?}", ucid))?;
    let cfg = &db.ephemeral.cfg;
    let (n, reset_after) = cfg.default_lives[&typ];
    let now = Utc::now();
    let label = life_type_user_label(db, typ, LivesFormat::Panel);
    Ok(format_life_type(
        &label,
        typ,
        player,
        cfg,
        now,
        false,
        None,
        n,
        reset_after,
        LivesFormat::Panel,
    ))
}

fn format_lives_reset_remaining(d: Duration) -> CompactString {
    let d = d.max(Duration::zero());
    let hrs = d.num_hours();
    let min = d.num_minutes() - hrs * 60;
    format_compact!("{hrs}:{min:02}")
}

fn format_life_type(
    label: &str,
    _typ: LifeType,
    player: &db::player::Player,
    cfg: &Cfg,
    now: DateTime<Utc>,
    include_slot_reserve: bool,
    active_life_type: Option<LifeType>,
    n: u8,
    reset_after: u32,
    format: LivesFormat,
) -> CompactString {
    let body = match player.lives.get(&_typ) {
        None => format_compact!("{label} {n}/{n}"),
        Some((reset, cur)) => {
            let since_reset = now - *reset;
            let reset = format_lives_reset_remaining(
                Duration::seconds(reset_after as i64) - since_reset,
            );
            if include_slot_reserve
                && cfg.limited_lives
                && cfg.lives_birth
                && active_life_type == Some(_typ)
            {
                format_compact!("{label} 1+{cur}/{n} RST {reset}")
            } else {
                format_compact!("{label} {cur}/{n} RST {reset}")
            }
        }
    };
    match format {
        LivesFormat::Chat => format_compact!("[ {body} ]"),
        LivesFormat::Panel => body,
    }
}

fn format_downed_pilots_line(db: &Db, ucid: &Ucid, format: LivesFormat) -> CompactString {
    let counts = db.csar_downed_counts(ucid);
    if counts.is_empty() {
        return CompactString::default();
    }
    let mut parts: SmallVec<[CompactString; 5]> = smallvec![];
    for typ in db::csar::LIFE_TYPE_DISPLAY_ORDER {
        if let Some(n) = counts.get(&typ) {
            let label = life_type_user_label(db, typ, format);
            parts.push(match format {
                LivesFormat::Chat => format_compact!("[ {label} {n} ]"),
                LivesFormat::Panel => format_compact!("{label} {n}"),
            });
        }
    }
    format_compact!(", Downed pilots : {}", parts.join(""))
}

fn lives(
    db: &mut Db,
    ucid: &Ucid,
    typfilter: Option<LifeType>,
    include_slot_reserve: bool,
    format: LivesFormat,
) -> Result<CompactString> {
    db.maybe_reset_lives(ucid, Utc::now())?;
    let player = db.player(ucid).ok_or_else(|| anyhow!("no such player {:?}", ucid))?;
    let cfg = &db.ephemeral.cfg;
    let active_life_type = player
        .current_slot
        .as_ref()
        .and_then(|(slot, _)| db.ephemeral.get_slot_info(slot))
        .and_then(|sifo| cfg.life_types.get(&sifo.typ))
        .copied();
    let now = Utc::now();
    let mut parts: SmallVec<[CompactString; 6]> = smallvec![];
    for typ in db::csar::LIFE_TYPE_DISPLAY_ORDER {
        if typfilter.is_some() && typfilter != Some(typ) {
            continue;
        }
        let Some(&(n, reset_after)) = cfg.default_lives.get(&typ) else {
            continue;
        };
        let label = life_type_user_label(db, typ, format);
        parts.push(format_life_type(
            &label,
            typ,
            player,
            cfg,
            now,
            include_slot_reserve,
            active_life_type,
            n,
            reset_after,
            format,
        ));
    }
    let mut msg = format_compact!("Your lives : ");
    msg.push_str(&parts.join(""));
    if typfilter.is_none() {
        msg.push_str(&format_downed_pilots_line(db, ucid, format));
    }
    Ok(msg)
}

fn message_life(
    ctx: &mut Context,
    slot: &SlotId,
    typ: Option<LifeType>,
    msg: &str,
) -> Result<()> {
    let uid = slot.as_unit_id().ok_or_else(|| anyhow!("not a unit"))?;
    let ucid = ctx
        .db
        .ephemeral
        .player_in_slot(slot)
        .ok_or_else(|| anyhow!("no player in slot {:?}", slot))?
        .clone();
    let mut msg = CompactString::new(msg);
    if let Ok(lives) = lives(&mut ctx.db, &ucid, typ, true, LivesFormat::Panel) {
        msg.push_str(&lives)
    }
    ctx.db.ephemeral.msgs().panel_to_unit(10, false, uid, msg);
    Ok(())
}

fn return_lives(lua: MizLua, ctx: &mut Context, ts: DateTime<Utc>) {
    macro_rules! or_false {
        ($e:expr) => {
            match $e {
                Ok(r) => r,
                Err(_) => return false,
            }
        };
    }
    let db = &mut ctx.db;
    ctx.recently_landed.retain(|id, landed_ts| {
        if ts - *landed_ts >= Duration::seconds(10) {
            let unit = or_false!(Unit::get_instance(lua, id));
            let pos = or_false!(unit.get_ground_position());
            let slot = or_false!(unit.slot());
            if db.land(slot, pos.0, &unit) {
                return false;
            }
        }
        true
    });
}

fn advise_captureable(ctx: &mut Context) -> Result<()> {
    let cur_cap = ctx.db.capturable_objectives();
    for oid in &cur_cap {
        let dur = ctx.captureable.entry(*oid).or_default();
        *dur += 1;
        if *dur == 10 {
            let obj = ctx.db.objective(oid)?;
            let m = format_compact!(
                "{} is now capturable",
                ctx.db.objective_f10_map_label(obj)
            );
            ctx.db.ephemeral.msgs().panel_to_all(30, false, m);
        }
    }
    ctx.captureable.retain(|oid, _| cur_cap.contains(oid));
    Ok(())
}

fn advise_captured(ctx: &mut Context, lua: MizLua, ts: DateTime<Utc>) -> Result<()> {
    for (side, oid) in ctx.db.check_capture(lua, ts)? {
        let name = ctx.db.objective_f10_map_label(ctx.db.objective(&oid)?);
        let mcap = format_compact!("our forces have captured {}", name);
        let mlost = format_compact!("we have lost {}", name);
        ctx.db.ephemeral.msgs().panel_to_side(15, false, side, mcap);
        ctx.db.ephemeral.msgs().panel_to_side(15, false, side.opposite(), mlost);
        ctx.captureable.remove(&oid);
    }
    Ok(())
}

fn generate_ewr_reports(ctx: &mut Context, now: DateTime<Utc>) -> Result<()> {
    use std::fmt::Write;
    let mut msgs: SmallVec<[(UnitId, CompactString); 64]> = smallvec![];
    for (ucid, player, inst) in ctx.db.instanced_players() {
        let uid = match player.current_slot.as_ref().and_then(|(sl, _)| sl.as_unit_id()) {
            Some(uid) => uid,
            None => continue,
        };
        let braa_to_chickens = ctx.ewr.where_chicken(
            now,
            false,
            false,
            ucid,
            player,
            inst,
            &ctx.db,
            ctx.db.ephemeral.cfg.ewr_mode,
            ctx.db.ephemeral.cfg.ewr_delay,
        );
        if !braa_to_chickens.is_empty() {
            let mut report = format_compact!("Bandits BRAA\n");
            write!(report, "{}\n", ewr::HEADER)?;
            for gibbraa in braa_to_chickens {
                write!(report, "{gibbraa}\n")?;
            }
            msgs.push((uid, report));
        }
    }
    for (uid, msg) in msgs {
        ctx.db.ephemeral.msgs().panel_to_unit(10, false, uid, msg)
    }
    Ok(())
}

fn check_auto_shutdown(
    ctx: &mut Context,
    lua: MizLua,
    now: DateTime<Utc>,
) -> Result<AdminResult> {
    if let Some(asd) = ctx.shutdown.as_mut() {
        if asd.when - now <= Duration::minutes(30) && !asd.thirty_minute_warning {
            asd.thirty_minute_warning = true;
            ctx.db.ephemeral.msgs().panel_to_all(
                60,
                false,
                "The server will restart in 30 minutes",
            );
        }
        if asd.when - now <= Duration::minutes(10) && !asd.ten_minute_warning {
            asd.ten_minute_warning = true;
            ctx.db.ephemeral.msgs().panel_to_all(
                60,
                true,
                "The server will restart in 10 minutes",
            );
        }
        if asd.when - now <= Duration::minutes(5) && !asd.five_minute_warning {
            asd.five_minute_warning = true;
            ctx.db.ephemeral.msgs().panel_to_all(
                60,
                true,
                "The server will restart in 5 minutes",
            )
        }
        if asd.when - now <= Duration::minutes(1) && !asd.one_minute_warning {
            asd.one_minute_warning = true;
            ctx.db.ephemeral.msgs().panel_to_all(
                60,
                true,
                "The server will restart in one minute",
            )
        }
        if now > asd.when {
            return admin::admin_shutdown(ctx, lua, None);
        }
    }
    if let Some(victor) = ctx.db.check_victory(now) {
        return admin::admin_shutdown(ctx, lua, Some(Some(victor)));
    }
    Ok(AdminResult::Continue)
}

fn handle_player_leave_unit_no_initiator(
    ctx: &mut Context,
    lua: MizLua,
    start_ts: DateTime<Utc>,
) {
    let changing: SmallVec<[Ucid; 4]> = ctx
        .connected
        .id_by_ucid
        .keys()
        .filter(|ucid| {
            ctx.db
                .persisted
                .players
                .get(ucid)
                .is_some_and(|p| p.changing_slots)
        })
        .copied()
        .collect();
    if changing.is_empty() {
        debug!("PlayerLeaveUnit without initiator (observer or slot handoff)");
        return;
    }
    for ucid in changing {
        let slot = ctx
            .db
            .persisted
            .players
            .get(&ucid)
            .and_then(|p| p.current_slot.as_ref().map(|(s, _)| *s));
        let Some(slot) = slot else {
            ctx.db.player_deslot(&ucid);
            continue;
        };
        let Some(objid) = ctx.db.ephemeral.get_object_id_by_slot(&slot).cloned() else {
            ctx.db.player_deslot_slot(&ucid, &slot);
            continue;
        };
        match ctx.db.player_left_unit(lua, start_ts, &objid) {
            Ok((_, Some((ucid, slot, typ)), deslot)) => {
                info!("life returned on deslot for {ucid} ({typ})");
                let mut msg = CompactString::new("life returned\n");
                if let Ok(l) = format_lives_total(&mut ctx.db, &ucid, typ) {
                    msg.push_str(&l);
                }
                if let Some(miz_gid) =
                    ctx.db.ephemeral.get_slot_info(&slot).map(|sifo| sifo.miz_gid)
                {
                    if let Ok(trigger) = Trigger::singleton(lua) {
                        if let Ok(action) = trigger.action() {
                            let _ = action.out_text_for_group(
                                miz_gid,
                                msg.into(),
                                10,
                                false,
                            );
                        }
                    }
                }
                if let Some((ucid, slot)) = deslot {
                    ctx.db.player_deslot_slot(&ucid, &slot);
                }
            }
            Ok((_, None, deslot)) => {
                if let Some((ucid, slot)) = deslot {
                    ctx.db.player_deslot_slot(&ucid, &slot);
                }
            }
            Err(e) => error!("player left unit (no initiator) failed {:?}", e),
        }
    }
}

fn force_players_to_spectators(ctx: &mut Context, net: &Net, ts: DateTime<Utc>) {
    for (_, ids) in ctx.db.ephemeral.players_to_force_to_spectators(ts) {
        for ucid in ids {
            match ctx.connected.id_by_ucid.get(&ucid) {
                None => warn!("no id for player ucid {:?}", ucid),
                Some(id) => {
                    info!("forcing player {} to spectators", ucid);
                    if let Err(e) =
                        net.force_player_slot(*id, Side::Neutral, SlotId::Spectator)
                    {
                        error!("error forcing player {:?} to spectators {:?}", id, e);
                    }
                    match net.get_slot(*id) {
                        Err(_) => ctx.db.ephemeral.force_player_to_spectators(&ucid),
                        Ok((side, slot)) => {
                            if side != Side::Neutral || !slot.is_spectator() {
                                ctx.db.ephemeral.force_player_to_spectators(&ucid)
                            }
                        }
                    }
                }
            }
        }
    }
}

fn update_jtac_contacts(ctx: &mut Context, lua: MizLua) {
    match ctx.jtac.update_contacts(lua, &mut ctx.landcache, &mut ctx.db) {
        Err(e) => error!("could not update jtac contacts {e}"),
        Ok(dirty_menus) => {
            let mut dirty_slots: SmallVec<[SlotId; 16]> = smallvec![];
            for (side, oids) in dirty_menus {
                for (_, player, _) in ctx.db.instanced_players() {
                    if player.side == side {
                        if let Some((slot, _)) = player.current_slot.as_ref() {
                            let mut dead: SmallVec<[JtId; 4]> = smallvec![];
                            let mut expunge = false;
                            if let Some(subd) = ctx.subscribed_jtac_menus.get_mut(&slot) {
                                let pinned: SmallVec<[ObjectiveId; 16]> = subd
                                    .pinned
                                    .iter()
                                    .filter_map(|jt| match ctx.jtac.get(jt) {
                                        Ok(jt) => Some(jt.location().oid),
                                        Err(_) => {
                                            dead.push(*jt);
                                            None
                                        }
                                    })
                                    .collect();
                                for oid in &oids {
                                    if subd.subscribed_objectives.contains(oid) {
                                        if !dirty_slots.contains(slot) {
                                            dirty_slots.push(*slot);
                                        }
                                    }
                                    if !pinned.contains(oid) {
                                        subd.subscribed_objectives.remove(oid);
                                    }
                                }
                                expunge = subd.subscribed_objectives.is_empty();
                            }
                            if dead.len() > 0 {
                                let dead = dead.drain(..);
                                if let Some(subd) =
                                    ctx.subscribed_jtac_menus.get_mut(slot)
                                {
                                    for jtid in dead {
                                        subd.pinned.remove(&jtid);
                                    }
                                }
                            }
                            if expunge {
                                ctx.subscribed_jtac_menus.remove(slot);
                            }
                        }
                    }
                }
            }
            for slot in dirty_slots {
                if let Err(e) = menu::jtac::init_jtac_menu_for_slot(ctx, lua, &slot) {
                    error!("could not init jtac menu for slot {slot}, {e:?}")
                }
            }
        }
    }
}

fn award_periodic_points(ctx: &mut Context, ts: DateTime<Utc>) {
    if let Some(points) = ctx.db.ephemeral.cfg.points.as_ref() {
        let (award, period) = points.periodic_point_gain;
        if award != 0 && period > 0 {
            let elapsed = (ts - ctx.last_periodic_points).num_seconds();
            if elapsed >= period as i64 {
                ctx.last_periodic_points = ts;
                for ifo in ctx.connected.info_by_player_id.values() {
                    ctx.db.adjust_points(&ifo.ucid, award, "periodic award")
                }
            }
        }
    }
}

fn run_slow_timed_events(
    lua: MizLua,
    ctx: &mut Context,
    perf: &mut PerfInner,
    path: &PathBuf,
    ts: DateTime<Utc>,
) -> Result<AdminResult> {
    let freq = Duration::seconds(ctx.db.ephemeral.cfg.slow_timed_events_freq as i64);
    if ts - ctx.last_slow_timed_events >= freq {
        let start_ts = Utc::now();
        ctx.last_slow_timed_events = start_ts;
        match check_auto_shutdown(ctx, lua, ts) {
            Ok(AdminResult::Continue) => (),
            Ok(AdminResult::Shutdown) => return Ok(AdminResult::Shutdown),
            Err(e) => error!("failed to check for auto shutdown {e:?}"),
        }
        for (oid, vh) in ctx.db.ephemeral.warehouses_to_sync() {
            if let Err(e) = ctx.db.sync_vehicle_at_obj(lua, oid, vh.clone()) {
                error!(
                    "failed to sync warehouse at objective {:?} vehicle {:?} {:?}",
                    oid, vh, e
                )
            }
        }
        return_lives(lua, ctx, ts);
        ctx.recently_born.retain(|_, ts| start_ts - *ts <= Duration::seconds(5));
        {
            // report kills
            let cfg = Arc::clone(&ctx.db.ephemeral.cfg);
            for dead in ctx.shots_out.bring_out_your_dead(ts) {
                info!("kill {:?}", dead);
                if let Some(points) = cfg.points.as_ref() {
                    ctx.db.award_kill_points(points, &dead)
                }
                ctx.do_bg_task(Task::Stat(Stat::Kill(dead)));
            }
        }
        if let Err(e) = ctx.db.maybe_do_repairs(ts) {
            error!("error doing repairs {:?}", e)
        }
        let spctx = crate::spawnctx::SpawnCtx::new(lua)?;
        if let Err(e) = ctx.db.maybe_do_production_repairs(lua, &spctx, &ctx.idx, ts) {
            error!("error doing OPR production repairs {:?}", e)
        }
        record_perf(&mut perf.do_repairs, start_ts);
        if let Err(e) = ctx.db.advance_actions(lua, &ctx.idx, &ctx.jtac, start_ts) {
            error!("could not advance actions {e:?}")
        }
        let ts = Utc::now();
        if let Err(e) = ctx.ewr.update_tracks(
            lua,
            &mut ctx.landcache,
            &ctx.db,
            ts,
            ctx.db.ephemeral.cfg.ewr_mode,
            ctx.db.ephemeral.cfg.ewr_delay,
        ) {
            error!("could not update ewr tracks {e}")
        }
        record_perf(&mut perf.ewr_tracks, ts);
        let ts = Utc::now();
        if let Err(e) = generate_ewr_reports(ctx, ts) {
            error!("could not generate ewr reports {e}")
        }
        record_perf(&mut perf.ewr_reports, ts);
        let ts = Utc::now();
        match ctx.db.cull_or_respawn_objectives(lua, &mut ctx.landcache, ts) {
            Err(e) => error!("could not cull or respawn objectives {e}"),
            Ok((threatened, cleared)) => {
                for oid in threatened {
                    let obj = ctx.db.objective(&oid)?;
                    let owner = obj.owner();
                    let msg = format_compact!(
                        "enemies spotted near {}",
                        ctx.db.objective_f10_map_label(obj)
                    );
                    ctx.db.ephemeral.msgs().panel_to_side(10, false, owner, msg)
                }
                for oid in cleared {
                    let obj = ctx.db.objective(&oid)?;
                    let owner = obj.owner();
                    let msg = format_compact!(
                        "{} is no longer threatened",
                        ctx.db.objective_f10_map_label(obj)
                    );
                    ctx.db.ephemeral.msgs().panel_to_side(10, false, owner, msg)
                }
            }
        }
        record_perf(&mut perf.unit_culling, ts);
        let ts = Utc::now();
        if let Err(e) = ctx.db.update_objectives_markup(lua) {
            error!("could not remark objectives {e}")
        }
        record_perf(&mut perf.remark_objectives, ts);
        let ts = Utc::now();
        update_jtac_contacts(ctx, lua);
        record_perf(&mut perf.update_jtac_contacts, ts);
        let now = Utc::now();
        if let Some(snap) = ctx.db.maybe_snapshot() {
            ctx.do_bg_task(bg::Task::SaveState(path.clone(), snap));
        }
        record_perf(&mut perf.snapshot, now);
        award_periodic_points(ctx, start_ts);
        record_perf(&mut perf.slow_timed, start_ts);
    }
    Ok(AdminResult::Continue)
}

fn run_timed_events(
    ctx: &mut Context,
    lua: MizLua,
    path: &PathBuf,
) -> Result<AdminResult> {
    let ts = Utc::now();
    let perf = Arc::make_mut(&mut unsafe { Perf::get_mut() }.inner);
    let net = Net::singleton(lua)?;
    let act = Trigger::singleton(lua)?.action()?;
    force_players_to_spectators(ctx, &net, ts);
    match ctx.db.update_unit_positions_incremental(lua, ts, ctx.last_unit_position) {
        Err(e) => error!("could not update unit positions {e}"),
        Ok((i, dead)) => {
            ctx.last_unit_position = i;
            for id in dead {
                if let Err(e) = unit_killed(lua, ctx, id.clone(), ts) {
                    error!("unit killed failed {:?} {:?}", id, e)
                }
            }
        }
    }
    record_perf(&mut perf.unit_positions, ts);
    let ts = Utc::now();
    match ctx.db.update_player_positions_incremental(lua, ts, ctx.last_player_position) {
        Err(e) => error!("could not update player positions {e}"),
        Ok((i, dead)) => {
            ctx.last_player_position = i;
            sync_airborne_voluntary_eject_fuel(ctx, lua);
            for id in dead {
                if let Err(e) = unit_killed(lua, ctx, id.clone(), ts) {
                    error!("unit killed failed {:?} {:?}", id, e)
                }
            }
        }
    }
    record_perf(&mut perf.player_positions, ts);
    sanitize_connected_airborne_locks(ctx, lua, ts);
    process_pending_airborne_deslot_penalties(ctx, ts);

    match run_slow_timed_events(lua, ctx, perf, path, ts) {
        Ok(AdminResult::Continue) => (),
        Ok(AdminResult::Shutdown) => return Ok(AdminResult::Shutdown),
        Err(e) => error!("error running slow timed events {:?}", e),
    }
    if let Some(slot) = ctx.menu_init_queue.shift_remove_index(0) {
        if let Err(e) = menu::init_for_slot(ctx, lua, &slot) {
            error!("could not init menus for slot {:?} {:?}", slot, e)
        }
    }
    let now = Utc::now();
    let spctx = SpawnCtx::new(lua)?;
    if let Err(e) = ctx.db.ephemeral.process_spawn_queue(
        perf,
        &ctx.db.persisted,
        ts,
        &ctx.idx,
        &spctx,
    ) {
        error!("error processing spawn queue {:?}", e)
    }
    record_perf(&mut perf.spawn_queue, now);
    if let Err(e) = ctx.db.try_run_deferred_tisp_initial_ships(lua, &ctx.idx, now) {
        error!("deferred TISP initial ships: {e:?}");
    }
    let now = Utc::now();
    if let Err(e) = advise_captured(ctx, lua, ts) {
        error!("error advise captured {:?}", e)
    }
    record_perf(&mut perf.advise_captured, now);
    let now = Utc::now();
    if let Err(e) = advise_captureable(ctx) {
        error!("error advise capturable {:?}", e)
    }
    record_perf(&mut perf.advise_capturable, now);
    let now = Utc::now();
    match ctx.jtac.update_target_positions(lua, now, &mut ctx.db) {
        Err(e) => error!("error updating jtac target positions {:?}", e),
        Ok(dead) => {
            for id in dead {
                if let Err(e) = unit_killed(lua, ctx, id.clone(), now) {
                    error!("unit killed failed {:?} {:?}", id, e)
                }
            }
        }
    }
    record_perf(&mut perf.jtac_target_positions, now);
    let now = Utc::now();
    let max_rate = ctx.db.ephemeral.cfg.max_msgs_per_second;
    ctx.db.ephemeral.msgs().process(max_rate, &net, &act);
    record_perf(&mut perf.process_messages, now);
    if let Err(e) = ctx.db.logistics_step(lua, perf, ts) {
        error!("error running logistics events {e:?}")
    }
    match run_admin_commands(ctx, lua) {
        Err(e) => error!("failed to run admin commands {e:?}"),
        Ok(AdminResult::Continue) => (),
        Ok(AdminResult::Shutdown) => return Ok(AdminResult::Shutdown),
    }
    if let Err(e) = run_action_commands(ctx, perf, lua) {
        error!("failed to run action commands {e:?}")
    }
    if let Err(e) = run_jtac_commands(ctx, lua) {
        error!("failed to run jtac commands {e:?}")
    }
    ctx.load_state.step();
    record_perf(&mut perf.timed_events, ts);
    ctx.log_perf(now);
    Ok(AdminResult::Continue)
}

fn start_timed_events(ctx: &mut Context, lua: MizLua, path: PathBuf) -> Result<()> {
    ctx.last_slow_timed_events = Utc::now();
    let timer = Timer::singleton(lua)?;
    timer.schedule_function(timer.get_time()? + 1., mlua::Value::Nil, {
        let path = path.clone();
        move |lua, _, now| {
            let ctx = unsafe { Context::get_mut() };
            match catch_unwind(AssertUnwindSafe(|| run_timed_events(ctx, lua, &path))) {
                Ok(Ok(AdminResult::Continue)) => (),
                Ok(Err(e)) => error!("failed to run timed events {:?}", e),
                Ok(Ok(AdminResult::Shutdown)) => {
                    println!("initiating DCS shutdown");
                    if let Some(id) = ctx.event_handler_id.take() {
                        World::singleton(lua)?
                            .remove_event_handler(id)
                            .context("removing event handler")?
                    }
                    Net::singleton(lua)?.dostring_in(
                        DcsLuaEnvironment::Server,
                        "DCS.setUserCallbacks({}); DCS.exitProcess()".into(),
                    )?;
                    println!("removing timer event");
                    return Ok(None);
                }
                Err(e) => match e.downcast_ref::<anyhow::Error>() {
                    Some(e) => {
                        error!("run_timed_events panicked {e:?} {}", Backtrace::capture())
                    }
                    None => {
                        error!("run_timed_events panicked {e:?} {}", Backtrace::capture())
                    }
                },
            }
            Ok(Some(now + 1.))
        }
    })?;
    Ok(())
}

fn delayed_init_miz(lua: MizLua) -> Result<()> {
    info!("init_miz: welcome to blue flag v3");
    let ctx = unsafe { Context::get_mut() };
    info!("indexing the miz");
    let miz = Miz::singleton(lua)?;
    ctx.idx = miz.index().context("indexing the mission")?;
    info!("adding event handlers");
    ctx.event_handler_id = Some(
        World::singleton(lua)?
            .add_event_handler(on_event)
            .context("adding event handlers")?,
    );
    let sortie = miz.sortie().context("getting the sortie")?;
    let path = {
        let s = Env::singleton(lua)?.get_value_dict_by_key(sortie)?;
        if s.is_empty() {
            bail!("missing sortie in miz file")
        }
        ctx.sortie = s;
        ctx.miz_state_path = PathBuf::from(Lfs::singleton(lua)?.writedir()?.as_str())
            .join(ctx.sortie.as_str());
        ctx.miz_state_path.clone()
    };
    debug!("sortie is {:?}", ctx.sortie);
    let cfg = Arc::new(Cfg::load(&path)?);
    info!(
        "campaign cfg: airborne_deslot_block={} airborne_deslot_penalty_secs={} airborne_deslot_penalty_points={} csar={}",
        cfg.airborne_deslot_block,
        cfg.airborne_deslot_penalty_secs,
        cfg.airborne_deslot_penalty_points,
        cfg.csar.enabled
    );
    let export_path = FowlMizExport::path(&path);
    let fowl_export = Arc::new(FowlMizExport::load_required(&path)?);
    info!("loaded Fowl mission export from {:?}", export_path);
    debug!(
        "Fowl mission export schema_version {} weapon_bridge_used={} blue_ws={} red_ws={}",
        fowl_export.schema_version,
        fowl_export.weapon_bridge_used,
        fowl_export.blue_weapon_ws.len(),
        fowl_export.red_weapon_ws.len()
    );
    ctx.do_bg_task(Task::CfgLoaded {
        sortie: ctx.sortie.clone(),
        cfg: Arc::clone(&cfg),
        admin_channel: Arc::clone(&ctx.external_admin_commands),
    });
    debug!("path to saved state is {:?}", path);
    info!("initializing db");
    let to_bg = ctx.to_background.as_ref().unwrap().clone();
    crate::db::tisp_init::validate_tisp_zones_on_water(lua, &miz)?;
    if !path.exists() {
        debug!("saved state doesn't exist, starting from default");
        ctx.do_bg_task(Task::Stat(Stat::NewRound { sortie: ctx.sortie.clone() }));
        ctx.db = Db::init(
            lua,
            cfg,
            &ctx.idx,
            &miz,
            to_bg,
            Arc::clone(&fowl_export),
        )
            .context("initalizing the mission")?;
        ctx.db
            .schedule_tisp_initial_ship_placement(Utc::now() + Duration::seconds(60));
    } else {
        debug!("saved state exists, loading it");
        ctx.db = Db::load(
            &miz,
            &ctx.idx,
            to_bg,
            cfg,
            &path,
            Arc::clone(&fowl_export),
        )
            .context("loading the saved state")?;
    }
    if ctx.db.ephemeral.cfg.front_line {
        let theatre = theatre_slug(lua);
        ctx.db
            .ephemeral
            .load_front_line_water_grid(&path, theatre.as_str());
    }
    ctx.shutdown = ctx
        .db
        .ephemeral
        .cfg
        .shutdown
        .map(|hrs| AutoShutdown::new(Utc::now() + Duration::hours(hrs as i64)));
    ctx.do_bg_task(Task::Stat(Stat::SessionStart {
        stop: ctx.shutdown.map(|a| a.when),
        cfg: Box::new((*ctx.db.ephemeral.cfg).clone()),
    }));
    info!("spawning units");
    ctx.respawn_groups(lua, &miz).context("setting up the mission after load")?;
    // Register players who were already connected before the mission loaded
    let net = Net::singleton(lua)?;
    for id in net.get_player_list()? {
        let id = match id { Ok(id) => id, Err(_) => continue };
        let ifo = match net.get_player_info(id) {
            Ok(ifo) => ifo,
            Err(e) => { error!("failed to get player info for {:?}: {:?}", id, e); continue; }
        };
        let ucid = match ifo.ucid() {
            Ok(Some(u)) => u,
            _ => continue,
        };
        let name = match ifo.name() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let addr = ifo.ip().ok().flatten();
        let _ = ctx.connected.player_connected(id, PlayerInfo { name: name.clone(), addr, ucid });
        ctx.db.player_connected(ucid, name.clone());
        let welcome = if let Some(player) = ctx.db.player(&ucid) {
            format_compact!(
                "Welcome back, {}! You are on the {:?} team. Type -help for commands.",
                name, player.side
            )
        } else {
            format_compact!("Welcome, {}! Type 'blue' or 'red' to join a team.", name)
        };
        ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), welcome);
    }
    info!("starting timed events");
    ctx.db.refresh_csar_after_load(lua);
    if let Err(e) = ctx.db.flush_markup_messages(lua) {
        warn!("csar mark rebuild flush failed {e:?}");
    }
    start_timed_events(ctx, lua, path).context("starting the timed events loop")?;
    Ok(())
}

fn on_mission_load_end(_lua: HooksLua) -> Result<()> {
    unsafe {
        Context::get_mut().load_state = LoadState::MissionLoaded { time: Utc::now() }
    };
    info!("mission loaded");
    Ok(())
}

fn on_player_disconnect(_: HooksLua, id: PlayerId) -> Result<()> {
    info!("onPlayerDisconnect({id})");
    let start_ts = Utc::now();
    let ctx = unsafe { Context::get_mut() };
    if let Some(ifo) = ctx.connected.player_disconnected(id) {
        info!("deslotting disconnected player {}", ifo.ucid);
        cancel_airborne_deslot_penalty(ctx, &ifo.ucid);
        ctx.db.player_disconnected(&ifo.ucid)
    }
    record_perf(
        &mut Arc::make_mut(&mut unsafe { Perf::get_mut() }.inner).dcs_hooks,
        start_ts,
    );
    Ok(())
}

fn on_simulation_frame(_: HooksLua) -> Result<()> {
    let frame = Arc::make_mut(&mut unsafe { Perf::get_mut() }.frame);
    let now = Utc::now();
    let ctx = unsafe { Context::get_mut() };
    match &mut ctx.last_frame {
        Some(last) => {
            if let Some(ns) = (now - *last).num_nanoseconds() {
                if ns >= 1 && ns <= 1_000_000_000 {
                    **frame += ns as u64;
                }
            }
            *last = now;
        }
        None => {
            ctx.last_frame = Some(now);
        }
    }
    Ok(())
}

fn init_hooks(lua: HooksLua) -> Result<()> {
    info!("setting user hooks");
    UserHooks::new(lua)
        .on_player_try_change_slot(on_player_try_change_slot)?
        .on_player_change_slot(on_player_change_slot)?
        .on_mission_load_end(on_mission_load_end)?
        .on_player_try_connect(on_player_try_connect)?
        .on_player_try_send_chat(on_player_try_send_chat)?
        .on_player_disconnect(on_player_disconnect)?
        .on_simulation_frame(on_simulation_frame)?
        .register()?;
    Ok(())
}

fn init_miz(lua: MizLua) -> Result<()> {
    info!("initializing mission");
    let timer = Timer::singleton(lua)?;
    let when = timer.get_time()? + 1.;
    timer.schedule_function(when, mlua::Value::Nil, move |lua, _, now| {
        let ctx = unsafe { Context::get_mut() };
        if ctx.load_state.init_ok() {
            if let Err(e) = delayed_init_miz(lua) {
                error!("THE MISSION CANNOT START: {:?}", e);
                let timer = Timer::singleton(lua)?;
                timer.schedule_function(
                    now + 1.,
                    mlua::Value::Nil,
                    move |lua, _, now| {
                        let ctx = unsafe { Context::get_mut() };
                        let screen_detail = e
                            .chain()
                            .map(|err| err.to_string())
                            .collect::<Vec<_>>()
                            .join("\n\n");
                        let _ = Trigger::singleton(lua)?.action()?.out_text(
                            format_compact!(
                                "THE MISSION CANNOT START BECAUSE OF AN ERROR\n\n{0}",
                                screen_detail.as_str()
                            )
                            .into(),
                            3600,
                            true,
                        );
                        ctx.load_state.step();
                        Ok(Some(now + 10.))
                    },
                )?;
            }
            Ok(None)
        } else {
            info!("waiting for the mission to finish loading");
            Ok(Some(now + 1.))
        }
    })?;
    Ok(())
}

#[mlua::lua_module]
fn bflib(lua: &Lua) -> LuaResult<LuaTable<'_>> {
    // ensure we capture backtraces on panic
    let _ = unsafe {
        std::env::set_var("RUST_BACKTRACE", "1"); // bactrace for panics
        std::env::set_var("RUST_LIB_BACKTRACE", "0"); // no backtrace for Error
    };
    unsafe { Context::get_mut() }.init_async_bg(lua.inner()).map_err(dcso3::lua_err)?;
    dcso3::create_root_module(lua, init_hooks, init_miz)
}
