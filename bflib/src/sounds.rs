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

use bfprotocols::fowl_miz_export::FowlMizExport;
use compact_str::format_compact;
use dcso3::{
    env::miz::{GroupId, UnitId},
    net::{DcsLuaEnvironment, Net, SlotId},
    trigger::Trigger,
    MizLua,
};
use log::{debug, info, warn};
use std::sync::Arc;

pub fn play_unit(export: &FowlMizExport, lua: MizLua, key: &str, unit: UnitId) {
    let Some(path) = export.sounds_player.get(key) else {
        return;
    };
    let Ok(trigger) = Trigger::singleton(lua) else {
        return;
    };
    let Ok(action) = trigger.action() else {
        return;
    };
    if let Err(e) = action.out_sound_for_unit(unit, path.clone().into()) {
        debug!("sound {key} for unit skipped: {e:?}");
    }
}

fn sound_path_safe(path: &str) -> bool {
    !path.contains('"') && !path.contains('\n') && !path.contains('\\')
}

fn dostring_mission(lua: MizLua, chunk: &str) -> bool {
    match Net::singleton(lua).and_then(|n| {
        n.dostring_in(DcsLuaEnvironment::Mission, chunk.into())
    }) {
        Ok(ret) => {
            let ret_s = ret.as_str();
            if !ret_s.is_empty() && ret_s != "nil" {
                info!("mission dostring returned: {ret_s}");
            }
            true
        }
        Err(e) => {
            warn!("mission dostring failed: {e:?}");
            false
        }
    }
}

/// Hooks have no `trigger.action`. Use dostring_in("mission") while player still occupies.
/// outSoundForGroup with the player's own miz_gid is reliable; outSoundForUnit is broken in MP.
pub fn play_life_return_from_hooks(
    export: &FowlMizExport,
    lua: MizLua,
    group: GroupId,
) -> bool {
    let Some(path) = export.sounds_player.get("life_return") else {
        warn!("life_return missing from fowl export sounds_player");
        return false;
    };
    if !sound_path_safe(path) {
        warn!("life_return path rejected: {path}");
        return false;
    }
    let chunk = format_compact!(
        "trigger.action.outSoundForGroup({}, \"{}\")",
        group.inner(),
        path
    );
    let ok = dostring_mission(lua, &chunk);
    if ok {
        info!("life_return outSoundForGroup({}) via mission bridge", group.inner());
    } else {
        warn!("life_return dostring_in failed for group {}", group.inner());
    }
    ok
}

pub fn play_player(export: &FowlMizExport, lua: MizLua, key: &str, slot: &SlotId) {
    let Some(unit) = slot.as_unit_id() else {
        return;
    };
    play_unit(export, lua, key, unit);
}

pub fn play_group(export: &FowlMizExport, lua: MizLua, key: &str, group: GroupId) {
    let Some(path) = export.sounds_player.get(key) else {
        return;
    };
    let Ok(trigger) = Trigger::singleton(lua) else {
        return;
    };
    let Ok(action) = trigger.action() else {
        return;
    };
    if let Err(e) = action.out_sound_for_group(group, path.clone().into()) {
        debug!("sound {key} for group skipped: {e:?}");
    }
}

pub fn play_unit_export(export: &Arc<FowlMizExport>, lua: MizLua, key: &str, unit: UnitId) {
    play_unit(export, lua, key, unit);
}

pub fn play_all(export: &FowlMizExport, lua: MizLua, key: &str) {
    let Some(path) = export.sounds_all.get(key) else {
        debug!("sound {key} for all skipped: not in fowl export");
        return;
    };
    let Ok(trigger) = Trigger::singleton(lua) else {
        return;
    };
    let Ok(action) = trigger.action() else {
        return;
    };
    if let Err(e) = action.out_sound(path.clone().into()) {
        warn!("sound {key} ({path}) for all failed: {e:?}");
    }
}

pub fn play_player_export(export: &Arc<FowlMizExport>, lua: MizLua, key: &str, slot: &SlotId) {
    play_player(export, lua, key, slot);
}

pub fn play_all_export(export: &Arc<FowlMizExport>, lua: MizLua, key: &str) {
    play_all(export, lua, key);
}
