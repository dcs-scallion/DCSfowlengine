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

mod acmi_sanitize;
mod admin;
mod bg;
mod chatcmd;
mod db;
mod ewr;
mod jtac;
mod landcache;
mod menu;
mod msgq;
mod setmissionstartdatetime;
mod shots;
mod sounds;
mod spawnctx;

extern crate nalgebra as na;
use crate::db::player::SlotAuth;
use admin::{AdminCommand, AdminResult, run_admin_commands, theatre_slug};
use anyhow::{anyhow, bail, Context as AnyhowContext, Result};
use bfprotocols::{
    cfg::{Cfg, LifeType, UnitTag, Vehicle},
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
    discord_map::{DiscordMapLiveCtx, DiscordMapPilot},
    group::BirthRes,
    player::{CaEnterRes, RegErr, TakeoffRes},
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
    HooksLua, LuaEnv, MizLua, Position3, String,
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

/// Periodic award per player on one coalition (mirrors payout logic).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PeriodicSideAward {
    pub amount: i32,
    pub balancing: bool,
}

pub(crate) fn balanced_side_gain(own: u32, opp: u32, gain: i32) -> PeriodicSideAward {
    if own == 0 || opp == 0 {
        return PeriodicSideAward {
            amount: 0,
            balancing: false,
        };
    }
    let scaled = (opp as i64 * gain as i64) / own as i64;
    if scaled < gain as i64 {
        PeriodicSideAward {
            amount: gain,
            balancing: false,
        }
    } else {
        PeriodicSideAward {
            amount: scaled as i32,
            balancing: true,
        }
    }
}

fn dcs_coalition_side_counts(lua: MizLua, ctx: &Context) -> Result<(u32, u32)> {
    let net = Net::singleton(lua)?;
    let mut online_red = 0u32;
    let mut online_blue = 0u32;
    for (&id, _) in &ctx.connected.info_by_player_id {
        let Ok(ifo) = net.get_player_info(id) else {
            continue;
        };
        let Ok(side) = ifo.side() else {
            continue;
        };
        match side {
            Side::Red => online_red += 1,
            Side::Blue => online_blue += 1,
            _ => {}
        }
    }
    let cfg = &ctx.db.ephemeral.cfg;
    online_red = online_red.saturating_add(cfg.debugging_online_red_players);
    online_blue = online_blue.saturating_add(cfg.debugging_online_blue_players);
    if cfg.debugging_online_red_players > 0 || cfg.debugging_online_blue_players > 0 {
        debug!(
            "balancing_point_gain debug headcount: red={online_red} blue={online_blue} \
             (includes red+{} blue+{} fake)",
            cfg.debugging_online_red_players, cfg.debugging_online_blue_players
        );
    }
    Ok((online_red, online_blue))
}

fn balancing_side_counts(lua: MizLua, ctx: &Context) -> Result<(u32, u32)> {
    dcs_coalition_side_counts(lua, ctx)
}

fn discord_map_live_ctx(lua: MizLua, ctx: &Context) -> Result<DiscordMapLiveCtx> {
    let (online_red, online_blue) = dcs_coalition_side_counts(lua, ctx)?;
    let pilots = collect_discord_map_pilots(lua, ctx)?;
    Ok(DiscordMapLiveCtx {
        generated_at: Utc::now(),
        shutdown_when: ctx.db.ephemeral.cfg.map_restart_when(
            Utc::now(),
            ctx.shutdown.map(|s| s.when),
        ),
        online_red,
        online_blue,
        blue_pilots: pilots.blue,
        red_pilots: pilots.red,
        spectators: pilots.spectators,
    })
}

#[derive(Debug, Default)]
struct DiscordMapPilotLists {
    blue: Vec<DiscordMapPilot>,
    red: Vec<DiscordMapPilot>,
    spectators: Vec<DiscordMapPilot>,
}

