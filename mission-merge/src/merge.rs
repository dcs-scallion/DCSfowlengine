use anyhow::{bail, Context, Result};
use mlua::{Lua, Table, Value};
use std::collections::{HashMap, HashSet};

const SIDES: [&str; 3] = ["blue", "red", "neutrals"];
const GROUP_KINDS: [&str; 5] = ["plane", "helicopter", "vehicle", "ship", "static"];

#[derive(Debug)]
pub struct MergeStats {
    pub groups: usize,
    pub units: usize,
    pub zones: usize,
    pub countries_created: usize,
    pub source_theatre: Option<String>,
    pub dest_theatre: Option<String>,
    pub warnings: Vec<String>,
    pub clusters_moved: usize,
    pub clusters_kept: usize,
}

pub fn load_mission_table<'lua>(lua: &'lua Lua, source: &str, label: &str) -> Result<Table<'lua>> {
    lua.load(source)
        .set_name(label)
        .exec()
        .with_context(|| format!("parsing {label} as Lua"))?;
    let mission: Table = lua
        .globals()
        .raw_get("mission")
        .with_context(|| format!("{label}: global 'mission' missing"))?;
    lua.globals().raw_set("mission", Value::Nil)?;
    Ok(mission)
}

pub fn merge_missions(lua: &Lua, source: Table, dest: Table) -> Result<MergeStats> {
    let source_theatre = theatre_of(&source)?;
    let dest_theatre = theatre_of(&dest)?;
    let mut warnings = Vec::new();
    if source_theatre == dest_theatre {
        if let Some(t) = &source_theatre {
            warnings.push(format!(
                "source and dest theatre are both {t:?}; coordinates are still copied as-is"
            ));
        }
    }

    let dest_idx = index_mission(&dest).context("indexing dest mission")?;
    let src_idx = index_mission(&source).context("indexing source mission")?;

    let mut collisions = Vec::new();
    for name in &src_idx.group_names {
        if dest_idx.group_names.contains(name) {
            collisions.push(format!("group {name}"));
        }
    }
    for name in &src_idx.unit_names {
        if dest_idx.unit_names.contains(name) {
            collisions.push(format!("unit {name}"));
        }
    }
    for name in &src_idx.zone_names {
        if dest_idx.zone_names.contains(name) {
            collisions.push(format!("zone {name}"));
        }
    }
    if !collisions.is_empty() {
        collisions.sort();
        bail!(
            "name collision(s) with dest mission (rename or remove from source or dest): {}",
            collisions.join(", ")
        );
    }

    if src_idx.groups == 0 && src_idx.zones == 0 {
        bail!("source mission has no groups and no trigger zones to copy");
    }

    let mut next_gid = dest_idx.max_gid.max(0) + 1;
    let mut next_uid = dest_idx.max_uid.max(0) + 1;
    let mut next_zid = dest_idx.max_zid.max(0) + 1;
    let mut gid_map: HashMap<i64, i64> = HashMap::new();
    let mut uid_map: HashMap<i64, i64> = HashMap::new();
    let mut zid_map: HashMap<i64, i64> = HashMap::new();
    let mut countries_created = 0usize;
    let mut groups_copied = 0usize;
    let mut units_copied = 0usize;
    let mut copied_groups: Vec<Table> = Vec::new();

    let src_coa: Table = source
        .raw_get("coalition")
        .context("source mission.coalition")?;
    for side in SIDES {
        let Ok(src_side) = src_coa.raw_get::<_, Table>(side) else {
            continue;
        };
        let Ok(src_countries) = src_side.raw_get::<_, Table>("country") else {
            continue;
        };
        for pair in src_countries.pairs::<Value, Table>() {
            let (_, src_country) = pair?;
            let cid: i64 = src_country
                .raw_get("id")
                .context("source country.id")?;
            let cname: String = src_country
                .raw_get("name")
                .unwrap_or_else(|_| format!("country-{cid}"));
            let dest_country = ensure_dest_country(
                lua,
                &dest,
                side,
                cid,
                &cname,
                &mut countries_created,
            )?;
            for kind in GROUP_KINDS {
                let Some(src_groups) = category_groups(&src_country, kind)? else {
                    continue;
                };
                let dest_groups = ensure_category_groups(lua, &dest_country, kind)?;
                let mut dest_len = dest_groups.raw_len();
                for gpair in src_groups.pairs::<Value, Table>() {
                    let (_, src_group) = gpair?;
                    let cloned = deep_clone_table(lua, &src_group)?;
                    remap_group_ids(
                        &cloned,
                        &mut next_gid,
                        &mut next_uid,
                        &mut gid_map,
                        &mut uid_map,
                        &mut units_copied,
                    )?;
                    dest_len += 1;
                    dest_groups.raw_set(dest_len, cloned.clone())?;
                    copied_groups.push(cloned);
                    groups_copied += 1;
                }
            }
        }
    }

    let dest_zones = ensure_dest_zones(lua, &dest)?;
    let mut dest_zone_len = dest_zones.raw_len();
    let mut zones_copied = 0usize;
    let mut copied_zones: Vec<Table> = Vec::new();
    if let Ok(src_triggers) = source.raw_get::<_, Table>("triggers") {
        if let Ok(src_zones) = src_triggers.raw_get::<_, Table>("zones") {
            for pair in src_zones.pairs::<Value, Table>() {
                let (_, src_zone) = pair?;
                let cloned = deep_clone_table(lua, &src_zone)?;
                if let Ok(old) = cloned.raw_get::<_, i64>("zoneId") {
                    let new_id = next_zid;
                    next_zid += 1;
                    zid_map.insert(old, new_id);
                    cloned.raw_set("zoneId", new_id)?;
                } else {
                    cloned.raw_set("zoneId", next_zid)?;
                    next_zid += 1;
                }
                dest_zone_len += 1;
                dest_zones.raw_set(dest_zone_len, cloned.clone())?;
                copied_zones.push(cloned);
                zones_copied += 1;
            }
        }
    }

    for group in &copied_groups {
        remap_nested_refs(
            Value::Table(group.clone()),
            &gid_map,
            &uid_map,
            &zid_map,
            &mut warnings,
        )?;
    }

    let mut clusters_moved = 0usize;
    let mut clusters_kept = 0usize;
    if source_theatre != dest_theatre {
        let rel = crate::relocate::relocate_copied(&dest, &copied_groups, &copied_zones)?;
        clusters_moved = rel.clusters_moved;
        clusters_kept = rel.clusters_kept;
        warnings.push(format!(
            "relocated {} off-map cluster(s) onto dest map; left {} cluster(s) already on-map (zone contents kept together)",
            rel.clusters_moved, rel.clusters_kept
        ));
    }

    bump_current_key(&dest, next_gid.max(next_uid).max(next_zid))?;

    Ok(MergeStats {
        groups: groups_copied,
        units: units_copied,
        zones: zones_copied,
        countries_created,
        source_theatre,
        dest_theatre,
        warnings,
        clusters_moved,
        clusters_kept,
    })
}

