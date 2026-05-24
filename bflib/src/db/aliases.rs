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

use super::objective::Objective;
use anyhow::Result;
use bfprotocols::{
    db::objective::ObjectiveKind,
    miz_trigger::SETTINGS_OBJECTIVE_ALIASES_ZONE,
    tisp::ship_pad_display_name,
};
use dcso3::{coalition::Side, env::miz::Miz};
use fxhash::FxHashMap;
use log::{info, warn};

/// Map trigger-zone property key → user-facing label (map markup, optional chat lookup).
pub fn load_objective_display_aliases(miz: &Miz) -> Result<FxHashMap<String, std::string::String>> {
    let mut out: FxHashMap<String, std::string::String> = FxHashMap::default();
    for zone in miz.triggers()? {
        let zone = zone?;
        if zone.name()?.as_str() != SETTINGS_OBJECTIVE_ALIASES_ZONE {
            continue;
        }
        for prop in zone.properties()? {
            let prop = prop?;
            let key = prop.key.trim();
            let value = prop.value.trim();
            if key.is_empty() {
                warn!(
                    "SETTINGS-aliases: ignoring empty property key in zone {:?}",
                    SETTINGS_OBJECTIVE_ALIASES_ZONE
                );
                continue;
            }
            if value.is_empty() {
                warn!(
                    "SETTINGS-aliases: ignoring empty value for key {:?}",
                    prop.key
                );
                continue;
            }
            if out
                .insert(key.to_string(), value.to_string())
                .is_some()
            {
                warn!(
                    "SETTINGS-aliases: duplicate key {:?}, keeping last value {:?}",
                    key, value
                );
            }
        }
        info!(
            "loaded {} objective display alias(es) from {:?}",
            out.len(),
            SETTINGS_OBJECTIVE_ALIASES_ZONE
        );
        break;
    }
    Ok(out)
}

fn owner_alias_prefix(side: Side) -> Option<char> {
    match side {
        Side::Blue => Some('B'),
        Side::Red => Some('R'),
        Side::Neutral => Some('N'),
    }
}

/// Lookup keys for `SETTINGS-aliases` (ME property key → display value).
fn objective_alias_lookup_keys(obj: &Objective) -> Vec<std::string::String> {
    let mut keys = vec![obj.name.to_string()];
    if let Some(p) = owner_alias_prefix(obj.owner) {
        let name = obj.name.as_str();
        if let Some(first) = name.split_whitespace().next() {
            if !first.is_empty() {
                keys.push(format!("{p}{first}"));
            }
        }
        let compact: String = name.chars().filter(|c| !c.is_whitespace()).collect();
        if !compact.is_empty() && compact != name {
            keys.push(format!("{p}{compact}"));
        }
    }
    keys
}

/// User-facing label for map markup; `obj.name` stays the internal id for logistics and save.
pub fn resolve_objective_display_name(
    aliases: &FxHashMap<String, std::string::String>,
    obj: &Objective,
) -> std::string::String {
    if let ObjectiveKind::Farp {
        mobile: true,
        pad_template,
        ..
    } = &obj.kind
    {
        if let Some(display) = aliases.get(pad_template.as_str()) {
            return display.clone();
        }
    }
    for key in objective_alias_lookup_keys(obj) {
        if let Some(display) = aliases.get(key.as_str()) {
            return display.clone();
        }
    }
    if let Some((_, display)) = aliases
        .iter()
        .find(|(_, v)| v.as_str() == obj.name.as_str())
    {
        return display.clone();
    }
    obj.name.to_string()
}

fn push_token(out: &mut Vec<std::string::String>, s: &str) {
    let s = s.trim();
    if !s.is_empty() && !out.iter().any(|x| x.eq_ignore_ascii_case(s)) {
        out.push(s.to_string());
    }
}

/// Short names players use in chat (`Gubskaya`, `Kashuri`, `Forrestal`) — no `R`/`B` prefix, no `ROAD FOB` suffix.
pub fn objective_chat_lookup_names(
    obj: &Objective,
    aliases: &FxHashMap<String, std::string::String>,
) -> Vec<std::string::String> {
    let mut out = Vec::new();
    push_token(&mut out, obj.name.as_str());
    if let Some(first) = obj.name.split_whitespace().next() {
        push_token(&mut out, first);
    }
    if let ObjectiveKind::Farp {
        mobile: true,
        pad_template,
        ..
    } = &obj.kind
    {
        push_token(&mut out, pad_template.as_str());
        push_token(&mut out, ship_pad_display_name(pad_template.as_str()).as_str());
    }
    let display = resolve_objective_display_name(aliases, obj);
    push_token(&mut out, display.as_str());
    if let Some(first) = display.split_whitespace().next() {
        push_token(&mut out, first);
    }
    out
}

/// Case-insensitive exact match on [`objective_chat_lookup_names`].
pub fn objective_matches_chat_name(
    obj: &Objective,
    aliases: &FxHashMap<String, std::string::String>,
    query: &str,
) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return false;
    }
    objective_chat_lookup_names(obj, aliases)
        .iter()
        .any(|n| n.eq_ignore_ascii_case(q))
}