fn collect_discord_map_pilots(lua: MizLua, ctx: &Context) -> Result<DiscordMapPilotLists> {
    let net = Net::singleton(lua)?;
    let mut blue = Vec::new();
    let mut red = Vec::new();
    let mut spectators = Vec::new();
    for (&id, connected) in &ctx.connected.info_by_player_id {
        let Ok(ifo) = net.get_player_info(id) else {
            continue;
        };
        let ping = ifo.ping().unwrap_or(0.).round().max(0.) as u32;
        let side = ifo.side().unwrap_or(Side::Neutral);
        let entry = DiscordMapPilot {
            name: connected.name.to_string(),
            ping,
        };
        match side {
            Side::Blue => blue.push(entry),
            Side::Red => red.push(entry),
            _ => spectators.push(entry),
        }
    }
    let by_name = |a: &DiscordMapPilot, b: &DiscordMapPilot| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    };
    blue.sort_by(by_name);
    red.sort_by(by_name);
    spectators.sort_by(by_name);
    Ok(DiscordMapPilotLists {
        blue,
        red,
        spectators,
    })
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
    /// Restart countdown warnings (panels when CFG `shutdown` or DCSServerBot schedule).
    restart_warnings: Option<AutoShutdown>,
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
            if let Err(e) = to_bg.send(task) {
                error!("background thread is dead, dropped task: {e:?}");
            }
        }
    }

    fn persist_campaign_state(&mut self) {
        self.db
            .campaign_flush_online_before_save(Utc::now());
        if let Some(snap) = self.db.maybe_snapshot() {
            self.do_bg_task(bg::Task::SaveState(self.miz_state_path.clone(), snap));
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
    // Grounded while still marked airborne (Land event lost): keep life-return window.
    let grounded_airborne: SmallVec<[DcsOid<ClassUnit>; 2]> = ctx
        .airborne
        .iter()
        .filter(|id| ctx.db.player_in_unit(false, id) == Some(*ucid))
        .cloned()
        .collect();
    for id in grounded_airborne {
        ctx.recently_landed
            .entry(id)
            .or_insert_with(Utc::now);
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

fn is_aircraft_or_helicopter(db: &Db, typ: &Vehicle) -> bool {
    match db.ephemeral.cfg.unit_classification.get(typ) {
        Some(tags) => tags.contains(UnitTag::Aircraft) || tags.contains(UnitTag::Helicopter),
        None => false,
    }
}

fn eligible_for_airborne_periodic_award(ctx: &Context, lua: MizLua, ucid: &Ucid) -> bool {
    let Some(player) = ctx.db.persisted.players.get(ucid) else {
        return false;
    };
    let typ = player
        .current_slot
        .as_ref()
        .and_then(|(_, inst)| inst.as_ref().map(|i| &i.typ))
        .or_else(|| {
            player
                .current_slot
                .as_ref()
                .and_then(|(slot, _)| ctx.db.ephemeral.get_slot_info(slot).map(|s| &s.typ))
        });
    let Some(typ) = typ else {
        return false;
    };
    if !is_aircraft_or_helicopter(&ctx.db, typ) {
        return false;
    }
    player_unit_in_air(ctx, lua, ucid, None)
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

fn spawn_setmissionstartdatetime(ctx: &Context, lua: MizLua, new_campaign: bool) {
    let miz_path = match crate::db::discord_map::resolve_mission_miz_path(lua, &ctx.miz_state_path)
    {
        Ok(p) => p,
        Err(e) => {
            error!("setmissionstartdatetime: resolve miz path failed: {e:?}");
            return;
        }
    };
    let cfg = &ctx.db.ephemeral.cfg;
    let args = crate::setmissionstartdatetime::SpawnArgs {
        cfg: &cfg.setmissionstartdatetime,
        campaign_stats_enabled: cfg.discord_map.campaign_stats,
        campaign_rounds: ctx.db.persisted.campaign_stats.campaign_rounds,
        rounds_per_day: cfg.discord_map.rounds_per_day,
        miz_path: &miz_path,
        new_campaign,
    };
    if let Err(e) = crate::setmissionstartdatetime::maybe_spawn(args) {
        error!("setmissionstartdatetime: {e:?}");
    }
}

fn flush_markup_if_pending(ctx: &mut Context, lua: MizLua) {
    if let Err(e) = ctx.db.flush_markup_messages(lua) {
        error!("could not flush markup messages {e:?}");
    }
}

fn is_combined_arms_control_slot(slot: &SlotId) -> bool {
    matches!(
        slot,
        SlotId::ArtilleryCommander(_, _)
            | SlotId::ForwardObserver(_, _)
            | SlotId::Observer(_, _)
            | SlotId::Instructor(_, _)
    )
}

fn ca_control_slot_priority(slot: &SlotId) -> u8 {
    match slot {
        SlotId::ArtilleryCommander(_, _) => 0,
        SlotId::ForwardObserver(_, _) => 1,
        SlotId::Observer(_, _) => 2,
        SlotId::Instructor(_, _) => 3,
        _ => 9,
    }
}

fn lookup_ucid_by_player_name(ctx: &Context, name: &str) -> Option<Ucid> {
    if let Some(ifo) = ctx.connected.get_by_name(name) {
        return Some(ifo.ucid);
    }
    ctx.db.persisted.players.into_iter().find_map(|(u, p)| {
        if p.name.as_str() == name || p.alts.into_iter().any(|a| a.as_str() == name) {
            Some(*u)
        } else {
            None
        }
    })
}

/// True when the player currently occupies an aircraft/helicopter campaign slot.
fn player_slotted_in_aircraft_or_helicopter(ctx: &Context, ucid: &Ucid) -> bool {
    let Some(player) = ctx.db.persisted.players.get(ucid) else {
        return false;
    };
    let Some((slot, _)) = player.current_slot.as_ref() else {
        return false;
    };
    match slot {
        SlotId::Unit(_) | SlotId::MultiCrew(_, _) => {
            // Ignore stale current_slot after switching to CA/FO (slot map already cleared).
            if ctx.db.ephemeral.player_in_slot(slot) != Some(ucid) {
                return false;
            }
            let Some(sifo) = ctx.db.ephemeral.get_slot_info(slot) else {
                return false;
            };
            is_aircraft_or_helicopter(&ctx.db, &sifo.typ)
        }
        _ => false,
    }
}

/// DCS often reports CA ground `getPlayerName()` as `"PLAYER"` on enter; Shot often has the real callsign.
fn resolve_combined_arms_controller(
    ctx: &Context,
    unit: &Unit,
    id: &DcsOid<ClassUnit>,
) -> Option<Ucid> {
    if let Ok(Some(name)) = unit.get_player_name() {
        if !name.is_empty() && !name.eq_ignore_ascii_case("PLAYER") {
            if let Some(ucid) = lookup_ucid_by_player_name(ctx, name.as_str()) {
                return Some(ucid);
            }
        }
    }
    let unit_side = ctx
        .db
        .ephemeral
        .get_uid_by_object_id(id)
        .and_then(|uid| ctx.db.persisted.units.get(uid))
        .map(|u| u.side)?;

    // Shot path: not in air ⇒ can only be firing from Combined Arms.
    let mut non_air: SmallVec<[Ucid; 4]> = smallvec![];
    for (ucid, player) in &ctx.db.persisted.players {
        if player.side != unit_side {
            continue;
        }
        if !ctx.connected.id_by_ucid.contains_key(ucid) {
            continue;
        }
        if player_slotted_in_aircraft_or_helicopter(ctx, ucid) {
            continue;
        }
        non_air.push(*ucid);
    }
    if non_air.len() == 1 {
        info!(
            "CA controller resolved via sole non-air player for {id:?} -> {}",
            non_air[0]
        );
        return Some(non_air[0]);
    }

    let mut best: Option<(Ucid, u8)> = None;
    let mut best_count = 0u32;
    for ucid in &non_air {
        let Some(player) = ctx.db.persisted.players.get(ucid) else {
            continue;
        };
        let Some((slot, _)) = player.current_slot.as_ref() else {
            continue;
        };
        if !is_combined_arms_control_slot(slot) {
            continue;
        }
        let pri = ca_control_slot_priority(slot);
        match best {
            None => {
                best = Some((*ucid, pri));
                best_count = 1;
            }
            Some((_, bpri)) if pri < bpri => {
                best = Some((*ucid, pri));
                best_count = 1;
            }
            Some((_, bpri)) if pri == bpri => {
                best_count += 1;
            }
            Some(_) => {}
        }
    }
    if best_count == 1 {
        let ucid = best.map(|(u, _)| u)?;
        info!("CA controller resolved via side CA slot for {id:?} -> {ucid}");
        Some(ucid)
    } else {
        if best_count > 1 {
            warn!(
                "CA: ambiguous controller for {id:?} ({best_count} CA-slot players on {unit_side:?})"
            );
        }
        None
    }
}

fn apply_combined_arms_control(
    lua: MizLua,
    ctx: &mut Context,
    unit: &Unit,
    id: DcsOid<ClassUnit>,
    ucid: Ucid,
    now: DateTime<Utc>,
) -> Result<()> {
    if player_slotted_in_aircraft_or_helicopter(ctx, &ucid) {
        info!("CA control skipped for {ucid}: still in aircraft/helicopter slot");
        return Ok(());
    }
    match ctx.db.on_combined_arms_enter(id, ucid, now)? {
        CaEnterRes::Rejected => {
            ctx.db.ephemeral.force_player_to_spectators(&ucid);
            Ok(())
        }
        CaEnterRes::LifeTaken => {
            let unit_id = unit.id()?;
            if let Err(e) = schedule_ca_life_taken_ui(lua, ucid, unit_id) {
                error!("could not schedule CA life taken UI {:?}", e);
            }
            Ok(())
        }
        CaEnterRes::NotApplicable | CaEnterRes::Ok => Ok(()),
    }
}

fn handle_combined_arms_enter(
    lua: MizLua,
    ctx: &mut Context,
    unit: &dcso3::unit::Unit,
    now: DateTime<Utc>,
) -> Result<()> {
    // getCategory() is Object.Category.UNIT for every unit — use getCategoryEx (Unit.Category).
    let cat = unit.get_category_ex()?;
    if cat != dcso3::unit::UnitCategory::GroundUnit {
        return Ok(());
    }
    let id = unit.object_id()?;
    if !ctx.db.is_combined_arms_life_unit(&id) {
        return Ok(());
    }
    let Some(ucid) = resolve_combined_arms_controller(ctx, unit, &id) else {
        warn!("CA enter: could not resolve controller for {id:?}");
        return Ok(());
    };
    apply_combined_arms_control(lua, ctx, unit, id, ucid, now)
}

/// Shot (and ShootingStart for MG/autocannon): real callsign often appears here; not-in-air ⇒ CA.
fn handle_combined_arms_shot(
    lua: MizLua,
    ctx: &mut Context,
    unit: &Unit,
    now: DateTime<Utc>,
) -> Result<()> {
    let cat = match unit.get_category_ex() {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    if cat != dcso3::unit::UnitCategory::GroundUnit {
        return Ok(());
    }
    let id = match unit.object_id() {
        Ok(id) => id,
        Err(_) => return Ok(()),
    };
    if !ctx.db.is_combined_arms_life_unit(&id) {
        return Ok(());
    }
    if ctx.db.ca_controller(&id).is_some() {
        return Ok(());
    }
    let Some(ucid) = resolve_combined_arms_controller(ctx, unit, &id) else {
        warn!("CA shot: could not resolve controller for {id:?}");
        return Ok(());
    };
    apply_combined_arms_control(lua, ctx, unit, id, ucid, now)
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
                // Still count war losses / kill bookkeeping; skip persisted unit_dead.
                ctx.shots_out.dead(id.clone(), now);
                ctx.db.campaign_record_player_airframe_loss(&ucid, &id);
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
    ctx.db.on_combined_arms_unit_dead(&id);
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
                match ctx.db.unit_born(lua, &unit, &ctx.connected, birth_place, b.subplace) {
                    Ok(BirthRes::None) => (),
                    Ok(BirthRes::OccupiedSlot(slot)) => {
                        if ctx.db.ephemeral.cfg.limited_lives && ctx.db.ephemeral.cfg.lives_birth {
                            let typ = ctx
                                .db
                                .ephemeral
                                .get_slot_info(&slot)
                                .and_then(|sifo| ctx.db.ephemeral.cfg.life_types.get(&sifo.typ))
                                .copied();
                            // Defer panel/sound so Birth returns before UI (cockpit switch is fragile).
                            if let Err(e) = schedule_life_taken_ui(lua, slot.clone(), typ) {
                                error!("could not schedule life taken UI {:?}", e)
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
                if let Err(e) = ctx.db.register_dynamic_cargo_static(lua, &st) {
                    error!("dynamic cargo register failed {:?} {:?}", st, e);
                }
            }
        }
        Event::PlayerEnterUnit(e) => {
            if let Some(initiator) = e.initiator {
                if let Ok(unit) = initiator.as_unit() {
                    match handle_combined_arms_enter(lua, ctx, &unit, start_ts) {
                        Ok(()) => {}
                        Err(err) => error!("combined arms enter failed: {err:?}"),
                    }
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
                let ca_oid = initiator.clone();
                let ca_return_life = Unit::get_instance(lua, &ca_oid)
                    .ok()
                    .and_then(|u| {
                        let exist = u.is_exist().unwrap_or(false);
                        let alive = u.get_life().map(|l| l > 0).unwrap_or(false);
                        Some(exist && alive)
                    })
                    .unwrap_or(false);
                let ca_unit_id = Unit::get_instance(lua, &ca_oid)
                    .ok()
                    .and_then(|u| u.id().ok());
                if let Some((ucid, returned)) =
                    ctx.db.on_combined_arms_leave(&ca_oid, ca_return_life)
                {
                    if returned {
                        let mut msg = CompactString::new("life returned\n");
                        if let Ok(l) =
                            format_lives_total(&mut ctx.db, &ucid, LifeType::CombinedArms)
                        {
                            msg.push_str(&l);
                        }
                        ctx.db.ephemeral.panel_to_player(&ctx.db.persisted, 10, &ucid, msg);
                        if let Some(unit_id) = ca_unit_id {
                            ctx.db.play_sound_unit(lua, "life_return", unit_id);
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
                        ctx.db.play_sound_player(lua, "life_return", &slot);
                        if let Some((ucid, slot)) = deslot {
                            ctx.db.player_deslot_slot(&ucid, &slot);
                        }
                    }
                    Ok((_, None, deslot)) => {
                        if let Some((ucid, slot)) = &deslot {
                            maybe_warn_life_not_returned(ctx, lua, *ucid, slot);
                        }
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
                if let Some(shooter) =
                    crate::shots::who_from_initiator(&ctx.db, e.initiator.as_ref())
                {
                    if let Err(e) = ctx.shots_out.hit_by_who(
                        &ctx.db,
                        start_ts,
                        dead,
                        &target,
                        shooter,
                        e.weapon_name.clone(),
                    ) {
                        error!("error processing hit event {:?}", e)
                    }
                } else if let Some(shooter) = e.initiator.and_then(|u| u.as_unit().ok()) {
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
            // Register CA controller before shot bookkeeping so who() sees the player.
            if let Err(err) = handle_combined_arms_shot(lua, ctx, &e.initiator, start_ts) {
                error!("combined arms shot failed: {err:?}");
            }
            if let Err(e) = ctx.shots_out.shot(&ctx.db, start_ts, &e) {
                error!("error processing shot event {:?}", e)
            }
            ()
        }
        Event::ShootingStart(e) => {
            // MG/autocannon do not emit Shot (Hoggit); same CA identity rule as Shot.
            if let Some(initiator) = e.initiator.as_ref().and_then(|o| o.as_unit().ok()) {
                if let Err(err) = handle_combined_arms_shot(lua, ctx, &initiator, start_ts) {
                    error!("combined arms shooting_start failed: {err:?}");
                }
            }
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
                            ctx.persist_campaign_state();
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
                    // Before deslot: player_in_unit still resolves for war-loss airframe bucket.
                    ctx.db.campaign_record_player_airframe_loss(&ucid, &id);
                    finish_airborne_exit(ctx, ucid, &id);
                    ctx.db.player_deslot(&ucid);
                }
                if let Err(e) = unit_killed(lua, ctx, id, start_ts) {
                    error!("2 unit killed failed {}", e)
                }
            }
        }
        Event::LandingAfterEjection(e) => {
            if ctx.db.csar_enabled() {
                let landing_pos = e
                    .initiator
                    .get_position()
                    .or_else(|_| {
                        e.initiator.get_point().map(|p| Position3 {
                            p,
                            x: p,
                            y: p,
                            z: p,
                        })
                    });
                match landing_pos {
                    Ok(pos) => {
                        match ctx.db.on_csar_landing_after_ejection(lua, start_ts, pos) {
                            Ok(true) => {
                                flush_markup_if_pending(ctx, lua);
                                ctx.persist_campaign_state();
                            }
                            Ok(false) => (),
                            Err(e) => warn!("csar landing update failed: {e:?}"),
                        }
                    }
                    Err(e) => warn!("csar landing event missing position: {e:?}"),
                }
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
                                message_life(ctx, lua, &slot, Some(typ), "life taken\n")
                            {
                                error!("could not display life taken message {:?}", e)
                            } else {
                                ctx.db.play_sound_player(lua, "life_taken", &slot);
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
                // Always arm life-return even if sanitize already cleared `airborne`.
                if !ctx.recently_born.contains_key(&id) {
                    let _ = ctx.airborne.remove(&id);
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
            let ctx = Context::get_mut();
            if let Err(e) = crate::acmi_sanitize::maybe_spawn(&ctx.db.ephemeral.cfg.acmi_sanitize) {
                error!("acmi_sanitize: {e:?}");
            }
            spawn_setmissionstartdatetime(ctx, lua, false);
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
                && (cfg.lives_birth || _typ == LifeType::CombinedArms)
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
    // 1+ only while still in players_by_slot (`current_slot` can linger after death),
    // or while controlling a Combined Arms ground unit.
    let active_life_type = player
        .current_slot
        .as_ref()
        .filter(|(slot, _)| {
            db.ephemeral
                .player_in_slot(slot)
                .is_some_and(|u| u == ucid)
        })
        .and_then(|(slot, _)| db.ephemeral.get_slot_info(slot))
        .and_then(|sifo| cfg.life_types.get(&sifo.typ))
        .copied()
        .or_else(|| {
            db.ephemeral
                .ca_oid_by_controller
                .contains_key(ucid)
                .then_some(LifeType::CombinedArms)
        });
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

const LIFE_TAKEN_SOUND_DELAY_SECS: f32 = 4.5;
/// Let Birth finish before life-taken panel (keeps DCS cockpit switch responsive).
const LIFE_TAKEN_UI_DELAY_SECS: f32 = 0.25;

fn schedule_life_taken_ui(lua: MizLua, slot: SlotId, typ: Option<LifeType>) -> Result<()> {
    let timer = Timer::singleton(lua)?;
    let when = timer.get_time()? + LIFE_TAKEN_UI_DELAY_SECS;
    timer.schedule_function(when, mlua::Value::Nil, move |lua, _, _| {
        let ctx = unsafe { Context::get_mut() };
        if let Err(e) = message_life(ctx, lua, &slot, typ, "life taken\n") {
            error!("could not display life taken message {:?}", e);
        } else if let Err(e) = schedule_life_taken_sound(lua, slot) {
            error!("could not schedule life taken sound {:?}", e);
        }
        Ok(None)
    })?;
    Ok(())
}

fn schedule_life_taken_sound(lua: MizLua, slot: SlotId) -> Result<()> {
    let timer = Timer::singleton(lua)?;
    let when = timer.get_time()? + LIFE_TAKEN_SOUND_DELAY_SECS;
    timer.schedule_function(when, mlua::Value::Nil, move |lua, _, _| {
        let ctx = unsafe { Context::get_mut() };
        ctx.db.play_sound_player(lua, "life_taken", &slot);
        Ok(None)
    })?;
    Ok(())
}

fn schedule_ca_life_taken_ui(lua: MizLua, ucid: Ucid, unit_id: UnitId) -> Result<()> {
    let timer = Timer::singleton(lua)?;
    let when = timer.get_time()? + LIFE_TAKEN_UI_DELAY_SECS;
    timer.schedule_function(when, mlua::Value::Nil, move |lua, _, _| {
        let ctx = unsafe { Context::get_mut() };
        let mut msg = CompactString::new("life taken\n");
        if let Ok(lives) = lives(
            &mut ctx.db,
            &ucid,
            Some(LifeType::CombinedArms),
            true,
            LivesFormat::Panel,
        ) {
            msg.push_str(&lives);
        }
        ctx.db
            .ephemeral
            .panel_to_player(&ctx.db.persisted, 10, &ucid, msg);
        if let Err(e) = schedule_ca_life_taken_sound(lua, unit_id) {
            error!("could not schedule CA life taken sound {:?}", e);
        }
        Ok(None)
    })?;
    Ok(())
}

fn schedule_ca_life_taken_sound(lua: MizLua, unit_id: UnitId) -> Result<()> {
    let timer = Timer::singleton(lua)?;
    let when = timer.get_time()? + LIFE_TAKEN_SOUND_DELAY_SECS;
    timer.schedule_function(when, mlua::Value::Nil, move |lua, _, _| {
        let ctx = unsafe { Context::get_mut() };
        ctx.db.play_sound_unit(lua, "life_taken", unit_id);
        Ok(None)
    })?;
    Ok(())
}

fn maybe_warn_life_not_returned(ctx: &mut Context, lua: MizLua, ucid: Ucid, slot: &SlotId) {
    let cfg = &ctx.db.ephemeral.cfg;
    if !(cfg.limited_lives && cfg.lives_birth) {
        return;
    }
    let Some(sifo) = ctx.db.ephemeral.get_slot_info(slot) else {
        return;
    };
    if !is_aircraft_or_helicopter(&ctx.db, &sifo.typ) {
        return;
    }
    let msg = CompactString::from(
        "Life not returned\nLand and stop at a friendly objective, then deslot",
    );
    if let Some(uid) = slot.as_unit_id() {
        ctx.db.ephemeral.msgs().panel_to_unit(12, false, uid, msg);
    } else if let Ok(trigger) = Trigger::singleton(lua) {
        if let Ok(action) = trigger.action() {
            let _ = action.out_text_for_group(sifo.miz_gid, msg.into(), 12, false);
        }
    }
    info!("life not returned on deslot for {ucid} (not marked landed at friendly objective)");
}

fn message_life(
    ctx: &mut Context,
    lua: MizLua,
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
    let play_life_return = msg.starts_with("life returned");
    ctx.db.ephemeral.msgs().panel_to_unit(10, false, uid, msg);
    if play_life_return {
        ctx.db.play_sound_player(lua, "life_return", slot);
    }
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
                if db.ephemeral.cfg.limited_lives && db.ephemeral.cfg.lives_birth {
                    let obj_label = db
                        .ephemeral
                        .player_in_slot(&slot)
                        .and_then(|ucid| db.persisted.players.get(ucid))
                        .and_then(|p| {
                            p.current_slot
                                .as_ref()
                                .and_then(|(_, inst)| inst.as_ref())
                                .and_then(|i| i.landed_at_objective)
                        })
                        .and_then(|oid| db.persisted.objectives.get(&oid))
                        .map(|o| db.objective_f10_map_label(o))
                        .unwrap_or_else(|| "friendly objective".to_string());
                    let msg = format_compact!(
                        "Landed at {obj_label}\nDeslot to return your life"
                    );
                    if let Some(uid) = slot.as_unit_id() {
                        db.ephemeral.msgs().panel_to_unit(15, false, uid, msg);
                    }
                }
                return false;
            }
            debug!(
                "land() not armed slot={slot:?} pos=({:.0},{:.0}) — outside friendly objective zone?",
                pos.0.x, pos.0.y
            );
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
    let captures = ctx.db.check_capture(lua, ts)?;
    if !captures.is_empty() {
        ctx.db.discord_map_debounce_post(ts);
    }
    for (side, oid) in captures {
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
            write!(report, "{}\n", ewr::report_header())?;
            for gibbraa in braa_to_chickens {
                write!(report, "{gibbraa}\n")?;
            }
            msgs.push((uid, report));
        }
    }
    for (uid, msg) in msgs {
        ctx.db.ephemeral.msgs().panel_to_unit(ewr::EWR_PANEL_DISPLAY_SECS, false, uid, msg)
    }
    Ok(())
}

fn sync_restart_warnings(ctx: &mut Context, when: DateTime<Utc>) -> &mut AutoShutdown {
    let warn = ctx.restart_warnings.get_or_insert_with(|| AutoShutdown::new(when));
    if warn.when != when {
        *warn = AutoShutdown::new(when);
    }
    warn
}

/// Advance wall clock for restart warnings when bflib became ready after mission load.
fn restart_warning_effective_now(now: DateTime<Utc>, cfg: &bfprotocols::cfg::Cfg, skew_secs: u32) -> DateTime<Utc> {
    if cfg.shutdown.is_some() || cfg.dcsserver_bot_scheduled_restart.is_none() {
        now
    } else {
        now + Duration::seconds(i64::from(skew_secs))
    }
}

fn check_auto_shutdown(
    ctx: &mut Context,
    lua: MizLua,
    now: DateTime<Utc>,
) -> Result<AdminResult> {
    let cfg = &ctx.db.ephemeral.cfg;
    let bflib_panels = cfg.shutdown.is_some()
        || cfg.dcsserver_bot_scheduled_restart.is_some();
    let warn_now = restart_warning_effective_now(now, cfg, ctx.db.restart_display_skew_secs());
    if let Some(when) = cfg.map_restart_when(now, ctx.shutdown.map(|s| s.when)) {
        let sound_key = {
            let warn = sync_restart_warnings(ctx, when);
            if !warn.one_minute_warning && when - warn_now <= Duration::minutes(1) {
                warn.one_minute_warning = true;
                Some(("warning_1", true, "The server will restart in one minute"))
            } else if !warn.five_minute_warning && when - warn_now <= Duration::minutes(5) {
                warn.five_minute_warning = true;
                Some(("warning_2", true, "The server will restart in 5 minutes"))
            } else if !warn.ten_minute_warning && when - warn_now <= Duration::minutes(10) {
                warn.ten_minute_warning = true;
                Some(("warning_3", true, "The server will restart in 10 minutes"))
            } else if !warn.thirty_minute_warning && when - warn_now <= Duration::minutes(30) {
                warn.thirty_minute_warning = true;
                Some(("warning_4", false, "The server will restart in 30 minutes"))
            } else {
                None
            }
        };
        if let Some((key, clear, panel)) = sound_key {
            if bflib_panels {
                ctx.db.ephemeral.msgs().panel_to_all(60, clear, panel);
            }
            ctx.db.play_sound_all(lua, key);
        }
        if bflib_panels && now > when {
            return admin::admin_shutdown(ctx, lua, None);
        }
    } else {
        ctx.restart_warnings = None;
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
                ctx.db.play_sound_player(lua, "life_return", &slot);
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

fn award_periodic_points(ctx: &mut Context, lua: MizLua, ts: DateTime<Utc>) {
    let Some(points) = ctx.db.ephemeral.cfg.points.as_ref() else {
        return;
    };
    let (gain, period) = points.periodic_point_gain;
    if period == 0 {
        return;
    }
    let elapsed = (ts - ctx.last_periodic_points).num_seconds();
    if elapsed < period as i64 {
        return;
    }
    ctx.last_periodic_points = ts;
    let airborne_only = points.periodic_award_airborne;

    if points.balancing_point_gain {
        // Balanced periodic gain only; penalties and other debits are unchanged.
        if gain <= 0 {
            return;
        }
        let (online_red, online_blue) = balancing_side_counts(lua, ctx).unwrap_or((0, 0));
        let red_award = balanced_side_gain(online_red, online_blue, gain);
        let blue_award = balanced_side_gain(online_blue, online_red, gain);
        for ifo in ctx.connected.info_by_player_id.values() {
            if airborne_only && !eligible_for_airborne_periodic_award(ctx, lua, &ifo.ucid) {
                continue;
            }
            let Some(player) = ctx.db.player(&ifo.ucid) else {
                continue;
            };
            let award = match player.side {
                Side::Red => &red_award,
                Side::Blue => &blue_award,
                _ => continue,
            };
            if award.amount == 0 {
                continue;
            }
            let why = if award.balancing {
                "periodic award (balancing)"
            } else {
                "periodic award"
            };
            ctx.db.adjust_points(&ifo.ucid, award.amount, why);
        }
    } else if gain != 0 {
        for ifo in ctx.connected.info_by_player_id.values() {
            if airborne_only && !eligible_for_airborne_periodic_award(ctx, lua, &ifo.ucid) {
                continue;
            }
            ctx.db.adjust_points(&ifo.ucid, gain, "periodic award");
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
        match discord_map_live_ctx(lua, ctx) {
            Ok(live) => {
                if let Err(e) =
                    ctx.db
                        .discord_map_tick(lua, ts, ctx.connected.len() > 0, &live)
                {
                    error!("discord map post failed: {e:#}");
                }
            }
            Err(e) => error!("discord map live ctx failed: {e:#}"),
        }
        for (oid, vh) in ctx.db.ephemeral.warehouses_to_sync() {
            if let Err(e) = ctx.db.sync_vehicle_at_obj(lua, oid, vh.clone()) {
                error!(
                    "failed to sync warehouse at objective {:?} vehicle {:?} {:?}",
                    oid, vh, e
                )
            }
        }
        if ctx.db.dynamic_cargo_enabled() {
            ctx.db.prune_missing_dynamic_cargo(lua);
            ctx.db.refresh_dynamic_cargo_snapshots(lua);
        } else {
            ctx.db.clear_dynamic_cargo_if_disabled();
        }
        return_lives(lua, ctx, ts);
        ctx.recently_born.retain(|_, ts| start_ts - *ts <= Duration::seconds(5));
        {
            // report kills
            let cfg = Arc::clone(&ctx.db.ephemeral.cfg);
            for dead in ctx.shots_out.bring_out_your_dead(ts) {
                info!("kill {:?}", dead);
                ctx.db.campaign_on_victim_killed(&dead);
                ctx.db.campaign_top10_on_kill(&dead);
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
        if let Err(e) = ctx.db.maybe_do_static_repairs(lua, &spctx, &ctx.idx, ts) {
            error!("error doing ME static repairs {:?}", e)
        }
        record_perf(&mut perf.do_repairs, start_ts);
        if let Err(e) = ctx.db.advance_actions(lua, &ctx.idx, &ctx.jtac, start_ts, perf) {
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
            ctx.db.ephemeral.cfg.ewr_antenna_height_m,
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
                let mut threat_changed: SmallVec<[ObjectiveId; 8]> = smallvec![];
                threat_changed.extend(threatened.iter().copied());
                threat_changed.extend(cleared.iter().copied());
                for oid in &threatened {
                    let obj = ctx.db.objective(oid)?;
                    let owner = obj.owner();
                    let msg = format_compact!(
                        "enemies spotted near {}",
                        ctx.db.objective_f10_map_label(obj)
                    );
                    ctx.db.ephemeral.msgs().panel_to_side(10, false, owner, msg)
                }
                for oid in &cleared {
                    let obj = ctx.db.objective(oid)?;
                    let owner = obj.owner();
                    let msg = format_compact!(
                        "{} is no longer threatened",
                        ctx.db.objective_f10_map_label(obj)
                    );
                    ctx.db.ephemeral.msgs().panel_to_side(10, false, owner, msg)
                }
                if !threat_changed.is_empty() {
                    if let Err(e) = ctx
                        .db
                        .sync_hub_production_for_opr_threat_feeds(&threat_changed)
                    {
                        error!("could not sync OLO production after threat change {e}");
                    }
                    db::logistics::refresh_virtual_resupply_threat_markups(
                        &ctx.db.persisted,
                        &mut ctx.db.ephemeral,
                        &threat_changed,
                    );
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
        ctx.db.campaign_flush_online_before_save(now);
        if let Some(snap) = ctx.db.maybe_snapshot() {
            ctx.do_bg_task(bg::Task::SaveState(path.clone(), snap));
        }
        record_perf(&mut perf.snapshot, now);
        award_periodic_points(ctx, lua, start_ts);
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
    let _act = Trigger::singleton(lua)?.action()?;
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
    if ctx.db.csar_enabled() {
        ctx.db.update_all_csar_pilots(lua, ts);
    }
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
    if let Err(e) = ctx.db.drain_pending_dep_farp_static_slot_releases(&spctx, &ctx.idx) {
        error!("error releasing DEP FARP static slots {:?}", e)
    }
    if let Err(e) = ctx.db.ephemeral.process_spawn_queue(
        perf,
        &ctx.db.persisted,
        ts,
        &ctx.idx,
        &spctx,
        &ctx.shots_out,
    ) {
        error!("error processing spawn queue {:?}", e)
    }
    record_perf(&mut perf.spawn_queue, now);
    if let Err(e) = ctx.db.try_run_deferred_tisp_initial_ships(
        lua,
        &ctx.idx,
        now,
        perf,
        &ctx.shots_out,
    ) {
        error!("deferred TISP initial ships: {e:?}");
    }
    ctx.db.try_announce_actions_unlocked(now);
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
    let now = Utc::now();
    let max_rate = ctx.db.ephemeral.cfg.max_msgs_per_second;
    if let Err(e) = ctx.db.process_markup_frame(lua, max_rate) {
        error!("could not process markup frame {e:?}");
    }
    record_perf(&mut perf.process_messages, now);
    ctx.load_state.step();
    record_perf(&mut perf.timed_events, ts);
    ctx.log_perf(now);
    Ok(AdminResult::Continue)
}

fn initiate_dcs_shutdown(ctx: &mut Context, lua: MizLua) -> Result<()> {
    info!("initiating DCS shutdown");
    if let Some(id) = ctx.event_handler_id.take() {
        World::singleton(lua)?
            .remove_event_handler(id)
            .context("removing event handler")?;
    }
    Net::singleton(lua)?.dostring_in(
        DcsLuaEnvironment::Server,
        "DCS.setUserCallbacks({}); DCS.exitProcess()".into(),
    )?;
    Ok(())
}

fn external_request_shutdown(lua: MizLua) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    if !ctx.load_state.init_ok() {
        bail!("mission not ready for shutdown");
    }
    admin::request_shutdown(ctx, lua)?;
    initiate_dcs_shutdown(ctx, lua)
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
                    if let Err(e) = initiate_dcs_shutdown(ctx, lua) {
                        error!("failed to initiate DCS shutdown {e:?}");
                    }
                    return Ok(None);
                }
                Err(e) => {
                    let detail = e
                        .downcast_ref::<anyhow::Error>()
                        .map(|e| format!("{e:?}"))
                        .or_else(|| e.downcast_ref::<&str>().map(|s| (*s).to_string()))
                        .unwrap_or_else(|| format!("{e:?}"));
                    error!(
                        "run_timed_events panicked {detail} {}",
                        Backtrace::capture()
                    );
                }
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
        "campaign cfg: airborne_deslot_block={} airborne_deslot_penalty_secs={} airborne_deslot_penalty_points={} csar={} virtual_resupply={} virtual_resupply_threatened_without_deliveries={}",
        cfg.airborne_deslot_block,
        cfg.airborne_deslot_penalty_secs,
        cfg.airborne_deslot_penalty_points,
        cfg.csar.enabled,
        cfg.virtual_resupply,
        cfg.virtual_resupply_threatened_without_deliveries,
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
            .campaign_on_mission_start(lua, false)
            .context("campaign stats init")?;
        let round_start = Utc::now();
        ctx.db.schedule_new_round_action_lock(
            round_start + Duration::seconds(db::mizinit::NEW_ROUND_ACTION_LOCK_SECS),
        );
        ctx.db.announce_new_round_action_lock();
        ctx.db
            .schedule_tisp_initial_ship_placement(round_start + Duration::seconds(60));
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
        ctx.db
            .campaign_on_mission_start(lua, true)
            .context("campaign stats round")?;
        db::ai_air::sweep_expired_owner_locks_at_round_start(&mut ctx.db);
    }
    if ctx.db.ephemeral.cfg.front_line {
        let theatre = theatre_slug(lua);
        ctx.db
            .ephemeral
            .load_front_line_water_grid(&path, theatre.as_str());
    }
    {
        let miz_path = db::discord_map::resolve_mission_miz_path(lua, &path)?;
        db::discord_map::init_discord_map(lua, &mut ctx.db, &miz, &miz_path, &path)
            .context("discord map init")?;
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
    db::discord_map::capture_restart_display_skew(lua, &mut ctx.db)
        .context("restart countdown skew")?;
    if ctx.db.ephemeral.cfg.discord_map.enabled {
        ctx.db
            .bootstrap_discord_map(lua, &discord_map_live_ctx(lua, ctx)?)
            .context("discord map bootstrap")?;
        ctx.db
            .schedule_discord_map_periodic(Utc::now(), ctx.connected.len() > 0);
    }
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
            format_compact!("Welcome, {}! Type -help for commands.", name)
        };
        ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), welcome);
    }
    info!("starting timed events");
    ctx.db.refresh_csar_after_load(lua);
    if let Err(e) = ctx.db.flush_markup_messages(lua) {
        warn!("csar mark rebuild flush failed {e:?}");
    }
    ctx.persist_campaign_state();
    start_timed_events(ctx, lua, path).context("starting the timed events loop")?;
    Ok(())
}

fn on_mission_load_end(_lua: HooksLua) -> Result<()> {
    crate::acmi_sanitize::reset_spawn_state();
    crate::setmissionstartdatetime::reset_spawn_state();
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
                error!("THE MISSION CANNOT START: {e:#}");
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
    let exports = dcso3::create_root_module(lua, init_hooks, init_miz)?;
    exports.set(
        "requestShutdown",
        lua.create_function(|lua, ()| {
            dcso3::wrap_f("requestShutdown", MizLua::from_env(lua), external_request_shutdown)
        })?,
    )?;
    Ok(exports)
}