fn bump_current_key(dest: &Table, next_id: i64) -> Result<()> {
    let old = dest
        .raw_get::<_, Value>("currentKey")
        .ok()
        .and_then(|v| match v {
            Value::Integer(i) => Some(i),
            Value::Number(n) if n.fract() == 0.0 => Some(n as i64),
            _ => None,
        })
        .unwrap_or(0);
    dest.raw_set("currentKey", old.max(next_id))?;
    Ok(())
}

struct MissionIndex {
    group_names: HashSet<String>,
    unit_names: HashSet<String>,
    zone_names: HashSet<String>,
    max_gid: i64,
    max_uid: i64,
    max_zid: i64,
    groups: usize,
    zones: usize,
}

fn theatre_of(mission: &Table) -> Result<Option<String>> {
    for key in ["theatre", "theater"] {
        if let Ok(Value::String(s)) = mission.raw_get::<_, Value>(key) {
            return Ok(Some(s.to_str()?.to_string()));
        }
    }
    Ok(None)
}

fn index_mission(mission: &Table) -> Result<MissionIndex> {
    let mut idx = MissionIndex {
        group_names: HashSet::new(),
        unit_names: HashSet::new(),
        zone_names: HashSet::new(),
        max_gid: 0,
        max_uid: 0,
        max_zid: 0,
        groups: 0,
        zones: 0,
    };
    if let Ok(coa) = mission.raw_get::<_, Table>("coalition") {
        for side in SIDES {
            let Ok(side_tbl) = coa.raw_get::<_, Table>(side) else {
                continue;
            };
            let Ok(countries) = side_tbl.raw_get::<_, Table>("country") else {
                continue;
            };
            for pair in countries.pairs::<Value, Table>() {
                let (_, country) = pair?;
                for kind in GROUP_KINDS {
                    let Some(groups) = category_groups(&country, kind)? else {
                        continue;
                    };
                    for gpair in groups.pairs::<Value, Table>() {
                        let (_, group) = gpair?;
                        idx.groups += 1;
                        if let Ok(name) = group.raw_get::<_, String>("name") {
                            idx.group_names.insert(name);
                        }
                        if let Ok(gid) = group.raw_get::<_, i64>("groupId") {
                            idx.max_gid = idx.max_gid.max(gid);
                        }
                        if let Ok(units) = group.raw_get::<_, Table>("units") {
                            for upair in units.pairs::<Value, Table>() {
                                let (_, unit) = upair?;
                                if let Ok(name) = unit.raw_get::<_, String>("name") {
                                    idx.unit_names.insert(name);
                                }
                                if let Ok(uid) = unit.raw_get::<_, i64>("unitId") {
                                    idx.max_uid = idx.max_uid.max(uid);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    if let Ok(triggers) = mission.raw_get::<_, Table>("triggers") {
        if let Ok(zones) = triggers.raw_get::<_, Table>("zones") {
            for pair in zones.pairs::<Value, Table>() {
                let (_, zone) = pair?;
                idx.zones += 1;
                if let Ok(name) = zone.raw_get::<_, String>("name") {
                    idx.zone_names.insert(name);
                }
                if let Ok(zid) = zone.raw_get::<_, i64>("zoneId") {
                    idx.max_zid = idx.max_zid.max(zid);
                }
            }
        }
    }
    Ok(idx)
}

fn category_groups<'lua>(country: &Table<'lua>, kind: &str) -> Result<Option<Table<'lua>>> {
    match country.raw_get::<_, Value>(kind)? {
        Value::Nil => Ok(None),
        Value::Table(cat) => match cat.raw_get::<_, Value>("group")? {
            Value::Nil => Ok(None),
            Value::Table(g) => Ok(Some(g)),
            other => bail!("country.{kind}.group is not a table ({other:?})"),
        },
        other => bail!("country.{kind} is not a table ({other:?})"),
    }
}

fn ensure_category_groups<'lua>(
    lua: &'lua Lua,
    country: &Table<'lua>,
    kind: &str,
) -> Result<Table<'lua>> {
    if let Some(g) = category_groups(country, kind)? {
        return Ok(g);
    }
    let cat = lua.create_table()?;
    let groups = lua.create_table()?;
    cat.raw_set("group", groups.clone())?;
    country.raw_set(kind, cat)?;
    Ok(groups)
}

fn ensure_dest_country<'lua>(
    lua: &'lua Lua,
    dest: &Table<'lua>,
    side: &str,
    cid: i64,
    cname: &str,
    created: &mut usize,
) -> Result<Table<'lua>> {
    let dest_coa: Table = dest.raw_get("coalition").context("dest mission.coalition")?;
    let dest_side: Table = match dest_coa.raw_get::<_, Value>(side)? {
        Value::Table(t) => t,
        Value::Nil => {
            let t = lua.create_table()?;
            t.raw_set("name", side)?;
            t.raw_set("country", lua.create_table()?)?;
            dest_coa.raw_set(side, t.clone())?;
            t
        }
        other => bail!("dest coalition.{side} is not a table ({other:?})"),
    };
    let countries: Table = match dest_side.raw_get::<_, Value>("country")? {
        Value::Table(t) => t,
        Value::Nil => {
            let t = lua.create_table()?;
            dest_side.raw_set("country", t.clone())?;
            t
        }
        other => bail!("dest coalition.{side}.country is not a table ({other:?})"),
    };
    for pair in countries.clone().pairs::<Value, Table>() {
        let (_, c) = pair?;
        let id: i64 = c.raw_get("id")?;
        if id == cid {
            return Ok(c);
        }
    }
    let tbl = lua.create_table()?;
    tbl.raw_set("id", cid)?;
    tbl.raw_set("name", cname)?;
    let n = countries.raw_len() + 1;
    countries.raw_set(n, tbl.clone())?;
    *created += 1;
    append_coalition_country_id(dest, side, cid)?;
    Ok(tbl)
}

fn append_coalition_country_id(dest: &Table, side: &str, cid: i64) -> Result<()> {
    let Ok(coalitions) = dest.raw_get::<_, Table>("coalitions") else {
        return Ok(());
    };
    let list: Table = match coalitions.raw_get::<_, Value>(side)? {
        Value::Table(t) => t,
        Value::Nil => return Ok(()),
        other => bail!("dest coalitions.{side} is not a table ({other:?})"),
    };
    for pair in list.clone().pairs::<Value, Value>() {
        let (_, v) = pair?;
        if int_of(&v) == Some(cid) {
            return Ok(());
        }
    }
    let n = list.raw_len() + 1;
    list.raw_set(n, cid)?;
    Ok(())
}

fn ensure_dest_zones<'lua>(lua: &'lua Lua, dest: &Table<'lua>) -> Result<Table<'lua>> {
    let triggers: Table = match dest.raw_get::<_, Value>("triggers")? {
        Value::Table(t) => t,
        Value::Nil => {
            let t = lua.create_table()?;
            dest.raw_set("triggers", t.clone())?;
            t
        }
        other => bail!("dest mission.triggers is not a table ({other:?})"),
    };
    match triggers.raw_get::<_, Value>("zones")? {
        Value::Table(t) => Ok(t),
        Value::Nil => {
            let t = lua.create_table()?;
            triggers.raw_set("zones", t.clone())?;
            Ok(t)
        }
        other => bail!("dest mission.triggers.zones is not a table ({other:?})"),
    }
}

fn deep_clone_table<'lua>(lua: &'lua Lua, src: &Table<'lua>) -> Result<Table<'lua>> {
    match deep_clone_value(lua, Value::Table(src.clone()))? {
        Value::Table(t) => Ok(t),
        _ => bail!("deep clone did not produce a table"),
    }
}

fn deep_clone_value<'lua>(lua: &'lua Lua, v: Value<'lua>) -> Result<Value<'lua>> {
    match v {
        Value::Table(t) => {
            let new = lua.create_table()?;
            if let Some(mt) = t.get_metatable() {
                new.set_metatable(Some(mt));
            }
            for pair in t.pairs::<Value, Value>() {
                let (k, val) = pair?;
                new.set(deep_clone_value(lua, k)?, deep_clone_value(lua, val)?)?;
            }
            Ok(Value::Table(new))
        }
        Value::Boolean(b) => Ok(Value::Boolean(b)),
        Value::Integer(i) => Ok(Value::Integer(i)),
        Value::Nil => Ok(Value::Nil),
        Value::Number(n) => Ok(Value::Number(n)),
        Value::String(s) => Ok(Value::String(lua.create_string(s.as_bytes())?)),
        other => bail!("cannot clone Lua value {other:?}"),
    }
}

fn remap_group_ids(
    group: &Table,
    next_gid: &mut i64,
    next_uid: &mut i64,
    gid_map: &mut HashMap<i64, i64>,
    uid_map: &mut HashMap<i64, i64>,
    units_copied: &mut usize,
) -> Result<()> {
    if let Ok(old) = group.raw_get::<_, i64>("groupId") {
        let new_id = *next_gid;
        *next_gid += 1;
        gid_map.insert(old, new_id);
        group.raw_set("groupId", new_id)?;
    } else {
        group.raw_set("groupId", *next_gid)?;
        *next_gid += 1;
    }
    if let Ok(units) = group.raw_get::<_, Table>("units") {
        for pair in units.pairs::<Value, Table>() {
            let (_, unit) = pair?;
            if let Ok(old) = unit.raw_get::<_, i64>("unitId") {
                let new_id = *next_uid;
                *next_uid += 1;
                uid_map.insert(old, new_id);
                unit.raw_set("unitId", new_id)?;
            } else {
                unit.raw_set("unitId", *next_uid)?;
                *next_uid += 1;
            }
            *units_copied += 1;
        }
    }
    Ok(())
}

fn remap_nested_refs(
    value: Value,
    gid_map: &HashMap<i64, i64>,
    uid_map: &HashMap<i64, i64>,
    zid_map: &HashMap<i64, i64>,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let Value::Table(tbl) = value else {
        return Ok(());
    };
    let keys: Vec<(Value, Value)> = tbl
        .clone()
        .pairs::<Value, Value>()
        .collect::<mlua::Result<Vec<_>>>()?;
    for (k, v) in keys {
        if let Some(name) = key_as_str(&k) {
            if let Some(old) = int_of(&v) {
                let mapped = match name.as_str() {
                    "groupId" => gid_map.get(&old).copied(),
                    "unitId" | "linkUnit" | "missionUnitId" => uid_map.get(&old).copied(),
                    "zoneId" => zid_map.get(&old).copied(),
                    _ => None,
                };
                if let Some(new_id) = mapped {
                    if new_id != old {
                        tbl.raw_set(k.clone(), new_id)?;
                    }
                } else if matches!(name.as_str(), "linkUnit") {
                    warnings.push(format!(
                        "linkUnit={old} was not in the copied units; left unchanged"
                    ));
                }
            }
        }
        if let Value::Table(_) = v {
            remap_nested_refs(v, gid_map, uid_map, zid_map, warnings)?;
        }
    }
    Ok(())
}

fn key_as_str(k: &Value) -> Option<String> {
    match k {
        Value::String(s) => s.to_str().ok().map(|s| s.to_string()),
        _ => None,
    }
}

fn int_of(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => Some(*i),
        Value::Number(n) if n.fract() == 0.0 => Some(*n as i64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"
mission = {
  theatre = "Caucasus",
  coalition = {
    blue = {
      name = "blue",
      country = {
        [1] = {
          id = 2,
          name = "USA",
          plane = {
            group = {
              [1] = {
                groupId = 10,
                name = "TTSBlueF18",
                units = {
                  [1] = {
                    unitId = 20,
                    name = "F18-1",
                    type = "FA-18C_hornet",
                    x = 1,
                    y = 2,
                    linkUnit = 20,
                  }
                }
              }
            }
          }
        }
      }
    },
    red = { name = "red", country = {} },
    neutrals = { name = "neutrals", country = {} }
  },
  triggers = {
    zones = {
      [1] = {
        zoneId = 5,
        name = "SETTINGS-static-slots-creation",
        x = 0,
        y = 0,
        radius = 100,
        hidden = true,
        type = 0,
      }
    }
  }
}
"#;

    const DEST: &str = r#"
mission = {
  theatre = "Syria",
  date = { Year = 2011, Month = 6, Day = 1 },
  map = { centerX = 0, centerY = 0, zoom = 1 },
  coalition = {
    blue = { name = "blue", country = {} },
    red = { name = "red", country = {} },
    neutrals = { name = "neutrals", country = {} }
  },
  coalitions = { blue = {}, red = {}, neutrals = {} },
  triggers = { zones = {} }
}
"#;

    const DEST_COLLIDE: &str = r#"
mission = {
  theatre = "Syria",
  coalition = {
    blue = { name = "blue", country = {} },
    red = { name = "red", country = {} },
    neutrals = { name = "neutrals", country = {} }
  },
  triggers = {
    zones = {
      [1] = { zoneId = 1, name = "SETTINGS-static-slots-creation", x = 0, y = 0, radius = 1, type = 0 }
    }
  }
}
"#;

    #[test]
    fn copies_groups_and_zones_keeps_dest_theatre() {
        let lua = Lua::new();
        let source = load_mission_table(&lua, SOURCE, "source").unwrap();
        let dest = load_mission_table(&lua, DEST, "dest").unwrap();
        let stats = merge_missions(&lua, source, dest.clone()).unwrap();
        assert_eq!(stats.groups, 1);
        assert_eq!(stats.units, 1);
        assert_eq!(stats.zones, 1);
        assert_eq!(stats.countries_created, 1);
        assert_eq!(stats.source_theatre.as_deref(), Some("Caucasus"));
        assert_eq!(stats.dest_theatre.as_deref(), Some("Syria"));
        let theatre: String = dest.raw_get("theatre").unwrap();
        assert_eq!(theatre, "Syria");
        let date: Table = dest.raw_get("date").unwrap();
        let year: i64 = date.raw_get("Year").unwrap();
        assert_eq!(year, 2011);
        let coa: Table = dest.raw_get("coalition").unwrap();
        let blue: Table = coa.raw_get("blue").unwrap();
        let countries: Table = blue.raw_get("country").unwrap();
        let c0: Table = countries.raw_get(1).unwrap();
        let planes: Table = c0.raw_get("plane").unwrap();
        let groups: Table = planes.raw_get("group").unwrap();
        let g: Table = groups.raw_get(1).unwrap();
        let name: String = g.raw_get("name").unwrap();
        assert_eq!(name, "TTSBlueF18");
        let gid: i64 = g.raw_get("groupId").unwrap();
        assert_eq!(gid, 1);
        let units: Table = g.raw_get("units").unwrap();
        let u: Table = units.raw_get(1).unwrap();
        let uid: i64 = u.raw_get("unitId").unwrap();
        assert_eq!(uid, 1);
        let link: i64 = u.raw_get("linkUnit").unwrap();
        assert_eq!(link, 1);
        let zones: Table = dest
            .raw_get::<_, Table>("triggers")
            .unwrap()
            .raw_get("zones")
            .unwrap();
        let z: Table = zones.raw_get(1).unwrap();
        let zn: String = z.raw_get("name").unwrap();
        assert_eq!(zn, "SETTINGS-static-slots-creation");
        let zid: i64 = z.raw_get("zoneId").unwrap();
        assert_eq!(zid, 1);
        let coalitions: Table = dest.raw_get("coalitions").unwrap();
        let blue_ids: Table = coalitions.raw_get("blue").unwrap();
        let first: i64 = blue_ids.raw_get(1).unwrap();
        assert_eq!(first, 2);
    }

    #[test]
    fn rejects_zone_name_collision() {
        let lua = Lua::new();
        let source = load_mission_table(&lua, SOURCE, "source").unwrap();
        let dest = load_mission_table(&lua, DEST_COLLIDE, "dest").unwrap();
        let err = merge_missions(&lua, source, dest).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("SETTINGS-static-slots-creation"), "{msg}");
    }

    const SOURCE_OFFMAP: &str = r#"
mission = {
  theatre = "Caucasus",
  coalition = {
    blue = {
      name = "blue",
      country = {
        [1] = {
          id = 2,
          name = "USA",
          plane = {
            group = {
              [1] = {
                groupId = 1,
                name = "InsideZone",
                x = 800020,
                y = 800000,
                units = {
                  [1] = { unitId = 1, name = "U1", type = "FA-18C_hornet", x = 800020, y = 800000 }
                }
              },
              [2] = {
                groupId = 2,
                name = "FarPark",
                x = 800000,
                y = 900000,
                units = {
                  [1] = { unitId = 2, name = "U2", type = "FA-18C_hornet", x = 800000, y = 900000 }
                }
              }
            }
          }
        }
      }
    },
    red = { name = "red", country = {} },
    neutrals = { name = "neutrals", country = {} }
  },
  triggers = {
    zones = {
      [1] = {
        zoneId = 1,
        name = "ParkA",
        x = 800000,
        y = 800000,
        radius = 100,
        type = 0,
      }
    }
  }
}
"#;

    #[test]
    fn relocates_offmap_clusters_keeps_units_in_zone() {
        let lua = Lua::new();
        let source = load_mission_table(&lua, SOURCE_OFFMAP, "source").unwrap();
        let dest = load_mission_table(&lua, DEST, "dest").unwrap();
        let stats = merge_missions(&lua, source, dest.clone()).unwrap();
        assert!(stats.clusters_moved >= 2, "{stats:?}");
        let coa: Table = dest.raw_get("coalition").unwrap();
        let blue: Table = coa.raw_get("blue").unwrap();
        let countries: Table = blue.raw_get("country").unwrap();
        let c0: Table = countries.raw_get(1).unwrap();
        let planes: Table = c0.raw_get("plane").unwrap();
        let groups: Table = planes.raw_get("group").unwrap();
        let g1: Table = groups.raw_get(1).unwrap();
        let g2: Table = groups.raw_get(2).unwrap();
        let z: Table = dest
            .raw_get::<_, Table>("triggers")
            .unwrap()
            .raw_get::<_, Table>("zones")
            .unwrap()
            .raw_get(1)
            .unwrap();
        let zx: f64 = num_tbl(&z, "x");
        let zy: f64 = num_tbl(&z, "y");
        let u1: Table = g1
            .raw_get::<_, Table>("units")
            .unwrap()
            .raw_get(1)
            .unwrap();
        let ux: f64 = num_tbl(&u1, "x");
        let uy: f64 = num_tbl(&u1, "y");
        let dx = ux - zx;
        let dy = uy - zy;
        assert!((dx - 20.0).abs() < 0.01, "unit must stay 20m east of zone, got {dx},{dy}");
        assert!(dy.abs() < 0.01, "unit must stay on zone y, got {dy}");
        assert!(zx.abs() <= 300_000.0 && zy.abs() <= 300_000.0);
        let g2x: f64 = num_tbl(&g2, "x");
        let g2y: f64 = num_tbl(&g2, "y");
        let dist = ((g2x - ux).powi(2) + (g2y - uy).powi(2)).sqrt();
        assert!(
            dist > 1_000.0,
            "far cluster must not be stacked on the zone cluster, dist={dist}"
        );
        assert!(g2x.abs() <= 300_000.0 && g2y.abs() <= 300_000.0);
    }

    fn num_tbl(t: &Table, key: &str) -> f64 {
        match t.raw_get::<_, Value>(key).unwrap() {
            Value::Number(n) => n,
            Value::Integer(i) => i as f64,
            other => panic!("{key} not a number: {other:?}"),
        }
    }
}
