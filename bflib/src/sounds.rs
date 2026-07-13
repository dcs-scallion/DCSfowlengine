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
use dcso3::{net::SlotId, trigger::Trigger, MizLua};
use log::debug;
use std::sync::Arc;

pub fn play_player(export: &FowlMizExport, lua: MizLua, key: &str, slot: &SlotId) {
    let Some(path) = export.sounds_player.get(key) else {
        return;
    };
    let Some(unit) = slot.as_unit_id() else {
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

pub fn play_all(export: &FowlMizExport, lua: MizLua, key: &str) {
    let Some(path) = export.sounds_all.get(key) else {
        return;
    };
    let Ok(trigger) = Trigger::singleton(lua) else {
        return;
    };
    let Ok(action) = trigger.action() else {
        return;
    };
    if let Err(e) = action.out_sound(path.clone().into()) {
        debug!("sound {key} for all skipped: {e:?}");
    }
}

pub fn play_player_export(export: &Arc<FowlMizExport>, lua: MizLua, key: &str, slot: &SlotId) {
    play_player(export, lua, key, slot);
}

pub fn play_all_export(export: &Arc<FowlMizExport>, lua: MizLua, key: &str) {
    play_all(export, lua, key);
}
