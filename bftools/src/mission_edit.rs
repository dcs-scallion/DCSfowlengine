//shell script -> pass in config (gets theatre/era from base miz) -> create both missions(clones) -> set server config
//start server

//on mission load end: crack open ~other~ mission, apply (all?) templates, resave

//save mission values in a struct

//crack open miz

//deserialize mission table

//edit mission table (crack open templates 1 at a time)

//repack miz
use crate::campaign_cfg;
use crate::payload_allowlist;
use crate::weapon_bridge;
use crate::MizCmd;
use anyhow::{bail, Context, Result};
use bfprotocols::{
    cfg::{Cfg, Deployable, DeployableKind},
    fowl_miz_export::{
        ObjectiveCoalitionStock, ObjectiveStockByCoalition, ObjectiveStockItem,
        ObjectiveStockLiquid, ObjectiveWarehouseDefaults,
    },
    miz_trigger::{
        fowl_trigger_zone_name_valid, FOWL_TRIGGER_ZONE_EXPECTED_PREFIXES_DISPLAY,
        SETTINGS_OBJECTIVE_ALIASES_ZONE,
    },
    tisp::{parse_tisp_zone_name, ship_pad_display_name, starts_with_tisp_prefix, TISP_PREFIX},
};
use compact_str::format_compact;
use dcso3::{
    azumith2d, change_heading,
    coalition::Side,
    controller::{MissionPoint, PointType},
    country::Country,
    env::miz::{
        self, Country as MizCountry, Group, GroupId, GroupKind, Miz, Property, Skill,
        TriggerZoneTyp,
    },
    normal2, path, pointing_towards2, value_to_json, DcsTableExt, LuaVec2, Quad2,
    Sequence, String, Vector2,
};
use log::{info, warn};
use mlua::{FromLua, IntoLua, Lua, Table, Value};
use nalgebra as na;
use serde::Deserialize;
use std::{
    collections::{HashMap, HashSet},
    f64::consts::PI,
    fmt::Display,
    fs::{self, File},
    io::{self, BufWriter},
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    ptr,
    str::FromStr,
    string::String as StdString,
};
use zip::{read::ZipArchive, write::FileOptions, ZipWriter};

static mut LUA: *const Lua = ptr::null();

pub trait DeepClone<'lua>: IntoLua<'lua> + FromLua<'lua> + Clone {
    fn deep_clone(&self, lua: &'lua Lua) -> Result<Self>;
}

impl<'lua, T> DeepClone<'lua> for T
where
    T: IntoLua<'lua> + FromLua<'lua> + Clone,
{
    fn deep_clone(&self, lua: &'lua Lua) -> Result<Self> {
        let v = match self.clone().into_lua(lua)? {
            Value::Boolean(b) => Value::Boolean(b),
            Value::Error(e) => Value::Error(e),
            Value::Function(f) => Value::Function(f),
            Value::Integer(i) => Value::Integer(i),
            Value::LightUserData(d) => Value::LightUserData(d),
            Value::Nil => Value::Nil,
            Value::Number(n) => Value::Number(n),
            Value::String(s) => Value::String(lua.create_string(s)?),
            Value::Table(t) => {
                let new = lua.create_table()?;
                new.set_metatable(t.get_metatable());
                for r in t.pairs::<Value, Value>() {
                    let (k, v) = r?;
                    new.set(k.deep_clone(lua)?, v.deep_clone(lua)?)?
                }
                Value::Table(new)
            }
            Value::Thread(t) => Value::Thread(t),
            Value::UserData(d) => Value::UserData(d),
        };
        Ok(T::from_lua(v, lua)?)
    }
}

struct TriggerZone {
    inner: miz::TriggerZone<'static>,
    objective_name: String,
    spawn_count: HashMap<String, isize>,
}

impl TriggerZone {
    pub fn new(zone: &Table<'static>) -> Result<Option<Self>> {
        let zone = zone.clone();
        let inner = miz::TriggerZone::from_lua(Value::Table(zone), unsafe { &*LUA })?;
        let name = inner.name()?;
        if name.starts_with('O') {
            if name.len() < 5 {
                bail!("trigger name {name} too short")
            }
            let t = TriggerZone {
                inner,
                objective_name: String::from(&name[4..]),
                spawn_count: HashMap::new(),
            };
            info!("added objective {}", &name[4..]);
            Ok(Some(t))
        } else {
            Ok(None)
        }
    }

    pub fn contains(&self, v: Vector2) -> Result<bool> {
        let center = self.inner.pos()?;
        match self.inner.typ()? {
            TriggerZoneTyp::Quad(q) => Ok(q.contains(LuaVec2(v))),
            TriggerZoneTyp::Circle { radius } => Ok(
                radius.powi(2) >= na::distance_squared(&v.into(), &center.into()),
            ),
        }
    }
}

/// Build-only overlay: client aircraft inside get internal fuel cleared (must match bflib `Zone::contains` idea).
struct TzfPlaneFuelZone {
    inner: miz::TriggerZone<'static>,
}

impl TzfPlaneFuelZone {
    fn try_from_trigger_table(zone: &Table<'static>) -> Result<Option<Self>> {
        let inner = miz::TriggerZone::from_lua(
            Value::Table(zone.clone()),
            unsafe { &*LUA },
        )?;
        let name = inner.name()?;
        if !name.starts_with("TZF") {
            return Ok(None);
        }
        info!("TZF plane fuel strip overlay: {}", name.as_str());
        Ok(Some(Self { inner }))
    }

    fn contains(&self, v: Vector2) -> Result<bool> {
        let center = self.inner.pos()?;
        match self.inner.typ()? {
            TriggerZoneTyp::Quad(q) => Ok(q.contains(LuaVec2(v))),
            TriggerZoneTyp::Circle { radius } => Ok(
                radius.powi(2) >= na::distance_squared(&v.into(), &center.into()),
            ),
        }
    }
}

fn strip_client_plane_internal_fuel_lua(unit: &Table) -> Result<()> {
    unit.raw_set("fuel", Value::Integer(0))?;
    if let Ok(pl) = unit.raw_get::<_, Table>("payload") {
        if matches!(pl.raw_get::<_, Value>("fuel"), Ok(v) if !v.is_nil()) {
            pl.raw_set("fuel", Value::Number(0.0))?;
        }
    }
    Ok(())
}

/// `dynSpawnTemplate` and carrier `TTSN*` deck slots: no ME internal fuel on unit/payload.
fn zero_dynamic_spawn_template_unit_fuel(unit: &Table) -> Result<()> {
    unit.raw_set("fuel", 0)?;
    if let Ok(pl) = unit.raw_get::<_, Table>("payload") {
        let _ = pl.raw_set("fuel", 0);
        let _ = pl.raw_set("fuel", Value::Number(0.0));
    }
    Ok(())
}

struct UnpackedMiz {
    root: PathBuf,
    files: HashMap<String, PathBuf>,
}

impl Drop for UnpackedMiz {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl UnpackedMiz {
    fn new(path: &Path) -> Result<Self> {
        let mut files: HashMap<String, PathBuf> = HashMap::new();
        let mut archive = ZipArchive::new(File::open(path).context("opening miz file")?)
            .context("unzipping miz")?;
        let mut root = PathBuf::from(path);
        root.set_extension("");
        info!("cracking open: {path:?}");
        for i in 0..archive.len() {
            let mut file = archive
                .by_index(i)
                .with_context(|| format_compact!("getting file {i}"))?;
            let dump_path = root.join(file.name());
            let dump_root = dump_path.parent().unwrap();
            fs::create_dir_all(dump_root)
                .with_context(|| format_compact!("creating {dump_root:?}"))?;
            let mut extracted_file = File::create(&dump_path)
                .with_context(|| format_compact!("creating {dump_path:?}"))?;
            io::copy(&mut file, &mut extracted_file)
                .with_context(|| format_compact!("copying {i} to {dump_path:?}"))?;
            files.insert(String::from(file.name()), dump_path);
        }
        Ok(Self { root, files })
    }

    fn pack(&self, destination_file: &Path) -> Result<()> {
        info!("repacking current miz to: {destination_file:?}");
        let file = File::create(&destination_file)
            .with_context(|| format_compact!("creating {:?}", destination_file))?;
        let zip_file = BufWriter::new(file);
        let mut zip_writer = ZipWriter::new(zip_file);
        for (zip_name, file_path) in &self.files {
            if file_path.is_dir() {
                continue;
            }
            let mut file = File::open(file_path)
                .with_context(|| format_compact!("opening file {:?}", file_path))?;
            zip_writer
                .start_file(zip_name.as_str(), FileOptions::default())
                .context("starting zip file")?;
            io::copy(&mut file, &mut zip_writer).context("writing to zip file")?;
            info!("added {file_path:?} to archive");
        }
        info!("{destination_file:?} good to go!");
        Ok(())
    }
}

struct LuaSerVal {
    value: Value<'static>,
    level: usize,
}

impl LuaSerVal {
    fn indented(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for _ in 0..self.level {
            write!(f, " ")?;
        }
        Ok(())
    }
}

impl Display for LuaSerVal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.value {
            Value::Boolean(b) => write!(f, "{b}"),
            Value::Integer(i) => write!(f, "{i}"),
            Value::Nil => write!(f, "nil"),
            Value::Number(n) => write!(f, "{n}"),
            Value::String(s) => write!(f, "\"{}\"", s.to_string_lossy()),
            Value::Table(tbl) => {
                macro_rules! write_elt {
                    ($k:expr, $v:expr) => {
                        let k = LuaSerVal { value: $k, level: self.level + 4 };
                        let v = LuaSerVal { value: $v, level: self.level + 4 };
                        k.indented(f).unwrap();
                        if v.value.is_table() {
                            write!(f, "[{k}] = {v}, -- end of [{k}]\n").unwrap();
                        } else {
                            write!(f, "[{k}] = {v},\n").unwrap();
                        }
                    };
                }
                let mut seq_max: Option<i64> = None;
                write!(f, "\n")?;
                self.indented(f)?;
                write!(f, "{{\n")?;
                if tbl.contains_key(1).unwrap() {
                    for (i, v) in tbl.clone().sequence_values().enumerate() {
                        let i = (i + 1) as i64;
                        let v = v.unwrap();
                        seq_max = Some(i);
                        write_elt!(Value::Integer(i), v);
                    }
                }
                tbl.for_each(|k: Value, v: Value| {
                    if let Some(max) = seq_max {
                        if let Some(ki) = k.as_integer() {
                            // Only skip 1..=max (sequence_values). Preserve [0], etc.
                            if (1..=max).contains(&ki) {
                                return Ok(());
                            }
                        }
                    }
                    write_elt!(k, v);
                    Ok(())
                })
                .unwrap();
                self.indented(f)?;
                write!(f, "}}")
            }
            Value::Error(_)
            | Value::Function(_)
            | Value::LightUserData(_)
            | Value::Thread(_)
            | Value::UserData(_) => {
                panic!("value type {:?} can't be serialized", self.value)
            }
        }
    }
}

/// DCS `getValueDictByKey(mission.sortie)` uses `l10n/DEFAULT/dictionary[ key ]` as the Saved Games stem
/// (`*_CFG`, state file, `*_fowl_export.json`). Set that string to the `--output` .miz stem.
fn sync_l10n_dictionary_sortie_stem_to_output_miz(base: &LoadedMiz, output: &Path) -> Result<()> {
    let stem = output
        .file_stem()
        .and_then(|s| s.to_str())
        .with_context(|| format!("--output has no UTF-8 file stem: {:?}", output))?;
    let sortie_key = base
        .mission
        .sortie()
        .context("read mission sortie (l10n dictionary key ref)")?;
    let dict_relpath = base
        .miz
        .files
        .keys()
        .find(|k| k.replace('\\', "/").ends_with("l10n/DEFAULT/dictionary"))
        .cloned()
        .with_context(|| {
            format!(
                "base miz has no l10n/DEFAULT/dictionary (first keys: {:?})",
                base.miz.files.keys().take(8).collect::<Vec<_>>()
            )
        })?;
    let dict_path = base
        .miz
        .files
        .get(&dict_relpath)
        .with_context(|| format!("missing path for {dict_relpath}"))?;
    let content = fs::read_to_string(dict_path)
        .with_context(|| format!("read l10n dictionary {:?}", dict_path))?;
    let needle = format!("[\"{sortie_key}\"] = \"");
    let mut new_content = std::string::String::with_capacity(content.len() + 32);
    let mut replaced = false;
    for line in content.lines() {
        if !replaced && line.contains(needle.as_str()) {
            if let Some(i) = line.find(&needle) {
                let rest = &line[i + needle.len()..];
                if let Some(end) = rest.find('"') {
                    new_content.push_str(&line[..i + needle.len()]);
                    new_content.push_str(stem);
                    new_content.push_str(&line[i + needle.len() + end..]);
                    replaced = true;
                    new_content.push('\n');
                    continue;
                }
            }
        }
        new_content.push_str(line);
        new_content.push('\n');
    }
    if !replaced {
        bail!(
            "l10n dictionary: no line with prefix {:?} (mission.sortie = {})",
            needle,
            sortie_key
        );
    }
    if !content.ends_with('\n') {
        new_content.pop();
    }
    fs::write(dict_path, new_content)
        .with_context(|| format!("write l10n dictionary {:?}", dict_path))?;
    info!(
        "l10n dictionary: {:?} [\"{}\"] = {:?} (DCS getValueDictByKey; matches --output .miz)",
        dict_relpath, sortie_key, stem
    );
    Ok(())
}

fn serialize_to_lua(key: &str, value: Value<'static>) -> Result<std::string::String> {
    let res = std::panic::catch_unwind(AssertUnwindSafe(move || {
        use std::fmt::Write;
        let mut s = std::string::String::with_capacity(128 * 1024 * 1024);
        write!(s, "{key} = {}", LuaSerVal { value, level: 0 })?;
        Ok::<_, anyhow::Error>(s)
    }));
    match res {
        Ok(s) => Ok(s?),
        Err(e) => {
            if let Some(e) = e.downcast_ref::<anyhow::Error>() {
                bail!("{e}");
            }
            if let Some(e) = e.downcast_ref::<&str>() {
                bail!("{e}")
            }
            if let Some(e) = e.downcast_ref::<std::string::String>() {
                bail!("{e}")
            }
            if let Some(e) = e.downcast_ref::<mlua::Error>() {
                bail!("{e}")
            }
            bail!("serialization failed")
        }
    }
}

struct LoadedMiz {
    miz: UnpackedMiz,
    mission: Miz<'static>,
    #[allow(dead_code)]
    options: Table<'static>,
    #[allow(dead_code)]
    warehouses: Table<'static>,
}

impl LoadedMiz {
    fn new(lua: &'static Lua, path: &Path) -> Result<Self> {
        let miz = UnpackedMiz::new(path)
            .with_context(|| format_compact!("unpacking {path:?}"))?;
        let mut mission = lua.create_table()?;
        let mut options = lua.create_table()?;
        let mut warehouses = lua.create_table()?;
        for (file_name, file) in &miz.files {
            if **file_name != "mission"
                && **file_name != "warehouses"
                && **file_name != "options"
            {
                continue;
            }
            info!("processing {file_name}");
            let file_content = fs::read_to_string(file)
                .with_context(|| format_compact!("error reading file {file:?}"))?;
            lua.load(&file_content)
                .exec()
                .with_context(|| format_compact!("loading {file_name} into lua"))?;
            if **file_name == "mission" {
                mission =
                    lua.globals().raw_get("mission").context("extracting mission")?;
            }
            if **file_name == "warehouses" {
                warehouses = lua
                    .globals()
                    .raw_get("warehouses")
                    .context("extracting warehouses")?;
            }
            if **file_name == "options" {
                options =
                    lua.globals().raw_get("options").context("extracting options")?;
            }
        }
        if mission.is_empty() {
            bail!("{path:?} did not contain a mission file")
        }
        if options.is_empty() {
            bail!("{path:?} did not contain an options file")
        }
        if warehouses.is_empty() {
            bail!("{path:?} did not contain a warehouses file")
        }
        Ok(Self {
            miz,
            mission: Miz::from_lua(Value::Table(mission), lua)?,
            options,
            warehouses,
        })
    }
}

fn vehicle(
    country: &Table<'static>,
    name: &str,
) -> Result<Box<dyn Iterator<Item = Result<Table<'static>>>>> {
    if !country.contains_key(name)? {
        Ok(Box::new([].into_iter()))
    } else {
        Ok(Box::new(
            country
                .raw_get::<_, Table>(name)?
                .raw_get::<_, Table>("group")?
                .pairs::<Value, Table>()
                .map(|r| Ok(r?.1)),
        ))
    }
}

fn increment_key(map: &mut HashMap<String, isize>, key: &str) -> isize {
    let n = map.entry(String::from(key)).or_default();
    *n += 1;
    *n
}

/// Property keys for pulling `TTS*` templates into `TS*` / `TTS*` zones (`include` is canonical;
/// `include_dyn_slots` is accepted so base/warehouse can use one name everywhere).
const INCLUDE_STATIC_SLOT_KEYS: &[&str] = &["include", "include_dyn_slots"];
/// Property keys for `TTD*` / `TTDN*` dynamic template references (`include_dyn_slots` is canonical;
/// `include` kept for older missions).
const INCLUDE_DYNAMIC_SLOT_KEYS: &[&str] = &["include_dyn_slots", "include"];

/// DEP* dynamic FARP aircraft allowlist (`TTDdynFARP` zone); not part of general land TTD policy merge.
const TTD_DYN_FARP_POLICY_ZONE: &str = "TTDdynFARP";
/// Naval warehouse ME `dynamicSpawn` after stock/linkDynTempl (keys `TTDN` + hull name, e.g. `TTDNRKuznecow`).
const SETTINGS_DYNAMIC_SPAWN_TTDN_ZONE: &str = "SETTINGS-dynamic-spawn-TTDN";
/// Ground hub ME `dynamicSpawn` after `patch_warehouse_dynamic_spawn_links` (O* prefix keys + `DEP*FARPPAD` for DEP template FARPs).
const SETTINGS_DYNAMIC_SPAWN_GROUND_ZONE: &str = "SETTINGS-dynamic-spawn";
const SETTINGS_DYNAMIC_SPAWN_DEP_FARP_KEY: &str = "DEP*FARPPAD";

fn parse_trigger_slot_quantity(value: &str) -> Result<usize> {
    let t = value.trim();
    if t.eq_ignore_ascii_case("x") {
        return Ok(1);
    }
    t.parse::<usize>().with_context(|| {
        format!("expected non-negative quantity or X for zone slot entry, got {t:?}")
    })
}

/// Emission: `(side,type)` must appear on at least one policy list when that axis is active.
fn ttd_policy_allows_emitted_type(
    land_allow: Option<&HashSet<(Side, String)>>,
    naval_allow: Option<&HashSet<(Side, String)>>,
    side: Side,
    unit_type: &String,
) -> bool {
    match (land_allow, naval_allow) {
        (None, None) => true,
        (Some(l), None) => l.contains(&(side, unit_type.clone())),
        (None, Some(n)) => n.contains(&(side, unit_type.clone())),
        (Some(l), Some(n)) => {
            l.contains(&(side, unit_type.clone())) || n.contains(&(side, unit_type.clone()))
        }
    }
}

/// Coalition DS row: ship rows use `TTDN*` membership; others use `TTD*`. Open if that axis has no policy zones.
fn ttd_policy_allows_coalition_warehouse_row(
    land_allow: Option<&HashSet<(Side, String)>>,
    naval_allow: Option<&HashSet<(Side, String)>>,
    naval_warehouse_row: bool,
    side: Side,
    unit_type: &String,
) -> bool {
    match (land_allow, naval_allow) {
        (None, None) => true,
        (Some(l), None) => {
            if naval_warehouse_row {
                true
            } else {
                l.contains(&(side, unit_type.clone()))
            }
        }
        (None, Some(n)) => {
            if naval_warehouse_row {
                n.contains(&(side, unit_type.clone()))
            } else {
                true
            }
        }
        (Some(l), Some(n)) => {
            if naval_warehouse_row {
                n.contains(&(side, unit_type.clone()))
            } else {
                l.contains(&(side, unit_type.clone()))
            }
        }
    }
}

struct SlotSpec {
    slots: HashMap<Side, HashMap<String, usize>>,
    naval_units: HashSet<(Side, String)>,
    margin: Option<f64>,
    spacing: Option<f64>,
}

impl SlotSpec {
    fn new(
        templates: &HashMap<String, SlotSpec>,
        props: Sequence<Property>,
        mark_naval: bool,
        include_keys: &[&str],
    ) -> Result<Self> {
        let mut slots: HashMap<Side, HashMap<String, usize>> = HashMap::default();
        let mut naval_units: HashSet<(Side, String)> = HashSet::default();
        let mut side = None;
        let mut margin = None;
        let mut spacing = None;
        let mut seen_includes: HashSet<String> = HashSet::default();
        for prop in props {
            let prop = prop?;
            if include_keys.iter().any(|&k| prop.key.as_ref() == k) {
                if !seen_includes.insert(prop.value.clone()) {
                    continue;
                }
                match templates.get(&prop.value) {
                    None => {
                        warn!(
                            "skipping property {:?} -> '{}' (template not loaded — name not present in TTS* / TTD* mission zones)",
                            prop.key, prop.value
                        );
                    }
                    Some(tmpl) => {
                        if let Some(v) = tmpl.margin {
                            margin = Some(v);
                        }
                        if let Some(v) = tmpl.spacing {
                            spacing = Some(v);
                        }
                        for (side, tmpl) in &tmpl.slots {
                            let slots = slots.entry(*side).or_default();
                            for (ac, n) in tmpl {
                                *slots.entry(ac.clone()).or_default() += *n;
                            }
                        }
                        naval_units.extend(tmpl.naval_units.iter().cloned());
                    }
                }
            } else if *prop.key == "margin" {
                margin = Some(prop.value.parse()?);
            } else if *prop.key == "spacing" {
                spacing = Some(prop.value.parse()?);
            } else {
                match Side::from_str(&prop.key) {
                    Ok(s) => side = Some(s),
                    Err(_) => match side {
                        None => {
                            bail!("expected Blue or Red before airframe declarations")
                        }
                        Some(side) => {
                            let unit_type = prop.key.clone();
                            *slots
                                .entry(side)
                                .or_default()
                                .entry(unit_type.clone())
                                .or_default() += parse_trigger_slot_quantity(prop.value.as_ref())?;
                            if mark_naval {
                                naval_units.insert((side, unit_type));
                            }
                        }
                    },
                }
            }
        }
        Ok(Self { slots, naval_units, margin, spacing })
    }
}

trait PosGenerator {
    fn next(&mut self) -> Result<Vector2>;
    fn azumith(&self) -> f64;
}

#[derive(Debug)]
struct SlotRadial {
    center: Vector2,
    slots: Vec<(f64, Vec<f64>)>,
    i: usize,
    j: usize,
    last_az: f64,
    name: String,
}

impl SlotRadial {
    fn new(
        name: String,
        radius: f64,
        center: Vector2,
        margin: Option<f64>,
        spacing: Option<f64>,
    ) -> Result<Self> {
        let margin = margin.unwrap_or(5.);
        let spacing = spacing.unwrap_or(25.);
        let mut radius = radius - margin;
        let mut step = (spacing / radius).asin();
        let mut slots: Vec<(f64, Vec<f64>)> = vec![(radius, vec![])];
        let mut i = 0;
        while radius >= spacing / 2. {
            if slots.len() <= i {
                radius -= spacing;
                step = (f64::min(1., f64::max(-1., spacing / radius))).asin();
                slots.push((radius, vec![]));
            } else {
                match slots[i].1.last().map(|az| *az) {
                    None => slots[i].1.push(0.),
                    Some(az) => {
                        let next2 = change_heading(az, step * 2.);
                        if next2 < az {
                            i += 1;
                        } else {
                            slots[i].1.push(change_heading(az, step));
                        }
                    }
                }
            }
        }
        Ok(Self { center, slots, i: 0, j: 0, last_az: PI, name })
    }
}

impl PosGenerator for SlotRadial {
    fn next(&mut self) -> Result<Vector2> {
        let (radius, az) = loop {
            match self.slots.get(self.i) {
                None => bail!("radial zone {} is full", self.name),
                Some((radius, azumiths)) => match azumiths.get(self.j) {
                    Some(az) => {
                        self.j += 1;
                        break (*radius, *az);
                    }
                    None => {
                        self.i += 1;
                        self.j = 0;
                    }
                },
            }
        };
        self.last_az = change_heading(az, PI);
        Ok(self.center + pointing_towards2(az) * radius)
    }

    fn azumith(&self) -> f64 {
        self.last_az
    }
}

struct SlotGrid {
    name: String,
    quad: Quad2,
    cr: Vector2,
    row_az: f64,
    row: Vector2,
    column: Vector2,
    current: Vector2,
    margin: f64,
    spacing: f64,
    max_edge: f64,
}

impl SlotGrid {
    fn new(
        name: String,
        quad: Quad2,
        margin: Option<f64>,
        spacing: Option<f64>,
    ) -> Result<Self> {
        let margin = margin.unwrap_or(5.);
        let spacing = spacing.unwrap_or(25.);
        let (p0, p1, _) = quad.longest_edge();
        let max_edge = na::distance(&p0.into(), &p1.into());
        let column = (p0 - p1).normalize();
        let row = normal2(column).normalize();
        // unit vectors pointing along the row and column axis of the grid that starts
        // at p0 and ends at p1
        let (row, column) = if quad.contains(LuaVec2(p0 + column + row)) {
            (row, column)
        } else if quad.contains(LuaVec2(p0 + column - row)) {
            (-row, column)
        } else if quad.contains(LuaVec2(p0 - column + row)) {
            (row, -column)
        } else if quad.contains(LuaVec2(p0 - column - row)) {
            (-row, -column)
        } else {
            bail!("the area {name} is too thin")
        };
        let p0 = p0 + row * margin + column * margin;
        Ok(Self {
            name,
            quad,
            cr: p0,
            row_az: azumith2d(row),
            row,
            column,
            current: p0,
            margin,
            spacing,
            max_edge,
        })
    }
}

impl PosGenerator for SlotGrid {
    fn next(&mut self) -> Result<Vector2> {
        if !self.quad.contains(LuaVec2(
            self.current + self.column * self.margin + self.row * self.margin,
        )) {
            bail!("zone {} is full", self.name)
        }
        let res = self.current;
        let p = self.current + self.column * self.spacing;
        if self.quad.contains(LuaVec2(p + self.column * self.margin)) {
            self.current = p;
            Ok(res)
        } else {
            let mut cr = self.cr + self.row * self.spacing;
            let mut moved = 0.;
            while !self.quad.contains(LuaVec2(cr - self.column * self.margin)) {
                cr = cr + self.column * 1.;
                moved += 1.;
                if moved > self.max_edge {
                    bail!("zone {} is full", self.name)
                }
            }
            self.cr = cr;
            self.current = cr;
            Ok(res)
        }
    }

    fn azumith(&self) -> f64 {
        self.row_az
    }
}

/// All carrier static slots share one ME parking anchor (DCS fans icons around the hull).
struct ConstantPosGenerator {
    pos: Vector2,
    heading: f64,
}

impl PosGenerator for ConstantPosGenerator {
    fn next(&mut self) -> Result<Vector2> {
        Ok(self.pos)
    }

    fn azumith(&self) -> f64 {
        self.heading
    }
}

fn ship_unit_deck_pose(base: &LoadedMiz, unit_id: i64) -> Result<(Vector2, f64)> {
    for side in [Side::Red, Side::Blue] {
        let coa = base.mission.coalition(side)?;
        for country in coa.countries()? {
            let country = country?;
            for group in vehicle(&country, "ship")? {
                let group = group?;
                for unit in group.raw_get::<_, Table>("units")?.pairs::<Value, Table>() {
                    let unit = unit?.1;
                    let id: i64 = unit.raw_get("unitId")?;
                    if id != unit_id {
                        continue;
                    }
                    let x: f64 = unit.raw_get("x")?;
                    let y: f64 = unit.raw_get("y")?;
                    let heading: f64 = unit.raw_get("heading").unwrap_or(0.);
                    return Ok((Vector2::new(x, y), heading));
                }
            }
        }
    }
    bail!("ship unitId {unit_id} not found in mission")
}

/// ME parking-picker origin for static carrier client slots (one world point per ship, not deck grid).
fn carrier_static_slot_reference_pose(
    base: &LoadedMiz,
    ship_unit_id: i64,
) -> Result<(Vector2, f64)> {
    let (ship_pos, ship_h) = ship_unit_deck_pose(base, ship_unit_id)?;
    // Ship-local offset from ME snap on Kuznetsov (reference hull heading below).
    const REF_HEADING: f64 = 2.2689280275926;
    const WORLD_DX: f64 = -58.119_326_064_5;
    const WORLD_DY: f64 = 50.704_130_380_32;
    let c0 = REF_HEADING.cos();
    let s0 = REF_HEADING.sin();
    let local_x = WORLD_DX * c0 - WORLD_DY * s0;
    let local_y = WORLD_DX * s0 + WORLD_DY * c0;
    let dc = ship_h.cos();
    let ds = ship_h.sin();
    let pos = Vector2::new(
        ship_pos.x + local_x * dc + local_y * ds,
        ship_pos.y - local_x * ds + local_y * dc,
    );
    Ok((pos, ship_h))
}

/// Warehouse carrier hulls stay off-map until Fowl spawns them (`move_farp_pad`).
fn set_warehouse_ships_late_activation(
    base: &mut LoadedMiz,
    ship_wh: &HashMap<i64, (Side, String)>,
) -> Result<()> {
    let names: HashSet<&String> = ship_wh.values().map(|(_, g)| g).collect();
    if names.is_empty() {
        return Ok(());
    }
    for side in [Side::Red, Side::Blue] {
        let coa = base.mission.coalition(side)?;
        for country in coa.countries()? {
            let country = country?;
            for group in vehicle(&country, "ship")? {
                let group = group?;
                let gname: String = group.raw_get("name")?;
                if names.contains(&gname) {
                    group.raw_set("lateActivation", true)?;
                }
            }
        }
    }
    Ok(())
}

/// ME pad group (`RKuznecow`, …) → Fowl objective display name from CFG deployables.
fn build_carrier_pad_objective_names(cfg: &Cfg) -> HashMap<String, String> {
    let mut out = HashMap::default();
    for side in [Side::Blue, Side::Red] {
        let Some(deployables) = cfg.deployables.get(&side) else {
            continue;
        };
        for dep in deployables {
            let DeployableKind::Objective(obj) = &dep.kind else {
                continue;
            };
            let Some(objective_name) = dep.path.last() else {
                continue;
            };
            for pad in &obj.pad_templates {
                out.insert(pad.clone(), objective_name.clone());
            }
        }
    }
    out
}

fn route_point_i64(p: &Table, key: &str) -> Option<i64> {
    match p.raw_get::<_, Value>(key).ok()? {
        Value::Integer(i) => Some(i),
        Value::Number(n) => Some(n as i64),
        _ => None,
    }
}

/// Ship warehouse `unitId` from a carrier deck client slot (`linkOffset` legacy or `TakeOffParking` + `linkUnit`/`helipadId`).
fn carrier_ship_unit_from_client_group(group: &Table) -> Result<Option<i64>> {
    let route: Table = group.raw_get("route")?;
    let points: Table = route.raw_get("points")?;
    if points.raw_len() < 1 {
        return Ok(None);
    }
    let p: Table = points.raw_get(1)?;
    if group.raw_get::<_, bool>("linkOffset").unwrap_or(false) {
        return Ok(route_point_i64(&p, "linkUnit"));
    }
    let typ: String = p.raw_get("type").unwrap_or_default();
    if typ.as_str() == "TakeOffParking" {
        return Ok(route_point_i64(&p, "linkUnit").or_else(|| route_point_i64(&p, "helipadId")));
    }
    Ok(None)
}

fn carrier_pad_from_client_group(base: &LoadedMiz, group: &Table) -> Result<Option<String>> {
    let Some(link_id) = carrier_ship_unit_from_client_group(group)? else {
        return Ok(None);
    };
    let ship_wh = collect_ship_warehouse_group_map(base)?;
    Ok(ship_wh.get(&link_id).map(|(_, g)| g.clone()))
}

fn client_slot_objective_name(
    unit_pos: Vector2,
    group: &Table,
    objectives: &[TriggerZone],
    base: &LoadedMiz,
    carrier_pads: &HashMap<String, String>,
) -> Result<Option<String>> {
    for obj in objectives {
        if obj.contains(unit_pos)? {
            return Ok(Some(obj.objective_name.clone()));
        }
    }
    if let Some(pad) = carrier_pad_from_client_group(base, group)? {
        if let Some(name) = carrier_pads.get(&pad) {
            return Ok(Some(name.clone()));
        }
    }
    Ok(None)
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum SlotType {
    Plane,
    Helicopter,
}

/// Emitted dynamic-spawn template groups sort late in ME lists (`zz`…).
const DYNAMIC_TEMPLATE_GROUP_PREFIX: &str = "zzDT-";
/// Older missions / weapon.miz may still use `DT-`; strip and match-load only.
const LEGACY_DYNAMIC_TEMPLATE_PREFIX: &str = "DT-";

#[inline]
fn is_dynamic_template_group_name(name: &str) -> bool {
    name.starts_with(DYNAMIC_TEMPLATE_GROUP_PREFIX)
        || name.starts_with(LEGACY_DYNAMIC_TEMPLATE_PREFIX)
}

const DYNAMIC_TEMPLATE_SLOT_PASSWORD: &str =
    "7ciAoh5eaeP:p3JJGYR5-nG5YZe0YhTgY1ArpqUsDShe-T0-li3VaL0";

fn apply_dynamic_template_group_visibility(
    group: &Group<'_>,
    slot_kind: SlotType,
) -> Result<()> {
    match slot_kind {
        SlotType::Plane | SlotType::Helicopter => {
            group.raw_set("hidden", true)?;
            group.raw_set("hiddenOnPlanner", true)?;
            group.raw_set("hiddenOnMFD", true)?;
        }
    }
    Ok(())
}

fn push_client_air_group_to_cjtf(
    lua: &'static Lua,
    mission: &Miz,
    side: Side,
    slot_kind: SlotType,
    tmpl: Group<'static>,
) -> Result<()> {
    let coa = mission.coalition(side)?;
    let cname = match side {
        Side::Blue => Country::CJTF_BLUE,
        Side::Red => Country::CJTF_RED,
        Side::Neutral => unreachable!(),
    };
    let country = match coa.country(cname)? {
        Some(c) => c,
        None => {
            let tbl = lua.create_table()?;
            tbl.raw_set("id", cname)?;
            tbl.raw_set(
                "name",
                match cname {
                    Country::CJTF_BLUE => "CJTF Blue",
                    Country::CJTF_RED => "CJTF Red",
                    _ => unreachable!(),
                },
            )?;
            coa.raw_get::<_, Table>("country")?.push(tbl)?;
            coa.country(cname)?.unwrap()
        }
    };
    let seq = match slot_kind {
        SlotType::Plane => {
            let plane = country.planes()?;
            if plane.len() > 0 {
                plane
            } else {
                let p = lua.create_table()?;
                p.raw_set("group", lua.create_table()?)?;
                country.raw_set("plane", p)?;
                country.planes()?
            }
        }
        SlotType::Helicopter => {
            let heli = country.helicopters()?;
            if heli.len() > 0 {
                heli
            } else {
                let h = lua.create_table()?;
                h.raw_set("group", lua.create_table()?)?;
                country.raw_set("helicopter", h)?;
                country.helicopters()?
            }
        }
    };
    seq.push(tmpl)?;
    Ok(())
}

/// Re-stamp datalink ownship `missionUnitId` after a new `unitId` is assigned (weapon clone keeps layout).
fn patch_datalink_mission_unit_ids(unit: &Table) -> Result<()> {
    if let Ok(Some(dl)) = unit.raw_get::<_, Option<Table>>("datalinks") {
        let uid = unit.raw_get::<_, i64>("unitId")?;
        let mut ok = false;
        if let Ok(ownship) =
            dl.raw_get_path::<Table>(&path!["Link16", "network", "teamMembers", 1])
        {
            ownship.raw_set("missionUnitId", uid)?;
            ok = true;
        }
        if let Ok(presets) =
            dl.raw_get_path::<Sequence<Table>>(&path!["IDM", "network", "presets"])
        {
            for preset in presets {
                let preset = preset?;
                if let Ok(ownship) = preset.raw_get_path::<Table>(&path!["members", 1]) {
                    ownship.raw_set("missionUnitId", uid)?;
                    ok = true;
                }
            }
        }
        if let Ok(ownship) =
            dl.raw_get_path::<Table>(&path!["SADL", "network", "teamMembers", 1])
        {
            ownship.raw_set("missionUnitId", uid)?;
            ok = true;
        }
        if !ok {
            bail!("unknown data link pattern, can't find ownship")
        }
    }
    Ok(())
}

struct VehicleTemplates {
    plane_slots: HashMap<Side, HashMap<String, Group<'static>>>,
    helicopter_slots: HashMap<Side, HashMap<String, Group<'static>>>,
    /// Optional `zzDT-*` / legacy `DT-*` / `dynSpawnTemplate` groups in weapon.miz (per side, kind, type).
    dt_weapon_source: HashMap<(Side, SlotType, String), Group<'static>>,
    payload: HashMap<Side, HashMap<String, Table<'static>>>,
    /// Every payload table seen in weapon templates (no per-unit-type overwrite).
    payload_all: HashMap<Side, Vec<Table<'static>>>,
    /// Payload variants per unit type (all occurrences in weapon templates).
    payload_variants: HashMap<Side, HashMap<String, Vec<Table<'static>>>>,
    prop_aircraft: HashMap<Side, HashMap<String, Table<'static>>>,
    radio: HashMap<Side, HashMap<String, Table<'static>>>,
    frequency: HashMap<Side, HashMap<String, Value<'static>>>,
}

impl VehicleTemplates {
    fn parse_setting_bool(raw: &str) -> Option<bool> {
        let s = raw.trim().to_ascii_lowercase();
        match s.as_str() {
            "1" | "true" | "yes" | "on" | "enable" | "enabled" | "active" => Some(true),
            "0" | "false" | "no" | "off" | "disable" | "disabled" | "inactive" => {
                Some(false)
            }
            _ => s.parse::<i64>().ok().map(|n| n != 0),
        }
    }

    fn load_zone_creation_settings(
        base: &LoadedMiz,
        settings_zone_name: &str,
    ) -> Result<HashMap<String, bool>> {
        let mut out = HashMap::new();
        for zone in base.mission.triggers()? {
            let zone = zone?;
            if zone.name()?.as_ref() != settings_zone_name {
                continue;
            }
            for prop in zone.properties()? {
                let prop = prop?;
                if let Some(v) = Self::parse_setting_bool(prop.value.as_ref()) {
                    out.insert(prop.key.clone(), v);
                } else {
                    warn!(
                        "ignoring invalid setting value '{}' in {} for key '{}'",
                        prop.value, settings_zone_name, prop.key
                    );
                }
            }
            break;
        }
        Ok(out)
    }

    fn zone_enabled_by_settings(
        settings: &HashMap<String, bool>,
        full_zone_name: &str,
    ) -> bool {
        // STRICT mode: only zones explicitly listed in SETTINGS-* are considered.
        // Missing/empty settings => nothing enabled.
        settings.get(&String::from(full_zone_name)).copied().unwrap_or(false)
    }

    fn normalize_group_route_to_turning(group: &Group<'static>) -> Result<()> {
        let route = group.route()?;
        route.set_points(
            route
                .points()?
                .into_iter()
                .map(|p| {
                    let mut p = p?;
                    p.typ = PointType::TurningPoint;
                    Ok(p)
                })
                .collect::<Result<Vec<MissionPoint>>>()?,
        )?;
        Ok(())
    }

    fn lua_empty_combo_task(lua: &Lua) -> Result<Table<'_>> {
        let task = lua.create_table()?;
        task.raw_set("id", "ComboTask")?;
        let params = lua.create_table()?;
        params.raw_set("tasks", lua.create_table()?)?;
        task.raw_set("params", params)?;
        Ok(task)
    }

    /// ME-style carrier static deck slot (no `linkOffset`; DCS docks via `helipadId` + `parking`).
    fn patch_static_carrier_deck_slot_group(
        lua: &Lua,
        grp: &Table,
        ship_unit_id: i64,
        pos: Vector2,
        radio_set: bool,
    ) -> Result<()> {
        grp.raw_set("hiddenOnMFD", true)?;
        grp.raw_set("tasks", lua.create_table()?)?;
        grp.raw_set("task", "Nothing")?;
        grp.raw_set("taskSelected", true)?;
        grp.raw_set("uncontrolled", false)?;
        grp.raw_set("uncontrollable", false)?;
        grp.raw_set("radioSet", radio_set)?;
        grp.raw_set("lateActivation", false)?;

        let route: Table = grp.raw_get("route")?;
        let points: Table = route.raw_get("points")?;
        let p: Table = points.raw_get(1)?;
        p.raw_set("type", "TakeOffParking")?;
        p.raw_set("alt", 0)?;
        p.raw_set("action", "From Parking Area")?;
        p.raw_set("alt_type", "BARO")?;
        p.raw_set("ETA", 0)?;
        p.raw_set("ETA_locked", true)?;
        p.raw_set("speed_locked", true)?;
        p.raw_set("airdromId", Value::Nil)?;
        p.raw_set("helipadId", ship_unit_id)?;
        p.raw_set("timeReFuAr", Value::Nil)?;
        p.raw_set("linkUnit", ship_unit_id)?;
        p.raw_set("task", Self::lua_empty_combo_task(lua)?)?;
        p.raw_set("formation_template", "")?;
        p.raw_set("x", pos.x)?;
        p.raw_set("y", pos.y)?;
        let props = lua.create_table()?;
        props.raw_set("addopt", lua.create_table()?)?;
        p.raw_set("properties", props)?;

        grp.raw_set("x", pos.x)?;
        grp.raw_set("y", pos.y)?;

        for u in grp.raw_get::<_, Table>("units")?.sequence_values::<Table>() {
            let u = u?;
            u.raw_set("alt", 0)?;
            u.raw_set("x", pos.x)?;
            u.raw_set("y", pos.y)?;
            u.raw_set("parking", "1")?;
            u.raw_set("parking_id", "1")?;
        }
        Ok(())
    }

    /// Patch existing Lua waypoint tables in place (preserves DCS-only fields `IntoLua` might drop).
    /// Land/airport and naval: first point `TakeOffParking` + baro alt 10; naval also sets `linkUnit`.
    fn patch_dt_route_points_lua_tables(
        lua: &Lua,
        grp: &Group<'static>,
        _slot_kind: SlotType,
        link_unit: Option<i64>,
    ) -> Result<()> {
        let route: Table = grp.raw_get("route").context("DT group missing route")?;
        let points: Table =
            route.raw_get("points").context("DT group route missing points")?;
        let n = points.raw_len();
        if n < 1 {
            return Ok(());
        }
        for i in 1..=n {
            let p: Table =
                points.raw_get(i).with_context(|| format_compact!("route point {i}"))?;
            if i == 1 {
                p.raw_set("type", "TakeOffParking")?;
                p.raw_set("alt", 10)?;
                p.raw_set("action", "From Parking Area")?;
                p.raw_set("alt_type", "BARO")?;
                p.raw_set("ETA", 0)?;
                p.raw_set("ETA_locked", true)?;
                p.raw_set("speed_locked", true)?;
                p.raw_set("airdromId", Value::Nil)?;
                p.raw_set("helipadId", Value::Nil)?;
                p.raw_set("timeReFuAr", Value::Nil)?;
                p.raw_set("task", Self::lua_empty_combo_task(lua)?)?;
                if let Some(id) = link_unit {
                    p.raw_set("linkUnit", id)?;
                } else {
                    p.raw_set("linkUnit", Value::Nil)?;
                }
            } else {
                p.raw_set("type", "Turning Point")?;
            }
        }
        Ok(())
    }

    /// Prefer `side`, then the opposite coalition (shared loadout / props across mirror templates).
    fn table_for_side_or_opposite<'a>(
        map: &'a HashMap<Side, HashMap<String, Table<'static>>>,
        side: Side,
        unit_type: &str,
    ) -> Option<&'a Table<'static>> {
        map.get(&side)
            .and_then(|m| m.get(unit_type))
            .or_else(|| map.get(&side.opposite()).and_then(|m| m.get(unit_type)))
    }

    fn table_for_side_only<'a>(
        map: &'a HashMap<Side, HashMap<String, Table<'static>>>,
        side: Side,
        unit_type: &str,
    ) -> Option<&'a Table<'static>> {
        map.get(&side).and_then(|m| m.get(unit_type))
    }

    fn unit_is_client_skill(unit: &Table) -> bool {
        unit.raw_get::<_, String>("skill")
            .map(|s| s.as_str() == "Client")
            .unwrap_or(false)
    }

    fn is_cjtf_country(country: &Table) -> Result<bool> {
        let id: Country = country.raw_get("id")?;
        Ok(matches!(id, Country::CJTF_BLUE | Country::CJTF_RED))
    }

    fn is_fowl_managed_client_air_group(group: &Table) -> Result<bool> {
        if group.raw_get::<_, bool>("dynSpawnTemplate").unwrap_or(false) {
            return Ok(false);
        }
        let gname: String = group.raw_get("name").unwrap_or_default();
        if is_dynamic_template_group_name(gname.as_str()) {
            return Ok(false);
        }
        Ok(true)
    }

    fn ingest_unit_weapon_profile(
        side: Side,
        unit_type: &str,
        unit: &Table<'static>,
        payload: &mut HashMap<Side, HashMap<String, Table<'static>>>,
        payload_all: &mut HashMap<Side, Vec<Table<'static>>>,
        payload_variants: &mut HashMap<Side, HashMap<String, Vec<Table<'static>>>>,
        prop_aircraft: &mut HashMap<Side, HashMap<String, Table<'static>>>,
        radio: &mut HashMap<Side, HashMap<String, Table<'static>>>,
        frequency: &mut HashMap<Side, HashMap<String, Value<'static>>>,
    ) {
        if !Self::unit_is_client_skill(unit) {
            return;
        }
        let unit_type = String::from(unit_type);
        if let Ok(w) = unit.raw_get::<_, Table>("payload") {
            payload_all.entry(side).or_default().push(w.clone());
            payload_variants
                .entry(side)
                .or_default()
                .entry(unit_type.clone())
                .or_default()
                .push(w.clone());
            payload.entry(side).or_default().insert(unit_type.clone(), w);
        }
        if let Ok(w) = unit.raw_get("AddPropAircraft") {
            prop_aircraft.entry(side).or_default().insert(unit_type.clone(), w);
        }
        if let Ok(w) = unit.raw_get("Radio") {
            radio.entry(side).or_default().insert(unit_type.clone(), w);
        }
        if let Ok(v) = unit.raw_get("frequency") {
            frequency.entry(side).or_default().insert(unit_type, v);
        }
    }

    /// ME mission payload + module props from `weapon*.miz` (per coalition only).
    fn apply_weapon_template_client_unit(
        &self,
        lua: &Lua,
        side: Side,
        unit_type: &str,
        unit: &Table,
        stn: &mut u64,
    ) -> Result<String> {
        if let Some(w) = Self::table_for_side_only(&self.payload, side, unit_type) {
            unit.set("payload", w.deep_clone(lua)?)?;
        } else {
            warn!("no weapon*.miz mission payload for {side}/{unit_type}");
        }

        let stn_string =
            if let Some(tmpl) = Self::table_for_side_only(&self.prop_aircraft, side, unit_type) {
                let tmpl = tmpl.deep_clone(lua)?;
                let stn_string = Self::stamp_stn_on_add_prop_table(&tmpl, stn)?;
                unit.set("AddPropAircraft", tmpl)?;
                stn_string
            } else {
                warn!("no weapon*.miz AddPropAircraft for {side}/{unit_type}");
                String::from("")
            };

        if let Some(w) = self.radio.get(&side).and_then(|t| t.get(unit_type)) {
            unit.set("Radio", w.deep_clone(lua)?)?;
        }
        if let Some(v) = self.frequency.get(&side).and_then(|t| t.get(unit_type)) {
            unit.set("frequency", v.deep_clone(lua)?)?;
        }
        Ok(stn_string)
    }

    fn stamp_stn_on_add_prop_table(ap: &Table, stn: &mut u64) -> Result<String> {
        if ap.contains_key("STN_L16")? {
            ap.raw_set("STN_L16", String::from(format_compact!("{:005o}", *stn)))?;
            let s = String::from(format_compact!(" STN#{:005o}", *stn));
            *stn += 1;
            Ok(s)
        } else {
            Ok(String::from(""))
        }
    }

    fn patch_emitted_dynamic_spawn_unit(
        &self,
        lua: &Lua,
        side: Side,
        unit_type: &str,
        unit: &Table,
    ) -> Result<()> {
        unit.raw_set("password", Value::Nil)?;
        let mut stn_unused = 0u64;
        self.apply_weapon_template_client_unit(
            lua,
            side,
            unit_type,
            unit,
            &mut stn_unused,
        )?;
        patch_datalink_mission_unit_ids(unit)?;
        zero_dynamic_spawn_template_unit_fuel(unit)?;
        Ok(())
    }

    fn new(wep: &LoadedMiz) -> Result<Self> {
        let mut plane_slots: HashMap<Side, HashMap<String, Group>> = HashMap::new();
        let mut helicopter_slots: HashMap<Side, HashMap<String, Group>> = HashMap::new();
        let mut dt_weapon_source: HashMap<(Side, SlotType, String), Group> =
            HashMap::new();
        let mut payload: HashMap<Side, HashMap<String, Table>> = HashMap::new();
        let mut payload_all: HashMap<Side, Vec<Table>> = HashMap::new();
        let mut payload_variants: HashMap<Side, HashMap<String, Vec<Table>>> =
            HashMap::new();
        let mut prop_aircraft: HashMap<Side, HashMap<String, Table>> = HashMap::new();
        let mut radio: HashMap<Side, HashMap<String, Table>> = HashMap::new();
        let mut frequency: HashMap<Side, HashMap<String, Value>> = HashMap::new();
        for (side, coa) in [Side::Blue, Side::Red]
            .into_iter()
            .map(|side| (side, wep.mission.coalition(side)))
        {
            let coa = coa?;
            for country in coa.countries()? {
                let country = country?;
                for (st, group) in country
                    .planes()
                    .context("getting planes")?
                    .into_iter()
                    .map(|p| (SlotType::Plane, p))
                    .chain(
                        country
                            .helicopters()
                            .context("getting helicopters")?
                            .into_iter()
                            .map(|p| (SlotType::Helicopter, p)),
                    )
                {
                    let group = group?;
                    let gname: String = group.raw_get("name").unwrap_or_default();
                    let is_dt =
                        group.raw_get::<_, bool>("dynSpawnTemplate").unwrap_or(false)
                            || is_dynamic_template_group_name(&gname);
                    if is_dt {
                        for unit in group
                            .raw_get::<_, Table>("units")
                            .context("getting dt template units")?
                            .pairs::<Value, Table>()
                        {
                            let unit = unit?.1;
                            let unit_type: String =
                                unit.raw_get("type").context("getting dt unit type")?;
                            dt_weapon_source
                                .insert((side, st, unit_type.clone()), group.clone());
                            // Mission payload maps: static slot templates only (not zzDT rows in weapon).
                        }
                        info!("registered dynamic template from weapon.miz: {gname}");
                        continue;
                    }
                    Self::normalize_group_route_to_turning(&group)?;
                    for unit in group
                        .raw_get::<_, Table>("units")
                        .context("getting units")?
                        .pairs::<Value, Table>()
                    {
                        let unit = unit?.1;
                        if !Self::unit_is_client_skill(&unit) {
                            continue;
                        }
                        let unit_type: String =
                            unit.raw_get("type").context("getting units")?;
                        match st {
                            SlotType::Helicopter => {
                                helicopter_slots.entry(side).or_default()
                            }
                            SlotType::Plane => plane_slots.entry(side).or_default(),
                        }
                        .insert(unit_type.clone(), group.clone());
                        info!("adding client slot template: {unit_type}");
                        Self::ingest_unit_weapon_profile(
                            side,
                            &unit_type,
                            &unit,
                            &mut payload,
                            &mut payload_all,
                            &mut payload_variants,
                            &mut prop_aircraft,
                            &mut radio,
                            &mut frequency,
                        );
                    }
                }
            }
        }
        Ok(Self {
            plane_slots,
            helicopter_slots,
            dt_weapon_source,
            payload,
            payload_all,
            payload_variants,
            prop_aircraft,
            radio,
            frequency,
        })
    }

    /// Slot-group or DT template present in weapon.miz for this coalition (inventory `aircrafts` rows must match).
    fn has_airframe_template_for_side(&self, side: Side, unit_type: &str) -> bool {
        let k: String = unit_type.to_string().into();
        self.plane_slots
            .get(&side)
            .is_some_and(|m| m.contains_key(&k))
            || self
                .helicopter_slots
                .get(&side)
                .is_some_and(|m| m.contains_key(&k))
            || self
                .dt_weapon_source
                .contains_key(&(side, SlotType::Plane, k.clone()))
            || self
                .dt_weapon_source
                .contains_key(&(side, SlotType::Helicopter, k))
    }

    /// Coalition-wide descriptor allowlist for ordnance (vote over templates; `restricted` = blocked).
    ///
    /// - Count **pylon** appearances and **restricted** appearances per descriptor across slot templates.
    /// - Count **mention** = number of templates that reference the string under pylons ∪ restricted (once per template).
    /// Allow if **any** template mounts it on pylons, or if `restricted_count < mention_count` (same CLSID blocked
    /// on one airframe but valid elsewhere). Deny if only ever blocked (pylon_count == 0 and restricted >= mention).
    fn payload_weapon_descriptor_union(&self, side: Side) -> HashSet<StdString> {
        let mut mention_count: HashMap<StdString, usize> = HashMap::new();
        let mut pylon_count: HashMap<StdString, usize> = HashMap::new();
        let mut restricted_count: HashMap<StdString, usize> = HashMap::new();
        if let Some(all) = self.payload_all.get(&side) {
            for t in all {
                let pyl = payload_allowlist::collect_pylon_descriptors(t);
                let rst = payload_allowlist::collect_restricted_descriptors(t);
                let mut seen = HashSet::<StdString>::new();
                seen.extend(pyl.iter().cloned());
                seen.extend(rst.iter().cloned());
                for d in seen {
                    *mention_count.entry(d).or_default() += 1;
                }
                for d in pyl {
                    *pylon_count.entry(d).or_default() += 1;
                }
                for d in rst {
                    *restricted_count.entry(d).or_default() += 1;
                }
            }
        }
        let keys: HashSet<StdString> = mention_count
            .keys()
            .chain(pylon_count.keys())
            .chain(restricted_count.keys())
            .cloned()
            .collect();
        let mut out = HashSet::new();
        for d in keys {
            let p = pylon_count.get(&d).copied().unwrap_or(0);
            let r = restricted_count.get(&d).copied().unwrap_or(0);
            let m = mention_count.get(&d).copied().unwrap_or(0);
            if p > 0 || r < m {
                out.insert(d);
            }
        }
        out
    }

    /// Every descriptor string under `payload.pylons` (no vote filter).
    fn payload_pylon_union_descriptors(&self, side: Side) -> HashSet<StdString> {
        let mut out = HashSet::new();
        if let Some(all) = self.payload_all.get(&side) {
            for t in all {
                out.extend(payload_allowlist::collect_pylon_descriptors(t));
            }
        }
        out
    }

    fn slot_unit_types(&self, side: Side) -> HashSet<StdString> {
        let mut out = HashSet::new();
        if let Some(m) = self.plane_slots.get(&side) {
            out.extend(m.keys().map(|k| k.to_string()));
        }
        if let Some(m) = self.helicopter_slots.get(&side) {
            out.extend(m.keys().map(|k| k.to_string()));
        }
        out
    }

    fn payload_unit_types(&self, side: Side) -> HashSet<StdString> {
        let mut out = HashSet::new();
        if let Some(by_type) = self.payload_variants.get(&side) {
            out.extend(by_type.keys().map(|k| k.to_string()));
        }
        out
    }

    /// Pylon or restricted `wsType` union from **all** `weapon*.miz` payload tables for this coalition
    /// (not only `slot_unit_types`, so e.g. dyn templates still count for strip / alias retain).
    fn payload_ws_for_slot_types(
        &self,
        br: &weapon_bridge::WeaponBridgeMap,
        side: Side,
        use_pylons: bool,
    ) -> HashSet<[i32; 4]> {
        let mut out = HashSet::new();
        let Some(payload_variants) = self.payload_variants.get(&side) else {
            return out;
        };
        for variants in payload_variants.values() {
            for payload in variants {
                let descriptors = if use_pylons {
                    payload_allowlist::collect_pylon_descriptors(payload)
                } else {
                    payload_allowlist::collect_restricted_descriptors(payload)
                };
                for descriptor in descriptors {
                    for ws in
                        br.ws_types_for_descriptor_or_key_substring(descriptor.as_str())
                    {
                        if ws != [0, 0, 0, 0] {
                            out.insert(ws);
                        }
                    }
                }
            }
        }
        out
    }

    /// Per-aircraft wsTypes from weapon template payloads → `fowl_weapon_payload_ws.json` (bftools).
    fn build_fowl_weapon_payload_ws_file(
        &self,
        br: &weapon_bridge::WeaponBridgeMap,
    ) -> weapon_bridge::FowlWeaponPayloadWsFile {
        let mut pylon_ws_by_side: HashMap<StdString, HashMap<StdString, Vec<[i32; 4]>>> =
            HashMap::new();
        let mut restricted_ws_by_side: HashMap<
            StdString,
            HashMap<StdString, Vec<[i32; 4]>>,
        > = HashMap::new();
        for side in [Side::Blue, Side::Red] {
            let side_s = side.to_str().to_string();
            let Some(by_type) = self.payload_variants.get(&side) else {
                continue;
            };
            let mut pyl_outer: HashMap<StdString, Vec<[i32; 4]>> = HashMap::new();
            let mut rst_outer: HashMap<StdString, Vec<[i32; 4]>> = HashMap::new();
            for (unit_type, variants) in by_type {
                let mut pyl_set = HashSet::<[i32; 4]>::new();
                let mut rst_set = HashSet::<[i32; 4]>::new();
                for payload in variants {
                    for d in payload_allowlist::collect_pylon_descriptors(payload) {
                        pyl_set.extend(
                            br.ws_types_for_descriptor_or_key_substring(d.as_str()),
                        );
                    }
                    for d in payload_allowlist::collect_restricted_descriptors(payload) {
                        rst_set.extend(
                            br.ws_types_for_descriptor_or_key_substring(d.as_str()),
                        );
                    }
                }
                pyl_set.retain(|w| *w != [0, 0, 0, 0]);
                rst_set.retain(|w| *w != [0, 0, 0, 0]);
                let mut pyl_vec: Vec<_> = pyl_set.into_iter().collect();
                pyl_vec.sort_by_key(|w| (w[0], w[1], w[2], w[3]));
                let mut rst_vec: Vec<_> = rst_set.into_iter().collect();
                rst_vec.sort_by_key(|w| (w[0], w[1], w[2], w[3]));
                if !pyl_vec.is_empty() {
                    pyl_outer.insert(unit_type.to_string(), pyl_vec);
                }
                if !rst_vec.is_empty() {
                    rst_outer.insert(unit_type.to_string(), rst_vec);
                }
            }
            if !pyl_outer.is_empty() {
                pylon_ws_by_side.insert(side_s.clone(), pyl_outer);
            }
            if !rst_outer.is_empty() {
                restricted_ws_by_side.insert(side_s, rst_outer);
            }
        }
        weapon_bridge::FowlWeaponPayloadWsFile {
            schema_version: 1,
            pylon_ws_by_side,
            restricted_ws_by_side,
        }
    }

    /// `wsType` strip set: restricted-only **per payload table**, aggregated over **all** coalition
    /// weapon-template payloads (any airframe table in `weapon*.miz`). If a store appears on pylons
    /// in at least one template, `pylon_count` > 0 and it is not stripped (F-14 AIM-54 on some but not all loadouts).
    fn payload_restricted_only_ws_for_slot_types(
        &self,
        br: &weapon_bridge::WeaponBridgeMap,
        side: Side,
    ) -> HashSet<[i32; 4]> {
        let mut mention_count: HashMap<[i32; 4], usize> = HashMap::new();
        let mut pylon_count: HashMap<[i32; 4], usize> = HashMap::new();
        let mut restricted_count: HashMap<[i32; 4], usize> = HashMap::new();
        let Some(payload_variants) = self.payload_variants.get(&side) else {
            return HashSet::new();
        };
        for variants in payload_variants.values() {
            for payload in variants {
                let pyl_desc = payload_allowlist::collect_pylon_descriptors(payload);
                let rst_desc = payload_allowlist::collect_restricted_descriptors(payload);
                let mut pyl_ws = HashSet::<[i32; 4]>::new();
                let mut rst_ws = HashSet::<[i32; 4]>::new();
                for d in &pyl_desc {
                    pyl_ws.extend(
                        br.ws_types_for_descriptor_or_key_substring(d.as_str())
                            .into_iter()
                            .filter(|&ws| ws != [0, 0, 0, 0]),
                    );
                }
                for d in &rst_desc {
                    rst_ws.extend(
                        br.ws_types_for_descriptor_or_key_substring(d.as_str())
                            .into_iter()
                            .filter(|&ws| ws != [0, 0, 0, 0]),
                    );
                }
                let mut seen_ws = HashSet::<[i32; 4]>::new();
                seen_ws.extend(pyl_ws.iter().copied());
                seen_ws.extend(rst_ws.iter().copied());
                for ws in seen_ws {
                    *mention_count.entry(ws).or_default() += 1;
                }
                for ws in pyl_ws {
                    *pylon_count.entry(ws).or_default() += 1;
                }
                for ws in rst_ws {
                    *restricted_count.entry(ws).or_default() += 1;
                }
            }
        }
        let mut out = HashSet::<[i32; 4]>::new();
        let keys: HashSet<[i32; 4]> =
            mention_count.keys().chain(restricted_count.keys()).copied().collect();
        for ws in keys {
            let p = pylon_count.get(&ws).copied().unwrap_or(0);
            let r = restricted_count.get(&ws).copied().unwrap_or(0);
            let m = mention_count.get(&ws).copied().unwrap_or(0);
            if p == 0 && r >= m && m > 0 {
                out.insert(ws);
            }
        }
        out
    }

    /// Descriptor keys passed into the weapon bridge for this coalition’s default-warehouse allowlist.
    /// Uses payload vote plus pylon-only keys that map strictly to fuel `(1,3,_,_)`.
    fn payload_warehouse_bridge_descriptor_keys(
        &self,
        br: &weapon_bridge::WeaponBridgeMap,
        side: Side,
    ) -> HashSet<StdString> {
        fn maps_only_fueltank_ws(
            br: &weapon_bridge::WeaponBridgeMap,
            descriptor: &str,
        ) -> bool {
            let set = br.ws_types_for_descriptor_or_key_substring(descriptor);
            !set.is_empty() && set.iter().all(|w| w[0] == 1 && w[1] == 3)
        }
        let vote = self.payload_weapon_descriptor_union(side);
        let pylons = self.payload_pylon_union_descriptors(side);
        let mut out = vote;
        for d in pylons {
            if out.contains(&d) {
                continue;
            }
            if maps_only_fueltank_ws(br, d.as_str()) {
                out.insert(d);
            }
        }
        out
    }

    /// Coalition-wide descriptors that are **restricted-only** (never mounted on pylons).
    ///
    /// A descriptor is strip-eligible only when it is not present on pylons in any template and
    /// is restricted in every template where it is mentioned.
    fn payload_restricted_union_descriptors(&self, side: Side) -> HashSet<StdString> {
        let mut mention_count: HashMap<StdString, usize> = HashMap::new();
        let mut pylon_count: HashMap<StdString, usize> = HashMap::new();
        let mut restricted_count: HashMap<StdString, usize> = HashMap::new();
        if let Some(all) = self.payload_all.get(&side) {
            for t in all {
                let pyl = payload_allowlist::collect_pylon_descriptors(t);
                let rst = payload_allowlist::collect_restricted_descriptors(t);
                let mut seen = HashSet::<StdString>::new();
                seen.extend(pyl.iter().cloned());
                seen.extend(rst.iter().cloned());
                for d in seen {
                    *mention_count.entry(d).or_default() += 1;
                }
                for d in pyl {
                    *pylon_count.entry(d).or_default() += 1;
                }
                for d in rst {
                    *restricted_count.entry(d).or_default() += 1;
                }
            }
        }
        let mut out = HashSet::<StdString>::new();
        let keys: HashSet<StdString> =
            mention_count.keys().chain(restricted_count.keys()).cloned().collect();
        for d in keys {
            let p = pylon_count.get(&d).copied().unwrap_or(0);
            let r = restricted_count.get(&d).copied().unwrap_or(0);
            let m = mention_count.get(&d).copied().unwrap_or(0);
            if p == 0 && r >= m && m > 0 {
                out.insert(d);
            }
        }
        out
    }

    /// Coalition-wide wsTypes that are restricted-only in payload templates.
    ///
    /// Counts are computed per template after descriptor -> wsType bridge mapping, so alias collisions
    /// (different descriptor keys mapping to the same wsType) are handled correctly.
    fn payload_restricted_only_weapon_ws(
        &self,
        br: &weapon_bridge::WeaponBridgeMap,
        side: Side,
    ) -> HashSet<[i32; 4]> {
        let mut mention_count: HashMap<[i32; 4], usize> = HashMap::new();
        let mut pylon_count: HashMap<[i32; 4], usize> = HashMap::new();
        let mut restricted_count: HashMap<[i32; 4], usize> = HashMap::new();

        if let Some(all) = self.payload_all.get(&side) {
            for t in all {
                let pyl_desc = payload_allowlist::collect_pylon_descriptors(t);
                let rst_desc = payload_allowlist::collect_restricted_descriptors(t);

                let mut pyl_ws = HashSet::<[i32; 4]>::new();
                let mut rst_ws = HashSet::<[i32; 4]>::new();
                for d in &pyl_desc {
                    pyl_ws.extend(
                        br.ws_types_for_descriptor_or_key_substring(d.as_str())
                            .into_iter()
                            .filter(|&ws| ws != [0, 0, 0, 0]),
                    );
                }
                for d in &rst_desc {
                    rst_ws.extend(
                        br.ws_types_for_descriptor_or_key_substring(d.as_str())
                            .into_iter()
                            .filter(|&ws| ws != [0, 0, 0, 0]),
                    );
                }

                let mut seen_ws = HashSet::<[i32; 4]>::new();
                seen_ws.extend(pyl_ws.iter().copied());
                seen_ws.extend(rst_ws.iter().copied());
                for ws in seen_ws {
                    *mention_count.entry(ws).or_default() += 1;
                }
                for ws in pyl_ws {
                    *pylon_count.entry(ws).or_default() += 1;
                }
                for ws in rst_ws {
                    *restricted_count.entry(ws).or_default() += 1;
                }
            }
        }

        let mut out = HashSet::<[i32; 4]>::new();
        let keys: HashSet<[i32; 4]> =
            mention_count.keys().chain(restricted_count.keys()).copied().collect();
        for ws in keys {
            let p = pylon_count.get(&ws).copied().unwrap_or(0);
            let r = restricted_count.get(&ws).copied().unwrap_or(0);
            let m = mention_count.get(&ws).copied().unwrap_or(0);
            if p == 0 && r >= m && m > 0 {
                out.insert(ws);
            }
        }
        out
    }

    /// Pylon-only footprint (`payload.pylons` → `wsType`). Excludes `restricted` so blocked stores do not
    /// inflate B/RDEFAULT anchor (`vote ∪ anchor`).
    fn payload_pylon_only_footprint_weapon_ws(
        &self,
        br: &weapon_bridge::WeaponBridgeMap,
        side: Side,
    ) -> HashSet<[i32; 4]> {
        const ZERO: [i32; 4] = [0, 0, 0, 0];
        let mut out = HashSet::new();
        if let Some(all) = self.payload_all.get(&side) {
            for t in all {
                for d in payload_allowlist::collect_pylon_descriptors(t) {
                    for ws in br.ws_types_for_descriptor_or_key_substring(d.as_str()) {
                        if ws != ZERO {
                            out.insert(ws);
                        }
                    }
                }
            }
        }
        out
    }

    /// `payload.pylons` → `wsType` for one unit (all `weapon*.miz` variants); used to gate `template_restricted`.
    fn payload_pylon_ws_for_unit_type(
        &self,
        br: &weapon_bridge::WeaponBridgeMap,
        side: Side,
        unit_type: &str,
    ) -> HashSet<[i32; 4]> {
        const ZERO: [i32; 4] = [0, 0, 0, 0];
        let mut out = HashSet::new();
        let Some(variants) =
            self.payload_variants.get(&side).and_then(|by_type| by_type.get(unit_type))
        else {
            return out;
        };
        for payload in variants {
            for d in payload_allowlist::collect_pylon_descriptors(payload) {
                for ws in br.ws_types_for_descriptor_or_key_substring(d.as_str()) {
                    if ws != ZERO {
                        out.insert(ws);
                    }
                }
            }
        }
        out
    }

    /// Same as `payload_pylon_ws_for_unit_type` but descriptor-exact only (no substring bridge).
    fn payload_pylon_ws_for_unit_type_exact(
        &self,
        br: &weapon_bridge::WeaponBridgeMap,
        side: Side,
        unit_type: &str,
    ) -> HashSet<[i32; 4]> {
        const ZERO: [i32; 4] = [0, 0, 0, 0];
        let mut out = HashSet::new();
        let Some(variants) =
            self.payload_variants.get(&side).and_then(|by_type| by_type.get(unit_type))
        else {
            return out;
        };
        for payload in variants {
            for d in payload_allowlist::collect_pylon_descriptors(payload) {
                if let Some(ws) = br.ws_type_for_descriptor(d.as_str()) {
                    if ws != ZERO {
                        out.insert(ws);
                    }
                }
            }
        }
        out
    }

    /// Ordnance `wsType`s a module may carry: payload descriptors, pylons, and bridge key list (no `aircraft_by_ws` union).
    fn module_ordnance_ws_for_unit_type(
        &self,
        br: &weapon_bridge::WeaponBridgeMap,
        side: Side,
        unit_type: &str,
    ) -> HashSet<[i32; 4]> {
        const ZERO: [i32; 4] = [0, 0, 0, 0];
        let mut out = HashSet::new();
        if let Some(payload_by_type) = self.payload.get(&side) {
            if let Some(payload) = payload_by_type.get(unit_type) {
                let desc = payload_allowlist::collect_module_descriptors(payload);
                for d in desc.supported {
                    for ws in br.ws_types_for_descriptor_or_key_substring(d.as_str()) {
                        if ws != ZERO && is_inventory_cap_ordnance_ws(ws) {
                            out.insert(ws);
                        }
                    }
                }
            }
        }
        for ws in self
            .payload_pylon_ws_for_unit_type_exact(br, side, unit_type)
            .into_iter()
            .chain(
                self.payload_pylon_ws_for_unit_type(br, side, unit_type)
                    .into_iter(),
            )
        {
            if is_inventory_cap_ordnance_ws(ws) {
                out.insert(ws);
            }
        }
        for ws in br.weapon_ws_for_aircraft_key_only(unit_type) {
            if ws != ZERO && is_inventory_cap_ordnance_ws(ws) {
                out.insert(ws);
            }
        }
        out
    }

    /// Exact pylon-only footprint for DEFAULT generation.
    ///
    /// DEFAULT must mirror stores explicitly mounted in `weapon*.miz`; substring bridge fallback can pull
    /// neighboring/cross-coalition variants and must not be used here.
    fn payload_pylon_only_footprint_weapon_ws_exact(
        &self,
        br: &weapon_bridge::WeaponBridgeMap,
        side: Side,
    ) -> HashSet<[i32; 4]> {
        const ZERO: [i32; 4] = [0, 0, 0, 0];
        let mut out = HashSet::new();
        if let Some(all) = self.payload_all.get(&side) {
            for t in all {
                for d in payload_allowlist::collect_pylon_descriptors(t) {
                    if let Some(ws) = br.ws_type_for_descriptor(d.as_str()) {
                        if ws != ZERO {
                            out.insert(ws);
                        }
                    }
                }
            }
        }
        out
    }

    /// Exact restricted-only footprint for DEFAULT deny.
    fn payload_restricted_only_weapon_ws_exact(
        &self,
        br: &weapon_bridge::WeaponBridgeMap,
        side: Side,
    ) -> HashSet<[i32; 4]> {
        let mut mention_count: HashMap<[i32; 4], usize> = HashMap::new();
        let mut pylon_count: HashMap<[i32; 4], usize> = HashMap::new();
        let mut restricted_count: HashMap<[i32; 4], usize> = HashMap::new();

        if let Some(all) = self.payload_all.get(&side) {
            for t in all {
                let pyl_desc = payload_allowlist::collect_pylon_descriptors(t);
                let rst_desc = payload_allowlist::collect_restricted_descriptors(t);
                let mut pyl_ws = HashSet::<[i32; 4]>::new();
                let mut rst_ws = HashSet::<[i32; 4]>::new();
                for d in &pyl_desc {
                    if let Some(ws) = br.ws_type_for_descriptor(d.as_str()) {
                        if ws != [0, 0, 0, 0] {
                            pyl_ws.insert(ws);
                        }
                    }
                }
                for d in &rst_desc {
                    if let Some(ws) = br.ws_type_for_descriptor(d.as_str()) {
                        if ws != [0, 0, 0, 0] {
                            rst_ws.insert(ws);
                        }
                    }
                }
                let mut seen_ws = HashSet::<[i32; 4]>::new();
                seen_ws.extend(pyl_ws.iter().copied());
                seen_ws.extend(rst_ws.iter().copied());
                for ws in seen_ws {
                    *mention_count.entry(ws).or_default() += 1;
                }
                for ws in pyl_ws {
                    *pylon_count.entry(ws).or_default() += 1;
                }
                for ws in rst_ws {
                    *restricted_count.entry(ws).or_default() += 1;
                }
            }
        }

        let mut out = HashSet::<[i32; 4]>::new();
        let keys: HashSet<[i32; 4]> =
            mention_count.keys().chain(restricted_count.keys()).copied().collect();
        for ws in keys {
            let p = pylon_count.get(&ws).copied().unwrap_or(0);
            let r = restricted_count.get(&ws).copied().unwrap_or(0);
            let m = mention_count.get(&ws).copied().unwrap_or(0);
            if p == 0 && r >= m && m > 0 {
                out.insert(ws);
            }
        }
        out
    }

    /// Footprint of ordnance referenced by this coalition’s slot payloads (`pylons` ∪ `restricted`) → `wsType`.
    fn payload_footprint_weapon_ws(
        &self,
        br: &weapon_bridge::WeaponBridgeMap,
        side: Side,
    ) -> HashSet<[i32; 4]> {
        const ZERO: [i32; 4] = [0, 0, 0, 0];
        let mut out = HashSet::new();
        if let Some(all) = self.payload_all.get(&side) {
            for t in all {
                let mut seen = HashSet::<StdString>::new();
                seen.extend(payload_allowlist::collect_pylon_descriptors(t));
                seen.extend(payload_allowlist::collect_restricted_descriptors(t));
                for d in seen {
                    for ws in br.ws_types_for_descriptor_or_key_substring(d.as_str()) {
                        if ws != ZERO {
                            out.insert(ws);
                        }
                    }
                }
            }
        }
        out
    }

    fn generate_slots(&self, lua: &Lua, base: &mut LoadedMiz) -> Result<()> {
        let idx = base.mission.index()?;
        let static_creation_settings =
            Self::load_zone_creation_settings(base, "SETTINGS-static-slots-creation")?;
        let mut templates = HashMap::default();
        let mut uid = idx.max_uid();
        let mut gid = idx.max_gid();
        uid.next();
        gid.next();
        for zone in base.mission.triggers()? {
            let zone = zone?;
            let name = zone.name()?;
            if let Some(s) = name.strip_prefix("TTSN") {
                if !Self::zone_enabled_by_settings(&static_creation_settings, &name) {
                    continue;
                }
                templates.insert(
                    String::from(s),
                    SlotSpec::new(
                        &HashMap::default(),
                        zone.properties()?,
                        true,
                        INCLUDE_STATIC_SLOT_KEYS,
                    )?,
                );
                info!("added naval slot template {s}")
            } else if let Some(s) = name.strip_prefix("TTS") {
                if !Self::zone_enabled_by_settings(&static_creation_settings, &name) {
                    continue;
                }
                templates.insert(
                    String::from(s),
                    SlotSpec::new(
                        &HashMap::default(),
                        zone.properties()?,
                        false,
                        INCLUDE_STATIC_SLOT_KEYS,
                    )?,
                );
                info!("added slot template {s}")
            }
        }
        let ship_wh_map = collect_ship_warehouse_group_map(base)?;
        let mut ship_unit_by_group: HashMap<String, i64> = HashMap::default();
        for (&unit_id, (_side, group_name)) in &ship_wh_map {
            if let Some(prev) = ship_unit_by_group.insert(group_name.clone(), unit_id) {
                warn!(
                    "multiple carrier warehouse ships share group name {:?}; static TTSN linkUnit uses unitId {}",
                    group_name, prev
                );
            }
        }
        let mut emit_static_slots_for_zone = |zone: dcso3::env::miz::TriggerZone<'_>,
                                             zone_name: &str,
                                             spec: &SlotSpec,
                                             naval_link_unit: Option<i64>| -> Result<()> {
            for (side, slots) in &spec.slots {
                let base_posgen: Box<dyn PosGenerator> = match zone.typ()? {
                    TriggerZoneTyp::Quad(quad) => Box::new(SlotGrid::new(
                        String::from(zone_name),
                        quad,
                        spec.margin,
                        spec.spacing,
                    )?),
                    TriggerZoneTyp::Circle { radius } => Box::new(SlotRadial::new(
                        String::from(zone_name),
                        radius,
                        zone.pos()?,
                        spec.margin,
                        spec.spacing,
                    )?),
                };
                let mut posgen: Box<dyn PosGenerator> = match naval_link_unit {
                    Some(ship_id) => {
                        let (pos, heading) =
                            carrier_static_slot_reference_pose(base, ship_id)?;
                        Box::new(ConstantPosGenerator { pos, heading })
                    }
                    None => base_posgen,
                };
                let coa = base.mission.coalition(*side)?;
                let cname = match side {
                    Side::Blue => Country::CJTF_BLUE,
                    Side::Red => Country::CJTF_RED,
                    Side::Neutral => unreachable!(),
                };
                let country = match coa.country(cname)? {
                    Some(c) => c,
                    None => {
                        let tbl = lua.create_table()?;
                        tbl.raw_set("id", cname)?;
                        tbl.raw_set(
                            "name",
                            match cname {
                                Country::CJTF_BLUE => "CJTF Blue",
                                Country::CJTF_RED => "CJTF Red",
                                _ => unreachable!(),
                            },
                        )?;
                        coa.raw_get::<_, Table>("country")?.push(tbl)?;
                        coa.country(cname)?.unwrap()
                    }
                };
                let helicopters = {
                    let heli = country.helicopters()?;
                    if heli.len() > 0 {
                        heli
                    } else {
                        let heli = lua.create_table()?;
                        heli.raw_set("group", lua.create_table()?)?;
                        country.raw_set("helicopter", heli)?;
                        country.helicopters()?
                    }
                };
                let planes = {
                    let plane = country.planes()?;
                    if plane.len() > 0 {
                        plane
                    } else {
                        let plane = lua.create_table()?;
                        plane.raw_set("group", lua.create_table()?)?;
                        country.raw_set("plane", plane)?;
                        country.planes()?
                    }
                };
                for (vehicle, n) in slots {
                    let (seq, tmpl) =
                        match self.plane_slots.get(side).and_then(|s| s.get(vehicle)) {
                            Some(t) => (&planes, t),
                            None => {
                                match self
                                    .helicopter_slots
                                    .get(side)
                                    .and_then(|s| s.get(vehicle))
                                {
                                    Some(t) => (&helicopters, t),
                                    None => {
                                        bail!("missing required slot template {vehicle}")
                                    }
                                }
                            }
                        };
                    for _ in 0..*n {
                        let tmpl = tmpl.deep_clone(lua)?;
                        let pos = posgen.next()?;
                        let route = tmpl.route()?;
                        let deck_slot = naval_link_unit.is_some();
                        route.set_points({
                            let mut first = true;
                            route
                                .points()?
                                .into_iter()
                                .map(|p| {
                                    let mut p = p?;
                                    if first {
                                        let is_naval = deck_slot
                                            || spec
                                                .naval_units
                                                .contains(&(*side, vehicle.clone()));
                                        p.typ = if is_naval {
                                            PointType::TakeOffParking
                                        } else {
                                            PointType::TakeOffGround
                                        };
                                        p.pos = LuaVec2(pos);
                                        first = false;
                                    } else {
                                        p.typ = PointType::TurningPoint;
                                    }
                                    Ok(p)
                                })
                                .collect::<Result<Vec<MissionPoint>>>()?
                        })?;
                        tmpl.set_route(route)?;
                        tmpl.set_id(gid)?;
                        tmpl.set_pos(pos)?;
                        let is_heli = self
                            .helicopter_slots
                            .get(side)
                            .and_then(|s| s.get(vehicle))
                            .is_some();
                        for u in tmpl.units()? {
                            let u = u?;
                            if u.skill()? != Skill::Client {
                                bail!("slot templates must be set to Client skill level")
                            }
                            u.set_id(uid)?;
                            u.set_heading(posgen.azumith())?;
                            u.set_pos(pos)?;
                            if naval_link_unit.is_some() {
                                zero_dynamic_spawn_template_unit_fuel(&u)?;
                            }
                            patch_datalink_mission_unit_ids(&u)
                                .with_context(|| format_compact!("unit {u:?}"))?;
                            let unit_type: String = u.raw_get("type")?;
                            let mut stn_unused = 0u64;
                            self.apply_weapon_template_client_unit(
                                lua,
                                *side,
                                &unit_type,
                                &u,
                                &mut stn_unused,
                            )
                            .with_context(|| {
                                format_compact!("static slot {vehicle} in zone {zone_name}")
                            })?;
                            uid.next();
                        }
                        if let Some(id) = naval_link_unit {
                            Self::patch_static_carrier_deck_slot_group(
                                lua,
                                &tmpl,
                                id,
                                pos,
                                is_heli,
                            )?;
                        }
                        gid.next();
                        seq.push(tmpl)?;
                    }
                }
            }
            Ok(())
        };
        // Land/air static placement: TS* (not TTSN* — carriers use the loop below).
        for zone in base.mission.triggers()? {
            let zone = zone?;
            let name = zone.name()?;
            if !name.starts_with("TS") || name.starts_with("TTSN") {
                continue;
            }
            let spec = SlotSpec::new(
                &templates,
                zone.properties()?,
                false,
                INCLUDE_STATIC_SLOT_KEYS,
            )?;
            emit_static_slots_for_zone(zone, name.as_ref(), &spec, None)?;
        }
        // Carrier deck static slots: TTSN* only, gated by SETTINGS-static-slots-creation.
        for zone in base.mission.triggers()? {
            let zone = zone?;
            let name = zone.name()?;
            let Some(pad) = name.strip_prefix("TTSN") else {
                continue;
            };
            if !Self::zone_enabled_by_settings(&static_creation_settings, name.as_ref()) {
                continue;
            }
            let Some(&ship_unit_id) = ship_unit_by_group.get(pad) else {
                warn!(
                    "skipping static carrier slots for {name}: no warehouse ship with group name {pad}"
                );
                continue;
            };
            let spec = SlotSpec::new(
                &templates,
                zone.properties()?,
                true,
                INCLUDE_STATIC_SLOT_KEYS,
            )?;
            emit_static_slots_for_zone(zone, name.as_ref(), &spec, Some(ship_unit_id))?;
            info!(
                "added static carrier deck slots for {pad} (zone {name}, linkUnit={ship_unit_id})"
            );
        }
        Ok(())
    }

    /// Emits `zzDT-<type>-<side>` groups (`dynSpawnTemplate` + warehouse `linkDynTempl`) into **base** only.
    /// `weapon*.miz` is read-only (static client templates); never written by bftools.
    fn emit_dynamic_spawn_templates(
        &self,
        lua: &'static Lua,
        base: &mut LoadedMiz,
    ) -> Result<DynamicSpawnEmit> {
        let idx = base.mission.index()?;
        let naval_dynamic_spawn_settings =
            Self::load_zone_creation_settings(base, SETTINGS_DYNAMIC_SPAWN_TTDN_ZONE)?;
        let mut uid = idx.max_uid();
        let mut gid = idx.max_gid();
        uid.next();
        gid.next();
        // Optional dynamic template filters from trigger zones:
        // - TTD*  = land dynamic template definitions
        // - TTDN* = naval dynamic template definitions
        // (both currently emit Turning Point route types)
        let mut dyn_templates: HashMap<String, SlotSpec> = HashMap::default();
        for zone in base.mission.triggers()? {
            let zone = zone?;
            let name = zone.name()?;
            if let Some(s) = name.strip_prefix("TTDN") {
                dyn_templates.insert(
                    String::from(s),
                    SlotSpec::new(
                        &HashMap::default(),
                        zone.properties()?,
                        true,
                        INCLUDE_DYNAMIC_SLOT_KEYS,
                    )?,
                );
            } else if let Some(s) = name.strip_prefix("TTD") {
                if name.as_str() == TTD_DYN_FARP_POLICY_ZONE {
                    continue;
                }
                dyn_templates.insert(
                    String::from(s),
                    SlotSpec::new(
                        &HashMap::default(),
                        zone.properties()?,
                        false,
                        INCLUDE_DYNAMIC_SLOT_KEYS,
                    )?,
                );
            }
        }
        // Land: `TTD*` (not `TTDN*`). Naval: `TTDN*`.
        // Listed `(side,type)` only (`X` or positive qty).
        let mut land_allowed_set: HashSet<(Side, String)> = HashSet::default();
        let mut naval_allowed_set: HashSet<(Side, String)> = HashSet::default();
        let mut have_land_policy_zones = false;
        let mut have_naval_policy_zones = false;
        for zone in base.mission.triggers()? {
            let zone = zone?;
            let name = zone.name()?;
            if name.starts_with("TTDN") {
                have_naval_policy_zones = true;
                let spec = SlotSpec::new(
                    &dyn_templates,
                    zone.properties()?,
                    false,
                    INCLUDE_DYNAMIC_SLOT_KEYS,
                )?;
                for (side, m) in spec.slots {
                    for (unit_type, count) in m {
                        if count > 0 {
                            naval_allowed_set.insert((side, unit_type));
                        }
                    }
                }
            } else if name.starts_with("TTD") {
                if name.as_str() == TTD_DYN_FARP_POLICY_ZONE {
                    continue;
                }
                have_land_policy_zones = true;
                let spec = SlotSpec::new(
                    &dyn_templates,
                    zone.properties()?,
                    false,
                    INCLUDE_DYNAMIC_SLOT_KEYS,
                )?;
                for (side, m) in spec.slots {
                    for (unit_type, count) in m {
                        if count > 0 {
                            land_allowed_set.insert((side, unit_type));
                        }
                    }
                }
            }
        }
        let land_allow =
            if have_land_policy_zones { Some(land_allowed_set) } else { None };
        let naval_allow =
            if have_naval_policy_zones { Some(naval_allowed_set) } else { None };

        let mut specs: Vec<(Side, SlotType, String, Group)> = Vec::new();
        for side in [Side::Red, Side::Blue] {
            if let Some(m) = self.plane_slots.get(&side) {
                for (unit_type, g) in m {
                    specs.push((side, SlotType::Plane, unit_type.clone(), g.clone()));
                }
            }
            if let Some(m) = self.helicopter_slots.get(&side) {
                for (unit_type, g) in m {
                    specs.push((
                        side,
                        SlotType::Helicopter,
                        unit_type.clone(),
                        g.clone(),
                    ));
                }
            }
        }
        specs.sort_by(|a, b| a.0.to_str().cmp(b.0.to_str()).then(a.2.cmp(&b.2)));
        // Emit dynamic templates for every aircraft/helicopter template present in weapon.miz.
        // Per-objective TTD/TTDN policy is applied later when warehouse rows are filled/pruned.
        // zzDT mission payload comes only from weapon*.miz (never static CJTF slots).

        let mut link_by_side_type: HashMap<(Side, String), GroupId> = HashMap::new();
        let mut emitted_names: HashSet<String> = HashSet::new();
        let slot_password = String::from(DYNAMIC_TEMPLATE_SLOT_PASSWORD);
        info!(
            "dynamic spawn template slot password (all {}* groups this build): {}",
            DYNAMIC_TEMPLATE_GROUP_PREFIX, slot_password
        );

        for (side, slot_kind, unit_type, src_default) in specs {
            let (src, from_weapon_dt) =
                match self.dt_weapon_source.get(&(side, slot_kind, unit_type.clone())) {
                    Some(g) => (g, true),
                    None => (&src_default, false),
                };
            // One template per coalition (radio etc.); mission-wide unique group names.
            let mut group_name = String::from(format_compact!(
                "{}{unit_type}-{}",
                DYNAMIC_TEMPLATE_GROUP_PREFIX,
                side.to_str()
            ));
            if emitted_names.contains(&group_name) {
                group_name = String::from(format_compact!(
                    "{}{unit_type}-{}-{}",
                    DYNAMIC_TEMPLATE_GROUP_PREFIX,
                    side.to_str(),
                    match slot_kind {
                        SlotType::Plane => "plane",
                        SlotType::Helicopter => "heli",
                    }
                ));
            }
            let kind = match slot_kind {
                SlotType::Plane => GroupKind::Plane,
                SlotType::Helicopter => GroupKind::Helicopter,
            };
            if emitted_names.contains(&group_name) {
                warn!("skipping dynamic template {group_name}, duplicate in weapon templates");
                continue;
            }
            if base.mission.get_group_by_name(&idx, kind, side, &group_name)?.is_some() {
                warn!(
                    "skipping dynamic template {group_name}, group name already exists"
                );
                continue;
            }

            let tmpl: Group<'static> = src.deep_clone(lua)?;
            // DCS + Fowl warehouse `linkDynTempl` require this flag on the template group.
            tmpl.raw_set("dynSpawnTemplate", true)?;
            tmpl.raw_set("lateActivation", false)?;
            tmpl.raw_set("uncontrolled", false)?;
            if from_weapon_dt {
                info!(
                    "{}{unit_type}-{}: using route from weapon.miz template",
                    DYNAMIC_TEMPLATE_GROUP_PREFIX,
                    side.to_str()
                );
            }
            // Force first-point route fields required by DCS dynamic templates.
            Self::patch_dt_route_points_lua_tables(lua, &tmpl, slot_kind, None)?;

            tmpl.set_name(group_name.clone())?;
            tmpl.set_id(gid)?;
            // DCS/ME read slot lock password on the **group** (after groupId in mission Lua), not on units.
            tmpl.raw_set("password", slot_password.clone())?;

            let mut unit_ord = 0;
            for u in tmpl.units()? {
                let u = u?;
                if u.skill()? != Skill::Client {
                    bail!(
                        "dynamic template source for {unit_type} must use Client skill"
                    );
                }
                unit_ord += 1;
                u.set_id(uid)?;
                u.set_name(String::from(format_compact!("{group_name}-{unit_ord}")))?;
                u.raw_set("skill", "Client")?;
                self.patch_emitted_dynamic_spawn_unit(
                    lua,
                    side,
                    &unit_type,
                    &u,
                )
                .with_context(|| format_compact!("{group_name} unit {unit_ord}"))?;
                uid.next();
            }

            apply_dynamic_template_group_visibility(&tmpl, slot_kind)?;

            let template_gid = tmpl.id()?;

            gid.next();

            push_client_air_group_to_cjtf(lua, &base.mission, side, slot_kind, tmpl)?;
            link_by_side_type.insert((side, unit_type), template_gid);
            emitted_names.insert(group_name.clone());
            info!("added dynamic spawn template {}", group_name);
        }

        // Per-hull templates: carrier DS needs route `linkUnit` = ship unitId (see static TTSN slots).
        let ship_wh_map = collect_ship_warehouse_group_map(base)?;
        let mut link_by_ship: HashMap<(Side, String, String), GroupId> = HashMap::default();
        let ship_hull_by_wid: HashMap<i64, String> = ship_wh_map
            .iter()
            .map(|(&wid, (_, hull))| (wid, String::from(hull.as_str())))
            .collect();
        let ship_aircraft_allow =
            build_ship_warehouse_aircraft_allow(base, &dyn_templates, &ship_wh_map)?;
        let mut naval_template_keys: HashSet<(Side, String, String)> = HashSet::default();
        for (&ship_unit_id, (side, hull_name)) in &ship_wh_map {
            let hull_key = format!("TTDN{}", hull_name.as_str());
            if naval_dynamic_spawn_settings
                .get(&String::from(hull_key.as_str()))
                .copied()
                == Some(false)
            {
                info!(
                    "skipping per-hull naval dynamic templates for {} ({SETTINGS_DYNAMIC_SPAWN_TTDN_ZONE} {hull_key}=false; use static deck slots)",
                    hull_name
                );
                continue;
            }
            let Some(allowed) = ship_aircraft_allow.get(&ship_unit_id) else {
                continue;
            };
            for (slot_side, unit_type) in allowed {
                if *slot_side != *side {
                    continue;
                }
                let unit_type = String::from(unit_type.as_str());
                let hull_dc = String::from(hull_name.as_str());
                let dedup = (*side, unit_type.clone(), hull_dc.clone());
                if !naval_template_keys.insert(dedup.clone()) {
                    continue;
                }
                let (slot_kind, src_default) =
                    if let Some(g) = self.plane_slots.get(side).and_then(|m| m.get(&unit_type)) {
                        (SlotType::Plane, g)
                    } else if let Some(g) =
                        self.helicopter_slots.get(side).and_then(|m| m.get(&unit_type))
                    {
                        (SlotType::Helicopter, g)
                    } else {
                        warn!(
                            "TTDN {:?}: {} listed but no plane/heli template in weapon.miz",
                            hull_name, unit_type
                        );
                        continue;
                    };
                let (src, from_weapon_dt) = match self
                    .dt_weapon_source
                    .get(&(*side, slot_kind, unit_type.clone()))
                {
                    Some(g) => (g, true),
                    None => (src_default, false),
                };
                let group_name = String::from(format_compact!(
                    "{}{unit_type}-{}-{hull_name}",
                    DYNAMIC_TEMPLATE_GROUP_PREFIX,
                    side.to_str()
                ));
                if emitted_names.contains(&group_name) {
                    warn!("skipping naval dynamic template {group_name}, duplicate");
                    continue;
                }
                let kind = match slot_kind {
                    SlotType::Plane => GroupKind::Plane,
                    SlotType::Helicopter => GroupKind::Helicopter,
                };
                if base
                    .mission
                    .get_group_by_name(&idx, kind, *side, &group_name)?
                    .is_some()
                {
                    warn!("skipping naval dynamic template {group_name}, already exists");
                    continue;
                }
                let tmpl: Group<'static> = src.deep_clone(lua)?;
                tmpl.raw_set("dynSpawnTemplate", true)?;
                tmpl.raw_set("lateActivation", false)?;
                tmpl.raw_set("uncontrolled", false)?;
                tmpl.raw_set("linkOffset", true)?;
                if from_weapon_dt {
                    info!(
                        "{group_name}: using route from weapon.miz template, linkUnit={ship_unit_id}"
                    );
                } else {
                    info!("{group_name}: linkUnit={ship_unit_id}");
                }
                Self::patch_dt_route_points_lua_tables(lua, &tmpl, slot_kind, Some(ship_unit_id))?;
                tmpl.set_name(group_name.clone())?;
                tmpl.set_id(gid)?;
                tmpl.raw_set("password", slot_password.clone())?;
                let mut unit_ord = 0;
                for u in tmpl.units()? {
                    let u = u?;
                    if u.skill()? != Skill::Client {
                        bail!(
                            "dynamic template source for {unit_type} must use Client skill"
                        );
                    }
                    unit_ord += 1;
                    u.set_id(uid)?;
                    u.set_name(String::from(format_compact!("{group_name}-{unit_ord}")))?;
                    u.raw_set("skill", "Client")?;
                    u.raw_set("alt", 0)?;
                    self.patch_emitted_dynamic_spawn_unit(
                        lua,
                        *side,
                        &unit_type,
                        &u,
                    )
                    .with_context(|| format_compact!("{group_name} unit {unit_ord}"))?;
                    uid.next();
                }
                apply_dynamic_template_group_visibility(&tmpl, slot_kind)?;
                let template_gid = tmpl.id()?;
                gid.next();
                push_client_air_group_to_cjtf(lua, &base.mission, *side, slot_kind, tmpl)?;
                link_by_ship.insert(dedup, template_gid);
                emitted_names.insert(group_name.clone());
                info!("added naval dynamic spawn template {}", group_name);
            }
        }

        Ok(DynamicSpawnEmit {
            link_by_side_type,
            link_by_ship,
            ship_hull_by_wid,
            land_allow,
            naval_allow,
            dyn_templates,
        })
    }

    fn apply(
        &self,
        lua: &Lua,
        objectives: &mut Vec<TriggerZone>,
        base: &mut LoadedMiz,
        tzf_plane_fuel: &[TzfPlaneFuelZone],
        carrier_pad_objectives: &HashMap<String, String>,
    ) -> Result<()> {
        let mut slots: HashMap<String, HashMap<String, usize>> = HashMap::default();
        let mut replace_count: HashMap<String, isize> = HashMap::new();
        let mut stn = 1u64;
        info!("applying weapon*.miz client profiles (CJTF slots only)");
        for (side, coa) in
            Side::ALL.into_iter().map(|side| (side, base.mission.coalition(side)))
        {
            let coa = coa?;
            for country in coa.raw_get::<_, Table>("country")?.pairs::<Value, Table>() {
                let country = country?.1;
                if !Self::is_cjtf_country(&country)? {
                    continue;
                }
                for group in vehicle(&country, "plane").context("getting planes")?.chain(
                    vehicle(&country, "helicopter").context("getting helicopters")?,
                ) {
                    let group = group.context("getting group")?;
                    if !Self::is_fowl_managed_client_air_group(&group)? {
                        continue;
                    }
                    for unit in group
                        .raw_get::<_, Table>("units")
                        .context("getting units")?
                        .pairs::<Value, Table>()
                    {
                        let unit = unit.context("getting unit")?.1;
                        // skip ai aircraft
                        if unit.raw_get::<_, String>("skill")?.as_str() != "Client" {
                            continue;
                        }
                        let unit_type: String = unit.raw_get("type")?;
                        let stn_string = self.apply_weapon_template_client_unit(
                            lua,
                            side,
                            &unit_type,
                            &unit,
                            &mut stn,
                        )?;
                        if carrier_pad_from_client_group(base, &group)?.is_some() {
                            zero_dynamic_spawn_template_unit_fuel(&unit)?;
                        }
                        increment_key(&mut replace_count, &unit_type);
                        let x = unit.get("x")?;
                        let y = unit.get("y")?;
                        let unit_pos = Vector2::new(x, y);
                        let mut strip_internal_fuel_plane = false;
                        for tzf in tzf_plane_fuel {
                            if tzf.contains(unit_pos)? {
                                strip_internal_fuel_plane = true;
                                break;
                            }
                        }
                        let mut found = false;
                        if let Some(obj_name) = client_slot_objective_name(
                            unit_pos,
                            &group,
                            objectives,
                            base,
                            carrier_pad_objectives,
                        )? {
                            for trigger_zone in &mut *objectives {
                                if trigger_zone.objective_name != obj_name {
                                    continue;
                                }
                                if !trigger_zone.contains(unit_pos)? {
                                    continue;
                                }
                                found = true;
                                let count = increment_key(
                                    &mut trigger_zone.spawn_count,
                                    &unit_type,
                                );
                                let new_name = String::from(format_compact!(
                                    "{} {} {}{}",
                                    trigger_zone.objective_name,
                                    &unit_type,
                                    count,
                                    stn_string
                                ));
                                unit.set("name", new_name.clone())?;
                                group.set("name", new_name)?;
                                if let Some(cnt) = slots
                                    .entry(trigger_zone.objective_name.clone())
                                    .or_insert_with(|| {
                                        let mut tbl = HashMap::default();
                                        if let Some(t) = self.payload.get(&side) {
                                            for k in t.keys() {
                                                tbl.insert(k.clone(), 0);
                                            }
                                        }
                                        tbl
                                    })
                                    .get_mut(&unit_type)
                                {
                                    *cnt += 1;
                                }
                                break;
                            }
                            if !found {
                                found = true;
                                let tbl = slots.entry(obj_name.clone()).or_insert_with(|| {
                                    let mut tbl = HashMap::default();
                                    if let Some(t) = self.payload.get(&side) {
                                        for k in t.keys() {
                                            tbl.insert(k.clone(), 0);
                                        }
                                    }
                                    tbl
                                });
                                let n = tbl.entry(unit_type.clone()).or_insert(0);
                                *n += 1;
                                let count = *n;
                                let new_name = String::from(format_compact!(
                                    "{obj_name} {unit_type} {count}{stn_string}"
                                ));
                                unit.set("name", new_name.clone())?;
                                group.set("name", new_name)?;
                            }
                        }
                        if strip_internal_fuel_plane {
                            strip_client_plane_internal_fuel_lua(&unit)?;
                        }
                        if !found {
                            bail!(
                                "unit {} is not associated with an objective",
                                value_to_json(&Value::Table(unit.clone()))
                            )
                        }
                    }
                }
            }
        }
        for (unit_type, amount) in replace_count {
            info!("patched {amount} client slot(s) for {unit_type}");
        }
        for (obj, slots) in slots {
            info!("objective {obj} slots:");
            let mut slots = Vec::from_iter(slots);
            slots.sort_by(|(_, c0), (_, c1)| c0.cmp(c1));
            for (typ, cnt) in slots {
                info!("    {typ}: {cnt}")
            }
        }
        Ok(())
    }
}

/// Copy built production BINVENTORY/RINVENTORY row (weapons, aircrafts, liquids, equipment).
fn overwrite_production_inventory_row_from_source(
    lua: &Lua,
    dst_row: &Table,
    src_row: &Table,
    row_label: &str,
) -> Result<()> {
    let dynamic_spawn = dst_row.raw_get::<_, Value>("dynamicSpawn").ok();
    let dynamic_cargo = dst_row.raw_get::<_, Value>("dynamicCargo").ok();
    for key in ["weapons", "aircrafts", "equipment"] {
        match src_row.raw_get::<_, Table>(key) {
            Ok(src_tbl) => dst_row.raw_set(key, src_tbl.deep_clone(lua)?)?,
            Err(_) => dst_row.raw_set(key, lua.create_table()?)?,
        }
    }
    for key in ["jet_fuel", "gasoline", "diesel", "methanol_mixture"] {
        match src_row.raw_get::<_, Table>(key) {
            Ok(src_tbl) => dst_row.raw_set(key, src_tbl.deep_clone(lua)?)?,
            Err(_) => {
                let _ = dst_row.raw_remove(key);
            }
        }
    }
    if let Some(v) = dynamic_spawn {
        if !v.is_nil() {
            dst_row.raw_set("dynamicSpawn", v)?;
        }
    }
    if let Some(v) = dynamic_cargo {
        if !v.is_nil() {
            dst_row.raw_set("dynamicCargo", v)?;
        }
    }
    info!(
        "{row_label}: wrote built production inventory (weapons, aircrafts, liquids, equipment)"
    );
    Ok(())
}

/// After all warehouse passes: refresh assembled mission B/RINVENTORY from bftools-built production rows.
fn mirror_assembled_production_inventory(
    lua: &Lua,
    base: &LoadedMiz,
    cfg: &MizCmd,
    built_blue: &Table,
    built_red: &Table,
) -> Result<()> {
    let (blue_id, red_id) =
        production_inventory_unit_ids(base, cfg).context("production inventory unitIds")?;
    let warehouses = base
        .warehouses
        .raw_get::<_, Table>("warehouses")
        .context("warehouses table")?;
    let dst_blue = warehouses
        .raw_get::<_, Table>(blue_id)
        .with_context(|| format_compact!("mission warehouse row {}", cfg.blue_production_template))?;
    let dst_red = warehouses
        .raw_get::<_, Table>(red_id)
        .with_context(|| format_compact!("mission warehouse row {}", cfg.red_production_template))?;
    overwrite_production_inventory_row_from_source(
        lua,
        &dst_blue,
        built_blue,
        cfg.blue_production_template.as_str(),
    )?;
    overwrite_production_inventory_row_from_source(
        lua,
        &dst_red,
        built_red,
        cfg.red_production_template.as_str(),
    )?;
    Ok(())
}

const LIQUID_STOCK_KEYS: [&str; 4] = ["jet_fuel", "gasoline", "diesel", "methanol_mixture"];

fn ordnance_ws_type(quad: [i32; 4]) -> bool {
    quad[0] == 4 && (4..=8).contains(&quad[1])
}

#[derive(Default)]
struct InventoryProductionMaps {
    weapon_by_ws: HashMap<[i32; 4], u32>,
    aircraft: HashMap<StdString, u32>,
    liquids: HashMap<StdString, u32>,
}

fn read_liquid_amount(tbl: &Table) -> u32 {
    if let Ok(v) = tbl.raw_get::<_, f64>("InitFuel") {
        return v.max(0.) as u32;
    }
    if let Ok(v) = tbl.raw_get::<_, i64>("InitFuel") {
        return v.max(0) as u32;
    }
    0
}

fn build_inventory_production_maps(inv_tpl: &Table) -> Result<InventoryProductionMaps> {
    let mut out = InventoryProductionMaps::default();
    if let Ok(weapons) = inv_tpl.raw_get::<_, Table>("weapons") {
        for pair in weapons.clone().pairs::<Value, Table>() {
            let (_, w) = pair?;
            let Some(ws) = read_weapon_ws_type(&w) else {
                continue;
            };
            let amt = w.raw_get::<_, u32>("initialAmount").unwrap_or(0);
            if amt > 0 {
                out.weapon_by_ws.insert(ws, amt);
            }
        }
    }
    if let Ok(aircrafts) = inv_tpl.raw_get::<_, Table>("aircrafts") {
        for cat in ["helicopters", "planes"] {
            let Ok(cat_tbl) = aircrafts.raw_get::<_, Table>(cat) else {
                continue;
            };
            for pair in cat_tbl.clone().pairs::<String, Table>() {
                let (unit_type, row) = pair?;
                let amt = row.raw_get::<_, u32>("initialAmount").unwrap_or(0);
                if amt > 0 {
                    out.aircraft.insert(unit_type.as_str().to_string(), amt);
                }
            }
        }
    }
    for key in LIQUID_STOCK_KEYS {
        let Ok(tbl) = inv_tpl.raw_get::<_, Table>(key) else {
            continue;
        };
        let amt = read_liquid_amount(&tbl);
        if amt > 0 {
            out.liquids.insert(key.to_string(), amt);
        }
    }
    Ok(out)
}

fn coalition_catalog_weapon_ws(row: &Table) -> Result<HashSet<[i32; 4]>> {
    let mut out = HashSet::default();
    if let Ok(weapons) = row.raw_get::<_, Table>("weapons") {
        for pair in weapons.clone().pairs::<Value, Table>() {
            let (_, w) = pair?;
            if let Some(ws) = read_weapon_ws_type(&w) {
                let amt = w.raw_get::<_, u32>("initialAmount").unwrap_or(0);
                if amt > 0 {
                    out.insert(ws);
                }
            }
        }
    }
    Ok(out)
}

/// Production inventory plus hub default row (KMGU etc. live on B/RDEFAULT, not B/RINVENTORY).
fn coalition_stock_export_weapon_catalog(
    built_inventory: &Table,
    hub_default: Option<&Table<'static>>,
) -> Result<HashSet<[i32; 4]>> {
    let mut out = coalition_catalog_weapon_ws(built_inventory)?;
    if let Some(def) = hub_default {
        out.extend(coalition_catalog_weapon_ws(def)?);
    }
    Ok(out)
}

fn copy_weapon_row_display_fields(dst: &Table, src: &Table) {
    for key in ["name", "desc", "displayName", "Name"] {
        if let Ok(s) = dst.raw_get::<_, String>(key) {
            if !s.as_str().trim().is_empty() && !s.eq_ignore_ascii_case("nil") {
                continue;
            }
        }
        if let Ok(s) = src.raw_get::<_, String>(key) {
            let t = s.as_str().trim();
            if !t.is_empty() && !t.eq_ignore_ascii_case("nil") {
                let _ = dst.raw_set(key, s);
            }
        }
    }
}

/// After carrier prune: copy B/RDEFAULT (+ optional B/RDEFAULT+ FARP) ordnance on the TTDN allowlist.
fn ensure_positive_default_ordnance_for_allowed_ws(
    lua: &Lua,
    wh: &Table,
    def_sources: &[&Table<'static>],
    allowed_ws: &HashSet<[i32; 4]>,
    mult: u32,
) -> Result<()> {
    for def_tpl in def_sources {
        let Ok(weapons) = def_tpl.raw_get::<_, Table>("weapons") else {
            continue;
        };
        for pair in weapons.clone().pairs::<Value, Table>() {
            let (_, w) = pair?;
            let Some(ws) = read_weapon_ws_type(&w) else {
                continue;
            };
            if !allowed_ws.contains(&ws) {
                continue;
            }
            let amt = w.raw_get::<_, u32>("initialAmount").unwrap_or(0);
            if amt == 0 {
                continue;
            }
            apply_zone_ws_weapon_stock(lua, wh, ws, amt, mult)?;
            if let Ok(dst_weapons) = wh.raw_get::<_, Table>("weapons") {
                for dst_pair in dst_weapons.clone().pairs::<Value, Table>() {
                    let (_, dst_w) = dst_pair?;
                    if read_weapon_ws_type(&dst_w) == Some(ws) {
                        copy_weapon_row_display_fields(&dst_w, &w);
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

fn objective_weapon_allowset(
    defaults: &HashMap<StdString, ObjectiveWarehouseDefaults>,
    objective_name: &str,
    side: Side,
) -> Option<HashSet<[i32; 4]>> {
    let d = defaults.get(objective_name)?;
    let list = match side {
        Side::Blue => &d.blue_weapon_ws,
        Side::Red => &d.red_weapon_ws,
        Side::Neutral => return None,
    };
    if list.is_empty() {
        return None;
    }
    Some(list.iter().copied().collect())
}

fn weapon_allowed_for_objective_stock(
    ws: [i32; 4],
    catalog: &HashSet<[i32; 4]>,
    allow: Option<&HashSet<[i32; 4]>>,
) -> bool {
    if !catalog.contains(&ws) {
        return false;
    }
    let Some(allow) = allow else {
        return true;
    };
    if !ordnance_ws_type(ws) {
        return true;
    }
    allow.contains(&ws)
}

fn extract_objective_coalition_stock(
    row: &Table,
    side: Side,
    inv_tpl: &Table<'static>,
    catalog: &HashSet<[i32; 4]>,
    defaults: &HashMap<StdString, ObjectiveWarehouseDefaults>,
    objective_name: &str,
) -> Result<ObjectiveCoalitionStock> {
    let prod = build_inventory_production_maps(inv_tpl)?;
    let allow = objective_weapon_allowset(defaults, objective_name, side);
    let mut out = ObjectiveCoalitionStock::default();
    if let Ok(weapons) = row.raw_get::<_, Table>("weapons") {
        for pair in weapons.clone().pairs::<Value, Table>() {
            let (_, w) = pair?;
            let Some(ws) = read_weapon_ws_type(&w) else {
                continue;
            };
            if !weapon_allowed_for_objective_stock(ws, catalog, allow.as_ref()) {
                continue;
            }
            let baseline = w.raw_get::<_, u32>("initialAmount").unwrap_or(0);
            if baseline == 0 {
                continue;
            }
            let Some(name) = weapon_row_export_key(&w) else {
                continue;
            };
            out.equipment.insert(
                name,
                ObjectiveStockItem {
                    baseline,
                    ws_type: ordnance_ws_type(ws).then_some(ws),
                    production: prod.weapon_by_ws.get(&ws).copied().unwrap_or(0),
                },
            );
        }
    }
    if let Ok(aircrafts) = row.raw_get::<_, Table>("aircrafts") {
        for cat in ["helicopters", "planes"] {
            let Ok(cat_tbl) = aircrafts.raw_get::<_, Table>(cat) else {
                continue;
            };
            for pair in cat_tbl.clone().pairs::<String, Table>() {
                let (unit_type, u) = pair?;
                let baseline = u.raw_get::<_, u32>("initialAmount").unwrap_or(0);
                if baseline == 0 {
                    continue;
                }
                out.equipment.insert(
                    unit_type.as_str().to_string(),
                    ObjectiveStockItem {
                        baseline,
                        ws_type: None,
                        production: prod
                            .aircraft
                            .get(unit_type.as_str())
                            .copied()
                            .unwrap_or(0),
                    },
                );
            }
        }
    }
    for key in LIQUID_STOCK_KEYS {
        let Ok(tbl) = row.raw_get::<_, Table>(key) else {
            continue;
        };
        let baseline = read_liquid_amount(&tbl);
        if baseline == 0 {
            continue;
        }
        out.liquids.insert(
            key.to_string(),
            ObjectiveStockLiquid {
                baseline,
                production: prod.liquids.get(key).copied().unwrap_or(0),
            },
        );
    }
    Ok(out)
}

struct ResolvedObjectiveWarehouse<'a> {
    row: Table<'a>,
    wh_id: i64,
    is_airport: bool,
}

fn resolve_airport_warehouse_by_zone_geometry<'a>(
    base: &'a LoadedMiz,
    allow: &ObjectiveDynAllow,
) -> Result<Option<ResolvedObjectiveWarehouse<'a>>> {
    let airports = base
        .warehouses
        .raw_get::<_, Table>("airports")
        .context("airports for objective stock geometry")?;
    let mut airport_wids = HashSet::default();
    for pair in airports.clone().pairs::<Value, Table>() {
        let (k, _) = pair?;
        if let Some(id) = warehouse_lua_key_i64(k) {
            airport_wids.insert(id);
        }
    }
    if airport_wids.is_empty() {
        return Ok(None);
    }
    let positions = collect_airport_positions_from_groups(base, &airport_wids)
        .context("airport positions for objective stock geometry")?;
    for (id, pos) in positions {
        if !allow.contains(pos) {
            continue;
        }
        let row = airports
            .raw_get::<_, Table>(id)
            .with_context(|| format_compact!("airport warehouse {id} (zone geometry)"))?;
        return Ok(Some(ResolvedObjectiveWarehouse {
            row,
            wh_id: id,
            is_airport: true,
        }));
    }
    Ok(None)
}

fn resolve_objective_warehouse<'a>(
    base: &'a LoadedMiz,
    allow: &ObjectiveDynAllow,
) -> Result<Option<ResolvedObjectiveWarehouse<'a>>> {
    if let Some(id) = allow.airbase_id {
        let airports = base
            .warehouses
            .raw_get::<_, Table>("airports")
            .context("airports for objective stock")?;
        match airports
            .raw_get::<_, Value>(id)
            .with_context(|| format_compact!("airport warehouse {id}"))?
        {
            Value::Table(row) => {
                return Ok(Some(ResolvedObjectiveWarehouse {
                    row,
                    wh_id: id,
                    is_airport: true,
                }));
            }
            Value::Nil => {
                let warehouses = base
                    .warehouses
                    .raw_get::<_, Table>("warehouses")
                    .context("warehouses for objective stock airbaseID")?;
                if let Ok(row) = warehouses.raw_get::<_, Table>(id) {
                    return Ok(Some(ResolvedObjectiveWarehouse {
                        row,
                        wh_id: id,
                        is_airport: false,
                    }));
                }
                warn!(
                    "objective zone {:?}: airbaseID {id} not in warehouses.airports or warehouses.warehouses; trying zone geometry",
                    allow.zone_name.as_str()
                );
                if let Some(resolved) =
                    resolve_airport_warehouse_by_zone_geometry(base, allow)?
                {
                    return Ok(Some(resolved));
                }
            }
            other => bail!(
                "airport warehouse {id}: expected table or nil, got {:?}",
                other
            ),
        }
    } else if let Some(resolved) = resolve_airport_warehouse_by_zone_geometry(base, allow)? {
        return Ok(Some(resolved));
    }
    let warehouses = base
        .warehouses
        .raw_get::<_, Table>("warehouses")
        .context("warehouses for objective stock")?;
    let mut ids = HashSet::default();
    for pair in warehouses.clone().pairs::<i64, Table>() {
        let (id, _) = pair?;
        ids.insert(id);
    }
    let positions = collect_warehouse_unit_positions(base, &ids)?;
    for (id, pos) in positions {
        if allow.contains(pos) {
            let row = warehouses
                .raw_get::<_, Table>(id)
                .with_context(|| format_compact!("warehouse {id}"))?;
            return Ok(Some(ResolvedObjectiveWarehouse {
                row,
                wh_id: id,
                is_airport: false,
            }));
        }
    }
    Ok(None)
}

fn stock_mult_for_objective(
    mult_cfg: &WarehouseStockMultConfig,
    allow: &ObjectiveDynAllow,
    resolved: Option<(i64, bool)>,
) -> Result<u32> {
    if allow.is_logistics_hub {
        return Ok(mult_cfg.hub_max.max(1));
    }
    if let Some((id, is_airport)) = resolved {
        return Ok(if is_airport {
            mult_cfg.mult_airport(id)
        } else {
            mult_cfg.mult_warehouse_row(id)
        });
    }
    Ok(mult_cfg.airbase_max.max(1))
}

/// Opposite-coalition baseline for export: BINVENTORY catalog × mult (same items Fowl tracks after capture).
fn synthesize_virtual_coalition_stock(
    inv_tpl: &Table<'static>,
    mult: u32,
    catalog: &HashSet<[i32; 4]>,
    defaults: &HashMap<StdString, ObjectiveWarehouseDefaults>,
    objective_name: &str,
    side: Side,
) -> Result<ObjectiveCoalitionStock> {
    let prod = build_inventory_production_maps(inv_tpl)?;
    let allow = objective_weapon_allowset(defaults, objective_name, side);
    let mult = mult.max(1);
    let mut out = ObjectiveCoalitionStock::default();
    if let Ok(weapons) = inv_tpl.raw_get::<_, Table>("weapons") {
        for pair in weapons.clone().pairs::<Value, Table>() {
            let (_, w) = pair?;
            let Some(ws) = read_weapon_ws_type(&w) else {
                continue;
            };
            if !weapon_allowed_for_objective_stock(ws, catalog, allow.as_ref()) {
                continue;
            }
            let production = prod.weapon_by_ws.get(&ws).copied().unwrap_or(0);
            if production == 0 {
                continue;
            }
            let baseline = production.saturating_mul(mult);
            let Some(name) = weapon_row_export_key(&w) else {
                continue;
            };
            out.equipment.insert(
                name,
                ObjectiveStockItem {
                    baseline,
                    ws_type: ordnance_ws_type(ws).then_some(ws),
                    production,
                },
            );
        }
    }
    if let Ok(aircrafts) = inv_tpl.raw_get::<_, Table>("aircrafts") {
        for cat in ["helicopters", "planes"] {
            let Ok(cat_tbl) = aircrafts.raw_get::<_, Table>(cat) else {
                continue;
            };
            for pair in cat_tbl.clone().pairs::<String, Table>() {
                let (unit_type, row) = pair?;
                let production = row.raw_get::<_, u32>("initialAmount").unwrap_or(0);
                if production == 0 {
                    continue;
                }
                let baseline = production.saturating_mul(mult);
                out.equipment.insert(
                    unit_type.as_str().to_string(),
                    ObjectiveStockItem {
                        baseline,
                        ws_type: None,
                        production,
                    },
                );
            }
        }
    }
    for key in LIQUID_STOCK_KEYS {
        let Ok(tbl) = inv_tpl.raw_get::<_, Table>(key) else {
            continue;
        };
        let production = read_liquid_amount(&tbl);
        if production == 0 {
            continue;
        }
        let baseline = production.saturating_mul(mult);
        out.liquids.insert(
            key.to_string(),
            ObjectiveStockLiquid {
                baseline,
                production,
            },
        );
    }
    Ok(out)
}

fn compute_virtual_objective_coalition_stock(
    inv_tpl: &Table<'static>,
    mult: u32,
    catalog: &HashSet<[i32; 4]>,
    defaults: &HashMap<StdString, ObjectiveWarehouseDefaults>,
    objective_name: &str,
    side: Side,
) -> Result<ObjectiveCoalitionStock> {
    synthesize_virtual_coalition_stock(
        inv_tpl,
        mult,
        catalog,
        defaults,
        objective_name,
        side,
    )
}

/// After mission fill: `.miz` row for ME coalition + opposite coalition virtual profile (export only).
pub fn build_objective_stock_export(
    _lua: &Lua,
    base: &LoadedMiz,
    obj_dyn_allow: &[ObjectiveDynAllow],
    mult_cfg: &WarehouseStockMultConfig,
    tpl: &WarehouseTemplate,
    built_blue: &Table,
    built_red: &Table,
    objective_defaults: &HashMap<StdString, ObjectiveWarehouseDefaults>,
) -> Result<HashMap<StdString, ObjectiveStockByCoalition>> {
    let blue_catalog = coalition_catalog_weapon_ws(built_blue)?;
    let red_catalog = coalition_catalog_weapon_ws(built_red)?;
    let mut out: HashMap<StdString, ObjectiveStockByCoalition> = HashMap::default();
    let mut seen: HashSet<StdString> = HashSet::default();
    for allow in obj_dyn_allow {
        let Some(objective_name) = allow.zone_name.as_str().get(4..) else {
            continue;
        };
        if !seen.insert(objective_name.to_string()) {
            continue;
        }
        let mut entry = ObjectiveStockByCoalition::default();
        let resolved_meta = resolve_objective_warehouse(base, allow)?;
        let mult = stock_mult_for_objective(
            mult_cfg,
            allow,
            resolved_meta
                .as_ref()
                .map(|r| (r.wh_id, r.is_airport)),
        )?;
        if let Some(resolved) = resolved_meta {
            let side = warehouse_side_for_default_apply(&resolved.row)?
                .unwrap_or(allow.side);
            let inv_tpl = match side {
                Side::Blue => &tpl.blue_inventory,
                Side::Red => &tpl.red_inventory,
                Side::Neutral => {
                    out.insert(objective_name.to_string(), entry);
                    continue;
                }
            };
            let catalog = match side {
                Side::Blue => &blue_catalog,
                Side::Red => &red_catalog,
                Side::Neutral => unreachable!(),
            };
            let stock = extract_objective_coalition_stock(
                &resolved.row,
                side,
                inv_tpl,
                catalog,
                objective_defaults,
                objective_name,
            )?;
            match side {
                Side::Blue => entry.blue = stock,
                Side::Red => entry.red = stock,
                Side::Neutral => {}
            }
            let opposite = side.opposite();
            let (opp_inv, opp_cat) = match opposite {
                Side::Blue => (&tpl.blue_inventory, &blue_catalog),
                Side::Red => (&tpl.red_inventory, &red_catalog),
                Side::Neutral => {
                    out.insert(objective_name.to_string(), entry);
                    continue;
                }
            };
            let virtual_stock = compute_virtual_objective_coalition_stock(
                opp_inv,
                mult,
                opp_cat,
                objective_defaults,
                objective_name,
                opposite,
            )?;
            match opposite {
                Side::Blue => entry.blue = virtual_stock,
                Side::Red => entry.red = virtual_stock,
                Side::Neutral => {}
            }
        } else {
            for side in [Side::Blue, Side::Red] {
                let (inv_tpl, catalog) = match side {
                    Side::Blue => (&tpl.blue_inventory, &blue_catalog),
                    Side::Red => (&tpl.red_inventory, &red_catalog),
                    Side::Neutral => continue,
                };
                let stock = compute_virtual_objective_coalition_stock(
                    inv_tpl,
                    mult,
                    catalog,
                    objective_defaults,
                    objective_name,
                    side,
                )?;
                match side {
                    Side::Blue => entry.blue = stock,
                    Side::Red => entry.red = stock,
                    Side::Neutral => {}
                }
            }
        }
        out.insert(objective_name.to_string(), entry);
    }
    info!(
        "fowl export objective_stock: {} objective(s) with blue/red logical profiles",
        out.len()
    );
    Ok(out)
}

/// `objective_stock` keys for ME naval carriers (`BForrestal` → `Forrestal`), aligned with bflib mobile FARP names.
fn merge_naval_ship_objective_stock_export(
    out: &mut HashMap<StdString, ObjectiveStockByCoalition>,
    base: &LoadedMiz,
    ship_wh_map: &HashMap<i64, (Side, String)>,
    mult_cfg: &WarehouseStockMultConfig,
    tpl: &WarehouseTemplate,
    built_blue: &Table,
    built_red: &Table,
    objective_defaults: &HashMap<StdString, ObjectiveWarehouseDefaults>,
) -> Result<()> {
    if ship_wh_map.is_empty() {
        return Ok(());
    }
    let warehouses = base
        .warehouses
        .raw_get::<_, Table>("warehouses")
        .context("warehouses for naval objective_stock export")?;
    let blue_catalog =
        coalition_stock_export_weapon_catalog(built_blue, Some(&tpl.blue_default))?;
    let red_catalog =
        coalition_stock_export_weapon_catalog(built_red, Some(&tpl.red_default))?;
    let mut added = 0usize;
    for (&wid, (side, group_name)) in ship_wh_map {
        let display_key = StdString::from(ship_pad_display_name(group_name.as_str()));
        if out.contains_key(&display_key) {
            warn!(
                "objective_stock: naval ship group {:?} key {:?} already present (O* zone?), skipping",
                group_name,
                display_key
            );
            continue;
        }
        let row = warehouses
            .raw_get::<_, Table>(wid)
            .with_context(|| format_compact!("naval warehouse row for ship {group_name}"))?;
        let mult = mult_cfg.mult_warehouse_row(wid);
        let (inv_tpl, catalog) = match side {
            Side::Blue => (&tpl.blue_inventory, &blue_catalog),
            Side::Red => (&tpl.red_inventory, &red_catalog),
            Side::Neutral => continue,
        };
        let stock = extract_objective_coalition_stock(
            &row,
            *side,
            inv_tpl,
            catalog,
            objective_defaults,
            display_key.as_str(),
        )?;
        let mut entry = ObjectiveStockByCoalition::default();
        match side {
            Side::Blue => entry.blue = stock,
            Side::Red => entry.red = stock,
            Side::Neutral => {}
        }
        let opposite = side.opposite();
        let (opp_inv, opp_cat) = match opposite {
            Side::Blue => (&tpl.blue_inventory, &blue_catalog),
            Side::Red => (&tpl.red_inventory, &red_catalog),
            Side::Neutral => {
                out.insert(display_key, entry);
                added += 1;
                continue;
            }
        };
        let virtual_stock = compute_virtual_objective_coalition_stock(
            opp_inv,
            mult,
            opp_cat,
            objective_defaults,
            display_key.as_str(),
            opposite,
        )?;
        match opposite {
            Side::Blue => entry.blue = virtual_stock,
            Side::Red => entry.red = virtual_stock,
            Side::Neutral => {}
        }
        out.insert(display_key, entry);
        added += 1;
    }
    if added > 0 {
        info!(
            "objective_stock: merged {added} naval ship pad profile(s) (e.g. Forrestal, Kuznecow)"
        );
    }
    Ok(())
}

/// DEP FARP pad `unitId` → coalition and ME pad group name (`DEPBFARPPAD0`, …); keys match bflib `pad_template`.
fn collect_dep_farp_warehouse_group_map(
    base: &LoadedMiz,
    dep_ids: &HashSet<i64>,
) -> Result<HashMap<i64, (Side, String)>> {
    if dep_ids.is_empty() {
        return Ok(HashMap::default());
    }
    let mut map = HashMap::default();
    for side in Side::ALL {
        let coa = base.mission.coalition(side)?;
        for country in coa.countries()? {
            let country = country?;
            for group in vehicle(&country, "static")?
                .chain(vehicle(&country, "plane")?)
                .chain(vehicle(&country, "helicopter")?)
            {
                let group = group?;
                let group_name: String = group.raw_get("name")?;
                for unit in group.raw_get::<_, Table>("units")?.pairs::<Value, Table>() {
                    let unit = unit?.1;
                    let id: i64 = unit.raw_get("unitId")?;
                    if dep_ids.contains(&id) {
                        map.insert(id, (side, group_name.clone()));
                    }
                }
            }
        }
    }
    if !map.is_empty() {
        info!(
            "DEP FARP export: {} pad warehouse id(s) mapped to ME group names",
            map.len()
        );
    }
    Ok(map)
}

/// `objective_stock` for deployable FARP template pads (bflib looks up by `pad_template`, e.g. `DEPBFARPPAD0`).
fn merge_dep_farp_objective_stock_export(
    out: &mut HashMap<StdString, ObjectiveStockByCoalition>,
    base: &LoadedMiz,
    dep_wh_map: &HashMap<i64, (Side, String)>,
    mult_cfg: &WarehouseStockMultConfig,
    tpl: &WarehouseTemplate,
    built_blue: &Table,
    built_red: &Table,
    objective_defaults: &HashMap<StdString, ObjectiveWarehouseDefaults>,
) -> Result<()> {
    if dep_wh_map.is_empty() {
        return Ok(());
    }
    let warehouses = base
        .warehouses
        .raw_get::<_, Table>("warehouses")
        .context("warehouses for DEP FARP objective_stock export")?;
    let blue_catalog =
        coalition_stock_export_weapon_catalog(built_blue, Some(&tpl.blue_default))?;
    let red_catalog =
        coalition_stock_export_weapon_catalog(built_red, Some(&tpl.red_default))?;
    let mut added = 0usize;
    for (&wid, (side, group_name)) in dep_wh_map {
        let key = StdString::from(group_name.as_str());
        if out.contains_key(&key) {
            warn!(
                "objective_stock: DEP FARP pad {:?} key {:?} already present, skipping",
                group_name,
                key
            );
            continue;
        }
        let row = warehouses
            .raw_get::<_, Table>(wid)
            .with_context(|| format_compact!("DEP FARP warehouse row for pad {group_name}"))?;
        let mult = mult_cfg.mult_warehouse_row(wid);
        let (inv_tpl, catalog) = match side {
            Side::Blue => (&tpl.blue_inventory, &blue_catalog),
            Side::Red => (&tpl.red_inventory, &red_catalog),
            Side::Neutral => continue,
        };
        let stock = extract_objective_coalition_stock(
            &row,
            *side,
            inv_tpl,
            catalog,
            objective_defaults,
            key.as_str(),
        )?;
        let mut entry = ObjectiveStockByCoalition::default();
        match side {
            Side::Blue => entry.blue = stock,
            Side::Red => entry.red = stock,
            Side::Neutral => {}
        }
        let opposite = side.opposite();
        let (opp_inv, opp_cat) = match opposite {
            Side::Blue => (&tpl.blue_inventory, &blue_catalog),
            Side::Red => (&tpl.red_inventory, &red_catalog),
            Side::Neutral => {
                out.insert(key, entry);
                added += 1;
                continue;
            }
        };
        let virtual_stock = compute_virtual_objective_coalition_stock(
            opp_inv,
            mult,
            opp_cat,
            objective_defaults,
            key.as_str(),
            opposite,
        )?;
        match opposite {
            Side::Blue => entry.blue = virtual_stock,
            Side::Red => entry.red = virtual_stock,
            Side::Neutral => {}
        }
        out.insert(key, entry);
        added += 1;
    }
    if added > 0 {
        info!(
            "objective_stock: merged {added} DEP FARP pad profile(s) (e.g. DEPBFARPPAD0)"
        );
    }
    Ok(())
}

const SETTINGS_AI_ZONE_PREFIX: &str = "SETTINGS-Ai-";

#[derive(Debug, Clone)]
struct AiTemplateStockSpec {
    template_name: StdString,
    airframe: StdString,
    objectives: HashMap<StdString, u32>,
}

fn objective_export_name_from_zone(zone_name: &str) -> Option<&str> {
    zone_name.get(4..)
}

fn resolve_ai_stock_objective<'a>(
    key: &str,
    obj_dyn_allow: &'a [ObjectiveDynAllow],
) -> Option<(&'a ObjectiveDynAllow, StdString)> {
    if let Some(o) = obj_dyn_allow
        .iter()
        .find(|o| o.zone_name.as_str().eq_ignore_ascii_case(key))
    {
        let name = objective_export_name_from_zone(o.zone_name.as_str())?;
        return Some((o, StdString::from(name)));
    }
    for o in obj_dyn_allow {
        if objective_export_name_from_zone(o.zone_name.as_str())
            .is_some_and(|n| n.eq_ignore_ascii_case(key))
        {
            return Some((
                o,
                StdString::from(objective_export_name_from_zone(o.zone_name.as_str())?),
            ));
        }
    }
    if key.len() >= 2 {
        let side = match key.chars().next()? {
            'R' | 'r' => Side::Red,
            'B' | 'b' => Side::Blue,
            _ => return None,
        };
        let rest = &key[1..];
        for o in obj_dyn_allow {
            if o.side != side {
                continue;
            }
            if objective_export_name_from_zone(o.zone_name.as_str())
                .is_some_and(|n| n.eq_ignore_ascii_case(rest))
            {
                return Some((
                    o,
                    StdString::from(objective_export_name_from_zone(o.zone_name.as_str())?),
                ));
            }
        }
    }
    None
}

fn load_ai_template_stock_settings(base: &LoadedMiz) -> Result<Vec<AiTemplateStockSpec>> {
    let mut out = Vec::new();
    for zone in base.mission.triggers()? {
        let zone = zone?;
        let name = zone.name()?;
        let Some(template_name) = name.as_str().strip_prefix(SETTINGS_AI_ZONE_PREFIX) else {
            continue;
        };
        if template_name.is_empty() {
            warn!("SETTINGS-Ai zone with empty template suffix, skipping");
            continue;
        }
        let mut airframe: Option<StdString> = None;
        let mut objectives = HashMap::new();
        for prop in zone.properties()? {
            let prop = prop?;
            let key = prop.key.as_str();
            if key.eq_ignore_ascii_case("airframe") || key.eq_ignore_ascii_case("aiframe") {
                airframe = Some(StdString::from(prop.value.as_str()));
            } else {
                match prop.value.trim().parse::<u32>() {
                    Ok(n) => {
                        objectives.insert(StdString::from(key), n);
                    }
                    Err(_) => warn!(
                        "SETTINGS-Ai-{template_name}: ignoring non-integer property {key}={}",
                        prop.value.as_ref()
                    ),
                }
            }
        }
        let Some(airframe) = airframe else {
            warn!("SETTINGS-Ai-{template_name}: missing airframe/aiframe property, skipping");
            continue;
        };
        out.push(AiTemplateStockSpec {
            template_name: StdString::from(template_name),
            airframe,
            objectives,
        });
    }
    if !out.is_empty() {
        info!("SETTINGS-Ai: loaded {} AI template stock zone(s)", out.len());
    }
    Ok(out)
}

fn warehouse_aircraft_category(wh: &Table<'_>, airframe: &str) -> &'static str {
    if let Ok(aircrafts) = wh.raw_get::<_, Table>("aircrafts") {
        if let Ok(helicopters) = aircrafts.raw_get::<_, Table>("helicopters") {
            if helicopters.raw_get::<_, Table>(airframe).is_ok() {
                return "helicopters";
            }
        }
    }
    "planes"
}

fn resolve_ai_stock_naval_warehouse<'a>(
    base: &'a LoadedMiz,
    ship_wh_map: &HashMap<i64, (Side, String)>,
    key: &str,
) -> Result<Option<(Table<'a>, Side, i64, StdString)>> {
    let Some((side, export_name)) = resolve_ai_stock_naval_target(ship_wh_map, key) else {
        return Ok(None);
    };
    let warehouses = base
        .warehouses
        .raw_get::<_, Table>("warehouses")
        .context("warehouses for SETTINGS-Ai naval stock")?;
    for (&wid, (wh_side, group_name)) in ship_wh_map {
        if *wh_side != side {
            continue;
        }
        if StdString::from(ship_pad_display_name(group_name.as_str()).as_str()) == export_name {
            let row = warehouses
                .raw_get(wid)
                .with_context(|| format_compact!("naval warehouse {wid} ({group_name})"))?;
            return Ok(Some((row, side, wid, export_name)));
        }
    }
    Ok(None)
}

fn ensure_ai_airframe_stock(
    lua: &Lua,
    wh: &Table<'_>,
    airframe: &str,
    side: Side,
    amount: u32,
    wid: Option<i64>,
    emit: &DynamicSpawnEmit,
) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    let aircrafts: Table = match wh.raw_get("aircrafts") {
        Ok(t) => t,
        Err(_) => {
            let t = lua.create_table()?;
            wh.raw_set("aircrafts", t.clone())?;
            t
        }
    };
    let cat = warehouse_aircraft_category(wh, airframe);
    let cat_tbl: Table = match aircrafts.raw_get(cat) {
        Ok(t) => t,
        Err(_) => {
            let t = lua.create_table()?;
            aircrafts.raw_set(cat, t.clone())?;
            t
        }
    };
    let row: Table = match cat_tbl.raw_get(airframe) {
        Ok(t) => t,
        Err(_) => {
            let t = lua.create_table()?;
            t.raw_set("initialAmount", 0u32)?;
            cat_tbl.raw_set(airframe, t.clone())?;
            t
        }
    };
    let cur: u32 = row.raw_get("initialAmount").unwrap_or(0);
    row.raw_set("initialAmount", cur.saturating_add(amount))?;
    let mut link = emit
        .link_by_side_type
        .get(&(side, String::from(airframe)))
        .map(|g| g.inner())
        .unwrap_or(0);
    if link == 0 {
        if let Some(wid) = wid {
            if let Some(hull) = emit.ship_hull_by_wid.get(&wid) {
                link = emit
                    .link_by_ship
                    .get(&(side, String::from(airframe), hull.clone()))
                    .map(|g| g.inner())
                    .unwrap_or(0);
            }
        }
    }
    if link != 0 {
        row.raw_set("linkDynTempl", link)?;
    }
    Ok(())
}

fn apply_settings_ai_template_stock(
    lua: &Lua,
    base: &LoadedMiz,
    specs: &[AiTemplateStockSpec],
    obj_dyn_allow: &[ObjectiveDynAllow],
    ship_wh_map: &HashMap<i64, (Side, String)>,
    emit: &DynamicSpawnEmit,
) -> Result<()> {
    for spec in specs {
        for (obj_key, amount) in &spec.objectives {
            if let Some((allow, export_name)) =
                resolve_ai_stock_objective(obj_key.as_str(), obj_dyn_allow)
            {
                let Some(resolved) = resolve_objective_warehouse(base, allow)? else {
                    warn!(
                        "SETTINGS-Ai-{}: no warehouse for objective {:?}",
                        spec.template_name.as_str(),
                        obj_key.as_str()
                    );
                    continue;
                };
                ensure_ai_airframe_stock(
                    lua,
                    &resolved.row,
                    spec.airframe.as_str(),
                    allow.side,
                    *amount,
                    Some(resolved.wh_id),
                    emit,
                )?;
                info!(
                    "SETTINGS-Ai-{}: {} +{} {} at warehouse {}",
                    spec.template_name.as_str(),
                    export_name.as_str(),
                    amount,
                    spec.airframe.as_str(),
                    resolved.wh_id
                );
                continue;
            }
            if let Some((row, side, wid, export_name)) =
                resolve_ai_stock_naval_warehouse(base, ship_wh_map, obj_key.as_str())?
            {
                ensure_ai_airframe_stock(
                    lua,
                    &row,
                    spec.airframe.as_str(),
                    side,
                    *amount,
                    Some(wid),
                    emit,
                )?;
                info!(
                    "SETTINGS-Ai-{}: {} +{} {} at naval warehouse {}",
                    spec.template_name.as_str(),
                    export_name.as_str(),
                    amount,
                    spec.airframe.as_str(),
                    wid
                );
                continue;
            }
            warn!(
                "SETTINGS-Ai-{}: unknown objective or ship key {:?}",
                spec.template_name.as_str(),
                obj_key.as_str()
            );
        }
    }
    Ok(())
}

fn resolve_ai_stock_naval_target(
    ship_wh_map: &HashMap<i64, (Side, String)>,
    key: &str,
) -> Option<(Side, StdString)> {
    for (_, (side, group_name)) in ship_wh_map {
        let display = ship_pad_display_name(group_name.as_str());
        if group_name.eq_ignore_ascii_case(key)
            || display.eq_ignore_ascii_case(key)
            || (key.len() >= 2
                && matches!(key.chars().next(), Some('B') | Some('b') | Some('R') | Some('r'))
                && group_name.eq_ignore_ascii_case(&key[1..]))
        {
            return Some((*side, StdString::from(display.as_str())));
        }
    }
    None
}

fn merge_ai_template_stock_export(
    out: &mut HashMap<StdString, ObjectiveStockByCoalition>,
    ai_template_airframes: &mut HashMap<std::string::String, std::string::String>,
    specs: &[AiTemplateStockSpec],
    obj_dyn_allow: &[ObjectiveDynAllow],
    ship_wh_map: &HashMap<i64, (Side, String)>,
) {
    for spec in specs {
        ai_template_airframes.insert(
            spec.template_name.as_str().into(),
            spec.airframe.as_str().into(),
        );
        for (obj_key, amount) in &spec.objectives {
            if *amount == 0 {
                continue;
            }
            let Some((side, export_name)) = resolve_ai_stock_objective(obj_key.as_str(), obj_dyn_allow)
                .map(|(allow, name)| (allow.side, name))
                .or_else(|| resolve_ai_stock_naval_target(ship_wh_map, obj_key.as_str()))
            else {
                warn!(
                    "SETTINGS-Ai-{}: export skip unknown objective {:?}",
                    spec.template_name.as_str(),
                    obj_key.as_str()
                );
                continue;
            };
            let entry = out.entry(export_name).or_default();
            let stock = match side {
                Side::Blue => &mut entry.blue,
                Side::Red => &mut entry.red,
                Side::Neutral => continue,
            };
            stock
                .equipment
                .entry(spec.airframe.as_str().into())
                .and_modify(|i| i.baseline = i.baseline.saturating_add(*amount))
                .or_insert(ObjectiveStockItem {
                    baseline: *amount,
                    ws_type: None,
                    production: 0,
                });
        }
    }
}

struct WarehouseTemplate {
    blue_inventory: Table<'static>,
    red_inventory: Table<'static>,
    /// Optional Invisible FARP warehouse rows: merged into B/RINVENTORY after validation (stock by `wsType`).
    blue_inventory_plus: Option<Table<'static>>,
    red_inventory_plus: Option<Table<'static>>,
    /// Trigger zone `BINVENTORY+` / `RINVENTORY+`: catalog module links + optional wsType rows (`Value`=ALL|FILTER|amount|module).
    zone_plus_blue: Vec<InventoryZonePlusModuleEntry>,
    zone_plus_red: Vec<InventoryZonePlusModuleEntry>,
    zone_ws_inventory_blue: HashMap<[i32; 4], WsZoneStockSpec>,
    zone_ws_inventory_red: HashMap<[i32; 4], WsZoneStockSpec>,
    /// Trigger zone `BDEFAULT+` / `RDEFAULT+`: same shapes as inventory+ zones.
    zone_default_plus_blue: Vec<InventoryZonePlusModuleEntry>,
    zone_default_plus_red: Vec<InventoryZonePlusModuleEntry>,
    zone_ws_default_blue: HashMap<[i32; 4], WsZoneStockSpec>,
    zone_ws_default_red: HashMap<[i32; 4], WsZoneStockSpec>,
    blue_default: Table<'static>,
    red_default: Table<'static>,
    blue_default_plus: Table<'static>,
    red_default_plus: Table<'static>,
    blue_all_fueltanks: Table<'static>,
    red_all_fueltanks: Table<'static>,
    blue_default_fueltanks: Table<'static>,
    red_default_fueltanks: Table<'static>,
}

/// Keeps `LoadedMiz` alive so template warehouse tables stay valid until repacking `warehouse<campaign_decade>.miz`.
struct WarehouseBundle {
    path: PathBuf,
    loaded: LoadedMiz,
    template: WarehouseTemplate,
}

/// Rows zeroed during build (`aircrafts` stock without weapon.miz template for that coalition).
#[derive(Debug, Clone)]
struct InventoryAircraftOrphanSanitized {
    row_name: StdString,
    side: Side,
    unit_type: StdString,
    category: StdString,
    previous_amount: u32,
}

fn zero_inventory_aircraft_rows_missing_weapon_template(
    vt: &VehicleTemplates,
    inv: &Table,
    side: Side,
    row_name: &str,
    out: &mut Vec<InventoryAircraftOrphanSanitized>,
) -> Result<()> {
    let Ok(aircrafts) = inv.raw_get::<_, Table>("aircrafts") else {
        return Ok(());
    };
    for cat in ["helicopters", "planes"] {
        let Ok(cat_tbl) = aircrafts.raw_get::<_, Table>(cat) else {
            continue;
        };
        for pair in cat_tbl.clone().pairs::<String, Table>() {
            let (unit_type, row) = pair?;
            let Ok(amt) = row.raw_get::<_, u32>("initialAmount") else {
                continue;
            };
            if amt == 0 {
                continue;
            }
            if vt.has_airframe_template_for_side(side, unit_type.as_str()) {
                continue;
            }
            row.raw_set("initialAmount", 0u32)?;
            warn!(
                "{}: '{}' [{}] initialAmount={} — no {:?} weapon.miz template; zeroed",
                row_name,
                unit_type,
                cat,
                amt,
                side
            );
            out.push(InventoryAircraftOrphanSanitized {
                row_name: row_name.to_string(),
                side,
                unit_type: unit_type.to_string(),
                category: cat.to_string(),
                previous_amount: amt,
            });
        }
    }
    Ok(())
}

fn sanitize_both_production_inventory_aircraft_templates(
    vt: &VehicleTemplates,
    blue_inv: &Table,
    red_inv: &Table,
) -> Result<Vec<InventoryAircraftOrphanSanitized>> {
    let mut out = Vec::new();
    zero_inventory_aircraft_rows_missing_weapon_template(
        vt,
        blue_inv,
        Side::Blue,
        "BINVENTORY",
        &mut out,
    )?;
    zero_inventory_aircraft_rows_missing_weapon_template(
        vt,
        red_inv,
        Side::Red,
        "RINVENTORY",
        &mut out,
    )?;
    Ok(out)
}

fn pack_warehouse_bundle_to_path(wb: &WarehouseBundle) -> Result<()> {
    let wh_file = wb
        .loaded
        .miz
        .files
        .get("warehouses")
        .context("warehouse bundle: missing unpacked warehouses file path")?;
    let s = serialize_to_lua(
        "warehouses",
        Value::Table(wb.loaded.warehouses.clone()),
    )?;
    fs::write(wh_file, &*s).context("serializing warehouse template warehouses")?;
    wb.loaded
        .miz
        .pack(&wb.path)
        .with_context(|| format_compact!("pack warehouse template {}", wb.path.display()))?;
    Ok(())
}

fn print_inventory_aircraft_orphan_editor_notice(
    cleared: &[InventoryAircraftOrphanSanitized],
    warehouse_miz_path: &Path,
    weapon_miz_path: &Path,
    campaign_decade: &str,
) {
    if cleared.is_empty() {
        return;
    }
    const RED: &str = "\x1b[31;1m";
    const RESET: &str = "\x1b[0m";
    let ln = |s: &str| println!("{RED}{s}{RESET}");
    let weapon_file = format!("weapon{campaign_decade}.miz");
    let warehouse_file = format!("warehouse{campaign_decade}.miz");

    fn side_word(side: Side) -> &'static str {
        match side {
            Side::Blue => "BLUE (BINVENTORY)",
            Side::Red => "RED (RINVENTORY)",
            Side::Neutral => "NEUTRAL",
        }
    }
    println!();
    ln("****************************************************************");
    ln(&format!(
        "*  NOTICE - B/RINVENTORY vs {weapon_file} (AUTO-ZERO; mission build stopped)"
    ));
    ln("****************************************************************");
    println!();
    ln("These types had non-zero stock under aircrafts (planes / helicopters) but no matching");
    ln(&format!(
        "coalition airframe template in `{weapon_file}` for this campaign (CFG campaign_decade={campaign_decade})."
    ));
    ln("`initialAmount` was set to 0 in the warehouse template on disk; output .miz was NOT written.");
    ln(&format!("  {}", warehouse_miz_path.display()));
    println!();
    ln("ROWS ZEROED (previous initialAmount):");
    for e in cleared {
        ln(&format!(
            "  * {} | {} | type={} [{}] had {}",
            e.row_name,
            side_word(e.side),
            e.unit_type,
            e.category,
            e.previous_amount
        ));
    }
    println!();
    ln("----------------------------------------------------------------");
    ln("EDITOR WORKFLOW - FOLLOW THIS ORDER:");
    println!();
    ln(&format!(
        "  1) FIRST - add airframe templates for each missing type in `{weapon_file}`:"
    ));
    ln(&format!("       {}", weapon_miz_path.display()));
    println!();
    ln(&format!(
        "  2) THEN - reopen `{warehouse_file}` BINVENTORY / RINVENTORY and restore counts:"
    ));
    ln(&format!("       {}", warehouse_miz_path.display()));
    println!();
    ln("  3) REBUILD the mission.");
    ln("----------------------------------------------------------------");
    ln("If you did not intentionally add new B/RINVENTORY rows in");
    ln(&format!(
        "`{weapon_file}`, the extra types may have come from a DCS update (new modules in the"
    ));
    ln("aircraft list). To add them to the campaign correctly, repeat from step 1.");
    ln("****************************************************************");
    println!();
}

fn lua_value_truthy(v: &Value) -> bool {
    match v {
        Value::Boolean(b) => *b,
        Value::Integer(i) => *i != 0,
        Value::Number(n) => *n != 0.0 && n.abs() > f64::EPSILON,
        Value::String(s) => {
            let Ok(s) = s.to_str() else {
                return false;
            };
            let t = s.trim().to_ascii_lowercase();
            !matches!(t.as_str(), "" | "0" | "false" | "no")
        }
        _ => false,
    }
}

/// ME `dynamicSpawn` is often stored as `1`/`0`, not a Lua boolean.
fn warehouse_dynamic_spawn_enabled(row: &Table) -> bool {
    row.raw_get::<_, Value>("dynamicSpawn")
        .map(|v| lua_value_truthy(&v))
        .unwrap_or(false)
}

/// Fowl stock fill / `patch_warehouse_dynamic_spawn_links` require finite export (`Unlimited Liquids` → `unlimitedFuel`).
fn warehouse_all_unlimited_off(row: &Table) -> bool {
    !["unlimitedFuel", "unlimitedMunitions", "unlimitedAircrafts"]
        .iter()
        .any(|&k| {
            row.raw_get::<_, Value>(k)
                .map(|v| lua_value_truthy(&v))
                .unwrap_or(false)
        })
}

/// ME `dynamicSpawn` on ship warehouse rows only; runs after fill / `linkDynTempl` patch.
fn apply_settings_dynamic_spawn_ttdn_naval_flags(
    base: &LoadedMiz,
    warehouses: &Table<'static>,
    ship_hull_by_wid: &HashMap<i64, String>,
) -> Result<()> {
    let settings = VehicleTemplates::load_zone_creation_settings(
        base,
        SETTINGS_DYNAMIC_SPAWN_TTDN_ZONE,
    )?;
    if settings.is_empty() {
        warn!(
            "trigger zone {SETTINGS_DYNAMIC_SPAWN_TTDN_ZONE} missing or has no bool properties; naval dynamicSpawn left as built"
        );
        return Ok(());
    }
    let wh_tbl: Table = warehouses
        .raw_get("warehouses")
        .context("warehouses.warehouses")?;
    for (&wid, hull) in ship_hull_by_wid {
        let key = format!("TTDN{hull}");
        let Some(enabled) = settings.get(&String::from(key.as_str())).copied() else {
            warn!(
                "naval warehouse {wid} ({hull}): no `{key}` in {SETTINGS_DYNAMIC_SPAWN_TTDN_ZONE}; dynamicSpawn unchanged"
            );
            continue;
        };
        let row: Table = wh_tbl
            .raw_get(wid)
            .with_context(|| format!("naval warehouse row {wid} ({hull})"))?;
        row.raw_set("dynamicSpawn", enabled)?;
        info!(
            "naval warehouse {wid} ({hull}): dynamicSpawn={enabled} ({SETTINGS_DYNAMIC_SPAWN_TTDN_ZONE})"
        );
    }
    Ok(())
}

fn validate_settings_dynamic_spawn_ground_keys(settings: &HashMap<String, bool>) -> Result<()> {
    for key in settings.keys() {
        if key.as_str() == SETTINGS_DYNAMIC_SPAWN_DEP_FARP_KEY {
            continue;
        }
        if key.contains('*') {
            bail!(
                "SETTINGS-dynamic-spawn: invalid property key {key:?} (only {:?} may contain `*`)",
                SETTINGS_DYNAMIC_SPAWN_DEP_FARP_KEY,
            );
        }
        if key.chars().count() < 3 {
            bail!(
                "SETTINGS-dynamic-spawn: invalid property key {key:?} (minimum three characters, e.g. objective zone prefix)"
            );
        }
    }
    Ok(())
}

fn spawn_settings_o_zone_prefix(zone_name: &str) -> StdString {
    zone_name.chars().take(3).collect()
}

fn objective_dyn_allow_for_spawn<'a>(
    wid: i64,
    is_airports_table: bool,
    obj_dyn_allow: &'a [ObjectiveDynAllow],
    warehouse_positions: &HashMap<i64, Vector2>,
) -> Option<&'a ObjectiveDynAllow> {
    if is_airports_table {
        obj_dyn_allow.iter().find(|o| o.airbase_id == Some(wid)).or_else(|| {
            warehouse_positions
                .get(&wid)
                .and_then(|&pos| objective_dyn_allow_geom_pick(obj_dyn_allow, pos))
        })
    } else {
        warehouse_positions
            .get(&wid)
            .and_then(|&pos| objective_dyn_allow_geom_pick(obj_dyn_allow, pos))
    }
}

fn resolved_ground_dynamic_spawn_setting(
    wid: i64,
    is_airports_table: bool,
    settings: &HashMap<String, bool>,
    mult_cfg: &WarehouseStockMultConfig,
    obj_dyn_allow: &[ObjectiveDynAllow],
    warehouse_positions: &HashMap<i64, Vector2>,
) -> Option<bool> {
    if mult_cfg.naval_warehouse_ids.contains(&wid) {
        return None;
    }
    if mult_cfg.dep_farp_warehouse_ids.contains(&wid) {
        return Some(
            settings
                .get(SETTINGS_DYNAMIC_SPAWN_DEP_FARP_KEY)
                .copied()
                .unwrap_or(false),
        );
    }
    let obj = objective_dyn_allow_for_spawn(wid, is_airports_table, obj_dyn_allow, warehouse_positions)?;
    let key = spawn_settings_o_zone_prefix(obj.zone_name.as_str());
    Some(settings.get(key.as_str()).copied().unwrap_or(false))
}

fn apply_settings_dynamic_spawn_ground_flags(
    base: &LoadedMiz,
    warehouses_root: &Table<'static>,
    mult_cfg: &WarehouseStockMultConfig,
    obj_dyn_allow: &[ObjectiveDynAllow],
    warehouse_positions: &HashMap<i64, Vector2>,
) -> Result<()> {
    let settings =
        VehicleTemplates::load_zone_creation_settings(base, SETTINGS_DYNAMIC_SPAWN_GROUND_ZONE)?;
    validate_settings_dynamic_spawn_ground_keys(&settings)?;
    if settings.is_empty() {
        warn!(
            "{} zone missing or has no valid bool properties — governed ground hubs resolve dynamicSpawn entries as false",
            SETTINGS_DYNAMIC_SPAWN_GROUND_ZONE
        );
    }
    for (is_airports, tbl_name) in [(true, "airports"), (false, "warehouses")] {
        let Ok(tbl) = warehouses_root.raw_get::<_, Table>(tbl_name) else {
            continue;
        };
        for pair in tbl.clone().pairs::<Value, Table>() {
            let (k, wh) = pair?;
            let Some(wid) = warehouse_lua_key_i64(k) else {
                continue;
            };
            let Some(enabled) = resolved_ground_dynamic_spawn_setting(
                wid,
                is_airports,
                &settings,
                mult_cfg,
                obj_dyn_allow,
                warehouse_positions,
            ) else {
                continue;
            };
            if !warehouse_all_unlimited_off(&wh) {
                continue;
            }
            wh.raw_set("dynamicSpawn", enabled)?;
        }
    }
    Ok(())
}

/// Red/blue rows get BDEFAULT/RDEFAULT; neutral build rows are cleared (see `empty_neutral_build_warehouse_row`).
fn warehouse_side_for_default_apply(row: &Table) -> Result<Option<Side>> {
    let s: String =
        row.raw_get("coalition").context("warehouse row missing coalition")?;
    match s.to_lowercase().as_str() {
        "red" => Ok(Some(Side::Red)),
        "blue" => Ok(Some(Side::Blue)),
        "neutral" => Ok(None),
        other => bail!(
            "warehouse coalition must be red, blue, or neutral for default apply (got {other:?})"
        ),
    }
}

fn warehouse_lua_key_i64(k: Value) -> Option<i64> {
    match k {
        Value::Integer(i) => Some(i),
        Value::Number(n) => Some(n as i64),
        Value::String(s) => s.to_str().ok()?.parse().ok(),
        _ => None,
    }
}

fn collect_droptank_ws_types_from_warehouse_row(
    row: &Table,
) -> Result<HashSet<[i32; 4]>> {
    let mut out = HashSet::new();
    let Ok(weapons) = row.raw_get::<_, Table>("weapons") else {
        return Ok(out);
    };
    for pair in weapons.clone().pairs::<Value, Table>() {
        let (_, weapon) = pair?;
        let Ok(wst) = weapon.raw_get::<_, Table>("wsType") else {
            continue;
        };
        let ws = [
            wst.raw_get(1).unwrap_or(0),
            wst.raw_get(2).unwrap_or(0),
            wst.raw_get(3).unwrap_or(0),
            wst.raw_get(4).unwrap_or(0),
        ];
        if ws[0] == 1 && ws[1] == 3 {
            out.insert(ws);
        }
    }
    Ok(out)
}

fn collect_droptank_ws_by_coalition_from_warehouses_root(
    root: &Table,
) -> Result<(HashSet<[i32; 4]>, HashSet<[i32; 4]>)> {
    let mut blue = HashSet::new();
    let mut red = HashSet::new();
    for section in ["airports", "warehouses"] {
        let tbl: Table = match root.raw_get(section) {
            Ok(t) => t,
            Err(_) => continue,
        };
        for pair in tbl.clone().pairs::<Value, Table>() {
            let (_, row) = pair?;
            let Some(side) = warehouse_side_for_default_apply(&row)? else {
                continue;
            };
            let set = collect_droptank_ws_types_from_warehouse_row(&row)?;
            match side {
                Side::Blue => blue.extend(set),
                Side::Red => red.extend(set),
                Side::Neutral => {}
            }
        }
    }
    if blue.is_empty() && !red.is_empty() {
        blue = red.clone();
    } else if red.is_empty() && !blue.is_empty() {
        red = blue.clone();
    }
    Ok((blue, red))
}

/// Fowl `WarehouseConfig` capacity multipliers for miz stock scaling (aligned with bflib `capacity_multiplier`).
struct WarehouseStockMultConfig {
    airbase_max: u32,
    hub_max: u32,
    fob_max: u32,
    farp_max: u32,
    carrier_airbase_max: u32,
    hub_airport_ids: HashSet<i64>,
    hub_warehouse_ids: HashSet<i64>,
    fob_warehouse_ids: HashSet<i64>,
    /// `warehouses` keys whose FARP/Invisible pad sits in a `BDEPFARP*`/`RDEPFARP*`/`NDEPFARP*` placement zone.
    dep_farp_warehouse_ids: HashSet<i64>,
    naval_warehouse_ids: HashSet<i64>,
}

impl WarehouseStockMultConfig {
    fn mult_airport(&self, id: i64) -> u32 {
        if self.hub_airport_ids.contains(&id) {
            self.hub_max.max(1)
        } else {
            self.airbase_max.max(1)
        }
    }

    fn mult_warehouse_row(&self, id: i64) -> u32 {
        if self.dep_farp_warehouse_ids.contains(&id) {
            self.farp_max.max(1)
        } else if self.hub_warehouse_ids.contains(&id) {
            self.hub_max.max(1)
        } else if self.naval_warehouse_ids.contains(&id) {
            self.carrier_airbase_max.max(1)
        } else if self.fob_warehouse_ids.contains(&id) {
            self.fob_max.max(1)
        } else {
            self.airbase_max.max(1)
        }
    }

    fn mult_dynamic_row(&self, id: i64, is_airports_table: bool) -> u32 {
        if self.dep_farp_warehouse_ids.contains(&id) {
            return self.farp_max.max(1);
        }
        if is_airports_table {
            self.mult_airport(id)
        } else if self.hub_warehouse_ids.contains(&id) {
            self.hub_max.max(1)
        } else if self.naval_warehouse_ids.contains(&id) {
            self.carrier_airbase_max.max(1)
        } else {
            self.fob_max.max(1)
        }
    }
}

fn production_inventory_unit_ids(base: &LoadedMiz, cfg: &MizCmd) -> Result<(i64, i64)> {
    let mut blue_inventory = 0i64;
    let mut red_inventory = 0i64;
    for coa in base.mission.raw_get::<_, Table>("coalition")?.pairs::<Value, Table>() {
        let coa = coa?.1;
        for country in coa.raw_get::<_, Table>("country")?.pairs::<Value, Table>() {
            let country = country?.1;
            if let Ok(iter) = vehicle(&country, "static") {
                for group in iter {
                    let group = group?;
                    for unit in
                        group.raw_get::<_, Table>("units")?.pairs::<Value, Table>()
                    {
                        let unit = unit?.1;
                        let typ: String = unit.raw_get("type")?;
                        let name: String = unit.raw_get("name")?;
                        let id: i64 = unit.raw_get("unitId")?;
                        if *typ == "FARP"
                            || *typ == "SINGLE_HELIPAD"
                            || *typ == "FARP_SINGLE_01"
                            || *typ == "Invisible FARP"
                        {
                            if *name == cfg.blue_production_template {
                                blue_inventory = id;
                            } else if *name == cfg.red_production_template {
                                red_inventory = id;
                            }
                        }
                    }
                }
            }
        }
    }
    Ok((blue_inventory, red_inventory))
}

fn scale_liquid_table_init_fuel(tbl: &Table, mult: u32) -> Result<()> {
    let m = mult.max(1) as f64;
    let base: f64 = match tbl.raw_get::<_, f64>("InitFuel") {
        Ok(x) => x,
        Err(_) => match tbl.raw_get::<_, i64>("InitFuel") {
            Ok(x) => x as f64,
            Err(_) => return Ok(()),
        },
    };
    tbl.raw_set("InitFuel", (base * m).clamp(0.0, 1e18))?;
    Ok(())
}

/// When destination `InitFuel` is zero, copy liquid block from inventory (keeps DCS fields); then scale `InitFuel` × mult.
fn merge_liquids_from_inventory_when_dst_empty(dst: &Table, src: &Table, lua: &Lua) -> Result<()> {
    for key in ["jet_fuel", "gasoline", "diesel", "methanol_mixture"] {
        let Ok(dst_l) = dst.raw_get::<_, Table>(key) else {
            continue;
        };
        let zero = match dst_l.raw_get::<_, f64>("InitFuel") {
            Ok(x) => x.abs() < 1e-9,
            Err(_) => match dst_l.raw_get::<_, i64>("InitFuel") {
                Ok(x) => x == 0,
                Err(_) => true,
            },
        };
        if !zero {
            continue;
        }
        let Ok(src_tbl) = src.raw_get::<_, Table>(key) else {
            continue;
        };
        dst.raw_set(key, src_tbl.deep_clone(lua)?)?;
    }
    Ok(())
}

/// Assembled mission: inventory liquids merged where dst fuel is zero, then `InitFuel` × `*_max`.
fn apply_inventory_liquids_scaled(dst: &Table, src: &Table, lua: &Lua, mult: u32) -> Result<()> {
    merge_liquids_from_inventory_when_dst_empty(dst, src, lua)?;
    for key in ["jet_fuel", "gasoline", "diesel", "methanol_mixture"] {
        if let Ok(t) = dst.raw_get::<_, Table>(key) {
            scale_liquid_table_init_fuel(&t, mult)?;
        }
    }
    Ok(())
}

fn preserve_dynamic_flags(
    lua: &Lua,
    new_row: &Table,
    old_row: &Table,
    preserve_liquids: bool,
) -> Result<()> {
    match old_row.raw_get::<_, Value>("dynamicSpawn") {
        Ok(v) if !v.is_nil() => new_row.raw_set("dynamicSpawn", v)?,
        _ => new_row.raw_set("dynamicSpawn", false)?,
    }
    match old_row.raw_get::<_, Value>("dynamicCargo") {
        Ok(v) if !v.is_nil() => new_row.raw_set("dynamicCargo", v)?,
        _ => new_row.raw_set("dynamicCargo", false)?,
    }
    if preserve_liquids {
        for key in ["jet_fuel", "gasoline", "diesel", "methanol_mixture"] {
            let v: Value = old_row.raw_get(key).unwrap_or(Value::Nil);
            if v.is_nil() {
                continue;
            }
            match v {
                Value::Table(t) => new_row.raw_set(key, t.deep_clone(lua)?)?,
                _ => new_row.raw_set(key, v)?,
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Default)]
enum WarehouseTemplateStockMode {
    /// B/RDEFAULT: hub `weapons` only (no A/C in template by design).
    #[default]
    DefaultHubWeapons,
    /// B/RINVENTORY: A/C amounts on matching mission rows; weapons merge over default where stock > 0.
    InventoryStock,
}

fn copy_aircraft_initial_amounts_scaled(
    dst_row: &Table,
    src_row: &Table<'static>,
    mult: u32,
) -> Result<()> {
    let (Ok(dst_aircrafts), Ok(src_aircrafts)) = (
        dst_row.raw_get::<_, Table>("aircrafts"),
        src_row.raw_get::<_, Table>("aircrafts"),
    ) else {
        return Ok(());
    };
    for cat in ["helicopters", "planes"] {
        let (Ok(dst_cat), Ok(src_cat)) = (
            dst_aircrafts.raw_get::<_, Table>(cat),
            src_aircrafts.raw_get::<_, Table>(cat),
        ) else {
            continue;
        };
        for pair in dst_cat.clone().pairs::<String, Table>() {
            let (unit_type, dst_unit) = pair?;
            let Ok(src_unit) = src_cat.raw_get::<_, Table>(unit_type.clone()) else {
                continue;
            };
            let Ok(src_amt) = src_unit.raw_get::<_, u32>("initialAmount") else {
                continue;
            };
            dst_unit.raw_set("initialAmount", src_amt.saturating_mul(mult))?;
        }
    }
    Ok(())
}

/// Template `weapons` (and optionally A/C) onto mission warehouse row, × capacity mult.
fn copy_initial_amounts_scaled(
    lua: &Lua,
    dst_row: &Table,
    src_row: &Table<'static>,
    mult: u32,
    mode: WarehouseTemplateStockMode,
) -> Result<()> {
    if matches!(mode, WarehouseTemplateStockMode::InventoryStock) {
        copy_aircraft_initial_amounts_scaled(dst_row, src_row, mult)?;
    }

    let Some(src_weapons) = src_row.raw_get::<_, Table>("weapons").ok() else {
        return Ok(());
    };

    match mode {
        WarehouseTemplateStockMode::DefaultHubWeapons => {
            let dst_weapons = lua.create_table()?;
            let mut idx = 1u32;
            for pair in src_weapons.clone().pairs::<Value, Table>() {
                let (_, src_w) = pair?;
                let cloned = src_w.deep_clone(lua)?;
                if let Ok(src_amt) = cloned.raw_get::<_, u32>("initialAmount") {
                    cloned.raw_set("initialAmount", src_amt.saturating_mul(mult))?;
                }
                dst_weapons.raw_set(idx, cloned)?;
                idx = idx.saturating_add(1);
            }
            dst_row.raw_set("weapons", dst_weapons)?;
        }
        WarehouseTemplateStockMode::InventoryStock => {
            let dst_weapons = match dst_row.raw_get::<_, Table>("weapons") {
                Ok(t) => t,
                Err(_) => {
                    let t = lua.create_table()?;
                    dst_row.raw_set("weapons", t.clone())?;
                    t
                }
            };
            let mut dst_idx_by_ws: HashMap<[i32; 4], Value> = HashMap::new();
            for pair in dst_weapons.clone().pairs::<Value, Table>() {
                let (k, w) = pair?;
                let Some(ws) = read_weapon_ws_type(&w) else {
                    continue;
                };
                dst_idx_by_ws.insert(ws, k);
            }
            for pair in src_weapons.clone().pairs::<Value, Table>() {
                let (_, src_w) = pair?;
                let src_amt = src_w.raw_get::<_, u32>("initialAmount").unwrap_or(0);
                if src_amt == 0 {
                    continue;
                }
                let scaled = src_amt.saturating_mul(mult);
                let Some(ws) = read_weapon_ws_type(&src_w) else {
                    continue;
                };
                if let Some(k) = dst_idx_by_ws.get(&ws) {
                    let dst_w = dst_weapons.raw_get::<_, Table>(k.clone())?;
                    dst_w.raw_set("initialAmount", scaled)?;
                } else {
                    let cloned = src_w.deep_clone(lua)?;
                    cloned.raw_set("initialAmount", scaled)?;
                    let mut new_idx = dst_weapons.raw_len().saturating_add(1);
                    if new_idx == 0 {
                        new_idx = 1;
                    }
                    dst_weapons.raw_set(new_idx, cloned)?;
                    dst_idx_by_ws.insert(ws, Value::Integer(new_idx as i64));
                }
            }
        }
    }
    Ok(())
}

/// Template stock onto mission warehouse (+ liquids). Mode selects B/RDEFAULT vs B/RINVENTORY fields.
fn apply_mission_warehouse_template_stock(
    lua: &Lua,
    dst_row: &Table,
    src_row: &Table<'static>,
    mult: u32,
    mode: WarehouseTemplateStockMode,
) -> Result<()> {
    copy_initial_amounts_scaled(lua, dst_row, src_row, mult, mode)?;
    apply_inventory_liquids_scaled(dst_row, src_row, lua, mult)
}

fn fill_static_mission_warehouse_from_templates<'a>(
    lua: &'a Lua,
    old_row: &Table<'a>,
    default_tpl: &Table<'static>,
    inventory_tpl: &Table<'static>,
    mult: u32,
) -> Result<Table<'a>> {
    let new_row = old_row.deep_clone(lua)?;
    preserve_dynamic_flags(lua, &new_row, old_row, true)?;
    apply_mission_warehouse_template_stock(
        lua,
        &new_row,
        default_tpl,
        mult,
        WarehouseTemplateStockMode::DefaultHubWeapons,
    )?;
    apply_inventory_liquids_scaled(&new_row, inventory_tpl, lua, mult)?;
    Ok(new_row)
}

#[derive(Clone, Copy)]
enum NeutralWarehouseBuildKind {
    /// Neutral airports without DS: cleared here. Neutral + DS: see `neutral_dynamic_spawn_airport_zero_stock_link_templates`.
    Airport,
    /// `warehouses.warehouses` neutral FARP etc.: `linkDynTempl` zeroed at build.
    Other,
}

/// Neutral + finite warehouse export: ME flags + level fields from campaign B4 / variant-A chat spec.
fn apply_neutral_dynamic_spawn_warehouse_flags(lua: &Lua, row: &Table) -> Result<()> {
    row.raw_set("dynamicCargo", true)?;
    row.raw_set("unlimitedFuel", false)?;
    row.raw_set("unlimitedMunitions", false)?;
    row.raw_set("unlimitedAircrafts", false)?;
    row.raw_set("OperatingLevel_Eqp", 0i64)?;
    row.raw_set("OperatingLevel_Air", 0i64)?;
    row.raw_set("OperatingLevel_Fuel", 10i64)?;
    row.raw_set("size", 0i64)?;
    row.raw_set("equipment", lua.create_table()?)?;
    Ok(())
}

/// Build-time only: neutral coalition rows start with no stock (runtime capture rules stay separate).
fn empty_neutral_build_warehouse_row(
    lua: &Lua,
    row: &Table,
    kind: NeutralWarehouseBuildKind,
) -> Result<()> {
    if warehouse_all_unlimited_off(row) {
        apply_neutral_dynamic_spawn_warehouse_flags(lua, row)?;
    }
    row.raw_set("weapons", lua.create_table()?)?;
    if let Ok(aircrafts) = row.raw_get::<_, Table>("aircrafts") {
        for cat in ["helicopters", "planes"] {
            let Ok(cat_tbl) = aircrafts.raw_get::<_, Table>(cat) else {
                continue;
            };
            for pair in cat_tbl.clone().pairs::<String, Table>() {
                let (_, u) = pair?;
                u.raw_set("initialAmount", 0u32)?;
                if matches!(kind, NeutralWarehouseBuildKind::Other) {
                    let _ = u.raw_set("linkDynTempl", 0i64);
                }
            }
        }
    }
    for key in ["jet_fuel", "gasoline", "diesel", "methanol_mixture"] {
        let Ok(t) = row.raw_get::<_, Table>(key) else {
            continue;
        };
        if t.raw_get::<_, f64>("InitFuel").is_ok() {
            t.raw_set("InitFuel", 0.0f64)?;
        } else if t.raw_get::<_, i64>("InitFuel").is_ok() {
            t.raw_set("InitFuel", 0i64)?;
        } else {
            t.raw_set("InitFuel", 0.0f64)?;
        }
    }
    Ok(())
}

fn coalition_inventory_positive_weapon_ws(
    inv: &Table<'static>,
    inv_plus: Option<&Table<'static>>,
) -> Result<HashSet<[i32; 4]>> {
    let mut u = campaign_cfg::collect_weapon_ws_types_positive_initial(inv)?;
    if let Some(plus) = inv_plus {
        u.extend(campaign_cfg::collect_weapon_ws_types_positive_initial(plus)?);
    }
    Ok(u)
}

fn apply_weapon_cfg_cap_scale_pass(
    warehouses_root: &Table,
    caps: &campaign_cfg::WarehouseDefaultsFromCfg,
    mult_cfg: &WarehouseStockMultConfig,
    skip_ids: &HashSet<i64>,
    blue_inventory_skip_ws: &HashSet<[i32; 4]>,
    red_inventory_skip_ws: &HashSet<[i32; 4]>,
) -> Result<()> {
    fn one_table(
        tbl: &Table,
        caps: &campaign_cfg::WarehouseDefaultsFromCfg,
        mult_cfg: &WarehouseStockMultConfig,
        skip_ids: &HashSet<i64>,
        blue_inventory_skip_ws: &HashSet<[i32; 4]>,
        red_inventory_skip_ws: &HashSet<[i32; 4]>,
        is_airports: bool,
    ) -> Result<()> {
        for pair in tbl.clone().pairs::<Value, Table>() {
            let (k, row) = pair?;
            let Some(wid) = warehouse_lua_key_i64(k) else {
                continue;
            };
            if skip_ids.contains(&wid) {
                continue;
            }
            let Ok(coa) = row.raw_get::<_, String>("coalition") else {
                continue;
            };
            if matches!(coa.to_lowercase().as_str(), "neutral" | "") {
                continue;
            }
            let mult = if is_airports {
                mult_cfg.mult_airport(wid)
            } else {
                mult_cfg.mult_warehouse_row(wid)
            };
            let skip_ws = match coa.to_lowercase().as_str() {
                "blue" => Some(blue_inventory_skip_ws),
                "red" => Some(red_inventory_skip_ws),
                _ => None,
            };
            campaign_cfg::scale_weapon_amounts_matching_cfg_cap(
                &row, caps, mult, skip_ws,
            )?;
        }
        Ok(())
    }
    let airports =
        warehouses_root.raw_get::<_, Table>("airports").context("scale pass airports")?;
    let warehouses = warehouses_root
        .raw_get::<_, Table>("warehouses")
        .context("scale pass warehouses")?;
    one_table(
        &airports,
        caps,
        mult_cfg,
        skip_ids,
        blue_inventory_skip_ws,
        red_inventory_skip_ws,
        true,
    )?;
    one_table(
        &warehouses,
        caps,
        mult_cfg,
        skip_ids,
        blue_inventory_skip_ws,
        red_inventory_skip_ws,
        false,
    )?;
    Ok(())
}

/// `Some(&set)` only when non-empty. An empty set must not act as allowlist (would remove every row).
fn warehouse_allowlist_for_filter(
    opt: &Option<HashSet<[i32; 4]>>,
) -> Option<&HashSet<[i32; 4]>> {
    opt.as_ref().filter(|s| !s.is_empty())
}

fn weapon_ws_type_label(ws: [i32; 4]) -> StdString {
    format!(
        "wsType [{},{},{},{}]",
        ws[0], ws[1], ws[2], ws[3]
    )
    .into()
}

fn warehouse_weapon_display_name(w: &Table) -> StdString {
    for key in ["name", "desc", "displayName", "Name"] {
        if let Ok(s) = w.raw_get::<_, String>(key) {
            let t = s.as_str().trim();
            if !t.is_empty() && !t.eq_ignore_ascii_case("nil") {
                return StdString::from(t);
            }
        }
    }
    if let Some(ws) = read_weapon_ws_type(w) {
        return weapon_ws_type_label(ws);
    }
    StdString::from("weapon row (no wsType)")
}

/// Export key for a weapon row; skips junk ME placeholders without wsType.
fn weapon_row_export_key(w: &Table) -> Option<StdString> {
    read_weapon_ws_type(w)?;
    let label = warehouse_weapon_display_name(w);
    if label == "weapon row (no wsType)" {
        return None;
    }
    Some(label)
}

fn read_weapon_ws_type(w: &Table) -> Option<[i32; 4]> {
    let wst: Table = w.raw_get("wsType").ok()?;
    Some([
        wst.raw_get(1).ok()?,
        wst.raw_get(2).ok()?,
        wst.raw_get(3).ok()?,
        wst.raw_get(4).ok()?,
    ])
}

#[derive(Clone, Debug)]
struct InventoryZonePlusModuleEntry {
    item_name: StdString,
    module: StdString,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum WsZoneDistributeScope {
    #[default]
    Filter,
    All,
}

#[derive(Clone, Debug, Default)]
struct WsZoneStockSpec {
    amount: Option<u32>,
    distribute: WsZoneDistributeScope,
    modules: HashSet<StdString>,
}

fn zone_property_is_editor_comment(s: &str) -> bool {
    s.starts_with("***")
}

fn parse_ws_type_zone_key(key: &str) -> Option<[i32; 4]> {
    let compact: StdString = key.chars().filter(|c| !c.is_whitespace()).collect();
    let inner = compact
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(compact.as_str());
    let parts: Vec<&str> = inner.split(',').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut ws = [0i32; 4];
    for (i, p) in parts.iter().enumerate() {
        ws[i] = p.parse().ok()?;
    }
    Some(ws)
}

fn ws_zone_stock_ws_for_policy_modules(
    specs: &HashMap<[i32; 4], WsZoneStockSpec>,
    policy_types: &HashSet<StdString>,
) -> HashSet<[i32; 4]> {
    let mut out = HashSet::new();
    for (&ws, spec) in specs {
        if spec
            .modules
            .iter()
            .any(|m| policy_types_include_module(policy_types, m.as_str()))
        {
            out.insert(ws);
        }
    }
    out
}

fn ws_zone_force_keep_ws(
    inv_specs: &HashMap<[i32; 4], WsZoneStockSpec>,
    def_specs: &HashMap<[i32; 4], WsZoneStockSpec>,
) -> HashSet<[i32; 4]> {
    let mut out = HashSet::new();
    for specs in [inv_specs, def_specs] {
        for (&ws, spec) in specs {
            if spec.distribute == WsZoneDistributeScope::All {
                out.insert(ws);
            }
        }
    }
    out
}

fn farp_positive_weapon_ws(plus: Option<&Table<'static>>) -> HashSet<[i32; 4]> {
    let Some(row) = plus else {
        return HashSet::new();
    };
    campaign_cfg::collect_weapon_ws_types_positive_initial(row).unwrap_or_default()
}

fn apply_zone_ws_weapon_stock(
    lua: &Lua,
    dst_row: &Table,
    ws: [i32; 4],
    amount: u32,
    mult: u32,
) -> Result<()> {
    let scaled = amount.saturating_mul(mult);
    if scaled == 0 {
        return Ok(());
    }
    let dst_weapons = match dst_row.raw_get::<_, Table>("weapons") {
        Ok(t) => t,
        Err(_) => {
            let t = lua.create_table()?;
            dst_row.raw_set("weapons", t.clone())?;
            t
        }
    };
    for pair in dst_weapons.clone().pairs::<Value, Table>() {
        let (_, w) = pair?;
        if read_weapon_ws_type(&w) == Some(ws) {
            w.raw_set("initialAmount", scaled)?;
            return Ok(());
        }
    }
    let w = lua.create_table()?;
    let wst = lua.create_table()?;
    wst.raw_set(1, ws[0])?;
    wst.raw_set(2, ws[1])?;
    wst.raw_set(3, ws[2])?;
    wst.raw_set(4, ws[3])?;
    w.raw_set("wsType", wst)?;
    w.raw_set("initialAmount", scaled)?;
    let mut new_idx = dst_weapons.raw_len().saturating_add(1);
    if new_idx == 0 {
        new_idx = 1;
    }
    dst_weapons.raw_set(new_idx, w)?;
    Ok(())
}

fn apply_zone_ws_stock_amounts(
    lua: &Lua,
    wh: &Table,
    inv_specs: &HashMap<[i32; 4], WsZoneStockSpec>,
    def_specs: &HashMap<[i32; 4], WsZoneStockSpec>,
    farp_inv_pos: &HashSet<[i32; 4]>,
    farp_def_pos: &HashSet<[i32; 4]>,
    mult: u32,
    scope: WsZoneDistributeScope,
    filter_allowed: Option<&HashSet<[i32; 4]>>,
    applied_ws: &mut HashSet<[i32; 4]>,
) -> Result<()> {
    let farp_skip = |ws: &[i32; 4]| farp_inv_pos.contains(ws) || farp_def_pos.contains(ws);
    for specs in [inv_specs, def_specs] {
        for (&ws, spec) in specs {
            if spec.distribute != scope {
                continue;
            }
            let Some(amt) = spec.amount else {
                continue;
            };
            if applied_ws.contains(&ws) {
                continue;
            }
            if farp_skip(&ws) {
                continue;
            }
            if let Some(allowed) = filter_allowed {
                if !allowed.contains(&ws) {
                    continue;
                }
            }
            apply_zone_ws_weapon_stock(lua, wh, ws, amt, mult)?;
            applied_ws.insert(ws);
        }
    }
    Ok(())
}

fn collect_inventory_weapon_display_name_to_ws(
    inv_row: &Table,
) -> Result<HashMap<StdString, [i32; 4]>> {
    let mut out: HashMap<StdString, [i32; 4]> = HashMap::new();
    let mut disp_all_ws: HashMap<StdString, HashSet<[i32; 4]>> = HashMap::new();
    let Ok(weapons) = inv_row.raw_get::<_, Table>("weapons") else {
        return Ok(out);
    };
    for pair in weapons.clone().pairs::<Value, Table>() {
        let (_, w) = pair?;
        let Some(ws) = read_weapon_ws_type(&w) else {
            continue;
        };
        if ws == [0, 0, 0, 0] {
            continue;
        }
        let disp = warehouse_weapon_display_name(&w);
        if disp.starts_with("wsType [") || disp == "weapon row (no wsType)" {
            continue;
        }
        disp_all_ws.entry(disp.clone()).or_default().insert(ws);
        out.entry(disp).or_insert(ws);
    }
    for (disp, types) in &disp_all_ws {
        if types.len() <= 1 {
            continue;
        }
        let Some(chosen) = out.get(disp).copied() else {
            continue;
        };
        let mut types_sorted: Vec<[i32; 4]> = types.iter().copied().collect();
        types_sorted.sort_unstable();
        let list = types_sorted
            .iter()
            .map(|w| format!("[{},{},{},{}]", w[0], w[1], w[2], w[3]))
            .collect::<Vec<_>>()
            .join(", ");
        warn!(
            "B/RINVENTORY+ rows share display name `{}` with different wsType; merge uses [{},{},{},{}]. Distinct wsTypes: {}",
            disp,
            chosen[0],
            chosen[1],
            chosen[2],
            chosen[3],
            list
        );
    }
    Ok(out)
}

/// B/RINVENTORY+ FARP rows (hybrid stock) are not in main inventory until merge; include them for
/// zone Key→wsType resolution and substring narrowing.
fn merge_inventory_plus_weapon_resolution_hints(
    inv_name_to_ws: &mut HashMap<StdString, [i32; 4]>,
    inv_weapon_ws: &mut HashSet<[i32; 4]>,
    plus: Option<&Table<'static>>,
) -> Result<()> {
    let Some(row) = plus else {
        return Ok(());
    };
    inv_weapon_ws.extend(collect_inventory_weapon_ws_set(row)?);
    for (k, v) in collect_inventory_weapon_display_name_to_ws(row)? {
        inv_name_to_ws.entry(k).or_insert(v);
    }
    Ok(())
}

#[derive(Default)]
struct WarehouseZonePlusDirectives {
    inventory_blue: Vec<InventoryZonePlusModuleEntry>,
    inventory_red: Vec<InventoryZonePlusModuleEntry>,
    ws_inventory_blue: HashMap<[i32; 4], WsZoneStockSpec>,
    ws_inventory_red: HashMap<[i32; 4], WsZoneStockSpec>,
    default_blue: Vec<InventoryZonePlusModuleEntry>,
    default_red: Vec<InventoryZonePlusModuleEntry>,
    ws_default_blue: HashMap<[i32; 4], WsZoneStockSpec>,
    ws_default_red: HashMap<[i32; 4], WsZoneStockSpec>,
}

fn compile_warehouse_zone_plus_directives(mission: &Miz) -> Result<WarehouseZonePlusDirectives> {
    let mut out = WarehouseZonePlusDirectives::default();
    for zone_r in mission.triggers()? {
        let zone = zone_r?;
        let zname = zone.name()?;
        let zname_ref = zname.as_ref();
        let (catalog_target, ws_target, farp_hint) = match zname_ref {
            "BINVENTORY+" => (
                &mut out.inventory_blue,
                &mut out.ws_inventory_blue,
                "BINVENTORY+",
            ),
            "RINVENTORY+" => (
                &mut out.inventory_red,
                &mut out.ws_inventory_red,
                "RINVENTORY+",
            ),
            "BDEFAULT+" => (
                &mut out.default_blue,
                &mut out.ws_default_blue,
                "BDEFAULT+",
            ),
            "RDEFAULT+" => (
                &mut out.default_red,
                &mut out.ws_default_red,
                "RDEFAULT+",
            ),
            _ => continue,
        };
        for prop_r in zone.properties()? {
            let prop = prop_r?;
            let item = prop.key.as_ref().trim();
            if item.is_empty() || zone_property_is_editor_comment(item) {
                continue;
            }
            let val = prop.value.as_ref().trim();
            if val.is_empty() {
                warn!(
                    "{zname_ref} zone: skip property with empty Value for item `{item}`"
                );
                continue;
            }
            if zone_property_is_editor_comment(val) {
                continue;
            }
            if let Some(ws) = parse_ws_type_zone_key(item) {
                let spec = ws_target.entry(ws).or_default();
                if val.eq_ignore_ascii_case("ALL") {
                    spec.distribute = WsZoneDistributeScope::All;
                } else if val.eq_ignore_ascii_case("FILTER") {
                    spec.distribute = WsZoneDistributeScope::Filter;
                } else if let Ok(amt) = val.parse::<u32>() {
                    spec.amount = Some(amt);
                    if spec.distribute == WsZoneDistributeScope::Filter && spec.modules.is_empty() {
                        // numeric-only row: coalition FILTER distribution
                    }
                } else {
                    spec.modules.insert(StdString::from(val));
                }
                continue;
            }
            if val.parse::<u32>().is_ok() {
                info!(
                    "{zname_ref} zone: skip catalog `{item}` = `{val}` (use wsType Key `[l1,l2,l3,l4]` for zone amounts, or Invisible FARP {farp_hint} rows)"
                );
                continue;
            }
            if val.eq_ignore_ascii_case("ALL") || val.eq_ignore_ascii_case("FILTER") {
                warn!(
                    "{zname_ref} zone: skip `{item}` = `{val}` (ALL/FILTER apply to wsType Keys only)"
                );
                continue;
            }
            catalog_target.push(InventoryZonePlusModuleEntry {
                item_name: StdString::from(item),
                module: StdString::from(val),
            });
        }
    }
    fn log_ws_zone_stats(label: &str, ws: &HashMap<[i32; 4], WsZoneStockSpec>) {
        if ws.is_empty() {
            return;
        }
        let mut all_amt = 0usize;
        let mut filter_amt = 0usize;
        let mut module_only = 0usize;
        for spec in ws.values() {
            if spec.amount.is_none() {
                if !spec.modules.is_empty() {
                    module_only += 1;
                }
                continue;
            }
            if spec.distribute == WsZoneDistributeScope::All {
                all_amt += 1;
            } else {
                filter_amt += 1;
            }
        }
        info!(
            "{label} trigger zone: {} wsType row(s) (ALL stock={all_amt}, FILTER stock={filter_amt}, module-only={module_only})",
            ws.len()
        );
    }
    if !out.inventory_blue.is_empty() {
        info!(
            "BINVENTORY+ trigger zone: {} catalog module link row(s)",
            out.inventory_blue.len()
        );
    }
    if !out.inventory_red.is_empty() {
        info!(
            "RINVENTORY+ trigger zone: {} catalog module link row(s)",
            out.inventory_red.len()
        );
    }
    if !out.default_blue.is_empty() {
        info!(
            "BDEFAULT+ trigger zone: {} catalog module link row(s)",
            out.default_blue.len()
        );
    }
    if !out.default_red.is_empty() {
        info!(
            "RDEFAULT+ trigger zone: {} catalog module link row(s)",
            out.default_red.len()
        );
    }
    log_ws_zone_stats("BINVENTORY+", &out.ws_inventory_blue);
    log_ws_zone_stats("RINVENTORY+", &out.ws_inventory_red);
    log_ws_zone_stats("BDEFAULT+", &out.ws_default_blue);
    log_ws_zone_stats("RDEFAULT+", &out.ws_default_red);
    Ok(out)
}

fn collect_inventory_weapon_ws_set(inv_row: &Table) -> Result<HashSet<[i32; 4]>> {
    let mut out = HashSet::new();
    let Ok(weapons) = inv_row.raw_get::<_, Table>("weapons") else {
        return Ok(out);
    };
    for pair in weapons.clone().pairs::<Value, Table>() {
        let (_, w) = pair?;
        if let Some(ws) = read_weapon_ws_type(&w) {
            if ws != [0, 0, 0, 0] {
                out.insert(ws);
            }
        }
    }
    Ok(out)
}

fn zone_item_label_ws_candidates(
    item_name: &str,
    inv_map: &HashMap<StdString, [i32; 4]>,
    br: &weapon_bridge::WeaponBridgeMap,
) -> HashSet<[i32; 4]> {
    let mut out = HashSet::new();
    if let Some(ws) = inv_map.get(item_name) {
        if ws != &[0, 0, 0, 0] {
            out.insert(*ws);
        }
    }
    let needle = item_name.trim();
    if !needle.is_empty() {
        for (k, ws) in inv_map {
            let ks = k.as_str();
            if ks.contains(needle) || needle.contains(ks) {
                if *ws != [0, 0, 0, 0] {
                    out.insert(*ws);
                }
            }
        }
    }
    for ws in br.ws_types_for_descriptor_or_key_substring(item_name) {
        if ws != [0, 0, 0, 0] {
            out.insert(ws);
        }
    }
    // ME label "AIM-7F" hits rack-only exact bridge key; missile uses "{AIM-7F}".
    if !item_name.starts_with('{') {
        let braced = format!("{{{item_name}}}");
        if let Some(ws) = br.ws_type_for_descriptor(&braced) {
            if ws != [0, 0, 0, 0] {
                out.insert(ws);
            }
        }
        for ws in br.ws_types_for_descriptor_or_key_substring(&braced) {
            if ws != [0, 0, 0, 0] {
                out.insert(ws);
            }
        }
    }
    out
}

fn record_inventory_zone_module_ws_export(
    export: &mut HashMap<StdString, HashMap<StdString, Vec<[i32; 4]>>>,
    module: &str,
    item_label: &str,
    wss: &[[i32; 4]],
) {
    let mut sorted: Vec<[i32; 4]> = wss.to_vec();
    sorted.sort_by_key(|w| (w[0], w[1], w[2], w[3]));
    sorted.dedup();
    export
        .entry(StdString::from(module))
        .or_default()
        .insert(StdString::from(item_label), sorted);
}

fn resolve_zone_link_ws_for_module(
    item_name: &str,
    module: &str,
    side: Side,
    vt: &VehicleTemplates,
    br: &weapon_bridge::WeaponBridgeMap,
    inv_map: &HashMap<StdString, [i32; 4]>,
    inv_weapon_ws: &HashSet<[i32; 4]>,
    label: &str,
) -> Vec<[i32; 4]> {
    let module_ws = vt.module_ordnance_ws_for_unit_type(br, side, module);
    if module_ws.is_empty() {
        warn!(
            "{label} zone: module `{module}` has no ordnance wsTypes from weapon.miz payload on side {:?}",
            side
        );
    }
    let label_ws: HashSet<[i32; 4]> = zone_item_label_ws_candidates(item_name, inv_map, br)
        .into_iter()
        .filter(|w| is_inventory_cap_ordnance_ws(*w))
        .collect();
    let mut matched: HashSet<[i32; 4]> = label_ws
        .iter()
        .copied()
        .filter(|w| module_ws.contains(w))
        .collect();
    // Warehouse stock uses missile wsTypes from RINVENTORY/BINVENTORY (initialAmount>0), not only payload racks.
    for w in label_ws.iter().copied() {
        if inv_weapon_ws.contains(&w) && is_inventory_cap_ordnance_ws(w) {
            matched.insert(w);
        }
    }
    // ME label vs braced CLSID (AIM-7F rack on payload, {AIM-7F} missile in warehouse).
    if !module_ws.is_empty() && !item_name.starts_with('{') {
        let braced = format!("{{{item_name}}}");
        if let Some(ws) = br.ws_type_for_descriptor(&braced) {
            if ws != [0, 0, 0, 0] && is_inventory_cap_ordnance_ws(ws) {
                matched.insert(ws);
            }
        }
    }
    if matched.is_empty() && !label_ws.is_empty() {
        let narrowed: Vec<[i32; 4]> = label_ws
            .iter()
            .copied()
            .filter(|w| inv_weapon_ws.contains(w))
            .collect();
        if narrowed.len() == 1 {
            matched.insert(narrowed[0]);
        } else if !narrowed.is_empty() {
            matched.extend(narrowed.into_iter().filter(|w| module_ws.contains(w)));
        }
    }
    if matched.is_empty() && !module_ws.is_empty() && !label_ws.is_empty() {
        warn!(
            "{label} zone: `{item_name}` -> `{module}`: label matched {} bridge/inventory wsType(s) but none are on this module's weapon.miz payload (module has {} ordnance wsType(s)); check Key spelling or payload",
            label_ws.len(),
            module_ws.len()
        );
        return Vec::new();
    }
    if matched.is_empty() {
        warn!(
            "{label} zone: skip `{item_name}` -> `{module}` (no ordnance wsType for label on this module)",
        );
        return Vec::new();
    }
    let mut out: Vec<[i32; 4]> = matched.into_iter().collect();
    out.sort_by_key(|w| (w[0], w[1], w[2], w[3]));
    let list = out
        .iter()
        .map(|w| format!("[{},{},{},{}]", w[0], w[1], w[2], w[3]))
        .collect::<Vec<_>>()
        .join(", ");
    info!(
        "{label} zone: `{item_name}` module `{module}` -> {} wsType(s): {}",
        out.len(),
        list
    );
    out
}

/// B/RDEFAULT+ zone links: trust editor Key/Value and map labels to ME warehouse wsTypes (e.g. AGM-65A -> [4,4,8,273]).
fn resolve_default_zone_link_ws_for_module(
    item_name: &str,
    module: &str,
    _side: Side,
    br: &weapon_bridge::WeaponBridgeMap,
    default_plus_map: &HashMap<StdString, [i32; 4]>,
    default_plus_weapon_ws: &HashSet<[i32; 4]>,
    label: &str,
) -> Vec<[i32; 4]> {
    let mut matched: HashSet<[i32; 4]> = HashSet::new();
    let label_ws: HashSet<[i32; 4]> = zone_item_label_ws_candidates(item_name, default_plus_map, br)
        .into_iter()
        .filter(|w| is_inventory_cap_ordnance_ws(*w))
        .collect();
    for w in &label_ws {
        if default_plus_weapon_ws.contains(w) {
            matched.insert(*w);
        }
    }
    let mut bridge_ws = HashSet::new();
    bridge_ws.extend(br.ws_types_for_descriptor_or_key_substring(item_name));
    if !item_name.starts_with('{') {
        let braced = format!("{{{item_name}}}");
        bridge_ws.extend(br.ws_types_for_descriptor_or_key_substring(&braced));
    }
    for ws in bridge_ws {
        if !is_inventory_cap_ordnance_ws(ws) {
            continue;
        }
        if label_ws.contains(&ws) || default_plus_weapon_ws.contains(&ws) {
            matched.insert(ws);
        }
    }
    if matched.is_empty() && !label_ws.is_empty() {
        warn!(
            "{label} zone: `{item_name}` -> `{module}`: no wsType resolved for default allowlist (check Key spelling or BDEFAULT+ FARP row wsType)",
        );
        return Vec::new();
    }
    if matched.is_empty() {
        warn!(
            "{label} zone: skip `{item_name}` -> `{module}` (no ordnance wsType for label)",
        );
        return Vec::new();
    }
    let mut out: Vec<[i32; 4]> = matched.into_iter().collect();
    out.sort_by_key(|w| (w[0], w[1], w[2], w[3]));
    let list = out
        .iter()
        .map(|w| format!("[{},{},{},{}]", w[0], w[1], w[2], w[3]))
        .collect::<Vec<_>>()
        .join(", ");
    info!(
        "{label} zone: `{item_name}` module `{module}` -> {} default wsType(s): {}",
        out.len(),
        list
    );
    out
}

fn default_zone_ws_for_policy_modules(
    br: &weapon_bridge::WeaponBridgeMap,
    side: Side,
    zone_entries: &[InventoryZonePlusModuleEntry],
    ws_zone_specs: &HashMap<[i32; 4], WsZoneStockSpec>,
    policy_types: &HashSet<StdString>,
    default_plus_nm: &HashMap<StdString, [i32; 4]>,
    default_plus_ws: &HashSet<[i32; 4]>,
    zone_label: &str,
) -> HashSet<[i32; 4]> {
    let mut out = ws_zone_stock_ws_for_policy_modules(ws_zone_specs, policy_types);
    for e in zone_entries {
        if !policy_types_include_module(policy_types, e.module.as_str()) {
            continue;
        }
        for ws in resolve_default_zone_link_ws_for_module(
            e.item_name.as_str(),
            e.module.as_str(),
            side,
            br,
            default_plus_nm,
            default_plus_ws,
            zone_label,
        ) {
            out.insert(ws);
        }
    }
    out
}

fn build_default_plus_resolution_maps(
    default_plus: Option<&Table<'static>>,
) -> Result<(HashMap<StdString, [i32; 4]>, HashSet<[i32; 4]>)> {
    let Some(row) = default_plus else {
        return Ok((HashMap::new(), HashSet::new()));
    };
    let mut nm = collect_inventory_weapon_display_name_to_ws(row)?;
    let mut ws = collect_inventory_weapon_ws_set(row)?;
    merge_inventory_plus_weapon_resolution_hints(&mut nm, &mut ws, Some(row))?;
    Ok((nm, ws))
}

fn merge_default_zone_plus_into_allowlist(
    br: Option<&weapon_bridge::WeaponBridgeMap>,
    side: Side,
    entries: &[InventoryZonePlusModuleEntry],
    default_plus_map: &HashMap<StdString, [i32; 4]>,
    default_plus_weapon_ws: &HashSet<[i32; 4]>,
    into_allowlist: &mut HashSet<[i32; 4]>,
    sources: &mut HashMap<[i32; 4], HashSet<StdString>>,
    label: &str,
) -> Result<HashSet<[i32; 4]>> {
    let mut added: HashSet<[i32; 4]> = HashSet::default();
    let Some(br) = br else {
        if !entries.is_empty() {
            warn!(
                "{label} zone: {} module link row(s) ignored (weapon bridge required)",
                entries.len()
            );
        }
        return Ok(added);
    };
    for e in entries {
        let wss = resolve_default_zone_link_ws_for_module(
            e.item_name.as_str(),
            e.module.as_str(),
            side,
            br,
            default_plus_map,
            default_plus_weapon_ws,
            label,
        );
        for ws in wss {
            if !is_inventory_cap_ordnance_ws(ws) {
                continue;
            }
            into_allowlist.insert(ws);
            sources
                .entry(ws)
                .or_default()
                .insert(format!("{label} zone"));
            added.insert(ws);
        }
    }
    Ok(added)
}

fn merge_inventory_zone_plus_into_allowlist(
    br: Option<&weapon_bridge::WeaponBridgeMap>,
    vt: Option<&VehicleTemplates>,
    side: Side,
    entries: &[InventoryZonePlusModuleEntry],
    inv_name_to_ws: &HashMap<StdString, [i32; 4]>,
    inv_weapon_ws: Option<&HashSet<[i32; 4]>>,
    into_allowlist: &mut HashSet<[i32; 4]>,
    mut zone_export: Option<&mut HashMap<StdString, HashMap<StdString, Vec<[i32; 4]>>>>,
    label: &str,
) -> Result<HashSet<[i32; 4]>> {
    let mut added: HashSet<[i32; 4]> = HashSet::default();
    let Some(br) = br else {
        if !entries.is_empty() {
            warn!(
                "{label} zone: {} module link row(s) ignored (weapon bridge required)",
                entries.len()
            );
        }
        return Ok(added);
    };
    let Some(vt) = vt else {
        if !entries.is_empty() {
            warn!(
                "{label} zone: {} module link row(s) ignored (weapon templates required)",
                entries.len()
            );
        }
        return Ok(added);
    };
    let empty_inv_ws = HashSet::new();
    let inv_ws = inv_weapon_ws.unwrap_or(&empty_inv_ws);
    for e in entries {
        let wss = resolve_zone_link_ws_for_module(
            e.item_name.as_str(),
            e.module.as_str(),
            side,
            vt,
            br,
            inv_name_to_ws,
            inv_ws,
            label,
        );
        if wss.is_empty() {
            continue;
        }
        if let Some(exp) = zone_export.as_mut() {
            record_inventory_zone_module_ws_export(exp, e.module.as_str(), e.item_name.as_str(), &wss);
        }
        let mut one = HashSet::new();
        one.insert(e.module.clone());
        let implied = br.weapon_ws_for_aircrafts(&one);
        for ws in wss {
            if !is_inventory_cap_ordnance_ws(ws) {
                continue;
            }
            if !implied.contains(&ws) {
                into_allowlist.insert(ws);
            }
            added.insert(ws);
        }
    }
    Ok(added)
}

/// Drops `row.weapons` whose `wsType` is in `strip_ws`; if `allowed_ws` is set, keeps only rows in allowlist.
/// When `inventory_allowlist_plus_hint` is set and a row is dropped by allowlist, logs `warn!` for mission editors.
fn prune_warehouse_weapons_row(
    lua: &Lua,
    row: &Table,
    strip_ws: &HashSet<[i32; 4]>,
    allowed_ws: Option<&HashSet<[i32; 4]>>,
    log_label: &str,
    inventory_allowlist_plus_hint: Option<&'static str>,
) -> Result<usize> {
    let Ok(weapons) = row.raw_get::<_, Table>("weapons") else {
        return Ok(0);
    };
    let new_weapons = lua.create_table()?;
    let mut out_i = 1u32;
    let mut removed = 0usize;
    for pair in weapons.clone().pairs::<Value, Table>() {
        let (_, w) = pair?;
        let wst: Table = match w.raw_get("wsType") {
            Ok(t) => t,
            Err(_) => {
                removed += 1;
                continue;
            }
        };
        let ws = [
            wst.raw_get(1).unwrap_or(0),
            wst.raw_get(2).unwrap_or(0),
            wst.raw_get(3).unwrap_or(0),
            wst.raw_get(4).unwrap_or(0),
        ];
        if ws == [0, 0, 0, 0] {
            removed += 1;
            continue;
        }
        if strip_ws.contains(&ws) {
            removed += 1;
            continue;
        }
        if let Some(allowed) = allowed_ws {
            if !allowed.contains(&ws) {
                if let Some(plus) = inventory_allowlist_plus_hint {
                    let disp = warehouse_weapon_display_name(&w);
                        warn!(
                            "{}: removed {} (wsType [{}, {}, {}, {}]): not allowed by weapon.miz coalition slot templates; add Invisible FARP B/RINVENTORY+ warehouse rows for extra stock, or trigger zone B/RINVENTORY+ module links (Key=item label, Value=airframe module type). Hint: {}.",
                            log_label,
                            disp,
                            ws[0],
                            ws[1],
                            ws[2],
                            ws[3],
                            plus,
                        );
                }
                removed += 1;
                continue;
            }
        }
        new_weapons.raw_set(out_i, w)?;
        out_i = out_i.saturating_add(1);
    }
    row.raw_set("weapons", new_weapons)?;
    if removed > 0 {
        info!(
            "{log_label}: removed {removed} weapon row(s) (restricted-only ws + {})",
            if allowed_ws.is_some() { "allowlist filter" } else { "no allowlist" }
        );
    }
    Ok(removed)
}

impl WarehouseTemplate {
    fn new(wht: &LoadedMiz, cfg: &MizCmd) -> Result<Self> {
        let mut blue_inventory_id = 0;
        let mut red_inventory_id = 0;
        let mut blue_inventory_plus_id: Option<i64> = None;
        let mut red_inventory_plus_id: Option<i64> = None;
        let mut blue_default_id = 0;
        let mut red_default_id = 0;
        let mut blue_default_plus_id = 0;
        let mut red_default_plus_id = 0;
        let mut blue_all_fueltanks_id = 0;
        let mut red_all_fueltanks_id = 0;
        let mut blue_default_fueltanks_id = 0;
        let mut red_default_fueltanks_id = 0;
        for coa in wht.mission.raw_get::<_, Table>("coalition")?.pairs::<Value, Table>() {
            let coa = coa?.1;
            for country in coa.raw_get::<_, Table>("country")?.pairs::<Value, Table>() {
                let country = country?.1;
                for group in vehicle(&country, "static")? {
                    let group = group?;
                    for unit in
                        group.raw_get::<_, Table>("units")?.pairs::<Value, Table>()
                    {
                        let unit = unit?.1;
                        if *unit.raw_get::<_, String>("type")? == "Invisible FARP" {
                            let name = unit.raw_get::<_, String>("name")?;
                            let id = unit.raw_get::<_, i64>("unitId")?;
                            if *name == "BDEFAULT" {
                                blue_default_id = id;
                            } else if *name == "RDEFAULT" {
                                red_default_id = id;
                            } else if *name == "BDEFAULT+" {
                                blue_default_plus_id = id;
                            } else if *name == "RDEFAULT+" {
                                red_default_plus_id = id;
                            } else if *name == "BALLFUELTANKS" {
                                blue_all_fueltanks_id = id;
                            } else if *name == "RALLFUELTANKS" {
                                red_all_fueltanks_id = id;
                            } else if *name == "BDEFAULTFUELTANKS" {
                                blue_default_fueltanks_id = id;
                            } else if *name == "RDEFAULTFUELTANKS" {
                                red_default_fueltanks_id = id;
                            } else if *name == "BINVENTORY+" {
                                blue_inventory_plus_id = Some(id);
                            } else if *name == "RINVENTORY+" {
                                red_inventory_plus_id = Some(id);
                            } else if *name == cfg.blue_production_template {
                                blue_inventory_id = id;
                            } else if *name == cfg.red_production_template {
                                red_inventory_id = id;
                            } else {
                                bail!(
                                    "invalid warehouse template, unexpected {name} invisible farp"
                                )
                            }
                        }
                    }
                }
            }
        }
        if blue_inventory_id == 0 {
            bail!("missing warehouse template {}", cfg.blue_production_template)
        }
        if red_inventory_id == 0 {
            bail!("missing warehouse template {}", cfg.red_production_template)
        }
        if blue_default_id == 0 {
            bail!("missing warehouse template BDEFAULT (Fowl 2.0: replace DEFAULT with BDEFAULT+RDEFAULT)")
        }
        if red_default_id == 0 {
            bail!("missing warehouse template RDEFAULT (Fowl 2.0: replace DEFAULT with BDEFAULT+RDEFAULT)")
        }
        if blue_default_plus_id == 0 {
            bail!("missing warehouse template BDEFAULT+")
        }
        if red_default_plus_id == 0 {
            bail!("missing warehouse template RDEFAULT+")
        }
        if blue_all_fueltanks_id == 0 {
            bail!("missing warehouse template BALLFUELTANKS")
        }
        if red_all_fueltanks_id == 0 {
            bail!("missing warehouse template RALLFUELTANKS")
        }
        if blue_default_fueltanks_id == 0 {
            bail!("missing warehouse template BDEFAULTFUELTANKS")
        }
        if red_default_fueltanks_id == 0 {
            bail!("missing warehouse template RDEFAULTFUELTANKS")
        }
        let warehouses = wht
            .warehouses
            .raw_get::<_, Table>("warehouses")
            .context("getting warehouses")?;
        let blue_inventory_plus = if let Some(id) = blue_inventory_plus_id {
            Some(
                warehouses
                    .raw_get(id)
                    .with_context(|| format!("getting BINVENTORY+ warehouse row id {id}"))?,
            )
        } else {
            None
        };
        let red_inventory_plus = if let Some(id) = red_inventory_plus_id {
            Some(
                warehouses
                    .raw_get(id)
                    .with_context(|| format!("getting RINVENTORY+ warehouse row id {id}"))?,
            )
        } else {
            None
        };
        let zone_directives = compile_warehouse_zone_plus_directives(&wht.mission)?;
        Ok(Self {
            blue_inventory: warehouses
                .raw_get(blue_inventory_id)
                .context("getting blue inventory")?,
            red_inventory: warehouses
                .raw_get(red_inventory_id)
                .context("getting red inventory")?,
            blue_inventory_plus,
            red_inventory_plus,
            zone_plus_blue: zone_directives.inventory_blue,
            zone_plus_red: zone_directives.inventory_red,
            zone_ws_inventory_blue: zone_directives.ws_inventory_blue,
            zone_ws_inventory_red: zone_directives.ws_inventory_red,
            zone_default_plus_blue: zone_directives.default_blue,
            zone_default_plus_red: zone_directives.default_red,
            zone_ws_default_blue: zone_directives.ws_default_blue,
            zone_ws_default_red: zone_directives.ws_default_red,
            blue_default: warehouses
                .raw_get(blue_default_id)
                .context("getting BDEFAULT inventory")?,
            red_default: warehouses
                .raw_get(red_default_id)
                .context("getting RDEFAULT inventory")?,
            blue_default_plus: warehouses
                .raw_get(blue_default_plus_id)
                .context("getting BDEFAULT+ inventory")?,
            red_default_plus: warehouses
                .raw_get(red_default_plus_id)
                .context("getting RDEFAULT+ inventory")?,
            blue_all_fueltanks: warehouses
                .raw_get(blue_all_fueltanks_id)
                .context("getting BALLFUELTANKS inventory")?,
            red_all_fueltanks: warehouses
                .raw_get(red_all_fueltanks_id)
                .context("getting RALLFUELTANKS inventory")?,
            blue_default_fueltanks: warehouses
                .raw_get(blue_default_fueltanks_id)
                .context("getting BDEFAULTFUELTANKS inventory")?,
            red_default_fueltanks: warehouses
                .raw_get(red_default_fueltanks_id)
                .context("getting RDEFAULTFUELTANKS inventory")?,
        })
    }

    fn apply(
        &self,
        lua: &'static Lua,
        cfg: &MizCmd,
        base: &mut LoadedMiz,
        warehouse_caps: Option<&campaign_cfg::WarehouseDefaultsFromCfg>,
        bridge_gen: Option<(&VehicleTemplates, &weapon_bridge::WeaponBridgeMap)>,
        weapon_airframes: &VehicleTemplates,
        objective_aircraft_by_side: &HashMap<
            StdString,
            HashMap<Side, HashSet<StdString>>,
        >,
        _droptank_ws_from_weapon_warehouses: &(HashSet<[i32; 4]>, HashSet<[i32; 4]>),
        mult_cfg: &WarehouseStockMultConfig,
    ) -> Result<(
        bfprotocols::fowl_miz_export::FowlMizExport,
        Vec<InventoryAircraftOrphanSanitized>,
        Table<'static>,
        Table<'static>,
    )> {
        fn copy_weapons_subtable(
            lua: &Lua,
            dst_row: &Table,
            src_row: &Table,
            label: &str,
        ) -> Result<()> {
            let w = src_row.raw_get::<_, Table>("weapons").with_context(|| {
                format_compact!("{label}: missing weapons table on generated default row")
            })?;
            dst_row.raw_set("weapons", w.deep_clone(lua)?).with_context(|| {
                format_compact!("{label}: mirror weapons onto template row")
            })?;
            Ok(())
        }
        fn sorted_weapon_ws(opt: &Option<HashSet<[i32; 4]>>) -> Vec<[i32; 4]> {
            let Some(s) = opt else {
                return Vec::new();
            };
            let mut v: Vec<_> = s.iter().copied().collect();
            v.sort_by_key(|w| (w[0], w[1], w[2], w[3]));
            v
        }

        fn sorted_fueltank_ws(set: &HashSet<[i32; 4]>) -> Vec<[i32; 4]> {
            let mut v: Vec<_> =
                set.iter().copied().filter(|w| w[0] == 1 && w[1] == 3).collect();
            v.sort_by_key(|w| (w[0], w[1], w[2], w[3]));
            v
        }

        fn sorted_strings(set: &HashSet<StdString>) -> Vec<StdString> {
            let mut v: Vec<StdString> = set.iter().cloned().collect();
            v.sort();
            v
        }

        fn fuel_usable_by_aircraft(
            vt: &VehicleTemplates,
            br: &weapon_bridge::WeaponBridgeMap,
            side: Side,
            slot_types: &HashSet<StdString>,
            ws: [i32; 4],
        ) -> Vec<StdString> {
            let mut out = Vec::<StdString>::new();
            let mut sorted_types = sorted_strings(slot_types);
            for unit_type in sorted_types.drain(..) {
                let mut used = false;
                let mut one = HashSet::new();
                one.insert(unit_type.clone());
                if br.fueltank_ws_for_aircrafts(&one).contains(&ws) {
                    used = true;
                }
                if !used {
                    if let Some(variants) = vt
                        .payload_variants
                        .get(&side)
                        .and_then(|by_type| by_type.get(unit_type.as_str()))
                    {
                        'payloads: for payload in variants {
                            for descriptor in
                                payload_allowlist::collect_pylon_descriptors(payload)
                            {
                                if br
                                    .ws_types_for_descriptor_or_key_substring(
                                        descriptor.as_str(),
                                    )
                                    .contains(&ws)
                                {
                                    used = true;
                                    break 'payloads;
                                }
                            }
                        }
                    }
                }
                if used {
                    out.push(unit_type);
                }
            }
            out
        }

        fn log_fueltank_ws_list(
            row_name: &str,
            ws_list: &[[i32; 4]],
            vt: &VehicleTemplates,
            br: &weapon_bridge::WeaponBridgeMap,
            side: Side,
            slot_types: &HashSet<StdString>,
        ) {
            for ws in ws_list {
                let item_names = br.display_names_for_ws_type(*ws, 3).join(" | ");
                let item_names = if item_names.is_empty() {
                    "unknown".to_string()
                } else {
                    item_names
                };
                let usable_by = fuel_usable_by_aircraft(vt, br, side, slot_types, *ws);
                let usable_by = if usable_by.is_empty() {
                    "unknown".to_string()
                } else {
                    usable_by.join(",")
                };
                info!(
                    "{row_name}: fuel wsType [{}, {}, {}, {}] item_names={} usable_by={}",
                    ws[0], ws[1], ws[2], ws[3], item_names, usable_by
                );
            }
        }

        fn is_zero_ws(ws: [i32; 4]) -> bool {
            ws == [0, 0, 0, 0]
        }

        fn read_weapon_ws(weapon: &Table) -> Option<[i32; 4]> {
            let wst: Table = weapon.raw_get("wsType").ok()?;
            Some([
                wst.raw_get(1).ok()?,
                wst.raw_get(2).ok()?,
                wst.raw_get(3).ok()?,
                wst.raw_get(4).ok()?,
            ])
        }

        fn weapon_amount_for_ws(row: &Table, needle: [i32; 4]) -> Result<Option<u32>> {
            let Ok(weapons) = row.raw_get::<_, Table>("weapons") else {
                return Ok(None);
            };
            for pair in weapons.clone().pairs::<Value, Table>() {
                let (_, weapon) = pair?;
                let Some(ws) = read_weapon_ws(&weapon) else {
                    continue;
                };
                if ws == needle {
                    let amt = weapon.raw_get::<_, u32>("initialAmount").unwrap_or(0);
                    return Ok(Some(amt));
                }
            }
            Ok(None)
        }

        fn log_agm65_diag(label: &str, row_name: &str, row: &Table) -> Result<()> {
            const AGM_WS: [[i32; 4]; 4] =
                [[4, 4, 8, 273], [4, 4, 8, 274], [4, 4, 32, 3097], [4, 4, 32, 3099]];
            for ws in AGM_WS {
                match weapon_amount_for_ws(row, ws)? {
                    Some(amt) => info!(
                        "diag AGM-65 {label} {row_name}: wsType [{}, {}, {}, {}] present initialAmount={}",
                        ws[0], ws[1], ws[2], ws[3], amt
                    ),
                    None => info!(
                        "diag AGM-65 {label} {row_name}: wsType [{}, {}, {}, {}] missing",
                        ws[0], ws[1], ws[2], ws[3]
                    ),
                }
            }
            Ok(())
        }

        fn log_default_source_rows(
            row_name: &str,
            row: &Table,
            sources_by_ws: &HashMap<[i32; 4], HashSet<StdString>>,
            br: &weapon_bridge::WeaponBridgeMap,
        ) -> Result<()> {
            let Ok(weapons) = row.raw_get::<_, Table>("weapons") else {
                return Ok(());
            };
            let mut rows = Vec::<([i32; 4], u32, StdString, StdString)>::new();
            for pair in weapons.clone().pairs::<Value, Table>() {
                let (_, weapon) = pair?;
                let Some(ws) = read_weapon_ws(&weapon) else {
                    continue;
                };
                if is_zero_ws(ws) {
                    continue;
                }
                let amount = weapon.raw_get::<_, u32>("initialAmount").unwrap_or(0);
                let source_templates = sources_by_ws
                    .get(&ws)
                    .map(|sources| sorted_strings(sources).join(","))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "unknown".to_string());
                let item_names = br.display_names_for_ws_type(ws, 3).join(" | ");
                let item_names = if item_names.is_empty() {
                    "unknown".to_string()
                } else {
                    item_names
                };
                rows.push((ws, amount, item_names, source_templates));
            }
            rows.sort_by_key(|(ws, _, _, _)| (ws[0], ws[1], ws[2], ws[3]));
            for (ws, amount, item_names, source_templates) in rows {
                info!(
                    "{row_name}: final source wsType [{}, {}, {}, {}] item_names={} initialAmount={} source_templates={}",
                    ws[0], ws[1], ws[2], ws[3], item_names, amount, source_templates
                );
            }
            Ok(())
        }

        /// Only rows with `initialAmount > 0` — zeroed-by-validation weapons must not enter the export
        /// or bflib would keep treating them as allowed DCS warehouse rows.
        fn collect_inventory_weapon_ws(row: &Table) -> Result<HashSet<[i32; 4]>> {
            let mut out = HashSet::new();
            let Ok(weapons) = row.raw_get::<_, Table>("weapons") else {
                return Ok(out);
            };
            for pair in weapons.clone().pairs::<Value, Table>() {
                let (_, weapon) = pair?;
                let Some(ws) = read_weapon_ws(&weapon) else {
                    continue;
                };
                if is_zero_ws(ws) {
                    continue;
                }
                let Ok(amt) = weapon.raw_get::<_, u32>("initialAmount") else {
                    continue;
                };
                if amt > 0 {
                    out.insert(ws);
                }
            }
            Ok(out)
        }

        fn module_weapon_ws_by_side(
            vt: &VehicleTemplates,
            br: &weapon_bridge::WeaponBridgeMap,
            side: Side,
        ) -> HashMap<StdString, HashSet<[i32; 4]>> {
            let mut out: HashMap<StdString, HashSet<[i32; 4]>> = HashMap::default();
            let Some(payload_by_type) = vt.payload.get(&side) else {
                return out;
            };
            for (unit_type, payload) in payload_by_type {
                let mut ws_set: HashSet<[i32; 4]> = HashSet::new();
                let desc = payload_allowlist::collect_module_descriptors(payload);
                for d in desc.supported {
                    for ws in br.ws_types_for_descriptor_or_key_substring(d.as_str()) {
                        if !is_zero_ws(ws) {
                            ws_set.insert(ws);
                        }
                    }
                }
                out.insert(unit_type.to_string(), ws_set);
            }
            out
        }

        fn replace_default_weapons_from_allowlist_minus_inventory(
            lua: &Lua,
            row: &Table,
            allowed_ws: &HashSet<[i32; 4]>,
            inventory_row: &Table,
            inventory_positive_block_ws: Option<&HashSet<[i32; 4]>>,
            row_name: &str,
        ) -> Result<usize> {
            // Only wsTypes with real stock in BINVENTORY/RINVENTORY — zero rows are ME placeholders and
            // must not suppress BDEFAULT/RDEFAULT rows (notably external tanks `[1,3,_,_]` absent from hook bridge JSON).
            let inv_ws: HashSet<[i32; 4]> =
                if let Some(expanded) = inventory_positive_block_ws {
                    expanded
                        .iter()
                        .copied()
                        .filter(|ws| !(ws[0] == 1 && ws[1] == 3))
                        .collect()
                } else {
                    campaign_cfg::collect_weapon_ws_types_positive_initial(inventory_row)?
                        .into_iter()
                        .filter(|ws| !(ws[0] == 1 && ws[1] == 3))
                        .collect()
                };
            let mut list: Vec<[i32; 4]> =
                allowed_ws.iter().copied().filter(|ws| !inv_ws.contains(ws)).collect();
            list.sort_by_key(|w| (w[0], w[1], w[2], w[3]));
            let weapons = lua.create_table()?;
            for (i, ws) in list.iter().enumerate() {
                let entry = lua.create_table()?;
                let wst = lua.create_table()?;
                wst.raw_set(1, ws[0])?;
                wst.raw_set(2, ws[1])?;
                wst.raw_set(3, ws[2])?;
                wst.raw_set(4, ws[3])?;
                entry.raw_set("wsType", wst)?;
                entry.raw_set("initialAmount", 0u32)?;
                weapons.raw_set(i + 1, entry)?;
            }
            row.raw_set("weapons", weapons)?;
            info!(
                "{row_name}: rebuilt weapons from allowlist minus inventory (initialAmount>0 only) (rows={})",
                list.len()
            );
            Ok(list.len())
        }

        fn replace_weapons_row_with_ws_list(
            lua: &Lua,
            row: &Table,
            ws_list: &[[i32; 4]],
            initial_amount: u32,
            row_name: &str,
        ) -> Result<usize> {
            let weapons = lua.create_table()?;
            for (i, ws) in ws_list.iter().enumerate() {
                let entry = lua.create_table()?;
                let wst = lua.create_table()?;
                wst.raw_set(1, ws[0])?;
                wst.raw_set(2, ws[1])?;
                wst.raw_set(3, ws[2])?;
                wst.raw_set(4, ws[3])?;
                entry.raw_set("wsType", wst)?;
                entry.raw_set("initialAmount", initial_amount)?;
                weapons.raw_set(i + 1, entry)?;
            }
            row.raw_set("weapons", weapons)?;
            info!(
                "{row_name}: rebuilt fuel diagnostics row (rows={}, initialAmount={})",
                ws_list.len(),
                initial_amount
            );
            Ok(ws_list.len())
        }

        fn validate_inventory_weapons(
            row: &Table,
            allowed_ws: Option<&HashSet<[i32; 4]>>,
            row_name: &str,
            manual_plus_unit: Option<&'static str>,
        ) -> Result<()> {
            let Some(allowed_ws) = allowed_ws else {
                warn!("{row_name}: weapon bridge missing; skipping BINVENTORY/RINVENTORY validation");
                return Ok(());
            };
            let Ok(weapons) = row.raw_get::<_, Table>("weapons") else {
                return Ok(());
            };
            let mut zeroed = 0usize;
            let mut kept_nonzero = 0usize;
            for pair in weapons.clone().pairs::<Value, Table>() {
                let (_, weapon) = pair?;
                let Some(ws) = read_weapon_ws(&weapon) else {
                    continue;
                };
                let Ok(cur) = weapon.raw_get::<_, u32>("initialAmount") else {
                    continue;
                };
                if cur == 0 {
                    continue;
                }
                if allowed_ws.contains(&ws) {
                    kept_nonzero += 1;
                } else {
                    weapon.raw_set("initialAmount", 0u32)?;
                    zeroed += 1;
                    if let Some(plus) = manual_plus_unit {
                        let disp = warehouse_weapon_display_name(&weapon);
                        warn!(
                            "{}: zeroed {} (wsType [{}, {}, {}, {}]) initialAmount={}: not allowed by weapon.miz coalition slot templates; add extra stock on Invisible FARP B/RINVENTORY+ warehouse rows (merged after validation), or add allowlist module links via trigger zone B/RINVENTORY+ (Key=item label, Value=airframe module type). Editor hint: {}.",
                            row_name,
                            disp,
                            ws[0],
                            ws[1],
                            ws[2],
                            ws[3],
                            cur,
                            plus,
                        );
                    } else {
                        info!(
                            "{row_name}: zeroed forbidden weapon wsType [{}, {}, {}, {}] amount {}",
                            ws[0], ws[1], ws[2], ws[3], cur
                        );
                    }
                }
            }
            info!(
                "{row_name}: inventory validation complete (kept_nonzero={}, zeroed={})",
                kept_nonzero, zeroed
            );
            Ok(())
        }

        fn merge_inventory_plus_overwrite(
            lua: &Lua,
            dst_row: &Table,
            plus_row: &Table,
            row_name: &str,
            restrict_appends_to: Option<&HashSet<[i32; 4]>>,
            copy_only_nonzero: bool,
            allow_append: bool,
        ) -> Result<()> {
            let Ok(dst_weapons) = dst_row.raw_get::<_, Table>("weapons") else {
                return Ok(());
            };
            let Ok(plus_weapons) = plus_row.raw_get::<_, Table>("weapons") else {
                return Ok(());
            };
            let mut dst_idx_by_ws: HashMap<[i32; 4], Value> = HashMap::new();
            for pair in dst_weapons.clone().pairs::<Value, Table>() {
                let (k, w) = pair?;
                let Some(ws) = read_weapon_ws(&w) else {
                    continue;
                };
                dst_idx_by_ws.insert(ws, k);
            }
            let mut overridden = 0usize;
            let mut appended = 0usize;
            let mut skipped_zero = 0usize;
            for pair in plus_weapons.clone().pairs::<Value, Table>() {
                let (_, src_w) = pair?;
                let Some(ws) = read_weapon_ws(&src_w) else {
                    continue;
                };
                let Ok(src_amt) = src_w.raw_get::<_, u32>("initialAmount") else {
                    continue;
                };
                if copy_only_nonzero && src_amt == 0 {
                    skipped_zero += 1;
                    continue;
                }
                if let Some(k) = dst_idx_by_ws.get(&ws) {
                    let dst_w = dst_weapons.raw_get::<_, Table>(k.clone())?;
                    let prev = dst_w.raw_get::<_, u32>("initialAmount").unwrap_or(0);
                    if ws[0] == 1 && ws[1] == 3 && src_amt == 0 && prev > 0 {
                        info!(
                            "{row_name}: keep fuel wsType [{}, {}, {}, {}] amount {} (skip + row zero placeholder)",
                            ws[0], ws[1], ws[2], ws[3], prev
                        );
                        continue;
                    }
                    dst_w.raw_set("initialAmount", src_amt)?;
                    overridden += 1;
                    info!(
                        "{row_name}: override wsType [{}, {}, {}, {}] {} -> {} from + row",
                        ws[0], ws[1], ws[2], ws[3], prev, src_amt
                    );
                } else {
                    if !allow_append {
                        continue;
                    }
                    if let Some(allowed) = restrict_appends_to {
                        if !allowed.contains(&ws) {
                            continue;
                        }
                    }
                    let mut new_idx = dst_weapons.raw_len() + 1;
                    if new_idx == 0 {
                        new_idx = 1;
                    }
                    dst_weapons.raw_set(new_idx, src_w.deep_clone(lua)?)?;
                    appended += 1;
                    info!(
                        "{row_name}: append wsType [{}, {}, {}, {}] amount {} from + row",
                        ws[0], ws[1], ws[2], ws[3], src_amt
                    );
                }
            }
            info!(
                "{row_name}: + merge complete (overridden={}, appended={}, skipped_zero={})",
                overridden, appended, skipped_zero
            );
            Ok(())
        }

        fn zero_default_weapons_present_in_positive_inventory(
            default_row: &Table,
            inventory_row: &Table,
            inventory_positive_block_ws: Option<&HashSet<[i32; 4]>>,
            default_name: &str,
            inventory_name: &str,
        ) -> Result<usize> {
            let inv_ws = if let Some(expanded) = inventory_positive_block_ws {
                expanded.clone()
            } else {
                collect_inventory_weapon_ws(inventory_row)?
            };
            if inv_ws.is_empty() {
                info!(
                    "{default_name}: no positive wsTypes in {inventory_name}; skip default/inventory de-dup"
                );
                return Ok(0);
            }
            let Ok(default_weapons) = default_row.raw_get::<_, Table>("weapons") else {
                return Ok(0);
            };
            let mut zeroed = 0usize;
            for pair in default_weapons.clone().pairs::<Value, Table>() {
                let (_, weapon) = pair?;
                let Some(ws) = read_weapon_ws(&weapon) else {
                    continue;
                };
                if is_zero_ws(ws) || !inv_ws.contains(&ws) {
                    continue;
                }
                let Ok(cur) = weapon.raw_get::<_, u32>("initialAmount") else {
                    continue;
                };
                if cur == 0 {
                    continue;
                }
                weapon.raw_set("initialAmount", 0u32)?;
                zeroed += 1;
                info!(
                    "{default_name}: zeroed wsType [{}, {}, {}, {}] amount {} (present with nonzero stock in {inventory_name})",
                    ws[0], ws[1], ws[2], ws[3], cur
                );
            }
            info!(
                "{default_name}: final de-dup vs {inventory_name} complete (zeroed={})",
                zeroed
            );
            Ok(zeroed)
        }

        let inventory_aircraft_orphans_cleared = sanitize_both_production_inventory_aircraft_templates(
            weapon_airframes,
            &self.blue_inventory,
            &self.red_inventory,
        )?;
        let mut blue_inv_name_ws =
            collect_inventory_weapon_display_name_to_ws(&self.blue_inventory)?;
        let mut red_inv_name_ws =
            collect_inventory_weapon_display_name_to_ws(&self.red_inventory)?;
        let mut blue_inv_ws = collect_inventory_weapon_ws_set(&self.blue_inventory)?;
        let mut red_inv_ws = collect_inventory_weapon_ws_set(&self.red_inventory)?;
        merge_inventory_plus_weapon_resolution_hints(
            &mut blue_inv_name_ws,
            &mut blue_inv_ws,
            self.blue_inventory_plus.as_ref(),
        )?;
        merge_inventory_plus_weapon_resolution_hints(
            &mut red_inv_name_ws,
            &mut red_inv_ws,
            self.red_inventory_plus.as_ref(),
        )?;
        let (blue_default_plus_nm, blue_default_plus_ws) =
            build_default_plus_resolution_maps(Some(&self.blue_default_plus))?;
        let (red_default_plus_nm, red_default_plus_ws) =
            build_default_plus_resolution_maps(Some(&self.red_default_plus))?;

        let blue_master = self.blue_default.deep_clone(lua)?;
        let red_master = self.red_default.deep_clone(lua)?;
        let weapon_bridge_used = bridge_gen.is_some();
        let mut blue_allowed_ws: Option<HashSet<[i32; 4]>> = None;
        let mut red_allowed_ws: Option<HashSet<[i32; 4]>> = None;
        let mut blue_inventory_allowed_ws: Option<HashSet<[i32; 4]>> = None;
        let mut red_inventory_allowed_ws: Option<HashSet<[i32; 4]>> = None;
        let mut blue_inventory_positive_block_ws: Option<HashSet<[i32; 4]>> = None;
        let mut red_inventory_positive_block_ws: Option<HashSet<[i32; 4]>> = None;
        let mut blue_strip_ws: HashSet<[i32; 4]> = HashSet::new();
        let mut red_strip_ws: HashSet<[i32; 4]> = HashSet::new();
        let mut blue_default_sources: HashMap<[i32; 4], HashSet<StdString>> =
            HashMap::new();
        let mut red_default_sources: HashMap<[i32; 4], HashSet<StdString>> =
            HashMap::new();
        let mut blue_fowl_export_union: Option<HashSet<[i32; 4]>> = None;
        let mut red_fowl_export_union: Option<HashSet<[i32; 4]>> = None;
        let mut blue_inventory_zone_module_ws: HashMap<
            StdString,
            HashMap<StdString, Vec<[i32; 4]>>,
        > = HashMap::default();
        let mut red_inventory_zone_module_ws: HashMap<
            StdString,
            HashMap<StdString, Vec<[i32; 4]>>,
        > = HashMap::default();
        let mut objective_defaults: HashMap<
            StdString,
            bfprotocols::fowl_miz_export::ObjectiveWarehouseDefaults,
        > = HashMap::default();
        if let Some((vt, br)) = bridge_gen {
            let blue_module_ws = module_weapon_ws_by_side(vt, br, Side::Blue);
            let red_module_ws = module_weapon_ws_by_side(vt, br, Side::Red);
            for (objective_name, by_side) in objective_aircraft_by_side {
                let blue_aircraft =
                    by_side.get(&Side::Blue).cloned().unwrap_or_else(HashSet::default);
                let red_aircraft =
                    by_side.get(&Side::Red).cloned().unwrap_or_else(HashSet::default);
                let mut blue_weapon_ws: HashSet<[i32; 4]> = HashSet::default();
                let mut red_weapon_ws: HashSet<[i32; 4]> = HashSet::default();
                blue_weapon_ws.extend(br.weapon_ws_for_aircrafts(&blue_aircraft));
                red_weapon_ws.extend(br.weapon_ws_for_aircrafts(&red_aircraft));
                for unit_type in &blue_aircraft {
                    if let Some(ws) = blue_module_ws.get(unit_type) {
                        blue_weapon_ws.extend(ws.iter().copied());
                    }
                }
                for unit_type in &red_aircraft {
                    if let Some(ws) = red_module_ws.get(unit_type) {
                        red_weapon_ws.extend(ws.iter().copied());
                    }
                }
                objective_defaults.insert(
                    objective_name.clone(),
                    bfprotocols::fowl_miz_export::ObjectiveWarehouseDefaults {
                        blue_aircraft: sorted_strings(&blue_aircraft),
                        red_aircraft: sorted_strings(&red_aircraft),
                        blue_weapon_ws: sorted_weapon_ws(&Some(blue_weapon_ws)),
                        red_weapon_ws: sorted_weapon_ws(&Some(red_weapon_ws)),
                    },
                );
            }
            info!(
                "fowl export objective defaults prepared: {} objective(s)",
                objective_defaults.len()
            );
            let configured_empty_fueltanks =
                warehouse_caps.map(|caps| caps.fueltanks_empty).unwrap_or(false);
            let template_fuel_ws_for_descriptor = |descriptor: &str| -> Option<[i32; 4]> {
                if let Some(ws) = br.ws_type_for_descriptor(descriptor) {
                    if ws[0] == 1 && ws[1] == 3 {
                        return Some(ws);
                    }
                }
                let fuel: HashSet<[i32; 4]> = br
                    .ws_types_for_descriptor_or_key_substring(descriptor)
                    .into_iter()
                    .filter(|ws| ws[0] == 1 && ws[1] == 3)
                    .collect();
                if fuel.len() == 1 {
                    return fuel.into_iter().next();
                }
                None
            };
            let bdesc = vt.payload_warehouse_bridge_descriptor_keys(br, Side::Blue);
            let rdesc = vt.payload_warehouse_bridge_descriptor_keys(br, Side::Red);
            let blue_slot_types_hs: HashSet<StdString> =
                vt.slot_unit_types(Side::Blue).into_iter().collect();
            let red_slot_types_hs: HashSet<StdString> =
                vt.slot_unit_types(Side::Red).into_iter().collect();
            let blue_payload_types_hs: HashSet<StdString> =
                vt.payload_unit_types(Side::Blue).into_iter().collect();
            let red_payload_types_hs: HashSet<StdString> =
                vt.payload_unit_types(Side::Red).into_iter().collect();
            let has_payload_sidecar = br.has_template_payload_ws();
            let lua_blue_pylon_ws = vt.payload_ws_for_slot_types(br, Side::Blue, true);
            let lua_red_pylon_ws = vt.payload_ws_for_slot_types(br, Side::Red, true);
            let blue_tmpl_ord = br.template_ordnance_allow_ws(
                "blue",
                &blue_slot_types_hs,
                &lua_blue_pylon_ws,
            );
            let red_tmpl_ord = br.template_ordnance_allow_ws(
                "red",
                &red_slot_types_hs,
                &lua_red_pylon_ws,
            );
            let blue_tmpl_ord_seed_exp: HashSet<[i32; 4]> = if blue_tmpl_ord.is_empty() {
                HashSet::new()
            } else {
                br.expand_ws_alias_family(&blue_tmpl_ord)
            };
            let red_tmpl_ord_seed_exp: HashSet<[i32; 4]> = if red_tmpl_ord.is_empty() {
                HashSet::new()
            } else {
                br.expand_ws_alias_family(&red_tmpl_ord)
            };
            let tmpl_ordnance_effective =
                !blue_tmpl_ord.is_empty() || !red_tmpl_ord.is_empty();
            if tmpl_ordnance_effective {
                info!(
                    "warehouse allowlist: ordnance from Lua pylons ∪ fowl_weapon_payload_ws sidecar, ∩ weapon_ws_by_aircraft; strip = payload restricted-only vote (not raw restricted ws union)"
                );
            }
            let mut blue_template_fueltank_ws =
                br.fueltank_ws_for_aircrafts(&vt.slot_unit_types(Side::Blue));
            let mut red_template_fueltank_ws =
                br.fueltank_ws_for_aircrafts(&vt.slot_unit_types(Side::Red));
            if blue_template_fueltank_ws.is_empty() {
                for d in vt.payload_pylon_union_descriptors(Side::Blue) {
                    if let Some(ws) = template_fuel_ws_for_descriptor(d.as_str()) {
                        blue_template_fueltank_ws.insert(ws);
                    }
                }
            }
            if red_template_fueltank_ws.is_empty() {
                for d in vt.payload_pylon_union_descriptors(Side::Red) {
                    if let Some(ws) = template_fuel_ws_for_descriptor(d.as_str()) {
                        red_template_fueltank_ws.insert(ws);
                    }
                }
            }
            let mut bws = HashSet::new();
            let mut rws = HashSet::new();
            if !blue_tmpl_ord.is_empty() {
                bws = blue_tmpl_ord;
            } else {
                for d in &bdesc {
                    for ws in br.ws_types_for_descriptor_or_key_substring(d.as_str()) {
                        if !is_zero_ws(ws) {
                            bws.insert(ws);
                        }
                    }
                }
            }
            if !red_tmpl_ord.is_empty() {
                rws = red_tmpl_ord;
            } else {
                for d in &rdesc {
                    for ws in br.ws_types_for_descriptor_or_key_substring(d.as_str()) {
                        if !is_zero_ws(ws) {
                            rws.insert(ws);
                        }
                    }
                }
            }
            // Fuel tanks are stores (`[1,3,_,_]`) and many payload keys do not survive vote logic.
            // Seed fuel directly from all pylon descriptors, then apply the same strip/footprint filters below.
            for d in vt.payload_pylon_union_descriptors(Side::Blue) {
                for ws in br.ws_types_for_descriptor_or_key_substring(d.as_str()) {
                    if ws[0] == 1 && ws[1] == 3 {
                        bws.insert(ws);
                    }
                }
            }
            for d in vt.payload_pylon_union_descriptors(Side::Red) {
                for ws in br.ws_types_for_descriptor_or_key_substring(d.as_str()) {
                    if ws[0] == 1 && ws[1] == 3 {
                        rws.insert(ws);
                    }
                }
            }
            // Ensure fuel from template aircraft map is seeded even when payload descriptors are sparse.
            bws.extend(blue_template_fueltank_ws.iter().copied());
            rws.extend(red_template_fueltank_ws.iter().copied());
            info!(
                "fuel diagnostics: Fueltanks_empty={} ignored for ALLFUELTANKS/default fuel workflow (no full/empty auto split)",
                configured_empty_fueltanks
            );
            let b_before_template = sorted_fueltank_ws(&bws).len();
            let r_before_template = sorted_fueltank_ws(&rws).len();
            if !blue_template_fueltank_ws.is_empty() {
                bws.retain(|ws| {
                    !(ws[0] == 1 && ws[1] == 3) || blue_template_fueltank_ws.contains(ws)
                });
            } else {
                warn!(
                    "fuel diagnostics: skipped blue template fuel filter (bridge aircraft map and payload fallback both empty)"
                );
            }
            if !red_template_fueltank_ws.is_empty() {
                rws.retain(|ws| {
                    !(ws[0] == 1 && ws[1] == 3) || red_template_fueltank_ws.contains(ws)
                });
            } else {
                warn!(
                    "fuel diagnostics: skipped red template fuel filter (bridge aircraft map and payload fallback both empty)"
                );
            }
            let b_after_template = sorted_fueltank_ws(&bws).len();
            let r_after_template = sorted_fueltank_ws(&rws).len();
            if b_before_template != b_after_template
                || r_before_template != r_after_template
            {
                info!(
                    "fuel diagnostics: template-aircraft filter removed fuel wsTypes absent in active slot templates (blue -{}, red -{})",
                    b_before_template.saturating_sub(b_after_template),
                    r_before_template.saturating_sub(r_after_template)
                );
            }
            info!(
                    "fuel diagnostics: seeded from payload bridge + weapon warehouse templates (all full/empty aliases, filtered by template aircraft fuel map) -> blue={} red={}",
                sorted_fueltank_ws(&bws).len(),
                sorted_fueltank_ws(&rws).len()
            );
            let blue_all_fuel_ws = sorted_fueltank_ws(&bws);
            let red_all_fuel_ws = sorted_fueltank_ws(&rws);
            log_fueltank_ws_list(
                "BALLFUELTANKS",
                &blue_all_fuel_ws,
                vt,
                br,
                Side::Blue,
                &blue_slot_types_hs,
            );
            log_fueltank_ws_list(
                "RALLFUELTANKS",
                &red_all_fuel_ws,
                vt,
                br,
                Side::Red,
                &red_slot_types_hs,
            );
            let all_fuel_preview_amount = warehouse_caps
                .map(|caps| caps.fueltanks)
                .filter(|amount| *amount > 0)
                .unwrap_or(1);
            replace_weapons_row_with_ws_list(
                lua,
                &self.blue_all_fueltanks,
                &blue_all_fuel_ws,
                all_fuel_preview_amount,
                "BALLFUELTANKS",
            )?;
            replace_weapons_row_with_ws_list(
                lua,
                &self.red_all_fueltanks,
                &red_all_fuel_ws,
                all_fuel_preview_amount,
                "RALLFUELTANKS",
            )?;
            info!(
                "fuel diagnostics: updated ALLFUELTANKS template rows (blue={} red={}); B/RDEFAULTFUELTANKS kept manual",
                blue_all_fuel_ws.len(),
                red_all_fuel_ws.len()
            );
            blue_strip_ws = vt.payload_restricted_only_ws_for_slot_types(br, Side::Blue);
            red_strip_ws = vt.payload_restricted_only_ws_for_slot_types(br, Side::Red);
            // Pull ws out of the strip set when it is plausibly carried: Lua pylons, sidecar pylons, or
            // `weapon_ws_by_aircraft` for slotted types (no `aircraft_by_ws` reverse). Narrow vs pruning with
            // “allowlist overrides strip” (that kept every `vote ∪ anchor` ws including true restricted-only junk).
            let mut blue_strip_rescue_seed =
                vt.payload_ws_for_slot_types(br, Side::Blue, true);
            let mut red_strip_rescue_seed =
                vt.payload_ws_for_slot_types(br, Side::Red, true);
            if has_payload_sidecar {
                blue_strip_rescue_seed.extend(
                    br.template_pylon_ws_union_for_side("blue", &blue_slot_types_hs),
                );
                red_strip_rescue_seed.extend(
                    br.template_pylon_ws_union_for_side("red", &red_slot_types_hs),
                );
            }
            blue_strip_rescue_seed.extend(
                br.weapon_ws_for_aircraft_keys_only(&blue_slot_types_hs)
                    .into_iter()
                    .filter(|w| !is_zero_ws(*w)),
            );
            red_strip_rescue_seed.extend(
                br.weapon_ws_for_aircraft_keys_only(&red_slot_types_hs)
                    .into_iter()
                    .filter(|w| !is_zero_ws(*w)),
            );
            let blue_strip_rescue_ws = br.expand_ws_alias_family(&blue_strip_rescue_seed);
            let red_strip_rescue_ws = br.expand_ws_alias_family(&red_strip_rescue_seed);
            blue_strip_ws.retain(|ws| !blue_strip_rescue_ws.contains(ws));
            red_strip_ws.retain(|ws| !red_strip_rescue_ws.contains(ws));
            for ws in &blue_strip_ws {
                if !blue_tmpl_ord_seed_exp.is_empty() {
                    if blue_tmpl_ord_seed_exp.contains(ws) {
                        continue;
                    }
                    let mut one = HashSet::new();
                    one.insert(*ws);
                    if br
                        .expand_ws_alias_family(&one)
                        .iter()
                        .any(|x| blue_tmpl_ord_seed_exp.contains(x))
                    {
                        continue;
                    }
                }
                bws.remove(ws);
            }
            for ws in &red_strip_ws {
                if !red_tmpl_ord_seed_exp.is_empty() {
                    if red_tmpl_ord_seed_exp.contains(ws) {
                        continue;
                    }
                    let mut one = HashSet::new();
                    one.insert(*ws);
                    if br
                        .expand_ws_alias_family(&one)
                        .iter()
                        .any(|x| red_tmpl_ord_seed_exp.contains(x))
                    {
                        continue;
                    }
                }
                rws.remove(ws);
            }
            let mut blue_payload_deny_seed: HashSet<[i32; 4]> =
                blue_strip_ws.iter().copied().collect();
            blue_payload_deny_seed.extend(
                br.template_restricted_ws_union_for_side("blue", &blue_slot_types_hs),
            );
            let mut red_payload_deny_seed: HashSet<[i32; 4]> =
                red_strip_ws.iter().copied().collect();
            red_payload_deny_seed.extend(
                br.template_restricted_ws_union_for_side("red", &red_slot_types_hs),
            );
            let blue_payload_deny = br.expand_ws_alias_family(&blue_payload_deny_seed);
            let red_payload_deny = br.expand_ws_alias_family(&red_payload_deny_seed);
            let payload_ws_blocked = |w: [i32; 4], deny: &HashSet<[i32; 4]>| -> bool {
                if deny.contains(&w) {
                    return true;
                }
                let mut one = HashSet::new();
                one.insert(w);
                br.expand_ws_alias_family(&one).iter().any(|x| deny.contains(x))
            };
            info!(
                "fuel diagnostics: after restricted strip -> blue={} red={}",
                sorted_fueltank_ws(&bws).len(),
                sorted_fueltank_ws(&rws).len()
            );
            // Drop wsTypes that appear only on the opposite coalition’s weapon templates (BINVENTORY is often universal).
            let blue_foot_ws = vt.payload_footprint_weapon_ws(br, Side::Blue);
            let red_foot_ws = vt.payload_footprint_weapon_ws(br, Side::Red);
            let n_bws = bws.len();
            let n_rws = rws.len();
            bws.retain(|w| !(red_foot_ws.contains(w) && !blue_foot_ws.contains(w)));
            rws.retain(|w| !(blue_foot_ws.contains(w) && !red_foot_ws.contains(w)));
            let blue_fuel_after_footprint = sorted_fueltank_ws(&bws);
            let red_fuel_after_footprint = sorted_fueltank_ws(&rws);
            info!(
                "fuel diagnostics: after coalition footprint -> blue={} red={}",
                blue_fuel_after_footprint.len(),
                red_fuel_after_footprint.len()
            );
            info!(
                "fuel diagnostics wsType lists: blue={:?} red={:?}",
                blue_fuel_after_footprint, red_fuel_after_footprint
            );
            if bws.len() != n_bws || rws.len() != n_rws {
                info!(
                    "warehouse allowlist: dropped opposite-coalition-only wsTypes (blue −{} , red −{})",
                    n_bws.saturating_sub(bws.len()),
                    n_rws.saturating_sub(rws.len())
                );
            }
            if bws.is_empty() {
                warn!(
                    "blue warehouse allowlist empty (payload vote, after strip/footprint); BDEFAULT will not use empty-set allowlist filter"
                );
            }
            if rws.is_empty() {
                warn!(
                    "red warehouse allowlist empty (payload vote, after strip/footprint); RDEFAULT will not use empty-set allowlist filter"
                );
            }
            let bdesc_mapped = bdesc
                .iter()
                .filter(|d| {
                    !br.ws_types_for_descriptor_or_key_substring(d.as_str()).is_empty()
                })
                .count();
            let rdesc_mapped = rdesc
                .iter()
                .filter(|d| {
                    !br.ws_types_for_descriptor_or_key_substring(d.as_str()).is_empty()
                })
                .count();
            let low_conf_allowlist = bdesc.len() <= 4 && rdesc.len() <= 4;
            if low_conf_allowlist {
                let blue_fuel_ws: HashSet<[i32; 4]> =
                    bws.iter().copied().filter(|ws| ws[0] == 1 && ws[1] == 3).collect();
                let red_fuel_ws: HashSet<[i32; 4]> =
                    rws.iter().copied().filter(|ws| ws[0] == 1 && ws[1] == 3).collect();
                let blue_slot_types = vt.slot_unit_types(Side::Blue);
                let red_slot_types = vt.slot_unit_types(Side::Red);
                let mut blue_from_bridge = br
                    .weapon_ws_for_aircraft_keys_only(&blue_slot_types)
                    .into_iter()
                    .filter(|ws| !is_zero_ws(*ws))
                    .filter(|ws| ws[0] == 4 && ((4..=8).contains(&ws[1]) || ws[1] == 15))
                    .collect::<HashSet<[i32; 4]>>();
                let mut red_from_bridge = br
                    .weapon_ws_for_aircraft_keys_only(&red_slot_types)
                    .into_iter()
                    .filter(|ws| !is_zero_ws(*ws))
                    .filter(|ws| ws[0] == 4 && ((4..=8).contains(&ws[1]) || ws[1] == 15))
                    .collect::<HashSet<[i32; 4]>>();
                blue_from_bridge.extend(blue_fuel_ws.iter().copied());
                red_from_bridge.extend(red_fuel_ws.iter().copied());
                for ws in &blue_strip_ws {
                    blue_from_bridge.remove(ws);
                }
                for ws in &red_strip_ws {
                    red_from_bridge.remove(ws);
                }
                let _n_bws0 = bws.len();
                let _n_rws0 = rws.len();
                let n_bb = blue_from_bridge.len();
                let n_rb = red_from_bridge.len();
                if tmpl_ordnance_effective {
                    // Capped merge: bridge ∩ cap ∩ **alias-touch** expand(seed). Seed = Lua pylons ∪ sidecar ∪
                    // slot payload pylons (`payload_pylon_only_footprint`). Exact `gate.contains(w)` was +0 in
                    // Caucasus1987 (AIM-54 / AGM variants vs bridge `wsType`); family expand on each `w` fixes it.
                    let mut blue_pylon_seed: HashSet<[i32; 4]> =
                        lua_blue_pylon_ws.iter().copied().collect();
                    blue_pylon_seed.extend(
                        br.template_pylon_ws_union_for_side("blue", &blue_slot_types_hs),
                    );
                    blue_pylon_seed.extend(
                        vt.payload_pylon_only_footprint_weapon_ws(br, Side::Blue),
                    );
                    let mut red_pylon_seed: HashSet<[i32; 4]> =
                        lua_red_pylon_ws.iter().copied().collect();
                    red_pylon_seed.extend(
                        br.template_pylon_ws_union_for_side("red", &red_slot_types_hs),
                    );
                    red_pylon_seed
                        .extend(vt.payload_pylon_only_footprint_weapon_ws(br, Side::Red));
                    let cap_b = br.weapon_ws_for_aircraft_keys_only(&blue_slot_types_hs);
                    let cap_r = br.weapon_ws_for_aircraft_keys_only(&red_slot_types_hs);
                    let mut blue_capped = 0usize;
                    let mut red_capped = 0usize;
                    let bridge_touches = |w: [i32; 4],
                                          gate: &HashSet<[i32; 4]>|
                     -> bool {
                        if gate.contains(&w) {
                            return true;
                        }
                        let mut one = HashSet::new();
                        one.insert(w);
                        br.expand_ws_alias_family(&one).iter().any(|x| gate.contains(x))
                    };
                    let merge_capped_pass =
                        |bws: &mut HashSet<[i32; 4]>,
                         rws: &mut HashSet<[i32; 4]>,
                         blue_seed: &HashSet<[i32; 4]>,
                         red_seed: &HashSet<[i32; 4]>,
                         blue_n: &mut usize,
                         red_n: &mut usize,
                         run_blue: bool,
                         run_red: bool| {
                            let bg = br.expand_ws_alias_family(blue_seed);
                            let rg = br.expand_ws_alias_family(red_seed);
                            if run_blue {
                                for w in &blue_from_bridge {
                                    if payload_ws_blocked(*w, &blue_payload_deny) {
                                        continue;
                                    }
                                    if !cap_b.contains(w) || !bridge_touches(*w, &bg) {
                                        continue;
                                    }
                                    if bws.insert(*w) {
                                        *blue_n += 1;
                                    }
                                }
                            }
                            if run_red {
                                for w in &red_from_bridge {
                                    if payload_ws_blocked(*w, &red_payload_deny) {
                                        continue;
                                    }
                                    if !cap_r.contains(w) || !bridge_touches(*w, &rg) {
                                        continue;
                                    }
                                    if rws.insert(*w) {
                                        *red_n += 1;
                                    }
                                }
                            }
                        };
                    merge_capped_pass(
                        &mut bws,
                        &mut rws,
                        &blue_pylon_seed,
                        &red_pylon_seed,
                        &mut blue_capped,
                        &mut red_capped,
                        blue_tmpl_ord_seed_exp.is_empty(),
                        red_tmpl_ord_seed_exp.is_empty(),
                    );
                    let blue_after_first = blue_capped;
                    let red_after_first = red_capped;
                    if blue_after_first == 0 || red_after_first == 0 {
                        // Keep sparse rescue gated by slot/pylon-derived seeds only.
                        // Pulling wsTypes from B/RDEFAULT rows here reintroduces cross-coalition and restricted stores.
                        let blue_seed2 = blue_pylon_seed.clone();
                        let red_seed2 = red_pylon_seed.clone();
                        merge_capped_pass(
                            &mut bws,
                            &mut rws,
                            &blue_seed2,
                            &red_seed2,
                            &mut blue_capped,
                            &mut red_capped,
                            blue_after_first == 0,
                            red_after_first == 0,
                        );
                    }
                    warn!(
                        "warehouse allowlist: sparse payload bridge keys (blue {}/{}, red {}/{}) — template ordnance path active; capped bridge merge (seed=Lua∪sidecar∪slot pylons, alias ∩ cap) blue +{} (of {}), red +{} (of {})",
                        bdesc_mapped,
                        bdesc.len(),
                        rdesc_mapped,
                        rdesc.len(),
                        blue_capped,
                        n_bb,
                        red_capped,
                        n_rb
                    );
                } else {
                    warn!(
                        "warehouse allowlist fallback: sparse payload bridge keys (blue {}/{}, red {}/{}) -> using bridge module ws map minus payload-restricted ws (blue={} red={})",
                        bdesc_mapped,
                        bdesc.len(),
                        rdesc_mapped,
                        rdesc.len(),
                        blue_from_bridge.len(),
                        red_from_bridge.len()
                    );
                    bws = blue_from_bridge;
                    rws = red_from_bridge;
                }
            }
            bws.retain(|w| !payload_ws_blocked(*w, &blue_payload_deny));
            rws.retain(|w| !payload_ws_blocked(*w, &red_payload_deny));
            // B/RDEFAULT: per-airframe `template.restricted` + pylon evidence. B/RINVENTORY cap:
            // `weapon_ws_for_aircrafts` (includes `aircraft_by_ws`). Carrier `TTDN*` prune uses the same
            // per-type template cap as `side_cap_respects_template_restricted`; DEP FARP uses keys + fuel + filtered +.
            let mut blue_default_deny_exact =
                br.template_restricted_ws_union_for_side("blue", &blue_slot_types_hs);
            blue_default_deny_exact.extend(blue_strip_ws.iter().copied());
            let blue_default_deny = br.expand_ws_alias_family(&blue_default_deny_exact);
            let mut red_default_deny_exact =
                br.template_restricted_ws_union_for_side("red", &red_slot_types_hs);
            red_default_deny_exact.extend(red_strip_ws.iter().copied());
            let red_default_deny = br.expand_ws_alias_family(&red_default_deny_exact);
            let default_ws_blocked = |w: [i32; 4], deny: &HashSet<[i32; 4]>| -> bool {
                if deny.contains(&w) {
                    return true;
                }
                let mut one = HashSet::new();
                one.insert(w);
                br.expand_ws_alias_family(&one).iter().any(|x| deny.contains(x))
            };
            let side_cap_inventory_no_restricted = |types: &HashSet<StdString>| {
                let mut out = HashSet::<[i32; 4]>::new();
                let mut sources = HashMap::<[i32; 4], HashSet<StdString>>::new();
                for unit_type in types {
                    let mut one = HashSet::new();
                    one.insert(unit_type.clone());
                    for ws in br.weapon_ws_for_aircrafts(&one) {
                        if !(ws[0] == 4 && ((4..=8).contains(&ws[1]) || ws[1] == 15)) {
                            continue;
                        }
                        out.insert(ws);
                        sources.entry(ws).or_default().insert(unit_type.clone());
                    }
                }
                (out, sources)
            };
            let side_cap_respects_template_restricted =
                |side: Side, side_name: &str, types: &HashSet<StdString>| {
                    let mut out = HashSet::<[i32; 4]>::new();
                    let mut sources = HashMap::<[i32; 4], HashSet<StdString>>::new();
                    for unit_type in types {
                        let restricted =
                            br.template_restricted_ws_for_side_type(side_name, unit_type);
                        let pylon_ws = br.expand_ws_alias_family(
                            &vt.payload_pylon_ws_for_unit_type(
                                br,
                                side,
                                unit_type.as_str(),
                            ),
                        );
                        let mut candidates = br.weapon_ws_for_aircraft_key_only(unit_type);
                        candidates.extend(pylon_ws.iter().copied());
                        for ws in candidates {
                            if !(ws[0] == 4
                                && ((4..=8).contains(&ws[1]) || ws[1] == 15))
                            {
                                continue;
                            }
                            if restricted.contains(&ws) && !pylon_ws.contains(&ws) {
                                continue;
                            }
                            out.insert(ws);
                            sources.entry(ws).or_default().insert(unit_type.clone());
                        }
                    }
                    (out, sources)
                };
            let (mut blue_for_default, mut blue_side_template_sources) =
                side_cap_respects_template_restricted(
                    Side::Blue,
                    "blue",
                    &blue_payload_types_hs,
                );
            let (mut red_for_default, mut red_side_template_sources) =
                side_cap_respects_template_restricted(
                    Side::Red,
                    "red",
                    &red_payload_types_hs,
                );
            let (mut blue_for_inv, _blue_inv_sources) =
                side_cap_inventory_no_restricted(&blue_payload_types_hs);
            let (mut red_for_inv, _red_inv_sources) =
                side_cap_inventory_no_restricted(&red_payload_types_hs);
            merge_default_zone_plus_into_allowlist(
                Some(br),
                Side::Blue,
                &self.zone_default_plus_blue,
                &blue_default_plus_nm,
                &blue_default_plus_ws,
                &mut blue_for_default,
                &mut blue_default_sources,
                "BDEFAULT+",
            )?;
            merge_default_zone_plus_into_allowlist(
                Some(br),
                Side::Red,
                &self.zone_default_plus_red,
                &red_default_plus_nm,
                &red_default_plus_ws,
                &mut red_for_default,
                &mut red_default_sources,
                "RDEFAULT+",
            )?;
            merge_inventory_zone_plus_into_allowlist(
                Some(br),
                Some(vt),
                Side::Blue,
                &self.zone_plus_blue,
                &blue_inv_name_ws,
                Some(&blue_inv_ws),
                &mut blue_for_inv,
                Some(&mut blue_inventory_zone_module_ws),
                "BINVENTORY+",
            )?;
            merge_inventory_zone_plus_into_allowlist(
                Some(br),
                Some(vt),
                Side::Red,
                &self.zone_plus_red,
                &red_inv_name_ws,
                Some(&red_inv_ws),
                &mut red_for_inv,
                Some(&mut red_inventory_zone_module_ws),
                "RINVENTORY+",
            )?;
            let blue_default_fuel_ws =
                campaign_cfg::collect_weapon_ws_types_positive_initial(
                    &self.blue_default_fueltanks,
                )?
                .into_iter()
                .filter(|ws| ws[0] == 1 && ws[1] == 3)
                .collect::<HashSet<[i32; 4]>>();
            let red_default_fuel_ws =
                campaign_cfg::collect_weapon_ws_types_positive_initial(
                    &self.red_default_fueltanks,
                )?
                .into_iter()
                .filter(|ws| ws[0] == 1 && ws[1] == 3)
                .collect::<HashSet<[i32; 4]>>();
            let blue_default_fuel_list = sorted_fueltank_ws(&blue_default_fuel_ws);
            let red_default_fuel_list = sorted_fueltank_ws(&red_default_fuel_ws);
            log_fueltank_ws_list(
                "BDEFAULTFUELTANKS",
                &blue_default_fuel_list,
                vt,
                br,
                Side::Blue,
                &blue_slot_types_hs,
            );
            log_fueltank_ws_list(
                "RDEFAULTFUELTANKS",
                &red_default_fuel_list,
                vt,
                br,
                Side::Red,
                &red_slot_types_hs,
            );
            blue_strip_ws.retain(|ws| !blue_default_fuel_ws.contains(ws));
            red_strip_ws.retain(|ws| !red_default_fuel_ws.contains(ws));
            info!(
                "fuel diagnostics: manual DEFAULTFUELTANKS source -> blue={} red={} (auto fuel excluded from B/RDEFAULT)",
                blue_default_fuel_ws.len(),
                red_default_fuel_ws.len()
            );
            for ws in blue_default_fuel_ws.iter().copied() {
                blue_for_default.insert(ws);
                blue_for_inv.insert(ws);
                blue_side_template_sources
                    .entry(ws)
                    .or_default()
                    .insert("BDEFAULTFUELTANKS".to_string());
            }
            for ws in red_default_fuel_ws.iter().copied() {
                red_for_default.insert(ws);
                red_for_inv.insert(ws);
                red_side_template_sources
                    .entry(ws)
                    .or_default()
                    .insert("RDEFAULTFUELTANKS".to_string());
            }
            for (label, ws) in [
                ("GBU-12", [4, 5, 36, 38]),
                ("GBU-16", [4, 5, 36, 39]),
                ("Mk-20", [4, 5, 38, 45]),
                ("Mk-82", [4, 5, 9, 31]),
                ("Mk-83", [4, 5, 9, 32]),
                ("Mk-84", [4, 5, 9, 33]),
            ] {
                info!(
                    "diag default {label}: blue_cap={} blue_global_blocked={} blue_final={} red_cap={} red_global_blocked={} red_final={}",
                    blue_for_default.contains(&ws) || br.weapon_ws_for_aircraft_keys_only(&blue_slot_types_hs).contains(&ws),
                    default_ws_blocked(ws, &blue_default_deny),
                    blue_for_default.contains(&ws),
                    red_for_default.contains(&ws) || br.weapon_ws_for_aircraft_keys_only(&red_slot_types_hs).contains(&ws),
                    default_ws_blocked(ws, &red_default_deny),
                    red_for_default.contains(&ws),
                );
            }
            let blue_default_plus_ws =
                campaign_cfg::collect_weapon_ws_types_positive_initial(
                    &self.blue_default_plus,
                )?;
            let red_default_plus_ws =
                campaign_cfg::collect_weapon_ws_types_positive_initial(
                    &self.red_default_plus,
                )?;

            // DEFAULT+ applies to both lists; fowl export unions DEFAULT ∪ INVENTORY caps.
            let mut blue_default_allowlist = blue_for_default;
            let mut red_default_allowlist = red_for_default;
            let mut blue_inventory_allowlist = blue_for_inv;
            let mut red_inventory_allowlist = red_for_inv;
            blue_default_sources = blue_side_template_sources;
            red_default_sources = red_side_template_sources;
            for ws in blue_default_plus_ws
                .iter()
                .copied()
                .filter(|ws| !(ws[0] == 1 && ws[1] == 3))
            {
                blue_default_allowlist.insert(ws);
                blue_inventory_allowlist.insert(ws);
                blue_default_sources
                    .entry(ws)
                    .or_default()
                    .insert("BDEFAULT+".to_string());
            }
            for ws in red_default_plus_ws
                .iter()
                .copied()
                .filter(|ws| !(ws[0] == 1 && ws[1] == 3))
            {
                red_default_allowlist.insert(ws);
                red_inventory_allowlist.insert(ws);
                red_default_sources
                    .entry(ws)
                    .or_default()
                    .insert("RDEFAULT+".to_string());
            }
            replace_default_weapons_from_allowlist_minus_inventory(
                lua,
                &blue_master,
                &blue_default_allowlist,
                &self.blue_inventory,
                blue_inventory_positive_block_ws.as_ref(),
                "BDEFAULT",
            )
            .context("BDEFAULT weapons from allowlist")?;
            replace_default_weapons_from_allowlist_minus_inventory(
                lua,
                &red_master,
                &red_default_allowlist,
                &self.red_inventory,
                red_inventory_positive_block_ws.as_ref(),
                "RDEFAULT",
            )
            .context("RDEFAULT weapons from allowlist")?;
            log_agm65_diag("after_allowlist_rebuild", "BDEFAULT", &blue_master)?;
            log_agm65_diag("after_allowlist_rebuild", "RDEFAULT", &red_master)?;
            let blue_inv_positive =
                campaign_cfg::collect_weapon_ws_types_positive_initial(
                    &self.blue_inventory,
                )?
                .into_iter()
                .filter(|ws| !is_zero_ws(*ws))
                .collect::<HashSet<[i32; 4]>>();
            let red_inv_positive =
                campaign_cfg::collect_weapon_ws_types_positive_initial(
                    &self.red_inventory,
                )?
                .into_iter()
                .filter(|ws| !is_zero_ws(*ws))
                .collect::<HashSet<[i32; 4]>>();
            // De-dup B/RDEFAULT vs B/RINVENTORY by exact wsType only.
            // Alias-family blocking is too broad for mixed launcher/weapon variants
            // (e.g. AGM-114/AGM-65 variants), which must stay in DEFAULT when absent in INVENTORY.
            blue_inventory_positive_block_ws = Some(blue_inv_positive);
            red_inventory_positive_block_ws = Some(red_inv_positive);
            blue_allowed_ws = Some(blue_default_allowlist.clone());
            red_allowed_ws = Some(red_default_allowlist.clone());
            info!(
                "warehouse allowlist: B/RDEFAULT (template.restricted) blue={} red={}; B/RINVENTORY (DCS for coalition airframes) blue={} red={}; inventory does not self-whitelist",
                blue_default_allowlist.len(),
                red_default_allowlist.len(),
                blue_inventory_allowlist.len(),
                red_inventory_allowlist.len()
            );
            blue_inventory_allowed_ws =
                Some(br.expand_ws_alias_family(&blue_inventory_allowlist));
            red_inventory_allowed_ws =
                Some(br.expand_ws_alias_family(&red_inventory_allowlist));
            blue_fowl_export_union =
                Some(br.expand_ws_alias_family(&blue_inventory_allowlist));
            red_fowl_export_union =
                Some(br.expand_ws_alias_family(&red_inventory_allowlist));
        }
        if bridge_gen.is_none() {
            let mut _zone_allow_dummy = HashSet::new();
            merge_inventory_zone_plus_into_allowlist(
                None,
                None,
                Side::Blue,
                &self.zone_plus_blue,
                &blue_inv_name_ws,
                None,
                &mut _zone_allow_dummy,
                None,
                "BINVENTORY+",
            )?;
            merge_inventory_zone_plus_into_allowlist(
                None,
                None,
                Side::Red,
                &self.zone_plus_red,
                &red_inv_name_ws,
                None,
                &mut _zone_allow_dummy,
                None,
                "RINVENTORY+",
            )?;
        }
        if let Some(caps) = warehouse_caps {
            if caps.has_any_nonzero_cap() {
                campaign_cfg::apply_default_counts_to_weapons(&blue_master, caps)
                    .context("campaign cfg BDEFAULT")?;
                campaign_cfg::apply_default_counts_to_weapons(&red_master, caps)
                    .context("campaign cfg RDEFAULT")?;
            }
        }
        // Re-apply manual DEFAULT+ amounts onto already-allowed rows only.
        // No append here: + must not introduce new wsTypes to DEFAULT.
        merge_inventory_plus_overwrite(
            lua,
            &blue_master,
            &self.blue_default_plus,
            "BDEFAULT",
            warehouse_allowlist_for_filter(&blue_allowed_ws),
            true,
            false,
        )?;
        merge_inventory_plus_overwrite(
            lua,
            &red_master,
            &self.red_default_plus,
            "RDEFAULT",
            warehouse_allowlist_for_filter(&red_allowed_ws),
            true,
            false,
        )?;
        if let Some(caps) = warehouse_caps {
            if caps.has_any_nonzero_cap() {
                // Ensure any still-zero DEFAULT rows (after allowlist rebuild/+ overrides) get cfg baseline counts.
                campaign_cfg::fill_zero_weapon_amounts_from_cfg(&blue_master, caps, 1)
                    .context("fill zero BDEFAULT weapons after allowlist rebuild")?;
                campaign_cfg::fill_zero_weapon_amounts_from_cfg(&red_master, caps, 1)
                    .context("fill zero RDEFAULT weapons after allowlist rebuild")?;
            }
        }
        // Stage 2 fuel workflow: DEFAULT fuel amounts come only from B/RDEFAULTFUELTANKS.
        merge_inventory_plus_overwrite(
            lua,
            &blue_master,
            &self.blue_default_fueltanks,
            "BDEFAULT fuel",
            warehouse_allowlist_for_filter(&blue_allowed_ws),
            true,
            false,
        )?;
        merge_inventory_plus_overwrite(
            lua,
            &red_master,
            &self.red_default_fueltanks,
            "RDEFAULT fuel",
            warehouse_allowlist_for_filter(&red_allowed_ws),
            true,
            false,
        )?;
        prune_warehouse_weapons_row(
            lua,
            &blue_master,
            &blue_strip_ws,
            warehouse_allowlist_for_filter(&blue_allowed_ws),
            "BDEFAULT",
            None,
        )?;
        prune_warehouse_weapons_row(
            lua,
            &red_master,
            &red_strip_ws,
            warehouse_allowlist_for_filter(&red_allowed_ws),
            "RDEFAULT",
            None,
        )?;
        zero_default_weapons_present_in_positive_inventory(
            &blue_master,
            &self.blue_inventory,
            blue_inventory_positive_block_ws.as_ref(),
            "BDEFAULT",
            "BINVENTORY",
        )?;
        zero_default_weapons_present_in_positive_inventory(
            &red_master,
            &self.red_inventory,
            red_inventory_positive_block_ws.as_ref(),
            "RDEFAULT",
            "RINVENTORY",
        )?;
        if let Some((_, br)) = bridge_gen {
            log_default_source_rows("BDEFAULT", &blue_master, &blue_default_sources, br)?;
            log_default_source_rows("RDEFAULT", &red_master, &red_default_sources, br)?;
        }
        log_agm65_diag("after_default_finalize", "BDEFAULT", &blue_master)?;
        log_agm65_diag("after_default_finalize", "RDEFAULT", &red_master)?;

        // Option B: filtered masters become template B/RDEFAULT (mission fill + repack use these rows).
        if weapon_bridge_used {
            copy_weapons_subtable(
                lua,
                &self.blue_default,
                &blue_master,
                "BDEFAULT template",
            )?;
            copy_weapons_subtable(
                lua,
                &self.red_default,
                &red_master,
                "RDEFAULT template",
            )?;
            info!(
                "warehouse template BDEFAULT/RDEFAULT: mirrored filtered `weapons` from allowlist (weapon*.miz policy)"
            );
        }

        let mut blue_inventory = 0;
        let mut red_inventory = 0;
        let mut whids = vec![];
        for coa in base.mission.raw_get::<_, Table>("coalition")?.pairs::<Value, Table>()
        {
            let coa = coa?.1;
            for country in coa.raw_get::<_, Table>("country")?.pairs::<Value, Table>() {
                let country = country?.1;
                if let Ok(iter) = vehicle(&country, "static") {
                    for group in iter {
                        let group = group?;
                        for unit in
                            group.raw_get::<_, Table>("units")?.pairs::<Value, Table>()
                        {
                            let unit = unit?.1;
                            let typ: String = unit.raw_get("type")?;
                            let name: String = unit.raw_get("name")?;
                            let id: i64 = unit.raw_get("unitId")?;
                            if *typ == "FARP"
                                || *typ == "SINGLE_HELIPAD"
                                || *typ == "FARP_SINGLE_01"
                                || *typ == "Invisible FARP"
                            {
                                if *name == cfg.blue_production_template {
                                    blue_inventory = id;
                                } else if *name == cfg.red_production_template {
                                    red_inventory = id;
                                } else {
                                    whids.push(id);
                                }
                            }
                        }
                    }
                }
            }
        }
        let airports = base
            .warehouses
            .raw_get::<_, Table>("airports")
            .context("getting airports")?;
        let warehouses = base
            .warehouses
            .raw_get::<_, Table>("warehouses")
            .context("getting warehouses")?;
        let mut airport_ids = vec![];
        for wh in airports.clone().pairs::<i64, Table>() {
            let (id, _) = wh?;
            airport_ids.push(id);
        }
        for id in airport_ids {
            let old_row = airports
                .raw_get::<_, Table>(id)
                .with_context(|| format_compact!("getting airport {id}"))?;
            let side_opt = warehouse_side_for_default_apply(&old_row)
                .with_context(|| format_compact!("airport warehouse {id}"))?;
            if side_opt.is_none() {
                if warehouse_all_unlimited_off(&old_row) {
                    info!(
                        "airport warehouse {id}: neutral + finite warehouse export — stock/templates deferred to patch_warehouse_dynamic_spawn_links"
                    );
                } else {
                    empty_neutral_build_warehouse_row(
                        lua,
                        &old_row,
                        NeutralWarehouseBuildKind::Airport,
                    )?;
                    info!("airport warehouse {id}: neutral — cleared build stock");
                }
                continue;
            }
            let side = side_opt.unwrap();
            if warehouse_all_unlimited_off(&old_row) {
                // Liquids prefilled in `patch_warehouse_dynamic_spawn_links` (same mult as weapons/aircraft).
                continue;
            }
            let (def_tpl, inv_tpl) = match side {
                Side::Blue => (&self.blue_default, &self.blue_inventory),
                Side::Red => (&self.red_default, &self.red_inventory),
                Side::Neutral => unreachable!("filtered above"),
            };
            let new_row = fill_static_mission_warehouse_from_templates(
                lua,
                &old_row,
                def_tpl,
                inv_tpl,
                mult_cfg.mult_airport(id),
            )
            .with_context(|| format_compact!("airport {id}"))?;
            airports
                .set(id, new_row)
                .with_context(|| format_compact!("setting airport {id}"))?;
        }
        for id in whids {
            let old_row = match warehouses
                .raw_get::<_, Value>(id)
                .with_context(|| format_compact!("getting warehouse {id}"))?
            {
                Value::Nil => {
                    info!(
                        "pad unitId {id}: no warehouses.warehouses row (multi-pad hub shares one warehouse); skipping default apply",
                    );
                    continue;
                }
                Value::Table(t) => t,
                other => bail!(
                    "getting warehouse {id}: expected table or nil, got {:?}",
                    other
                ),
            };
            let side_opt = warehouse_side_for_default_apply(&old_row)
                .with_context(|| format_compact!("warehouse {id}"))?;
            if side_opt.is_none() {
                empty_neutral_build_warehouse_row(
                    lua,
                    &old_row,
                    NeutralWarehouseBuildKind::Other,
                )?;
                info!("warehouse {id}: neutral — cleared build stock");
                continue;
            }
            let side = side_opt.unwrap();
            if warehouse_all_unlimited_off(&old_row) {
                // Finite-export hubs: patched only in `patch_warehouse_dynamic_spawn_links`.
                continue;
            }
            let (def_tpl, inv_tpl) = match side {
                Side::Blue => (&self.blue_default, &self.blue_inventory),
                Side::Red => (&self.red_default, &self.red_inventory),
                Side::Neutral => unreachable!("filtered above"),
            };
            let new_row = fill_static_mission_warehouse_from_templates(
                lua,
                &old_row,
                def_tpl,
                inv_tpl,
                mult_cfg.mult_warehouse_row(id),
            )
            .with_context(|| format_compact!("warehouse {id}"))?;
            warehouses
                .set(id, new_row)
                .with_context(|| format_compact!("setting warehouse {id}"))?
        }
        let old_red_inventory = warehouses
            .raw_get::<_, Table>(red_inventory)
            .context("getting current red inventory")?;
        let new_red_inventory = self.red_inventory.deep_clone(lua)?;
        preserve_dynamic_flags(lua, &new_red_inventory, &old_red_inventory, false)?;
        validate_inventory_weapons(
            &new_red_inventory,
            warehouse_allowlist_for_filter(&red_inventory_allowed_ws),
            "RINVENTORY",
            Some("Invisible FARP RINVENTORY+ and/or trigger zone RINVENTORY+"),
        )?;
        prune_warehouse_weapons_row(
            lua,
            &new_red_inventory,
            &red_strip_ws,
            warehouse_allowlist_for_filter(&red_inventory_allowed_ws),
            "RINVENTORY",
            Some("Invisible FARP RINVENTORY+ and/or trigger zone RINVENTORY+"),
        )?;
        if let Some(plus) = self.red_inventory_plus.as_ref() {
            merge_inventory_plus_overwrite(
                lua,
                &new_red_inventory,
                plus,
                "RINVENTORY",
                None,
                true,
                true,
            )?;
        }
        if let (Some(u), Some(plus)) =
            (red_fowl_export_union.as_mut(), self.red_inventory_plus.as_ref())
        {
            u.extend(campaign_cfg::collect_weapon_ws_types_positive_initial(plus)?);
        }
        log_agm65_diag("after_inventory_finalize", "RINVENTORY", &new_red_inventory)?;
        let red_weapon_export = if red_fowl_export_union.is_some() {
            sorted_weapon_ws(&red_fowl_export_union)
        } else if warehouse_allowlist_for_filter(&red_allowed_ws).is_some() {
            sorted_weapon_ws(&red_allowed_ws)
        } else {
            if red_allowed_ws.as_ref().is_some_and(|s| s.is_empty()) {
                warn!(
                    "red warehouse allowlist empty; fowl export uses RINVENTORY initialAmount>0 rows"
                );
            }
            sorted_weapon_ws(&Some(collect_inventory_weapon_ws(&new_red_inventory)?))
        };
        warehouses
            .set(red_inventory, new_red_inventory.clone())
            .context("setting red inventory")?;
        let old_blue_inventory = warehouses
            .raw_get::<_, Table>(blue_inventory)
            .context("getting current blue inventory")?;
        let new_blue_inventory = self.blue_inventory.deep_clone(lua)?;
        preserve_dynamic_flags(lua, &new_blue_inventory, &old_blue_inventory, false)?;
        validate_inventory_weapons(
            &new_blue_inventory,
            warehouse_allowlist_for_filter(&blue_inventory_allowed_ws),
            "BINVENTORY",
            Some("Invisible FARP BINVENTORY+ and/or trigger zone BINVENTORY+"),
        )?;
        prune_warehouse_weapons_row(
            lua,
            &new_blue_inventory,
            &blue_strip_ws,
            warehouse_allowlist_for_filter(&blue_inventory_allowed_ws),
            "BINVENTORY",
            Some("Invisible FARP BINVENTORY+ and/or trigger zone BINVENTORY+"),
        )?;
        if let Some(plus) = self.blue_inventory_plus.as_ref() {
            merge_inventory_plus_overwrite(
                lua,
                &new_blue_inventory,
                plus,
                "BINVENTORY",
                None,
                true,
                true,
            )?;
        }
        if let (Some(u), Some(plus)) =
            (blue_fowl_export_union.as_mut(), self.blue_inventory_plus.as_ref())
        {
            u.extend(campaign_cfg::collect_weapon_ws_types_positive_initial(plus)?);
        }
        log_agm65_diag("after_inventory_finalize", "BINVENTORY", &new_blue_inventory)?;
        // bflib export: production inventory wsTypes only (not B/RDEFAULT).
        let blue_weapon_export = if blue_fowl_export_union.is_some() {
            sorted_weapon_ws(&blue_fowl_export_union)
        } else if warehouse_allowlist_for_filter(&blue_allowed_ws).is_some() {
            sorted_weapon_ws(&blue_allowed_ws)
        } else {
            if blue_allowed_ws.as_ref().is_some_and(|s| s.is_empty()) {
                warn!(
                    "blue warehouse allowlist empty; fowl export uses BINVENTORY initialAmount>0 rows"
                );
            }
            sorted_weapon_ws(&Some(collect_inventory_weapon_ws(&new_blue_inventory)?))
        };
        info!(
            "fowl export weapon wsTypes: blue={} red={} (with bridge: full payload allowlist; else inventory initialAmount>0 only)",
            blue_weapon_export.len(),
            red_weapon_export.len()
        );
        warehouses
            .set(blue_inventory, new_blue_inventory.clone())
            .context("setting blue inventory")?;
        let built_production_blue = new_blue_inventory
            .deep_clone(lua)
            .context("clone built BINVENTORY for assembled mission + warehouse repack")?;
        let built_production_red = new_red_inventory
            .deep_clone(lua)
            .context("clone built RINVENTORY for assembled mission + warehouse repack")?;
        base.warehouses.set("airports", airports)?;
        base.warehouses.set("warehouses", warehouses)?;
        // Repack warehouse*.miz: same built production rows as assembled mission (inventory-only, post-validate).
        overwrite_production_inventory_row_from_source(
            lua,
            &self.blue_inventory,
            &built_production_blue,
            "BINVENTORY template (built)",
        )?;
        overwrite_production_inventory_row_from_source(
            lua,
            &self.red_inventory,
            &built_production_red,
            "RINVENTORY template (built)",
        )?;
        if !weapon_bridge_used {
            warn!(
                "weapon bridge missing: template BDEFAULT/RDEFAULT `weapons` not updated from allowlist"
            );
        }
        if !blue_inventory_zone_module_ws.is_empty() || !red_inventory_zone_module_ws.is_empty() {
            info!(
                "fowl export inventory_zone_module_ws: blue_modules={} red_modules={}",
                blue_inventory_zone_module_ws.len(),
                red_inventory_zone_module_ws.len()
            );
        }
        Ok((
            bfprotocols::fowl_miz_export::FowlMizExport {
                schema_version: 5,
                weapon_bridge_used,
                blue_weapon_ws: blue_weapon_export,
                red_weapon_ws: red_weapon_export,
                objective_defaults,
                blue_inventory_zone_module_ws,
                red_inventory_zone_module_ws,
                objective_stock: HashMap::new(),
                ai_template_airframes: HashMap::new(),
            },
            inventory_aircraft_orphans_cleared,
            built_production_blue,
            built_production_red,
        ))
    }
}

/// Emitted `DT_*` templates and allow-lists for where each type may offer dynamic spawn.
struct DynamicSpawnEmit {
    link_by_side_type: HashMap<(Side, String), GroupId>,
    /// Per hull (`RKuznecow`, …): coalition templates with route `linkUnit` = ship `unitId`.
    link_by_ship: HashMap<(Side, String, String), GroupId>,
    /// Naval warehouse id → ME ship group name (for `linkDynTempl` lookup).
    ship_hull_by_wid: HashMap<i64, String>,
    /// `Some` when any enabled `TTD*` policy zone exists (membership list for land / non-ship DS).
    land_allow: Option<HashSet<(Side, String)>>,
    /// `Some` when any enabled `TTDN*` policy zone exists (ship warehouse DS).
    naval_allow: Option<HashSet<(Side, String)>>,
    /// All loaded TTD* / TTDN* templates (name-without-prefix → spec); needed for per-base zone filtering.
    dyn_templates: HashMap<String, SlotSpec>,
}

/// Ship `unitId` → coalition side and **group** name (Fowl naval template key).
fn collect_ship_warehouse_group_map(
    base: &LoadedMiz,
) -> Result<HashMap<i64, (Side, String)>> {
    let warehouses_tbl = base
        .warehouses
        .raw_get::<_, Table>("warehouses")
        .context("getting warehouses for ship id scan")?;
    let mut map = HashMap::default();
    for side in [Side::Red, Side::Blue] {
        let coa = base.mission.coalition(side)?;
        for country in coa.countries()? {
            let country = country?;
            for group in vehicle(&country, "ship")? {
                let group = group?;
                let group_name: String = group.raw_get("name")?;
                for unit in group.raw_get::<_, Table>("units")?.pairs::<Value, Table>() {
                    let unit = unit?.1;
                    let id: i64 = unit.raw_get("unitId")?;
                    if !warehouses_tbl
                        .raw_get::<_, Value>(id)
                        .map(|v| !v.is_nil())
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    map.insert(id, (side, group_name.clone()));
                }
            }
        }
    }
    if !map.is_empty() {
        info!(
            "dynamic spawn: treating {} warehouse id(s) as naval (ship unitId match)",
            map.len()
        );
    }
    Ok(map)
}

/// All trigger zone names in the mission (exact strings, for naval carrier naming checks).
fn collect_trigger_zone_names(base: &LoadedMiz) -> Result<HashSet<std::string::String>> {
    let mut names = HashSet::default();
    for zone in base.mission.triggers()? {
        let zone = zone?;
        names.insert(zone.name()?.as_ref().to_string());
    }
    Ok(names)
}

/// Fowl mission rule for carriers with a warehouse row:
/// - Group name `name_naval_template` = `{R|B}` + hull id (e.g. `RKuznecow`, `BTarawa`); first letter must match coalition (R/red, B/blue).
/// - Static slot zone: `TTSN` + `name_naval_template` (e.g. `TTSNRKuznecow`).
/// - Naval dynamic zone: `TTDN` + `name_naval_template` (e.g. `TTDNRKuznecow`).
///
/// Violations fail the FowlTools `miz` build so invalid carrier setups never reach `bflib`.
fn audit_naval_carrier_mission_rules(
    ship_wh: &HashMap<i64, (Side, String)>,
    zone_names: &HashSet<std::string::String>,
) -> Result<()> {
    let mut errors: Vec<std::string::String> = Vec::new();
    for (&wid, (side, group_name)) in ship_wh {
        let mut reasons: Vec<&'static str> = Vec::new();
        let bytes = group_name.as_bytes();
        let prefix_ok = bytes.len() >= 2 && matches!(bytes[0], b'R' | b'B');
        if !prefix_ok {
            reasons.push("group name must be {R|B} + hull id (e.g. RKuznecow)");
        } else {
            let prefix = bytes[0] as char;
            let coalition_matches = match side {
                Side::Red => prefix == 'R',
                Side::Blue => prefix == 'B',
                Side::Neutral => false,
            };
            if !coalition_matches {
                reasons.push(
                    "first letter of group name must match coalition (R=red, B=blue)",
                );
            }
        }
        let static_zone = format!("TTSN{group_name}");
        let dyn_zone = format!("TTDN{group_name}");
        if prefix_ok && !zone_names.contains(static_zone.as_str()) {
            reasons.push("missing static slots trigger zone TTSN + group name");
        }
        if prefix_ok && !zone_names.contains(dyn_zone.as_str()) {
            reasons.push("missing naval dynamic trigger zone TTDN + group name");
        }
        if !reasons.is_empty() {
            errors.push(format!(
                "naval carrier warehouse unitId {} (group {:?}): Fowl naming rule violated — {}; \
                 expected trigger zones {:?} and {:?}",
                wid,
                group_name,
                reasons.join("; "),
                static_zone,
                dyn_zone
            ));
        }
    }
    if !errors.is_empty() {
        bail!(
            "naval carrier mission rules failed (fix or remove ship warehouse rows):\n{}",
            errors.join("\n")
        );
    }
    Ok(())
}

fn inventory_aircraft_type_names(inv: &Table, cat: &str) -> Result<HashSet<StdString>> {
    let mut out: HashSet<StdString> = HashSet::default();
    let Ok(aircrafts) = inv.raw_get::<_, Table>("aircrafts") else {
        return Ok(out);
    };
    let Ok(cat_tbl) = aircrafts.raw_get::<_, Table>(cat) else {
        return Ok(out);
    };
    for pair in cat_tbl.clone().pairs::<String, Table>() {
        let (ut, _) = pair?;
        out.insert(ut.to_string());
    }
    Ok(out)
}

/// B4: neutral + Dynamic Spawn **airport** row — empty weapons/fuel, `aircrafts` rows from B/RDEFAULT ∪ B/RINVENTORY
/// (all `initialAmount` 0), `linkDynTempl` like coalition DS (prefer blue template, else red).
fn neutral_dynamic_spawn_airport_zero_stock_link_templates(
    lua: &Lua,
    wh: &Table<'static>,
    wid: i64,
    emit: &DynamicSpawnEmit,
    mult_cfg: &WarehouseStockMultConfig,
    blue_def: &Table<'static>,
    red_def: &Table<'static>,
    blue_inv: &Table<'static>,
    red_inv: &Table<'static>,
) -> Result<()> {
    apply_neutral_dynamic_spawn_warehouse_flags(lua, wh)?;
    wh.raw_set("weapons", lua.create_table()?)?;
    for key in ["jet_fuel", "gasoline", "diesel", "methanol_mixture"] {
        let Ok(t) = wh.raw_get::<_, Table>(key) else {
            continue;
        };
        if t.raw_get::<_, f64>("InitFuel").is_ok() {
            t.raw_set("InitFuel", 0.0f64)?;
        } else if t.raw_get::<_, i64>("InitFuel").is_ok() {
            t.raw_set("InitFuel", 0i64)?;
        } else {
            t.raw_set("InitFuel", 0.0f64)?;
        }
    }
    let aircrafts = lua.create_table()?;
    for cat in ["helicopters", "planes"] {
        let mut names = inventory_aircraft_type_names(blue_def, cat)?;
        names.extend(inventory_aircraft_type_names(red_def, cat)?);
        names.extend(inventory_aircraft_type_names(blue_inv, cat)?);
        names.extend(inventory_aircraft_type_names(red_inv, cat)?);
        let mut sorted: Vec<StdString> = names.into_iter().collect();
        sorted.sort();
        let cat_tbl = lua.create_table()?;
        for ut in &sorted {
            let row = lua.create_table()?;
            row.raw_set("initialAmount", 0u32)?;
            cat_tbl.raw_set(ut.as_str(), row)?;
        }
        aircrafts.raw_set(cat, cat_tbl)?;
    }
    wh.raw_set("aircrafts", aircrafts)?;
    let aircrafts: Table = wh.raw_get("aircrafts")?;
    let use_naval_filter = mult_cfg.naval_warehouse_ids.contains(&wid);
    for cat in ["helicopters", "planes"] {
        let Ok(cat_tbl) = aircrafts.raw_get::<_, Table>(cat) else {
            continue;
        };
        for pair in cat_tbl.clone().pairs::<String, Table>() {
            let (unit_type, row) = pair?;
            let link_blue = emit
                .link_by_side_type
                .get(&(Side::Blue, unit_type.clone()))
                .map(|g| g.inner())
                .unwrap_or(0);
            let link_red = emit
                .link_by_side_type
                .get(&(Side::Red, unit_type.clone()))
                .map(|g| g.inner())
                .unwrap_or(0);
            let allowed_b = if use_naval_filter {
                emit.naval_allow.as_ref().map_or(true, |s| {
                    s.contains(&(Side::Blue, unit_type.clone()))
                })
            } else {
                emit.land_allow
                    .as_ref()
                    .map_or(true, |s| s.contains(&(Side::Blue, unit_type.clone())))
            };
            let allowed_r = if use_naval_filter {
                emit.naval_allow.as_ref().map_or(true, |s| {
                    s.contains(&(Side::Red, unit_type.clone()))
                })
            } else {
                emit.land_allow
                    .as_ref()
                    .map_or(true, |s| s.contains(&(Side::Red, unit_type.clone())))
            };
            let link = if allowed_b && link_blue != 0 {
                link_blue
            } else if allowed_r && link_red != 0 {
                link_red
            } else {
                0i64
            };
            row.raw_set("linkDynTempl", link)?;
        }
    }
    Ok(())
}

/// Geometry extracted from a trigger zone (owns no Lua state).
enum ObjectiveZoneGeom {
    Circle { center: Vector2, radius: f64 },
    Quad(Quad2),
}

/// Per-objective (O*) zone: extracted geometry + side → allowed unit types.
///
/// Zone coalition is read from the 4th character of the zone name: 'R' = Red, 'B' = Blue.
struct ObjectiveDynAllow {
    /// ME trigger zone name (first three chars map `SETTINGS-dynamic-spawn`).
    zone_name: StdString,
    geom: ObjectiveZoneGeom,
    /// Coalition this base belongs to (from 4th letter of O* zone name).
    side: Side,
    /// OLO* logistics hubs keep full coalition A/C stock (no include_dyn_slots pruning).
    is_logistics_hub: bool,
    /// Optional explicit airport warehouse id (for `warehouses.airports` keys).
    airbase_id: Option<i64>,
    per_side: HashMap<Side, HashSet<StdString>>,
}

impl ObjectiveDynAllow {
    fn contains(&self, v: Vector2) -> bool {
        match &self.geom {
            ObjectiveZoneGeom::Circle { center, radius } => {
                radius.powi(2) >= na::distance_squared(&v.into(), &(*center).into())
            }
            ObjectiveZoneGeom::Quad(q) => q.contains(LuaVec2(v)),
        }
    }
}

/// When a position sits under several objectives, prefer non-`OLO*` zones over logistics overlays:
/// `OLO*` skips `include_dyn_slots` pruning, and naive `Iterator::find` ordering can leak coalition stock.
fn objective_dyn_allow_geom_pick<'a>(
    obj_dyn_allow: &'a [ObjectiveDynAllow],
    pos: Vector2,
) -> Option<&'a ObjectiveDynAllow> {
    let matched: Vec<&'a ObjectiveDynAllow> =
        obj_dyn_allow.iter().filter(|o| o.contains(pos)).collect();
    if matched.is_empty() {
        return None;
    }
    if matched.len() == 1 {
        return Some(matched[0]);
    }
    let non_lo: Vec<&'a ObjectiveDynAllow> = matched
        .iter()
        .copied()
        .filter(|o| !o.is_logistics_hub)
        .collect();
    let mut pool = if non_lo.is_empty() {
        matched
    } else {
        non_lo
    };
    pool.sort_by(|a, b| a.zone_name.cmp(&b.zone_name));
    pool.first().copied()
}

/// Allowed `(Side, aircraft type)` for **`warehouses.warehouses`** dynamic rows on DEP template FARPs (`BDEPFARP*`/`…`).
/// Controlled only by zone `TTDdynFARP`; excludes overlap with objective `TTDLogi` pruning.
fn build_dyn_farp_aircraft_allow(
    base: &LoadedMiz,
    dyn_templates: &HashMap<String, SlotSpec>,
) -> Result<Option<HashSet<(Side, StdString)>>> {
    let mut found_zone = false;
    let mut allowed: HashSet<(Side, StdString)> = HashSet::default();
    for zone in base.mission.triggers()? {
        let zone = zone?;
        let name = zone.name()?;
        if name.as_str() != TTD_DYN_FARP_POLICY_ZONE {
            continue;
        }
        found_zone = true;
        let spec = SlotSpec::new(
            dyn_templates,
            zone.properties()?,
            false,
            INCLUDE_DYNAMIC_SLOT_KEYS,
        )?;
        for (side, m) in spec.slots {
            for (unit_type, count) in m {
                if count > 0 {
                    allowed.insert((side, StdString::from(unit_type.as_str())));
                }
            }
        }
    }
    if !found_zone {
        return Ok(None);
    }
    info!(
        "{}: {} allowed (coalition, aircraft type) pair(s) for DEP dynamic FARP A/C stock",
        TTD_DYN_FARP_POLICY_ZONE,
        allowed.len()
    );
    Ok(Some(allowed))
}

/// Per-ship warehouse `unitId` → allowed `(Side, aircraft type)` from **`TTDN` + ship group name**
/// (e.g. group `RKuznecow` → zone `TTDNRKuznecow`). Hulls without a matching `TTDN*` zone fail the build.
fn build_ship_warehouse_aircraft_allow(
    base: &LoadedMiz,
    dyn_templates: &HashMap<String, SlotSpec>,
    ship_wh_map: &HashMap<i64, (Side, String)>,
) -> Result<HashMap<i64, HashSet<(Side, StdString)>>> {
    let mut out: HashMap<i64, HashSet<(Side, StdString)>> = HashMap::default();
    for (&wid, (_side_unit, group_name)) in ship_wh_map {
        let zone_full = format!("TTDN{}", group_name);
        let mut found = false;
        for zone in base.mission.triggers()? {
            let zone = zone?;
            if zone.name()?.as_str() != zone_full.as_str() {
                continue;
            }
            found = true;
            let spec = SlotSpec::new(
                dyn_templates,
                zone.properties()?,
                false,
                INCLUDE_DYNAMIC_SLOT_KEYS,
            )?;
            let mut allowed: HashSet<(Side, StdString)> = HashSet::default();
            for (side, m) in spec.slots {
                for (unit_type, count) in m {
                    if count > 0 {
                        allowed.insert((side, StdString::from(unit_type.as_str())));
                    }
                }
            }
            let n = allowed.len();
            out.insert(wid, allowed);
            info!(
                "TTDN + {:?}: {} allowed (coalition, aircraft type) pair(s) for ship warehouse {}",
                group_name,
                n,
                wid
            );
            break;
        }
        if !found {
            bail!("carrier warehouse {}: missing naval dynamic trigger zone `{}`", wid, zone_full);
        }
    }
    Ok(out)
}

/// Per DEP pad group name (`DEPBFARPPAD0`, …): `objective_defaults` from `TTDdynFARP` (export extract + bflib filters).
fn extend_objective_defaults_for_dep_farps(
    defaults: &mut HashMap<StdString, bfprotocols::fowl_miz_export::ObjectiveWarehouseDefaults>,
    dep_wh_map: &HashMap<i64, (Side, String)>,
    dyn_farp_allow: &HashSet<(Side, StdString)>,
    br: &weapon_bridge::WeaponBridgeMap,
) -> Result<usize> {
    if dep_wh_map.is_empty() || dyn_farp_allow.is_empty() {
        return Ok(0);
    }
    let mut added = 0usize;
    for (_, (owner_side, pad_name)) in dep_wh_map {
        let key = StdString::from(pad_name.as_str());
        if defaults.contains_key(&key) {
            continue;
        }
        let mut policy_types: HashSet<StdString> = HashSet::default();
        for (side, ut) in dyn_farp_allow {
            if *side == *owner_side {
                policy_types.insert(ut.clone());
            }
        }
        if policy_types.is_empty() {
            continue;
        }
        let (blue_aircraft, red_aircraft) = match owner_side {
            Side::Blue => (policy_types.clone(), HashSet::default()),
            Side::Red => (HashSet::default(), policy_types.clone()),
            Side::Neutral => continue,
        };
        let mut blue_weapon_ws: HashSet<[i32; 4]> = HashSet::default();
        let mut red_weapon_ws: HashSet<[i32; 4]> = HashSet::default();
        blue_weapon_ws.extend(br.weapon_ws_for_aircraft_keys_only(&blue_aircraft));
        red_weapon_ws.extend(br.weapon_ws_for_aircraft_keys_only(&red_aircraft));
        let mut blue_aircraft_vec: Vec<_> = blue_aircraft.into_iter().collect();
        blue_aircraft_vec.sort();
        let mut red_aircraft_vec: Vec<_> = red_aircraft.into_iter().collect();
        red_aircraft_vec.sort();
        let mut blue_ws_vec: Vec<_> = blue_weapon_ws.into_iter().collect();
        blue_ws_vec.sort_by_key(|w| (w[0], w[1], w[2], w[3]));
        let mut red_ws_vec: Vec<_> = red_weapon_ws.into_iter().collect();
        red_ws_vec.sort_by_key(|w| (w[0], w[1], w[2], w[3]));
        defaults.insert(
            key,
            bfprotocols::fowl_miz_export::ObjectiveWarehouseDefaults {
                blue_aircraft: blue_aircraft_vec,
                red_aircraft: red_aircraft_vec,
                blue_weapon_ws: blue_ws_vec,
                red_weapon_ws: red_ws_vec,
            },
        );
        added += 1;
    }
    if added > 0 {
        info!(
            "objective_defaults: added {added} DEP FARP pad profile(s) from {TTD_DYN_FARP_POLICY_ZONE}"
        );
    }
    Ok(added)
}

/// `objective_defaults` for `Kuznetsov` / `Forrestal` keys (TTDN* air wing) — aligns export extract and bflib filters with `O*` hubs.
fn extend_objective_defaults_for_naval_hulls(
    defaults: &mut HashMap<StdString, bfprotocols::fowl_miz_export::ObjectiveWarehouseDefaults>,
    base: &LoadedMiz,
    ship_wh_map: &HashMap<i64, (Side, String)>,
    dyn_templates: &HashMap<String, SlotSpec>,
    br: &weapon_bridge::WeaponBridgeMap,
) -> Result<usize> {
    let mut added = 0usize;
    for (_, (owner_side, group_name)) in ship_wh_map {
        let display_key = StdString::from(ship_pad_display_name(group_name.as_str()));
        if defaults.contains_key(&display_key) {
            continue;
        }
        let zone_full = format!("TTDN{}", group_name);
        let mut policy_types: HashSet<StdString> = HashSet::default();
        let mut found_zone = false;
        for zone in base.mission.triggers()? {
            let zone = zone?;
            if zone.name()?.as_str() != zone_full.as_str() {
                continue;
            }
            found_zone = true;
            let spec = SlotSpec::new(
                dyn_templates,
                zone.properties()?,
                false,
                INCLUDE_DYNAMIC_SLOT_KEYS,
            )?;
            if let Some(m) = spec.slots.get(owner_side) {
                for (unit_type, count) in m {
                    if *count > 0 {
                        policy_types.insert(StdString::from(unit_type.as_str()));
                    }
                }
            }
            break;
        }
        if !found_zone || policy_types.is_empty() {
            continue;
        }
        let (blue_aircraft, red_aircraft) = match owner_side {
            Side::Blue => (policy_types.clone(), HashSet::default()),
            Side::Red => (HashSet::default(), policy_types.clone()),
            Side::Neutral => continue,
        };
        let mut blue_weapon_ws: HashSet<[i32; 4]> = HashSet::default();
        let mut red_weapon_ws: HashSet<[i32; 4]> = HashSet::default();
        blue_weapon_ws.extend(br.weapon_ws_for_aircrafts(&blue_aircraft));
        red_weapon_ws.extend(br.weapon_ws_for_aircrafts(&red_aircraft));
        let mut blue_aircraft_vec: Vec<_> = blue_aircraft.into_iter().collect();
        blue_aircraft_vec.sort();
        let mut red_aircraft_vec: Vec<_> = red_aircraft.into_iter().collect();
        red_aircraft_vec.sort();
        let mut blue_ws_vec: Vec<_> = blue_weapon_ws.into_iter().collect();
        blue_ws_vec.sort_by_key(|w| (w[0], w[1], w[2], w[3]));
        let mut red_ws_vec: Vec<_> = red_weapon_ws.into_iter().collect();
        red_ws_vec.sort_by_key(|w| (w[0], w[1], w[2], w[3]));
        defaults.insert(
            display_key,
            bfprotocols::fowl_miz_export::ObjectiveWarehouseDefaults {
                blue_aircraft: blue_aircraft_vec,
                red_aircraft: red_aircraft_vec,
                blue_weapon_ws: blue_ws_vec,
                red_weapon_ws: red_ws_vec,
            },
        );
        added += 1;
    }
    if added > 0 {
        info!(
            "objective_defaults: added {added} naval hull profile(s) from TTDN* (e.g. Kuznetsov, Forrestal)"
        );
    }
    Ok(added)
}

/// Build per-objective zone dynamic allow map from O* zones that have `include_dyn_slots`.
///
/// Each O* zone's `include_dyn_slots` values are TTD template names (without `TTD` prefix).
/// Resolved via `dyn_templates`; union across all referenced templates, per side.
fn build_objective_dyn_allow(
    base: &LoadedMiz,
    dyn_templates: &HashMap<String, SlotSpec>,
) -> Result<Vec<ObjectiveDynAllow>> {
    let mut out: Vec<ObjectiveDynAllow> = Vec::new();
    for zone in base.mission.triggers()? {
        let zone = zone?;
        let name = zone.name()?;
        if !name.starts_with('O') {
            continue;
        }
        let is_logistics_hub = name.starts_with("OLO");
        // 4th character (index 3) of O* zone name: 'R' = Red, 'B' = Blue.
        let base_side = match name.chars().nth(3) {
            Some('R') | Some('r') => Side::Red,
            Some('B') | Some('b') => Side::Blue,
            other => {
                warn!(
                    "O* zone {:?} has unknown coalition letter at pos 4 ({:?}), skipping",
                    name.as_str(),
                    other
                );
                continue;
            }
        };
        let mut per_side: HashMap<Side, HashSet<StdString>> = HashMap::default();
        let mut airbase_id: Option<i64> = None;
        for prop in zone.properties()? {
            let prop = prop?;
            if INCLUDE_DYNAMIC_SLOT_KEYS.iter().any(|&k| prop.key.as_ref() == k) {
                let tmpl_name = prop.value.as_str();
                if let Some(spec) = dyn_templates.get(tmpl_name) {
                    for (side, m) in &spec.slots {
                        let entry = per_side.entry(*side).or_default();
                        for (ut, count) in m {
                            if *count > 0 {
                                entry.insert(StdString::from(ut.as_str()));
                            }
                        }
                    }
                }
            } else if prop.key.eq_ignore_ascii_case("airbaseID") {
                let raw = prop.value.trim();
                if !raw.is_empty() {
                    match raw.parse::<i64>() {
                        Ok(id) => airbase_id = Some(id),
                        Err(_) => warn!(
                            "O* zone {:?}: invalid airbaseID value {:?} (expected integer airport warehouse key)",
                            name.as_str(),
                            prop.value.as_ref()
                        ),
                    }
                }
            }
        }
        let center = zone.pos()?;
        let geom = match zone.typ()? {
            TriggerZoneTyp::Circle { radius } => {
                ObjectiveZoneGeom::Circle { center, radius }
            }
            TriggerZoneTyp::Quad(q) => ObjectiveZoneGeom::Quad(q),
        };
        let znm = StdString::from(name.as_str());
        out.push(ObjectiveDynAllow {
            zone_name: znm,
            geom,
            side: base_side,
            is_logistics_hub,
            airbase_id,
            per_side,
        });
    }
    info!(
        "per-base dyn-allow: {} O* zone(s) with include_dyn_slots TTD refs",
        out.len()
    );
    Ok(out)
}

/// FARP/FOB warehouse unit positions from mission data (keyed by unitId = warehouse key).
fn collect_warehouse_unit_positions(
    base: &LoadedMiz,
    warehouse_ids: &HashSet<i64>,
) -> Result<HashMap<i64, Vector2>> {
    let mut out: HashMap<i64, Vector2> = HashMap::default();
    for side in Side::ALL {
        let coa = base.mission.coalition(side)?;
        for country in coa.raw_get::<_, Table>("country")?.pairs::<Value, Table>() {
            let country = country?.1;
            for kind in ["static", "plane", "helicopter", "ship"] {
                let Ok(vt) = country.raw_get::<_, Table>(kind) else { continue };
                let Ok(groups) = vt.raw_get::<_, Table>("group") else { continue };
                for group in groups.clone().pairs::<Value, Table>() {
                    let group = group?.1;
                    let Ok(units) = group.raw_get::<_, Table>("units") else { continue };
                    for unit in units.clone().pairs::<Value, Table>() {
                        let unit = unit?.1;
                        let Ok(uid) = unit.raw_get::<_, i64>("unitId") else { continue };
                        if !warehouse_ids.contains(&uid) {
                            continue;
                        }
                        if let (Ok(x), Ok(y)) =
                            (unit.raw_get::<_, f64>("x"), unit.raw_get::<_, f64>("y"))
                        {
                            out.insert(uid, Vector2::new(x, y));
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Approximate airport positions from route waypoints carrying `airdromId`.
///
/// A parking-spot position is sufficient — it should be inside the O* zone covering the airfield.
fn collect_airport_positions_from_groups(
    base: &LoadedMiz,
    airport_ids: &HashSet<i64>,
) -> Result<HashMap<i64, Vector2>> {
    let mut out: HashMap<i64, Vector2> = HashMap::default();
    for side in Side::ALL {
        let coa = base.mission.coalition(side)?;
        for country in coa.raw_get::<_, Table>("country")?.pairs::<Value, Table>() {
            let country = country?.1;
            for kind in ["plane", "helicopter"] {
                let Ok(vt) = country.raw_get::<_, Table>(kind) else { continue };
                let Ok(groups) = vt.raw_get::<_, Table>("group") else { continue };
                for group in groups.clone().pairs::<Value, Table>() {
                    let group = group?.1;
                    let Ok(route) = group.raw_get::<_, Table>("route") else { continue };
                    let Ok(points) = route.raw_get::<_, Table>("points") else { continue };
                    for point in points.clone().pairs::<Value, Table>() {
                        let point = point?.1;
                        let Ok(aid) = point.raw_get::<_, i64>("airdromId") else { continue };
                        if !airport_ids.contains(&aid) || out.contains_key(&aid) {
                            continue;
                        }
                        if let (Ok(x), Ok(y)) =
                            (point.raw_get::<_, f64>("x"), point.raw_get::<_, f64>("y"))
                        {
                            out.insert(aid, Vector2::new(x, y));
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Dump airport warehouse rows joined to OAB*/OLO* zones via each zone's `airbaseID` property.
///
/// Run after `apply_objective_airbase_ids_from_export` so lines reflect export/manual mapping.
/// Coalition and dynamicSpawn come from `warehouses.airports[id]` when present.
fn log_airbase_objective_zone_mapping(base: &LoadedMiz) -> Result<()> {
    let airports = match base.warehouses.raw_get::<_, Table>("airports") {
        Ok(t) => t,
        Err(_) => {
            warn!("airbase-zone mapping: warehouses.airports table not found");
            return Ok(());
        }
    };
    let mut airport_rows: HashMap<i64, (StdString, bool)> = HashMap::default();
    for pair in airports.clone().pairs::<Value, Table>() {
        let (k, row) = pair?;
        let Some(id) = warehouse_lua_key_i64(k) else { continue };
        let coalition = row
            .raw_get::<_, String>("coalition")
            .map(|s| StdString::from(s.as_str().to_uppercase()))
            .unwrap_or_else(|_| StdString::from("UNKNOWN"));
        let dyn_spawn = warehouse_dynamic_spawn_enabled(&row);
        airport_rows.insert(id, (coalition, dyn_spawn));
    }

    let mut mapped: Vec<(i64, StdString, StdString, StdString)> = Vec::new();
    let mut zones_without_id: Vec<StdString> = Vec::new();

    for zone in base.mission.triggers()? {
        let zone = zone?;
        let name = zone.name()?;
        if !(name.starts_with("OAB") || name.starts_with("OLO")) {
            continue;
        }
        let zname = StdString::from(name.as_str());
        let mut aid: Option<i64> = None;
        for prop in zone.properties()? {
            let prop = prop?;
            if !prop.key.eq_ignore_ascii_case("airbaseID") {
                continue;
            }
            let raw = prop.value.trim();
            if raw.is_empty() {
                break;
            }
            if let Ok(id) = raw.parse::<i64>() {
                if id > 0 {
                    aid = Some(id);
                }
            }
            break;
        }
        match aid {
            Some(id) => {
                let (coalition, dyn_label) = match airport_rows.get(&id) {
                    Some((c, d)) => (
                        c.clone(),
                        StdString::from(if *d { "true" } else { "false" }),
                    ),
                    None => (
                        StdString::from("(missing row)"),
                        StdString::from("n/a"),
                    ),
                };
                mapped.push((id, coalition, dyn_label, zname));
            }
            None => zones_without_id.push(zname),
        }
    }

    mapped.sort_by(|a, b| a.0.cmp(&b.0));
    zones_without_id.sort_unstable();

    let claimed_ids: HashSet<i64> = mapped.iter().map(|(id, _, _, _)| *id).collect();

    info!(
        "=== AIRBASE ID <-> OAB*/OLO* ZONE (from zone airbaseID property; coalition/dynamicSpawn from warehouses.airports) ==="
    );
    info!(
        "  {:>6}  {:<12}  {:<14}  zone name",
        "ID", "coalition", "dynamicSpawn"
    );
    for (id, coalition, dyn_label, zname) in &mapped {
        info!(
            "  {:>6}  {:<12}  {:<14}  {}",
            id,
            coalition.as_str(),
            dyn_label.as_str(),
            zname.as_str()
        );
    }

    let mut orphans: Vec<i64> = airport_rows
        .keys()
        .copied()
        .filter(|id| !claimed_ids.contains(id))
        .collect();
    orphans.sort_unstable();
    if !orphans.is_empty() {
        info!(
            "  Airport warehouse id(s) not referenced by any OAB*/OLO* zone airbaseID: {:?}",
            orphans
        );
    }
    if !zones_without_id.is_empty() {
        info!(
            "  OAB*/OLO* zone(s) missing airbaseID property: {:?}",
            zones_without_id
        );
    }
    info!("=== END OF AIRBASE ID MAPPING ===");
    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
struct AirbaseExportRow {
    id: i64,
    name: StdString,
    #[serde(default)]
    trigger_zone_name: Option<StdString>,
    #[serde(default)]
    airbase_name: Option<StdString>,
}

#[derive(Debug, Clone, Deserialize)]
struct AirbaseExportDoc {
    airbases: Vec<AirbaseExportRow>,
}

#[derive(Debug, Default)]
struct ObjectiveAirbaseApplySummary {
    export_path: Option<PathBuf>,
    /// `(zone name, export id, export airbase name, previous parsed id if any)`
    filled: Vec<(StdString, i64, StdString, Option<i64>)>,
    unresolved: Vec<StdString>,
}

fn normalize_airbase_name(s: &str) -> StdString {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Matches `bflib::admin::slugify(_, allow_dot: false)` for theatre filenames.
fn slugify_airbase_export_theatre(raw: &str) -> StdString {
    let mut out = StdString::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches(|c| c == '.' || c == '_');
    if trimmed.is_empty() {
        StdString::from("unknown")
    } else {
        StdString::from(trimmed)
    }
}

fn pick_mission_theatre_raw(val: Value) -> Option<StdString> {
    match val {
        Value::String(s) => {
            let t = s.to_str().ok()?.trim();
            if t.is_empty() {
                None
            } else {
                Some(StdString::from(t))
            }
        }
        Value::Table(tbl) => {
            for key in ["name", "id", "code"] {
                if let Ok(Value::String(s)) = tbl.raw_get::<_, Value>(key) {
                    let t = s.to_str().ok()?.trim();
                    if !t.is_empty() {
                        return Some(StdString::from(t));
                    }
                }
            }
            None
        }
        _ => None,
    }
}

fn mission_theatre_slug_for_export_check(mission: &Miz<'_>) -> StdString {
    let label = mission
        .raw_get::<_, Value>("theatre")
        .ok()
        .and_then(pick_mission_theatre_raw)
        .or_else(|| {
            mission
                .raw_get::<_, Value>("theater")
                .ok()
                .and_then(pick_mission_theatre_raw)
        })
        .or_else(|| {
            mission
                .raw_get::<_, Value>("terrain")
                .ok()
                .and_then(pick_mission_theatre_raw)
        })
        .unwrap_or_else(|| StdString::from("unknown"));
    slugify_airbase_export_theatre(&label)
}

/// Theatre segment after first `_` in `fowl_airbase_export-DCS.version.{version}_{theatre}.json`.
/// Assumes the DCS version slug contains no `_` (same as current bflib exporter output).
fn airbase_export_filename_theatre_slug(path: &Path) -> Option<StdString> {
    let name = path.file_name()?.to_str()?;
    const PREFIX: &str = "fowl_airbase_export-DCS.version.";
    const SUFFIX: &str = ".json";
    if !name.starts_with(PREFIX) || !name.ends_with(SUFFIX) {
        return None;
    }
    let stem = &name[PREFIX.len()..name.len() - SUFFIX.len()];
    let (_, theatre) = stem.split_once('_')?;
    if theatre.is_empty() {
        return None;
    }
    Some(StdString::from(theatre))
}

fn warn_airbase_export_theatre_mismatch(mission: &Miz<'_>, export_path: &Path) {
    let mission_slug = mission_theatre_slug_for_export_check(mission);
    if mission_slug == "unknown" {
        warn!(
            "airbase export {:?}: mission theatre not found (expected mission.theatre / theater / terrain); skipping theatre check vs export filename.",
            export_path.display(),
        );
        return;
    }
    let Some(file_slug) = airbase_export_filename_theatre_slug(export_path) else {
        warn!(
            "airbase export {:?}: filename must contain \"_<theatre>\" after the DCS version (example: fowl_airbase_export-DCS.version.2.9.26.23303_Caucasus.json); cannot verify theatre.",
            export_path.display(),
        );
        warn!(
            "Fix: run this scenario on the intended map in DCS, ensure your UCID is listed under \"admins\" in the mission CFG, start the mission, chat \"-admin airbaseexport\", then move fowl_airbase_export-DCS.version.*_<map>.json from Saved Games\\DCS next to your base or weapon .miz and rebuild.",
        );
        return;
    };
    if file_slug.eq_ignore_ascii_case(mission_slug.as_str()) {
        return;
    }
    warn!(
        "airbase export {:?}: theatre slug {:?} (from filename) does not match mission theatre slug {:?}; this file is probably from a different map.",
        export_path.display(),
        file_slug.as_str(),
        mission_slug.as_str(),
    );
    warn!(
        "Fix: run this scenario on the correct map in DCS, ensure your UCID is listed under \"admins\" in the mission CFG, start the mission, chat \"-admin airbaseexport\", then replace this JSON with the new fowl_airbase_export-DCS.version.*_<map>.json from Saved Games\\DCS (copy it into this mission folder next to the base or weapon .miz) and rebuild.",
    );
}

fn resolve_airbase_export_path(cfg: &MizCmd) -> Result<Option<PathBuf>> {
    if let Some(ref p) = cfg.airbase_export {
        if p.is_file() {
            return Ok(Some(p.clone()));
        }
        bail!(
            "--airbase-export path does not exist or is not a file: {}",
            p.display()
        );
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = cfg.base.parent() {
        dirs.push(d.to_path_buf());
    }
    if let Some(d) = cfg.weapon.parent() {
        let pb = d.to_path_buf();
        if dirs.iter().all(|x| x != &pb) {
            dirs.push(pb);
        }
    }
    let mut latest: Option<(std::time::SystemTime, PathBuf)> = None;
    for dir in dirs {
        let Some(path) = find_latest_airbase_export_json(&dir)? else {
            continue;
        };
        let modified = fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        match &latest {
            Some((t, _)) if *t >= modified => (),
            _ => latest = Some((modified, path)),
        }
    }
    Ok(latest.map(|(_, p)| p))
}

fn find_latest_airbase_export_json(mission_dir: &Path) -> Result<Option<PathBuf>> {
    let mut latest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(mission_dir)
        .with_context(|| format_compact!("reading mission dir {}", mission_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !name.starts_with("fowl_airbase_export-DCS.version.") || !name.ends_with(".json") {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        match &latest {
            Some((t, _)) if *t >= modified => (),
            _ => latest = Some((modified, path)),
        }
    }
    Ok(latest.map(|(_, p)| p))
}

/// Reads first positive integer from `airbaseID` props and counts how many such props exist.
fn zone_airbase_id_scan(zone: &miz::TriggerZone) -> Result<(Option<i64>, usize)> {
    let mut count = 0usize;
    let mut parsed: Option<i64> = None;
    for prop in zone.properties()? {
        let prop = prop?;
        if !prop.key.eq_ignore_ascii_case("airbaseID") {
            continue;
        }
        count += 1;
        if parsed.is_none() {
            let raw = prop.value.trim();
            if let Ok(v) = raw.parse::<i64>() {
                if v > 0 {
                    parsed = Some(v);
                }
            }
        }
    }
    Ok((parsed, count))
}

/// Single canonical trigger property row: `key` = `airbaseID`, `value` = warehouse airport id.
/// Drops every existing `airbaseID` row (fixes duplicates / stale ME edits), appends one new row.
fn set_zone_airbase_id_property(zone: &miz::TriggerZone, id: i64) -> Result<()> {
    let lua = unsafe { &*LUA };
    let props: Table = zone.raw_get("properties")?;
    let mut keep: Vec<(StdString, StdString)> = Vec::new();
    for pair in props.clone().pairs::<Value, Table>() {
        let (_idx, prop) = pair?;
        let key: String = prop.raw_get("key").unwrap_or_else(|_| String::from(""));
        let val: String = prop.raw_get("value").unwrap_or_else(|_| String::from(""));
        if key.eq_ignore_ascii_case("airbaseID") {
            continue;
        }
        keep.push((
            StdString::from(key.as_str()),
            StdString::from(val.as_str()),
        ));
    }
    keep.push((
        StdString::from("airbaseID"),
        StdString::from(format_compact!("{id}").as_str()),
    ));
    let new_props = lua.create_table()?;
    for (i, (k, v)) in keep.into_iter().enumerate() {
        let row = lua.create_table()?;
        row.raw_set("key", String::from(k.as_str()))?;
        row.raw_set("value", String::from(v.as_str()))?;
        new_props.raw_set(i + 1, row)?;
    }
    zone.raw_set("properties", new_props)?;
    Ok(())
}

fn apply_objective_airbase_ids_from_export(
    base: &mut LoadedMiz,
    export_path: Option<&Path>,
) -> Result<ObjectiveAirbaseApplySummary> {
    let mut summary = ObjectiveAirbaseApplySummary::default();
    let Some(export_path) = export_path else {
        warn!(
            "airbase export JSON not found (use --airbase-export or place fowl_airbase_export-DCS.version.*.json next to --base or --weapon); continuing without auto-fill",
        );
        return Ok(summary);
    };
    let s = fs::read_to_string(&export_path)
        .with_context(|| format_compact!("reading {}", export_path.display()))?;
    let doc: AirbaseExportDoc = serde_json::from_str(&s)
        .with_context(|| format_compact!("parsing {}", export_path.display()))?;
    summary.export_path = Some(export_path.to_path_buf());

    let mut by_trigger_lc: HashMap<StdString, Vec<&AirbaseExportRow>> = HashMap::default();
    let mut by_norm: HashMap<StdString, Vec<&AirbaseExportRow>> = HashMap::default();
    for row in &doc.airbases {
        if row.id <= 0 {
            continue;
        }
        by_norm
            .entry(normalize_airbase_name(row.name.as_str()))
            .or_default()
            .push(row);
        if let Some(ref tz) = row.trigger_zone_name {
            let lc: StdString = tz.as_str().to_ascii_lowercase();
            by_trigger_lc.entry(lc).or_default().push(row);
        }
    }

    for zone in base.mission.triggers()? {
        let zone = zone?;
        let name = zone.name()?;
        if !(name.starts_with("OAB") || name.starts_with("OLO")) {
            continue;
        }
        let zname = StdString::from(name.as_str());
        let (parsed_id, airbase_prop_count) = zone_airbase_id_scan(&zone)?;
        let lc_key: StdString = zname.to_ascii_lowercase();

        let mut row_pick: Option<&AirbaseExportRow> = None;

        if let Some(cands) = by_trigger_lc.get(&lc_key) {
            match cands.len() {
                1 => row_pick = Some(cands[0]),
                _ => {
                    warn!(
                        "objective zone {:?}: ambiguous trigger_zone_name match -> {:?}",
                        zname.as_str(),
                        cands.iter().map(|r| r.name.clone()).collect::<Vec<_>>()
                    );
                    summary.unresolved.push(zname);
                    continue;
                }
            }
        }

        if row_pick.is_none() {
            let objective_name = name.as_str().get(4..).unwrap_or("");
            let norm = normalize_airbase_name(objective_name);
            if let Some(cands) = by_norm.get(&norm) {
                match cands.len() {
                    1 => row_pick = Some(cands[0]),
                    _ => {
                        warn!(
                            "objective zone {:?}: ambiguous export match for label {:?} -> {:?}",
                            zname.as_str(),
                            objective_name,
                            cands.iter().map(|r| r.name.clone()).collect::<Vec<_>>()
                        );
                        summary.unresolved.push(zname);
                        continue;
                    }
                }
            }
        }

        let Some(row) = row_pick else {
            summary.unresolved.push(zname);
            continue;
        };

        let already_ok =
            parsed_id == Some(row.id) && airbase_prop_count == 1;
        if already_ok {
            continue;
        }

        set_zone_airbase_id_property(&zone, row.id)?;
        summary
            .filled
            .push((zname.clone(), row.id, row.name.clone(), parsed_id));
    }

    if !summary.filled.is_empty() {
        info!(
            "airbase export {} wrote airbaseID on {} objective zone(s) (new, corrected, or deduped)",
            export_path.display(),
            summary.filled.len()
        );
        for (zone, id, src, prev) in &summary.filled {
            match prev {
                None => info!("  {:?} -> airbaseID={} (export {:?})", zone, id, src),
                Some(p) if *p != *id => {
                    info!(
                        "  {:?} -> airbaseID={} (replaced {}; export {:?})",
                        zone, id, p, src
                    );
                }
                Some(_) => info!("  {:?} -> airbaseID={} (deduped props; export {:?})", zone, id, src),
            }
        }
    } else {
        warn!(
            "airbase export {} loaded but no objective zone received auto-fill",
            export_path.display()
        );
    }
    Ok(summary)
}

/// `airbaseID` links an objective zone to a `warehouses.airports` warehouse id. DCS has no such row for
/// many OLO* FOB / strip logistics hubs; build still resolves ground warehouses by zone containment.
fn objective_zone_airbase_id_absent_is_expected(name: &str) -> bool {
    let u = name.to_ascii_uppercase();
    u.starts_with("OLO") && (u.contains("FOB") || u.contains("STRIP"))
}

fn extend_hub_warehouse_ids_for_olo_logistics(
    hub_warehouse_ids: &mut HashSet<i64>,
    obj_dyn_allow: &[ObjectiveDynAllow],
    base: &LoadedMiz,
) -> Result<()> {
    for allow in obj_dyn_allow {
        if !allow.is_logistics_hub {
            continue;
        }
        let Some(resolved) = resolve_objective_warehouse(base, allow)? else {
            continue;
        };
        if !resolved.is_airport {
            hub_warehouse_ids.insert(resolved.wh_id);
        }
    }
    Ok(())
}

/// Collect OAB*/OLO* zones that lack usable `warehouses.airports` binding via `airbaseID`.
///
/// Allowed gap: [`objective_zone_airbase_id_absent_is_expected`] (OLO* FOB/strip placeholders without airports row).
fn validate_objective_airbase_ids(base: &LoadedMiz) -> Result<(Vec<StdString>, Vec<(StdString, StdString)>)> {
    let mut missing: Vec<StdString> = Vec::new();
    let mut invalid: Vec<(StdString, StdString)> = Vec::new();
    for zone in base.mission.triggers()? {
        let zone = zone?;
        let name = zone.name()?;
        if !(name.starts_with("OAB") || name.starts_with("OLO")) {
            continue;
        }
        let mut found = false;
        for prop in zone.properties()? {
            let prop = prop?;
            if !prop.key.eq_ignore_ascii_case("airbaseID") {
                continue;
            }
            found = true;
            let raw = prop.value.trim();
            if raw.is_empty() {
                missing.push(StdString::from(name.as_str()));
                break;
            }
            match raw.parse::<i64>() {
                Ok(id) if id > 0 => (),
                _ => {
                    invalid.push((
                        StdString::from(name.as_str()),
                        StdString::from(raw),
                    ));
                }
            }
            break;
        }
        if !found {
            missing.push(StdString::from(name.as_str()));
        }
    }
    if missing.is_empty() && invalid.is_empty() {
        return Ok((missing, invalid));
    }
    missing.sort_unstable();
    invalid.sort_by(|a, b| a.0.cmp(&b.0));
    let missing_expected: Vec<&StdString> = missing
        .iter()
        .filter(|n| objective_zone_airbase_id_absent_is_expected(n.as_str()))
        .collect();
    if !missing_expected.is_empty() {
        info!(
            "objective zone(s) with no airbaseID (normal for OLO* FOB/strip hubs without a `warehouses.airports` id): {:?}",
            missing_expected
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
        );
    }
    Ok((missing, invalid))
}

/// End-of-run reminder: builders still produce a playable .miz, but stale `airbaseID`/`fowl_airbase_export` mis-binds warehouses.
fn warn_airbase_objective_bindings_follow_up_summary(
    missing_airbase: &[StdString],
    invalid_airbase: &[(StdString, StdString)],
    unresolved_surprising_export: &[StdString],
) {
    let required_missing: Vec<&str> = missing_airbase
        .iter()
        .filter(|n| !objective_zone_airbase_id_absent_is_expected(n.as_str()))
        .map(|s| s.as_str())
        .collect();
    if invalid_airbase.is_empty()
        && required_missing.is_empty()
        && unresolved_surprising_export.is_empty()
    {
        return;
    }
    warn!(
        "======================================================================"
    );
    warn!(
        "FowlTools WARN: Objective OAB*/OLO* bindings need completion (warehouse prune / airport identity)."
    );
    if !invalid_airbase.is_empty() {
        warn!(
            "Invalid `airbaseID` trigger property value (expected integer > 0 matching `warehouses.airports`): {:?}",
            invalid_airbase
        );
    }
    if !required_missing.is_empty() {
        warn!(
            "Missing non-empty `airbaseID` on objective zone(s) (every paved-airfield objective needs it; exempt OLO* with FOB or STRIP in the zone name): {:?}",
            required_missing,
        );
    }
    if !unresolved_surprising_export.is_empty() {
        warn!(
            "Airbase export JSON matched no zone name for {:?} — add entry or fix trigger name spelling so auto-fill finds the row.",
            unresolved_surprising_export
        );
    }
    warn!(
        "How to regenerate mapping: mission CFG lists your UCID under \"admins\", run the mission from DCS, chat `-admin airbaseexport`,"
    );
    warn!(
        "then copy `fowl_airbase_export-DCS.version.*.json` from Saved Games\\DCS to the scenario mission folder and rebuild."
    );
    warn!(
        "Until resolved, geography may classify some airports under an overlapping logistics zone (`OLO*`); update export to avoid silent stock mistakes."
    );
    warn!(
        "======================================================================"
    );
}

/// Wire `linkDynTempl` to emitted dynamic template group ids (`zzDT-*`, `dynSpawnTemplate`).
/// Scales BINVENTORY/RINVENTORY `initialAmount` like Fowl `WarehouseConfig::capacity`
/// (airport hub vs airbase; `warehouses` naval vs FOB vs airbase).
///
/// `TTD*` / `TTDN*`: listed `(side,type)` (quantity `X` or positive number) participates; omitted types do not,
/// when that axis has policy zones.
/// Ground ME `dynamicSpawn` is stamped from `SETTINGS-dynamic-spawn` after this pass (same phase as naval TTDN).
/// Rows are filled here only when `unlimitedFuel`, `unlimitedMunitions`, and `unlimitedAircrafts` are all false (finite export).
///
/// --- Planned carrier-specific pipeline (per-ship, not global naval union) ---
/// 1. For each ship warehouse (coalition side), prefill `aircrafts` rows from `BINVENTORY` / `RINVENTORY`
///    (same shape as today’s template merge).
/// 2. Set `linkDynTempl` only for rows with non-zero stock and a matching dynamic template group for `(side, type)`.
/// 3. For warehouse id tied to ship template key `K` (e.g. group name `RKuznecow` → zones `TTSNRKuznecow`,
///    `TTDNRKuznecow`), set `initialAmount = 0` for aircraft types not listed in that hull’s `TTDN*` zone
///    (stock caps the air wing). Keep `linkDynTempl` from DT_* emit for other coalition types so landed
///    aircraft can be warehoused and slotted later.
/// 4. `weapons` from B/RINVENTORY: when a weapon bridge is loaded, drop rows whose `wsType` is not allowed
///    for the same hub policy as A/C (`O*`: `weapon_ws_by_aircraft` keys only; per-hull `TTDN*` **carriers** and
///    `TTDdynFARP` **DEP dynamic FARPs** (one path per pad coalition): `dep_farp_weapon_allowlist` =
///    `naval_carrier_policy_weapon_allowlist` + policy-filtered `BINVENTORY+` / `RINVENTORY+` + `BDEFAULT+` / `RDEFAULT+`
///    (no global zone `All` bleed). Zone label rows require TTDdynFARP `Value` module + bridge ordnance for that type.
/// If ground bases still misbehave, consider generalising this 3-step pattern to airports / FARPs.
///
/// Carrier naming / zones are validated earlier by `audit_naval_carrier_mission_rules` (build fails if invalid).

fn is_inventory_cap_ordnance_ws(ws: [i32; 4]) -> bool {
    ws[0] == 4 && ((4..=8).contains(&ws[1]) || ws[1] == 15)
}

/// Per-hull `TTDN*`: bridge `weapon_ws_by_aircraft` keys (same base as `O*` warehouse prune) ∪ payload
/// pylons gated by `template.restricted`; plus `fueltank_by_aircraft`. No `expand_ws_alias_family` on
/// the merged set (avoids alias bleed; matches DEP FARP prune shape).
fn naval_carrier_policy_weapon_allowlist(
    br: &weapon_bridge::WeaponBridgeMap,
    vt: &VehicleTemplates,
    side: Side,
    policy_types: &HashSet<StdString>,
) -> HashSet<[i32; 4]> {
    let side_name = match side {
        Side::Blue => "blue",
        Side::Red => "red",
        Side::Neutral => return HashSet::new(),
    };
    let mut out = HashSet::new();
    for unit_type in policy_types {
        for ws in br.weapon_ws_for_aircraft_key_only(unit_type.as_str()) {
            if is_inventory_cap_ordnance_ws(ws) {
                out.insert(ws);
            }
        }
        let restricted =
            br.template_restricted_ws_for_side_type(side_name, unit_type.as_str());
        let pylon_raw =
            vt.payload_pylon_ws_for_unit_type_exact(br, side, unit_type.as_str());
        let pylon_gate = br.expand_ws_alias_family(&pylon_raw);
        for ws in pylon_raw {
            if !is_inventory_cap_ordnance_ws(ws) {
                continue;
            }
            if restricted.contains(&ws) && !pylon_gate.contains(&ws) {
                continue;
            }
            out.insert(ws);
        }
    }
    out.extend(br.fueltank_ws_for_aircrafts(policy_types));
    out
}

/// Single DEP FARP weapon allowlist: `TTDdynFARP` aircraft/pylons + policy-filtered `BINVENTORY+` / `RINVENTORY+` + `BDEFAULT+` / `RDEFAULT+` for that pad coalition (no global zone `All` bleed).
fn dep_farp_weapon_allowlist(
    br: &weapon_bridge::WeaponBridgeMap,
    vt: &VehicleTemplates,
    side: Side,
    policy_types: &HashSet<StdString>,
    inv_zone_plus: &[InventoryZonePlusModuleEntry],
    inv_ws_zone_specs: &HashMap<[i32; 4], WsZoneStockSpec>,
    inv_name_to_ws: &HashMap<StdString, [i32; 4]>,
    inv_weapon_ws: &HashSet<[i32; 4]>,
    def_zone_plus: &[InventoryZonePlusModuleEntry],
    def_ws_zone_specs: &HashMap<[i32; 4], WsZoneStockSpec>,
    def_name_to_ws: &HashMap<StdString, [i32; 4]>,
    def_weapon_ws: &HashSet<[i32; 4]>,
) -> HashSet<[i32; 4]> {
    let (inv_label, def_label) = match side {
        Side::Blue => ("BINVENTORY+", "BDEFAULT+"),
        Side::Red => ("RINVENTORY+", "RDEFAULT+"),
        Side::Neutral => ("B/RINVENTORY+", "B/RDEFAULT+"),
    };
    let mut s = naval_carrier_policy_weapon_allowlist(br, vt, side, policy_types);
    s.extend(inventory_plus_ordnance_ws_for_policy_modules(
        br,
        vt,
        side,
        policy_types,
        inv_zone_plus,
        inv_ws_zone_specs,
        inv_name_to_ws,
        inv_weapon_ws,
        inv_label,
    ));
    s.extend(default_zone_ws_for_policy_modules(
        br,
        side,
        def_zone_plus,
        def_ws_zone_specs,
        policy_types,
        def_name_to_ws,
        def_weapon_ws,
        def_label,
    ));
    s
}

fn policy_types_include_module(
    policy_types: &HashSet<StdString>,
    module: &str,
) -> bool {
    let m = weapon_bridge::normalized_aircraft_type_key(module);
    policy_types
        .iter()
        .any(|p| weapon_bridge::normalized_aircraft_type_key(p) == m)
}

/// Ordnance wsTypes from B/RINVENTORY+ zone links whose `Value` (module) is on this hub's carrier `TTDN*` or `TTDdynFARP` policy list.
fn inventory_plus_ordnance_ws_for_policy_modules(
    br: &weapon_bridge::WeaponBridgeMap,
    vt: &VehicleTemplates,
    side: Side,
    policy_types: &HashSet<StdString>,
    zone_plus: &[InventoryZonePlusModuleEntry],
    ws_zone_specs: &HashMap<[i32; 4], WsZoneStockSpec>,
    inv_name_to_ws: &HashMap<StdString, [i32; 4]>,
    inv_weapon_ws: &HashSet<[i32; 4]>,
    zone_label: &str,
) -> HashSet<[i32; 4]> {
    let mut out = ws_zone_stock_ws_for_policy_modules(ws_zone_specs, policy_types);
    let policy_weapon_ws = br.weapon_ws_for_aircraft_keys_only(policy_types);
    for e in zone_plus {
        if policy_types_include_module(policy_types, e.module.as_str()) {
            for ws in resolve_zone_link_ws_for_module(
                e.item_name.as_str(),
                e.module.as_str(),
                side,
                vt,
                br,
                inv_name_to_ws,
                inv_weapon_ws,
                zone_label,
            ) {
                out.insert(ws);
            }
        }
        // Label-only BINVENTORY+ rows: ordnance must be on the hub policy list, not whole coalition inventory.
        let label_ws: HashSet<[i32; 4]> = zone_item_label_ws_candidates(
            e.item_name.as_str(),
            inv_name_to_ws,
            br,
        )
        .into_iter()
        .filter(|w| is_inventory_cap_ordnance_ws(*w))
        .collect();
        // Label-only rows: zone `Value` must be on this hub's TTD policy list; wsType on that module's bridge list.
        for ws in label_ws {
            if !policy_types_include_module(policy_types, e.module.as_str()) {
                continue;
            }
            if policy_weapon_ws.contains(&ws) {
                out.insert(ws);
            }
        }
    }
    out
}

fn build_inventory_plus_resolution_maps(
    inv: Option<&Table<'static>>,
    plus: Option<&Table<'static>>,
) -> Result<(HashMap<StdString, [i32; 4]>, HashSet<[i32; 4]>)> {
    let Some(inv) = inv else {
        return Ok((HashMap::new(), HashSet::new()));
    };
    let mut nm = collect_inventory_weapon_display_name_to_ws(inv)?;
    let mut ws = collect_inventory_weapon_ws_set(inv)?;
    merge_inventory_plus_weapon_resolution_hints(&mut nm, &mut ws, plus)?;
    Ok((nm, ws))
}

fn patch_warehouse_dynamic_spawn_links(
    lua: &Lua,
    warehouses_root: &Table<'static>,
    emit: &DynamicSpawnEmit,
    blue_default: Option<&Table<'static>>,
    red_default: Option<&Table<'static>>,
    blue_inventory: Option<&Table<'static>>,
    red_inventory: Option<&Table<'static>>,
    blue_inventory_plus: Option<&Table<'static>>,
    red_inventory_plus: Option<&Table<'static>>,
    blue_default_plus: Option<&Table<'static>>,
    red_default_plus: Option<&Table<'static>>,
    zone_plus_blue: &[InventoryZonePlusModuleEntry],
    zone_plus_red: &[InventoryZonePlusModuleEntry],
    zone_ws_inventory_blue: &HashMap<[i32; 4], WsZoneStockSpec>,
    zone_ws_inventory_red: &HashMap<[i32; 4], WsZoneStockSpec>,
    zone_default_plus_blue: &[InventoryZonePlusModuleEntry],
    zone_default_plus_red: &[InventoryZonePlusModuleEntry],
    zone_ws_default_blue: &HashMap<[i32; 4], WsZoneStockSpec>,
    zone_ws_default_red: &HashMap<[i32; 4], WsZoneStockSpec>,
    mult_cfg: &WarehouseStockMultConfig,
    warehouse_caps: Option<&campaign_cfg::WarehouseDefaultsFromCfg>,
    obj_dyn_allow: &[ObjectiveDynAllow],
    warehouse_positions: &HashMap<i64, Vector2>,
    dyn_farp_aircraft_allow: Option<&HashSet<(Side, StdString)>>,
    ship_wh_aircraft_allow: Option<&HashMap<i64, HashSet<(Side, StdString)>>>,
    weapon_bridge: Option<&weapon_bridge::WeaponBridgeMap>,
    vehicle_templates: &VehicleTemplates,
) -> Result<()> {
    let (inv_plus_blue_nm, inv_plus_blue_ws) =
        build_inventory_plus_resolution_maps(blue_inventory, blue_inventory_plus)?;
    let (inv_plus_red_nm, inv_plus_red_ws) =
        build_inventory_plus_resolution_maps(red_inventory, red_inventory_plus)?;
    let (default_plus_blue_nm, default_plus_blue_ws) =
        build_default_plus_resolution_maps(blue_default_plus)?;
    let (default_plus_red_nm, default_plus_red_ws) =
        build_default_plus_resolution_maps(red_default_plus)?;
    let farp_inv_blue_pos = farp_positive_weapon_ws(blue_inventory_plus);
    let farp_inv_red_pos = farp_positive_weapon_ws(red_inventory_plus);
    let farp_def_blue_pos = farp_positive_weapon_ws(blue_default_plus);
    let farp_def_red_pos = farp_positive_weapon_ws(red_default_plus);

    fn patch_table(
        lua: &Lua,
        tbl: &Table<'static>,
        emit: &DynamicSpawnEmit,
        blue_default: Option<&Table<'static>>,
        red_default: Option<&Table<'static>>,
        blue_inventory: Option<&Table<'static>>,
        red_inventory: Option<&Table<'static>>,
        mult_cfg: &WarehouseStockMultConfig,
        is_airports_table: bool,
        _warehouse_caps: Option<&campaign_cfg::WarehouseDefaultsFromCfg>,
        obj_dyn_allow: &[ObjectiveDynAllow],
        warehouse_positions: &HashMap<i64, Vector2>,
        dyn_farp_aircraft_allow: Option<&HashSet<(Side, StdString)>>,
        ship_wh_aircraft_allow: Option<&HashMap<i64, HashSet<(Side, StdString)>>>,
        weapon_bridge: Option<&weapon_bridge::WeaponBridgeMap>,
        vehicle_templates: &VehicleTemplates,
        inv_plus_blue_nm: &HashMap<StdString, [i32; 4]>,
        inv_plus_blue_ws: &HashSet<[i32; 4]>,
        inv_plus_red_nm: &HashMap<StdString, [i32; 4]>,
        inv_plus_red_ws: &HashSet<[i32; 4]>,
        zone_plus_blue: &[InventoryZonePlusModuleEntry],
        zone_plus_red: &[InventoryZonePlusModuleEntry],
        zone_ws_inventory_blue: &HashMap<[i32; 4], WsZoneStockSpec>,
        zone_ws_inventory_red: &HashMap<[i32; 4], WsZoneStockSpec>,
        zone_default_plus_blue: &[InventoryZonePlusModuleEntry],
        zone_default_plus_red: &[InventoryZonePlusModuleEntry],
        zone_ws_default_blue: &HashMap<[i32; 4], WsZoneStockSpec>,
        zone_ws_default_red: &HashMap<[i32; 4], WsZoneStockSpec>,
        default_plus_blue_nm: &HashMap<StdString, [i32; 4]>,
        default_plus_blue_ws: &HashSet<[i32; 4]>,
        default_plus_red_nm: &HashMap<StdString, [i32; 4]>,
        default_plus_red_ws: &HashSet<[i32; 4]>,
        farp_inv_blue_pos: &HashSet<[i32; 4]>,
        farp_inv_red_pos: &HashSet<[i32; 4]>,
        farp_def_blue_pos: &HashSet<[i32; 4]>,
        farp_def_red_pos: &HashSet<[i32; 4]>,
        blue_default_plus: Option<&Table<'static>>,
        red_default_plus: Option<&Table<'static>>,
    ) -> Result<()> {
        let empty_inv_nm: HashMap<StdString, [i32; 4]> = HashMap::new();
        let empty_inv_ws: HashSet<[i32; 4]> = HashSet::new();
        let empty_zone_plus: &[InventoryZonePlusModuleEntry] = &[];
        let empty_ws_zone: HashMap<[i32; 4], WsZoneStockSpec> = HashMap::new();
        let empty_default_nm: HashMap<StdString, [i32; 4]> = HashMap::new();
        let empty_default_ws: HashSet<[i32; 4]> = HashSet::new();
        let empty_farp_pos: HashSet<[i32; 4]> = HashSet::new();

        for pair in tbl.clone().pairs::<Value, Table>() {
            let (k, wh) = pair?;
            let Some(wid) = warehouse_lua_key_i64(k) else {
                continue;
            };
            if !warehouse_all_unlimited_off(&wh) {
                warn!(
                    "warehouse {wid}: skipping dynamic-spawn fill patch (set unlimitedFuel, unlimitedMunitions, unlimitedAircrafts to false in ME)"
                );
                continue;
            }
            // O* airports: authoritative side from containing zone (4th letter R/B), else warehouse coalition.
            // DEP FARP / naval: prefer warehouse `coalition` so TTDdynFARP / TTDN policy keys match ME rows
            // when geometry overlaps another coalition's O* zone.
            let obj_zone: Option<&ObjectiveDynAllow> = if is_airports_table {
                // Airport rows are keyed by airport warehouse id; prefer explicit O* mapping via airbaseID.
                obj_dyn_allow
                    .iter()
                    .find(|o| o.airbase_id == Some(wid))
                    .or_else(|| {
                        warehouse_positions.get(&wid).and_then(|&pos| {
                            objective_dyn_allow_geom_pick(obj_dyn_allow, pos)
                        })
                    })
            } else {
                warehouse_positions
                    .get(&wid)
                    .and_then(|&pos| objective_dyn_allow_geom_pick(obj_dyn_allow, pos))
            };
            let mult = if let Some(allow) = obj_zone {
                if allow.is_logistics_hub {
                    mult_cfg.hub_max.max(1)
                } else if is_airports_table {
                    mult_cfg.mult_airport(wid)
                } else {
                    mult_cfg.mult_dynamic_row(wid, false)
                }
            } else if is_airports_table {
                mult_cfg.mult_airport(wid)
            } else {
                mult_cfg.mult_dynamic_row(wid, false)
            };
            let coa: String = wh.raw_get("coalition")?;
            let side_from_wh = match coa.to_lowercase().as_str() {
                "red" => Some(Side::Red),
                "blue" => Some(Side::Blue),
                _ => None,
            };
            // Naval / DEP FARP rows: trust warehouse `coalition` for TTDN / TTDdynFARP policy keys first.
            // O* geometry can overlap another coalition's zone and would mis-classify side vs `dyn_allow` / inventory copy.
            let side = if mult_cfg.dep_farp_warehouse_ids.contains(&wid)
                || mult_cfg.naval_warehouse_ids.contains(&wid)
            {
                side_from_wh.or_else(|| obj_zone.map(|o| o.side))
            } else {
                obj_zone.map(|o| o.side).or(side_from_wh)
            };

            let side = match side {
                Some(s) => s,
                None => {
                    // No O* zone and warehouse coalition is neutral.
                    if is_airports_table {
                        let blue_def = blue_default.context(
                            "BDEFAULT required for neutral Dynamic Spawn airports",
                        )?;
                        let red_def = red_default.context(
                            "RDEFAULT required for neutral Dynamic Spawn airports",
                        )?;
                        let blue_inv = blue_inventory.context(
                            "BINVENTORY required for neutral Dynamic Spawn airports",
                        )?;
                        let red_inv = red_inventory.context(
                            "RINVENTORY required for neutral Dynamic Spawn airports",
                        )?;
                        neutral_dynamic_spawn_airport_zero_stock_link_templates(
                            lua, &wh, wid, emit, mult_cfg, blue_def, red_def, blue_inv, red_inv,
                        )?;
                    } else {
                        empty_neutral_build_warehouse_row(
                            lua,
                            &wh,
                            NeutralWarehouseBuildKind::Other,
                        )?;
                    }
                    continue;
                }
            };

            let def = match side {
                Side::Blue => blue_default,
                Side::Red => red_default,
                Side::Neutral => None,
            };
            let inv = match side {
                Side::Blue => blue_inventory,
                Side::Red => red_inventory,
                Side::Neutral => None,
            };
            if let Some(def) = def {
                apply_mission_warehouse_template_stock(
                    lua,
                    &wh,
                    def,
                    mult,
                    WarehouseTemplateStockMode::DefaultHubWeapons,
                )?;
            }
            if let Some(inv) = inv {
                apply_mission_warehouse_template_stock(
                    lua,
                    &wh,
                    inv,
                    mult,
                    WarehouseTemplateStockMode::InventoryStock,
                )?;
            }
            let (ws_inv, ws_def, farp_inv, farp_def) = match side {
                Side::Blue => (
                    zone_ws_inventory_blue,
                    zone_ws_default_blue,
                    farp_inv_blue_pos,
                    farp_def_blue_pos,
                ),
                Side::Red => (
                    zone_ws_inventory_red,
                    zone_ws_default_red,
                    farp_inv_red_pos,
                    farp_def_red_pos,
                ),
                Side::Neutral => (
                    &empty_ws_zone,
                    &empty_ws_zone,
                    &empty_farp_pos,
                    &empty_farp_pos,
                ),
            };
            let is_dep_wh = mult_cfg.dep_farp_warehouse_ids.contains(&wid);
            let mut zone_stock_applied = HashSet::<[i32; 4]>::new();
            // DEP template pads: only TTDdynFARP-filtered zone stock (below), not global BINVENTORY+ All rows.
            if !is_dep_wh {
                apply_zone_ws_stock_amounts(
                    lua,
                    &wh,
                    ws_inv,
                    ws_def,
                    farp_inv,
                    farp_def,
                    mult,
                    WsZoneDistributeScope::All,
                    None,
                    &mut zone_stock_applied,
                )?;
            }
            let aircrafts: Table = wh.raw_get("aircrafts")?;
            for cat in ["helicopters", "planes"] {
                let Ok(cat_tbl) = aircrafts.raw_get::<_, Table>(cat) else {
                    continue;
                };
                for pair in cat_tbl.pairs::<String, Table>() {
                    let (unit_type, row) = pair?;
                    let link = if mult_cfg.naval_warehouse_ids.contains(&wid) {
                        emit
                            .ship_hull_by_wid
                            .get(&wid)
                            .and_then(|hull| {
                                emit.link_by_ship.get(&(
                                    side,
                                    unit_type.clone(),
                                    hull.clone(),
                                ))
                            })
                    } else {
                        None
                    }
                    .or_else(|| emit.link_by_side_type.get(&(side, unit_type.clone())))
                    .map(|gid| gid.inner())
                    .unwrap_or(0);
                    row.raw_set("linkDynTempl", link)?;
                }
            }
            // Per-base O* zone filter: zero initialAmount for types not in this base's
            // TTD allow list (derived from include_dyn_slots in its objective zone).
            // linkDynTempl is intentionally left as-is.
            if let Some(obj) = obj_zone {
                if obj.is_logistics_hub {
                    apply_zone_ws_stock_amounts(
                        lua,
                        &wh,
                        ws_inv,
                        ws_def,
                        farp_inv,
                        farp_def,
                        mult,
                        WsZoneDistributeScope::Filter,
                        None,
                        &mut zone_stock_applied,
                    )?;
                    continue;
                }
                // DEP template pads: `TTDdynFARP` + weapon allowlist below, not O* zone TTD / zone ws.
                if mult_cfg.dep_farp_warehouse_ids.contains(&wid) {
                    // fall through to DEP A/C prune and weapon bridge
                } else {
                let allowed_types = obj.per_side.get(&obj.side);
                let aircrafts: Table = wh.raw_get("aircrafts")?;
                for cat in ["helicopters", "planes"] {
                    let Ok(cat_tbl) = aircrafts.raw_get::<_, Table>(cat) else {
                        continue;
                    };
                    for pair in cat_tbl.pairs::<String, Table>() {
                        let (unit_type, row) = pair?;
                        let in_zone =
                            allowed_types.is_some_and(|s| s.contains(unit_type.as_str()));
                        if !in_zone {
                            let cur = row.raw_get::<_, u32>("initialAmount").unwrap_or(0);
                            if cur != 0 {
                                info!(
                                    "warehouse {wid}: zeroing {unit_type} amount={cur} (not in O* zone TTD allow)"
                                );
                                row.raw_set("initialAmount", 0u32)?;
                            }
                        }
                    }
                }
                }
            }
            // DEP* dynamic FARP template stocks: prune A/C rows by `TTDdynFARP` allowlist only.
            if mult_cfg.dep_farp_warehouse_ids.contains(&wid) {
                if let Some(dyn_allow) = dyn_farp_aircraft_allow {
                    let aircrafts: Table = wh.raw_get("aircrafts")?;
                    for cat in ["helicopters", "planes"] {
                        let Ok(cat_tbl) = aircrafts.raw_get::<_, Table>(cat) else {
                            continue;
                        };
                        for pair in cat_tbl.pairs::<String, Table>() {
                            let (unit_type, row) = pair?;
                            let key = (
                                side,
                                StdString::from(unit_type.as_str()),
                            );
                            let in_dyn = dyn_allow.contains(&key);
                            if !in_dyn {
                                let cur = row.raw_get::<_, u32>("initialAmount").unwrap_or(0);
                                if cur != 0 {
                                    info!(
                                        "warehouse {wid}: zeroing {unit_type} amount={cur} (not in {})",
                                        TTD_DYN_FARP_POLICY_ZONE
                                    );
                                    row.raw_set("initialAmount", 0u32)?;
                                }
                            }
                        }
                    }
                }
            }
            // Ship warehouse: `TTDN` + hull caps **initialAmount** only; leave `linkDynTempl` from DT_* emit so
            // types outside the hull air-wing list still carry dynamic templates (receive landed aircraft / slot later).
            if mult_cfg.naval_warehouse_ids.contains(&wid) {
                if let Some(m) = ship_wh_aircraft_allow {
                    if let Some(allow) = m.get(&wid) {
                        let aircrafts: Table = wh.raw_get("aircrafts")?;
                        for cat in ["helicopters", "planes"] {
                            let Ok(cat_tbl) = aircrafts.raw_get::<_, Table>(cat) else {
                                continue;
                            };
                            for pair in cat_tbl.pairs::<String, Table>() {
                                let (unit_type, row) = pair?;
                                let key = (
                                    side,
                                    StdString::from(unit_type.as_str()),
                                );
                                if !allow.contains(&key) {
                                    let cur =
                                        row.raw_get::<_, u32>("initialAmount").unwrap_or(0);
                                    if cur != 0 {
                                        info!(
                                            "warehouse {wid}: zeroing {unit_type} amount={cur} (not in TTDN ship allow for this hull)"
                                        );
                                        row.raw_set("initialAmount", 0u32)?;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if let Some(br) = weapon_bridge {
                let mut policy_types: Option<HashSet<StdString>> = None;
                if mult_cfg.naval_warehouse_ids.contains(&wid) {
                    if let Some(m) = ship_wh_aircraft_allow {
                        if let Some(allow) = m.get(&wid) {
                            let mut hs = HashSet::<StdString>::default();
                            for (s, ut) in allow {
                                if *s == side {
                                    hs.insert(ut.clone());
                                }
                            }
                            policy_types = Some(hs);
                        }
                    }
                } else if mult_cfg.dep_farp_warehouse_ids.contains(&wid) {
                    if let Some(da) = dyn_farp_aircraft_allow {
                        let hs: HashSet<StdString> = da
                            .iter()
                            .filter(|(s, _)| *s == side)
                            .map(|(_, ut)| ut.clone())
                            .collect();
                        policy_types = Some(hs);
                    }
                } else if let Some(obj) = obj_zone {
                    if !obj.is_logistics_hub
                        && !mult_cfg.dep_farp_warehouse_ids.contains(&wid)
                        && !mult_cfg.naval_warehouse_ids.contains(&wid)
                    {
                        if let Some(ts) = obj.per_side.get(&obj.side) {
                            policy_types = Some(ts.iter().cloned().collect());
                        }
                    }
                }

                if let Some(names) = policy_types {
                    let empty_strip = HashSet::<[i32; 4]>::new();
                    let is_naval = mult_cfg.naval_warehouse_ids.contains(&wid);
                    let is_dep = mult_cfg.dep_farp_warehouse_ids.contains(&wid);
                    let (nm, ws, zplus, ws_zplus) = match side {
                        Side::Blue => (
                            inv_plus_blue_nm,
                            inv_plus_blue_ws,
                            zone_plus_blue,
                            zone_ws_inventory_blue,
                        ),
                        Side::Red => (
                            inv_plus_red_nm,
                            inv_plus_red_ws,
                            zone_plus_red,
                            zone_ws_inventory_red,
                        ),
                        _ => (
                            &empty_inv_nm,
                            &empty_inv_ws,
                            empty_zone_plus,
                            &empty_ws_zone,
                        ),
                    };
                    let (ws_def_zplus, def_zplus, def_nm, def_ws, def_label) = match side {
                        Side::Blue => (
                            zone_ws_default_blue,
                            zone_default_plus_blue,
                            default_plus_blue_nm,
                            default_plus_blue_ws,
                            "BDEFAULT+",
                        ),
                        Side::Red => (
                            zone_ws_default_red,
                            zone_default_plus_red,
                            default_plus_red_nm,
                            default_plus_red_ws,
                            "RDEFAULT+",
                        ),
                        Side::Neutral => (
                            &empty_ws_zone,
                            empty_zone_plus,
                            &empty_default_nm,
                            &empty_default_ws,
                            "B/RDEFAULT+",
                        ),
                    };
                    let mut allowed_ws = if is_naval {
                        naval_carrier_policy_weapon_allowlist(
                            br,
                            vehicle_templates,
                            side,
                            &names,
                        )
                    } else if is_dep {
                        dep_farp_weapon_allowlist(
                            br,
                            vehicle_templates,
                            side,
                            &names,
                            zplus,
                            ws_zplus,
                            nm,
                            ws,
                            def_zplus,
                            ws_def_zplus,
                            def_nm,
                            def_ws,
                        )
                    } else {
                        br.weapon_ws_for_aircraft_keys_only(&names)
                    };
                    if is_naval {
                        let zone_label = match side {
                            Side::Blue => "BINVENTORY+",
                            Side::Red => "RINVENTORY+",
                            Side::Neutral => "B/RINVENTORY+",
                        };
                        allowed_ws.extend(inventory_plus_ordnance_ws_for_policy_modules(
                            br,
                            vehicle_templates,
                            side,
                            &names,
                            zplus,
                            ws_zplus,
                            nm,
                            ws,
                            zone_label,
                        ));
                        allowed_ws.extend(default_zone_ws_for_policy_modules(
                            br,
                            side,
                            def_zplus,
                            ws_def_zplus,
                            &names,
                            def_nm,
                            def_ws,
                            def_label,
                        ));
                    } else if !is_dep {
                        let zone_label = match side {
                            Side::Blue => "BINVENTORY+",
                            Side::Red => "RINVENTORY+",
                            Side::Neutral => "B/RINVENTORY+",
                        };
                        allowed_ws.extend(inventory_plus_ordnance_ws_for_policy_modules(
                            br,
                            vehicle_templates,
                            side,
                            &names,
                            zplus,
                            ws_zplus,
                            nm,
                            ws,
                            zone_label,
                        ));
                        allowed_ws.extend(default_zone_ws_for_policy_modules(
                            br,
                            side,
                            def_zplus,
                            ws_def_zplus,
                            &names,
                            def_nm,
                            def_ws,
                            def_label,
                        ));
                    }
                    // Naval only: unconditional BINVENTORY+ `All` ws rows. DEP uses TTDdynFARP allowlist only.
                    if is_naval {
                        allowed_ws.extend(ws_zone_force_keep_ws(ws_zplus, ws_def_zplus));
                    }
                    apply_zone_ws_stock_amounts(
                        lua,
                        &wh,
                        ws_zplus,
                        ws_def_zplus,
                        farp_inv,
                        farp_def,
                        mult,
                        WsZoneDistributeScope::Filter,
                        Some(&allowed_ws),
                        &mut zone_stock_applied,
                    )?;
                    let wlog = if is_dep {
                        format!(
                            "warehouse {wid} weapons (DEP FARP {TTD_DYN_FARP_POLICY_ZONE} + policy B/RINVENTORY+ + B/RDEFAULT+)"
                        )
                    } else {
                        format!(
                            "warehouse {wid} weapons (B/RINVENTORY filtered to allowed-aircraft wsTypes)"
                        )
                    };
                    prune_warehouse_weapons_row(
                        lua,
                        &wh,
                        &empty_strip,
                        Some(&allowed_ws),
                        &wlog,
                        None,
                    )?;
                    if is_naval {
                        let (def_tpl, def_plus_tpl) = match side {
                            Side::Blue => (blue_default, blue_default_plus),
                            Side::Red => (red_default, red_default_plus),
                            Side::Neutral => (None, None),
                        };
                        let mut def_sources: Vec<&Table<'static>> = Vec::new();
                        if let Some(t) = def_tpl {
                            def_sources.push(t);
                        }
                        if let Some(t) = def_plus_tpl {
                            def_sources.push(t);
                        }
                        if !def_sources.is_empty() {
                            ensure_positive_default_ordnance_for_allowed_ws(
                                lua,
                                &wh,
                                &def_sources,
                                &allowed_ws,
                                mult,
                            )?;
                        }
                        let mut zone_filter_reapply = HashSet::<[i32; 4]>::new();
                        apply_zone_ws_stock_amounts(
                            lua,
                            &wh,
                            &empty_ws_zone,
                            ws_def_zplus,
                            &empty_farp_pos,
                            &empty_farp_pos,
                            mult,
                            WsZoneDistributeScope::Filter,
                            Some(&allowed_ws),
                            &mut zone_filter_reapply,
                        )?;
                    }
                }
            }
        }
        Ok(())
    }

    let airports =
        warehouses_root.raw_get::<_, Table>("airports").context("getting airports")?;
    patch_table(
        lua,
        &airports,
        emit,
        blue_default,
        red_default,
        blue_inventory,
        red_inventory,
        mult_cfg,
        true,
        warehouse_caps,
        obj_dyn_allow,
        warehouse_positions,
        dyn_farp_aircraft_allow,
        ship_wh_aircraft_allow,
        weapon_bridge,
        vehicle_templates,
        &inv_plus_blue_nm,
        &inv_plus_blue_ws,
        &inv_plus_red_nm,
        &inv_plus_red_ws,
        zone_plus_blue,
        zone_plus_red,
        zone_ws_inventory_blue,
        zone_ws_inventory_red,
        zone_default_plus_blue,
        zone_default_plus_red,
        zone_ws_default_blue,
        zone_ws_default_red,
        &default_plus_blue_nm,
        &default_plus_blue_ws,
        &default_plus_red_nm,
        &default_plus_red_ws,
        &farp_inv_blue_pos,
        &farp_inv_red_pos,
        &farp_def_blue_pos,
        &farp_def_red_pos,
        blue_default_plus,
        red_default_plus,
    )
    .context("patching airport linkDynTempl")?;

    let warehouses = warehouses_root
        .raw_get::<_, Table>("warehouses")
        .context("getting warehouses")?;
    patch_table(
        lua,
        &warehouses,
        emit,
        blue_default,
        red_default,
        blue_inventory,
        red_inventory,
        mult_cfg,
        false,
        warehouse_caps,
        obj_dyn_allow,
        warehouse_positions,
        dyn_farp_aircraft_allow,
        ship_wh_aircraft_allow,
        weapon_bridge,
        vehicle_templates,
        &inv_plus_blue_nm,
        &inv_plus_blue_ws,
        &inv_plus_red_nm,
        &inv_plus_red_ws,
        zone_plus_blue,
        zone_plus_red,
        zone_ws_inventory_blue,
        zone_ws_inventory_red,
        zone_default_plus_blue,
        zone_default_plus_red,
        zone_ws_default_blue,
        zone_ws_default_red,
        &default_plus_blue_nm,
        &default_plus_blue_ws,
        &default_plus_red_nm,
        &default_plus_red_ws,
        &farp_inv_blue_pos,
        &farp_inv_red_pos,
        &farp_def_blue_pos,
        &farp_def_red_pos,
        blue_default_plus,
        red_default_plus,
    )
    .context("patching warehouse linkDynTempl")?;
    Ok(())
}

fn compile_objectives(base: &LoadedMiz) -> Result<Vec<TriggerZone>> {
    let mut objectives = Vec::new();
    for zone in base
        .mission
        .raw_get::<_, Table>("triggers")
        .context("getting triggers")?
        .raw_get::<_, Table>("zones")
        .context("getting zones")?
        .pairs::<Value, Table>()
    {
        let zone = zone?.1;
        if let Some(t) = TriggerZone::new(&zone)? {
            objectives.push(t);
        }
    }
    Ok(objectives)
}

/// Default owner at index 3: `O` + type (`AB`/`FO`/`LO`) + `B`/`R`/`N` + display name (same as `mizinit`).
fn objective_default_owner_from_zone_name(zone_name: &str) -> Option<Side> {
    if !zone_name.starts_with('O') || zone_name.len() < 4 {
        return None;
    }
    match zone_name.as_bytes()[3] {
        b'B' => Some(Side::Blue),
        b'R' => Some(Side::Red),
        b'N' => Some(Side::Neutral),
        _ => None,
    }
}

fn count_objectives_by_default_owner(objectives: &[TriggerZone]) -> (usize, usize, usize) {
    let mut blue = 0usize;
    let mut red = 0usize;
    let mut neutral = 0usize;
    for obj in objectives {
        let Ok(zone_name) = obj.inner.name() else {
            continue;
        };
        if zone_name.len() >= 3 && &zone_name[1..3] == "PR" {
            continue;
        }
        match objective_default_owner_from_zone_name(&zone_name) {
            Some(Side::Blue) => blue += 1,
            Some(Side::Red) => red += 1,
            Some(Side::Neutral) => neutral += 1,
            _ => {}
        }
    }
    (blue, red, neutral)
}

fn count_objective_kind_by_owner(objectives: &[TriggerZone], kind: &str) -> (usize, usize) {
    let mut blue = 0usize;
    let mut red = 0usize;
    for obj in objectives {
        let Ok(zone_name) = obj.inner.name() else {
            continue;
        };
        if !zone_name.starts_with('O') || zone_name.len() < 4 {
            continue;
        }
        if &zone_name[1..3] != kind {
            continue;
        }
        match zone_name.as_bytes()[3] {
            b'B' => blue += 1,
            b'R' => red += 1,
            _ => {}
        }
    }
    (blue, red)
}

fn count_factory_statics_in_opr_zone(
    base: &LoadedMiz,
    zone: &TriggerZone,
    factory_types: &std::collections::HashSet<StdString>,
) -> Result<u32> {
    let mut n = 0u32;
    for (_side, coa) in
        Side::ALL.into_iter().map(|side| (side, base.mission.coalition(side)))
    {
        let coa = coa?;
        for country in coa.countries()? {
            let country = country?;
            for group in country.statics()? {
                let group = group?;
                for unit in group
                    .raw_get::<_, Table>("units")?
                    .pairs::<Value, Table>()
                {
                    let unit = unit?.1;
                    let typ: String = unit.raw_get("type")?;
                    if !factory_types.contains(&StdString::from(typ.as_str())) {
                        continue;
                    }
                    let x: f64 = unit.raw_get("x")?;
                    let y: f64 = unit.raw_get("y")?;
                    if zone.contains(Vector2::new(x, y))? {
                        n += 1;
                    }
                }
            }
        }
    }
    Ok(n)
}

fn set_zone_capacity_property(zone: &miz::TriggerZone, capacity: u32) -> Result<()> {
    let lua = unsafe { &*LUA };
    let props: Table = zone.raw_get("properties")?;
    let mut keep: Vec<(StdString, StdString)> = Vec::new();
    for pair in props.clone().pairs::<Value, Table>() {
        let (_idx, prop) = pair?;
        let key: String = prop.raw_get("key").unwrap_or_else(|_| String::from(""));
        let val: String = prop.raw_get("value").unwrap_or_else(|_| String::from(""));
        if key.eq_ignore_ascii_case("capacity") {
            continue;
        }
        keep.push((
            StdString::from(key.as_str()),
            StdString::from(val.as_str()),
        ));
    }
    keep.push((
        StdString::from("Capacity"),
        StdString::from(format_compact!("{capacity}").as_str()),
    ));
    let new_props = lua.create_table()?;
    for (i, (k, v)) in keep.into_iter().enumerate() {
        let row = lua.create_table()?;
        row.raw_set("key", String::from(k.as_str()))?;
        row.raw_set("value", String::from(v.as_str()))?;
        new_props.raw_set(i + 1, row)?;
    }
    zone.raw_set("properties", new_props)?;
    Ok(())
}

fn apply_opr_zone_capacity_properties(
    base: &LoadedMiz,
    objectives: &[TriggerZone],
    factory_types: &std::collections::HashSet<StdString>,
) -> Result<()> {
    if factory_types.is_empty() {
        warn!("production_factory_units missing in campaign CFG; skipping OPR Capacity properties");
        return Ok(());
    }
    for obj in objectives {
        let zone_name = obj.inner.name()?;
        if zone_name.len() < 4 || &zone_name[1..3] != "PR" {
            continue;
        }
        let n = count_factory_statics_in_opr_zone(base, obj, factory_types)?;
        set_zone_capacity_property(&obj.inner, n)?;
        info!("OPR zone {zone_name}: Capacity={n}");
    }
    Ok(())
}

const OBJECTIVE_ZONE_KINDS: [&str; 4] = ["AB", "FO", "LO", "PR"];

/// `O` + kind (`AB`/`FO`/`LO`/`PR`) + owner (`B`/`R`/`N`) + display; stem = all after `O` (bflib key).
fn validate_objective_zone_names(objectives: &[TriggerZone]) -> Result<()> {
    let mut stems: HashMap<StdString, StdString> = HashMap::new();
    let mut legacy_key_zones: HashMap<StdString, Vec<StdString>> = HashMap::new();
    let mut errors: Vec<compact_str::CompactString> = Vec::new();

    for obj in objectives {
        let Ok(zone_name) = obj.inner.name() else {
            continue;
        };
        if !zone_name.starts_with('O') {
            continue;
        }
        if zone_name.len() <= 4 {
            errors.push(format_compact!(
                "zone {zone_name}: too short; expected O + kind (AB/FO/LO/PR) + owner (B/R/N) + display name"
            ));
            continue;
        }
        let kind = &zone_name[1..3];
        if !OBJECTIVE_ZONE_KINDS.contains(&kind) {
            errors.push(format_compact!(
                "zone {zone_name}: unknown objective kind {kind}; expected AB, FO, LO, or PR"
            ));
        }
        match zone_name.as_bytes()[3] {
            b'B' | b'R' | b'N' => {}
            c => errors.push(format_compact!(
                "zone {zone_name}: invalid owner coalition byte {:?}; expected B, R, or N after kind",
                c as char
            )),
        }
        let stem = StdString::from(&zone_name[1..]);
        if let Some(prev) = stems.insert(stem.clone(), StdString::from(zone_name.as_str())) {
            errors.push(format_compact!(
                "duplicate objective stem O{stem}: zones {prev} and {zone_name}"
            ));
        }
        legacy_key_zones
            .entry(StdString::from(&zone_name[3..]))
            .or_default()
            .push(StdString::from(zone_name.as_str()));
    }

    for (legacy_key, zones) in legacy_key_zones {
        if zones.len() < 2 {
            continue;
        }
        let kinds: HashSet<StdString> = zones.iter().map(|z| StdString::from(&z[1..3])).collect();
        if kinds.len() > 1 {
            errors.push(format_compact!(
                "zones {:?} share owner+display suffix {legacy_key:?} but use different kinds {:?}; \
                 use distinct display names or coalition letters (e.g. OPRBKutaisi vs OABBKutaisi2)",
                zones,
                kinds
            ));
        }
    }

    if !errors.is_empty() {
        bail!(
            "FowlTools ERROR: objective zone name validation failed. {}",
            errors.join(" | ")
        );
    }
    Ok(())
}

/// OPR* zones must be quad (square) perimeters in the ME, not circles.
fn validate_opr_zone_geometry(objectives: &[TriggerZone]) -> Result<()> {
    let mut errors = Vec::new();
    for obj in objectives {
        let Ok(zone_name) = obj.inner.name() else {
            continue;
        };
        if zone_name.len() < 4 || &zone_name[1..3] != "PR" {
            continue;
        }
        match obj.inner.typ() {
            Ok(TriggerZoneTyp::Quad(_)) => {}
            Ok(TriggerZoneTyp::Circle { .. }) => {
                errors.push(format_compact!(
                    "OPR zone {zone_name}: circle trigger zone; use a quad (square) zone in the ME"
                ));
            }
            Err(e) => errors.push(format_compact!(
                "OPR zone {zone_name}: could not read zone geometry: {e}"
            )),
        }
    }
    if !errors.is_empty() {
        bail!(
            "FowlTools ERROR: OPR* zone geometry validation failed. {}",
            errors.join(" | ")
        );
    }
    Ok(())
}

/// Each OPR* zone must contain at least one ME static whose type is in `production_factory_units`.
fn validate_opr_zones_have_factory_statics(
    base: &LoadedMiz,
    objectives: &[TriggerZone],
    factory_types: &std::collections::HashSet<StdString>,
) -> Result<()> {
    let mut opr_zones: Vec<(String, &TriggerZone)> = Vec::new();
    for obj in objectives {
        let Ok(zone_name) = obj.inner.name() else {
            continue;
        };
        if zone_name.len() < 4 || &zone_name[1..3] != "PR" {
            continue;
        }
        opr_zones.push((zone_name.clone(), obj));
    }
    if opr_zones.is_empty() {
        return Ok(());
    }
    if factory_types.is_empty() {
        bail!(
            "FowlTools ERROR: base.miz defines OPR* zone(s) but campaign CFG has no production_factory_units; \
             add production_factory_units to the Fowl *_CFG JSON"
        );
    }
    let mut errors: Vec<compact_str::CompactString> = Vec::new();
    for (zone_name, obj) in &opr_zones {
        let n = count_factory_statics_in_opr_zone(base, obj, factory_types)?;
        if n == 0 {
            errors.push(format_compact!(
                "OPR zone {zone_name}: no static matching production_factory_units; \
                 place at least one factory unit listed in production_factory_units inside this zone in the ME"
            ));
        }
    }
    if !errors.is_empty() {
        bail!(
            "FowlTools ERROR: OPR* factory static validation failed. {}",
            errors.join(" | ")
        );
    }
    Ok(())
}

/// Factory statics inside OPR* zones, once per static per coalition (overlap-safe).
fn count_production_factories_by_coalition(
    base: &LoadedMiz,
    objectives: &[TriggerZone],
    factory_types: &std::collections::HashSet<StdString>,
) -> Result<(u32, u32)> {
    if factory_types.is_empty() {
        return Ok((0, 0));
    }
    let mut opr_zones: Vec<(Side, &TriggerZone)> = Vec::new();
    for obj in objectives {
        let Ok(zone_name) = obj.inner.name() else {
            continue;
        };
        if zone_name.len() < 4 || &zone_name[1..3] != "PR" {
            continue;
        }
        let side = match zone_name.as_bytes()[3] {
            b'B' => Side::Blue,
            b'R' => Side::Red,
            _ => continue,
        };
        opr_zones.push((side, obj));
    }
    if opr_zones.is_empty() {
        return Ok((0, 0));
    }
    let mut blue = 0u32;
    let mut red = 0u32;
    for (_side, coa) in Side::ALL.into_iter().map(|side| (side, base.mission.coalition(side))) {
        let coa = coa?;
        for country in coa.countries()? {
            let country = country?;
            for group in country.statics()? {
                let group = group?;
                for unit in group
                    .raw_get::<_, Table>("units")?
                    .pairs::<Value, Table>()
                {
                    let unit = unit?.1;
                    let typ: String = unit.raw_get("type")?;
                    if !factory_types.contains(&StdString::from(typ.as_str())) {
                        continue;
                    }
                    let x: f64 = unit.raw_get("x")?;
                    let y: f64 = unit.raw_get("y")?;
                    let pos = Vector2::new(x, y);
                    let mut in_blue = false;
                    let mut in_red = false;
                    for (side, zone) in &opr_zones {
                        if !zone.contains(pos)? {
                            continue;
                        }
                        match side {
                            Side::Blue => in_blue = true,
                            Side::Red => in_red = true,
                            Side::Neutral => {}
                        }
                    }
                    if in_blue {
                        blue += 1;
                    }
                    if in_red {
                        red += 1;
                    }
                }
            }
        }
    }
    Ok((blue, red))
}

/// Warn when factory static counts suggest overlapping OPR zones or map-wide bleed.
fn validate_opr_factory_static_counts(
    base: &LoadedMiz,
    objectives: &[TriggerZone],
    factory_types: &HashSet<StdString>,
) -> Result<()> {
    if factory_types.is_empty() {
        return Ok(());
    }
    let mut total = 0u32;
    let mut per_zone: Vec<(StdString, u32)> = Vec::new();
    for obj in objectives {
        let Ok(zone_name) = obj.inner.name() else {
            continue;
        };
        if zone_name.len() < 4 || &zone_name[1..3] != "PR" {
            continue;
        }
        let n = count_factory_statics_in_opr_zone(base, obj, factory_types)?;
        total = total.saturating_add(n);
        per_zone.push((StdString::from(zone_name.as_str()), n));
    }
    for (zone_name, n) in &per_zone {
        if *n > 32 {
            warn!(
                "OPR zone {zone_name}: {n} factory static(s) in zone; \
                 tighten the quad or reduce matching types"
            );
        }
    }
    let mut seen_positions: HashMap<(i64, i64), StdString> = HashMap::new();
    let mut duplicate_positions = 0u32;
    for obj in objectives {
        let Ok(zone_name) = obj.inner.name() else {
            continue;
        };
        if zone_name.len() < 4 || &zone_name[1..3] != "PR" {
            continue;
        }
        for (_side, coa) in Side::ALL.into_iter().map(|side| (side, base.mission.coalition(side))) {
            let coa = coa?;
            for country in coa.countries()? {
                let country = country?;
                for group in country.statics()? {
                    let group = group?;
                    for unit in group
                        .raw_get::<_, Table>("units")?
                        .pairs::<Value, Table>()
                    {
                        let unit = unit?.1;
                        let typ: String = unit.raw_get("type")?;
                        if !factory_types.contains(&StdString::from(typ.as_str())) {
                            continue;
                        }
                        let x: f64 = unit.raw_get("x")?;
                        let y: f64 = unit.raw_get("y")?;
                        if !obj.contains(Vector2::new(x, y))? {
                            continue;
                        }
                        let key = ((x * 10.).round() as i64, (y * 10.).round() as i64);
                        if let Some(prev) = seen_positions.insert(key, StdString::from(zone_name.as_str())) {
                            if prev != StdString::from(zone_name.as_str()) {
                                duplicate_positions += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    if duplicate_positions > 0 {
        warn!(
            "{duplicate_positions} factory static position(s) fall inside multiple OPR* zones; \
             bflib links each static once (nearest OPR) — tighten quads to avoid double Capacity counts (total in zones: {total})"
        );
    }
    Ok(())
}

fn validate_logistics_zones_not_neutral(objectives: &[TriggerZone]) -> Result<()> {
    let mut bad = Vec::new();
    for obj in objectives {
        let Ok(zone_name) = obj.inner.name() else {
            continue;
        };
        if zone_name.len() >= 4
            && zone_name.starts_with("OLO")
            && zone_name.as_bytes()[3] == b'N'
        {
            bad.push(zone_name.clone());
        }
    }
    if !bad.is_empty() {
        bail!(
            "FowlTools ERROR: neutral logistics zones OLON* are not allowed (use OLOB* or OLOR*): {:?}",
            bad
        );
    }
    Ok(())
}

fn validate_production_zone_counts(objectives: &[TriggerZone]) -> Result<()> {
    let (opr_blue, opr_red) = count_objective_kind_by_owner(objectives, "PR");
    let (olo_blue, olo_red) = count_objective_kind_by_owner(objectives, "LO");
    let mut errors = Vec::new();
    if opr_blue < olo_blue {
        errors.push(format_compact!(
            "Blue has {} OLO* but only {} OPR* zone(s); add at least {} OPR* zone(s) to base.miz",
            olo_blue,
            opr_blue,
            olo_blue - opr_blue
        ));
    }
    if opr_red < olo_red {
        errors.push(format_compact!(
            "Red has {} OLO* but only {} OPR* zone(s); add at least {} OPR* zone(s) to base.miz",
            olo_red,
            opr_red,
            olo_red - opr_red
        ));
    }
    if !errors.is_empty() {
        bail!(
            "FowlTools ERROR: OPR* >= OLO* validation failed. {}",
            errors.join(" | ")
        );
    }
    Ok(())
}

fn compile_tzf_plane_fuel_zones(base: &LoadedMiz) -> Result<Vec<TzfPlaneFuelZone>> {
    let mut out = Vec::new();
    for zone in base
        .mission
        .raw_get::<_, Table>("triggers")
        .context("getting triggers")?
        .raw_get::<_, Table>("zones")
        .context("getting zones")?
        .pairs::<Value, Table>()
    {
        let zone = zone?.1;
        if let Some(z) = TzfPlaneFuelZone::try_from_trigger_table(&zone)? {
            out.push(z);
        }
    }
    Ok(out)
}

fn collect_objective_aircraft_by_side(
    base: &LoadedMiz,
    objectives: &[TriggerZone],
    carrier_pad_objectives: &HashMap<String, String>,
) -> Result<HashMap<StdString, HashMap<Side, HashSet<StdString>>>> {
    let mut out: HashMap<StdString, HashMap<Side, HashSet<StdString>>> =
        HashMap::default();
    for (side, coa) in
        Side::ALL.into_iter().map(|side| (side, base.mission.coalition(side)))
    {
        let coa = coa?;
        for country in coa.raw_get::<_, Table>("country")?.pairs::<Value, Table>() {
            let country = country?.1;
            for group in vehicle(&country, "plane")
                .context("getting planes")?
                .chain(vehicle(&country, "helicopter").context("getting helicopters")?)
            {
                let group = group.context("getting group")?;
                for unit in group
                    .raw_get::<_, Table>("units")
                    .context("getting units")?
                    .pairs::<Value, Table>()
                {
                    let unit = unit.context("getting unit")?.1;
                    if unit.raw_get::<_, String>("skill")?.as_str() != "Client" {
                        continue;
                    }
                    let unit_type: String = unit.raw_get("type")?;
                    let x = unit.get("x")?;
                    let y = unit.get("y")?;
                    let unit_pos = Vector2::new(x, y);
                    if let Some(obj_name) = client_slot_objective_name(
                        unit_pos,
                        &group,
                        objectives,
                        base,
                        carrier_pad_objectives,
                    )? {
                        out.entry(StdString::from(obj_name.as_str()))
                            .or_default()
                            .entry(side)
                            .or_default()
                            .insert(unit_type.to_string());
                    } else {
                        bail!(
                            "slot unit {} is not associated with an objective",
                            value_to_json(&Value::Table(unit.clone()))
                        );
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Fallback grouping when pads have no `warehouses.warehouses` rows (slave-only / odd exports).
fn logistics_hub_logical_name(display_name: &str) -> StdString {
    match display_name.rsplit_once('-') {
        Some((base, suf))
            if !suf.is_empty() && suf.chars().all(|c| c.is_ascii_digit()) =>
        {
            base.to_string()
        }
        _ => display_name.to_string(),
    }
}

/// Same key as warehouse backend when `warehouses.airports[id]` has finite stock export (all unlimited toggles off).
fn airports_dynamic_spawn_backend_key(ap: &Table<'static>, unit_id: i64) -> Result<Option<i64>> {
    match ap
        .raw_get::<_, Value>(unit_id)
        .with_context(|| format_compact!("warehouses.airports[{unit_id}]"))?
    {
        Value::Table(t) => {
            if warehouse_all_unlimited_off(&t) {
                Ok(Some(unit_id))
            } else {
                Ok(None)
            }
        }
        Value::Nil => Ok(None),
        other => bail!(
            "warehouses.airports[{unit_id}]: expected table or nil, got {:?}",
            other
        ),
    }
}

/// Resolve DEP pad `unitId` → backend key scanning template then `--base`.
fn dep_farp_backend_key_for_pad(
    unit_id: i64,
    warehouse_row_tables: &[&Table<'static>],
    airports_row_tables: &[&Table<'static>],
) -> Result<Option<i64>> {
    for wh in warehouse_row_tables {
        if farp_own_warehouse_key(wh, unit_id)?.is_some() {
            return Ok(Some(unit_id));
        }
    }
    for ap in airports_row_tables {
        if airports_dynamic_spawn_backend_key(ap, unit_id)?.is_some() {
            return Ok(Some(unit_id));
        }
    }
    Ok(None)
}

/// `warehouses.warehouses` key for this pad (`unitId`), or `Nil` → shares another pad's row (`whids` apply loop).
fn farp_own_warehouse_key(
    wh_warehouse: &Table<'static>,
    unit_id: i64,
) -> Result<Option<i64>> {
    match wh_warehouse
        .raw_get::<_, Value>(unit_id)
        .with_context(|| format_compact!("warehouses.warehouses[{unit_id}]"))?
    {
        Value::Table(_) => Ok(Some(unit_id)),
        Value::Nil => Ok(None),
        other => bail!(
            "warehouses.warehouses[{unit_id}]: expected table or nil, got {:?}",
            other
        ),
    }
}

fn mission_trigger_zone_contains(
    zone: &miz::TriggerZone<'_>,
    v: Vector2,
) -> Result<bool> {
    let center = zone.pos()?;
    Ok(match zone.typ()? {
        TriggerZoneTyp::Circle { radius } => {
            radius.powi(2) >= na::distance_squared(&v.into(), &center.into())
        }
        TriggerZoneTyp::Quad(q) => q.contains(LuaVec2(v)),
    })
}

fn is_dep_farp_placement_zone(name: impl AsRef<str>) -> bool {
    let n = name.as_ref();
    n.starts_with("BDEPFARP")
        || n.starts_with("RDEPFARP")
        || n.starts_with("NDEPFARP")
}

/// Pad groups for the shipped DEP FARP theatre template (`DEPBFARPPAD0`, …) — naming does not match `BDEPFARP*` triggers.
fn is_dep_named_template_pad_group(group_name: &str) -> bool {
    group_name.starts_with("DEPBFARP")
        || group_name.starts_with("DEPRFARP")
        || group_name.starts_with("DEPNFARP")
}

/// When `--base` omitted trigger placement zones (`BDEPFARP*`/`…`), classify pads via template group naming.
fn extend_dep_farp_backend_ids_from_template_named_groups(
    base: &LoadedMiz,
    warehouse_row_tables: &[&Table<'static>],
    airports_row_tables: &[&Table<'static>],
    out: &mut HashSet<i64>,
) -> Result<()> {
    for (_side, coa) in Side::ALL.into_iter().map(|side| (side, base.mission.coalition(side))) {
        let coa = coa?;
        for country in coa.countries()? {
            let country = country?;
            for group in vehicle(&country, "static")?
                .chain(vehicle(&country, "plane")?)
                .chain(vehicle(&country, "helicopter")?)
            {
                let group = group?;
                let Ok(group_name) = group.raw_get::<_, String>("name") else {
                    continue;
                };
                if !is_dep_named_template_pad_group(&group_name) {
                    continue;
                }
                let Ok(units) = group.raw_get::<_, Table>("units") else {
                    continue;
                };
                for unit in units.clone().pairs::<Value, Table>() {
                    let unit = unit?.1;
                    let typ: String = unit.raw_get("type")?;
                    let typ_s = typ.as_str();
                    if typ_s != "FARP"
                        && typ_s != "SINGLE_HELIPAD"
                        && typ_s != "FARP_SINGLE_01"
                        && typ_s != "Invisible FARP"
                    {
                        continue;
                    }
                    let unit_id: i64 = unit.raw_get("unitId")?;
                    if dep_farp_backend_key_for_pad(
                        unit_id,
                        warehouse_row_tables,
                        airports_row_tables,
                    )?
                    .is_some()
                    {
                        out.insert(unit_id);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Backend keys (`warehouses.warehouses` or `warehouses.airports`) for DEP FARP template pads.
///
/// (1) FARP-type units inside optional trigger zones `BDEPFARP*` / `RDEPFARP*` / `NDEPFARP*`.
/// (2) FARP-type units in static groups named `DEPBFARP*` / `DEPRFARP*` / `DEPNFARP*` (Caucasus-style template).
///
/// Rows may appear only under `--base` OR only under `warehouse<campaign_decade>.miz`; pass both lookups.
fn collect_dep_farp_warehouse_ids(
    base: &LoadedMiz,
    warehouse_row_tables: &[&Table<'static>],
    airports_row_tables: &[&Table<'static>],
) -> Result<HashSet<i64>> {
    let mut out: HashSet<i64> = HashSet::default();
    for zr in base.mission.triggers()? {
        let zone = zr?;
        let name = zone.name()?;
        if !is_dep_farp_placement_zone(name.as_str()) {
            continue;
        }
        for (_side, coa) in
            Side::ALL.into_iter().map(|side| (side, base.mission.coalition(side)))
        {
            let coa = coa?;
            for country in coa.countries()? {
                let country = country?;
                for group in vehicle(&country, "static")?
                    .chain(vehicle(&country, "plane")?)
                    .chain(vehicle(&country, "helicopter")?)
                {
                    let group = group?;
                    for unit in
                        group.raw_get::<_, Table>("units")?.pairs::<Value, Table>()
                    {
                        let unit = unit?.1;
                        let typ: String = unit.raw_get("type")?;
                        let typ_s = typ.as_str();
                        if typ_s != "FARP"
                            && typ_s != "SINGLE_HELIPAD"
                            && typ_s != "FARP_SINGLE_01"
                            && typ_s != "Invisible FARP"
                        {
                            continue;
                        }
                        let x: f64 = unit.raw_get("x")?;
                        let y: f64 = unit.raw_get("y")?;
                        if !mission_trigger_zone_contains(&zone, Vector2::new(x, y))? {
                            continue;
                        }
                        let unit_id: i64 = unit.raw_get("unitId")?;
                        if dep_farp_backend_key_for_pad(
                            unit_id,
                            warehouse_row_tables,
                            airports_row_tables,
                        )?
                        .is_some()
                        {
                            out.insert(unit_id);
                        }
                    }
                }
            }
        }
    }
    let n_from_triggers = out.len();
    extend_dep_farp_backend_ids_from_template_named_groups(
        base,
        warehouse_row_tables,
        airports_row_tables,
        &mut out,
    )?;
    let n_from_groups = out.len().saturating_sub(n_from_triggers);
    info!(
        "DEP FARP backend keys: {} from B/R/NDEPFARP placement trigger zone(s), +{} from DEPBFARP*/DEPRFARP*/DEPNFARP* pad group name(s), {} unique total",
        n_from_triggers,
        n_from_groups,
        out.len()
    );
    Ok(out)
}

fn validate_single_airbase_per_objective(
    objectives: &[TriggerZone],
    base: &LoadedMiz,
) -> Result<()> {
    // Prefer distinct `warehouses.warehouses` backend keys — matches ME/shared-stock semantics (see `for id in whids`).
    let wh_warehouse = base
        .warehouses
        .raw_get::<_, Table>("warehouses")
        .context("getting warehouses.warehouses for objective airbase audit")?;

    let mut errors: Vec<std::string::String> = vec![];

    for obj in objectives {
        let mut backends: Vec<(i64, StdString)> = vec![];
        let mut display_names_in_zone: Vec<StdString> = vec![];
        for (_side, coa) in
            Side::ALL.into_iter().map(|side| (side, base.mission.coalition(side)))
        {
            let coa = coa?;
            for country in coa.countries()? {
                let country = country?;
                for group in country.statics()? {
                    let group = group?;
                    for unit in
                        group.raw_get::<_, Table>("units")?.pairs::<Value, Table>()
                    {
                        let unit = unit?.1;
                        let typ: String = unit.raw_get("type")?;
                        let typ_s = typ.as_str();
                        if typ_s != "FARP"
                            && typ_s != "SINGLE_HELIPAD"
                            && typ_s != "FARP_SINGLE_01"
                            && typ_s != "Invisible FARP"
                        {
                            continue;
                        }
                        let name: String = unit.raw_get("name")?;
                        let unit_id: i64 = unit.raw_get("unitId")?;
                        let x: f64 = unit.raw_get("x")?;
                        let y: f64 = unit.raw_get("y")?;
                        if obj.contains(Vector2::new(x, y))? {
                            display_names_in_zone.push(name.as_str().to_string());
                            if let Some(key) =
                                farp_own_warehouse_key(&wh_warehouse, unit_id)?
                            {
                                backends.push((key, name.as_str().to_string()));
                            }
                        }
                    }
                }
            }
        }

        backends.sort_by_key(|(id, _)| *id);
        backends.dedup_by_key(|(id, _)| *id);

        let multiple_backends = backends.len() > 1;
        let mut ambiguous_by_name_only = false;
        if backends.is_empty() && !display_names_in_zone.is_empty() {
            let mut labs = display_names_in_zone
                .iter()
                .map(|s| logistics_hub_logical_name(s.as_str()))
                .collect::<Vec<_>>();
            labs.sort();
            labs.dedup();
            ambiguous_by_name_only = labs.len() > 1;
        }

        if multiple_backends || ambiguous_by_name_only {
            let detail = if multiple_backends {
                backends
                    .iter()
                    .map(|(id, n)| format!("{} [warehouses.key={id}]", n))
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                let mut labs = display_names_in_zone
                    .iter()
                    .map(|s| logistics_hub_logical_name(s.as_str()))
                    .collect::<Vec<_>>();
                labs.sort();
                labs.dedup();
                labs.join(", ")
            };
            errors.push(format!(
                "objective {} has multiple airbases inside the trigger zone: {}",
                obj.objective_name, detail
            ));
        }
    }

    if !errors.is_empty() {
        bail!("{}", errors.join("\n"));
    }
    Ok(())
}

fn collect_hub_warehouse_ids_from_objectives(
    objectives: &[TriggerZone],
    base: &LoadedMiz,
) -> Result<HashSet<i64>> {
    let mut out: HashSet<i64> = HashSet::default();
    for obj in objectives {
        let zone_name = obj.inner.name()?;
        if !zone_name.starts_with("OLO") {
            continue;
        }
        for (_side, coa) in
            Side::ALL.into_iter().map(|side| (side, base.mission.coalition(side)))
        {
            let coa = coa?;
            for country in coa.countries()? {
                let country = country?;
                for group in vehicle(&country, "static")?
                    .chain(vehicle(&country, "plane")?)
                    .chain(vehicle(&country, "helicopter")?)
                {
                    let group = group?;
                    for unit in
                        group.raw_get::<_, Table>("units")?.pairs::<Value, Table>()
                    {
                        let unit = unit?.1;
                        let typ: String = unit.raw_get("type")?;
                        let typ_s = typ.as_str();
                        if typ_s != "FARP"
                            && typ_s != "SINGLE_HELIPAD"
                            && typ_s != "FARP_SINGLE_01"
                            && typ_s != "Invisible FARP"
                        {
                            continue;
                        }
                        let x: f64 = unit.raw_get("x")?;
                        let y: f64 = unit.raw_get("y")?;
                        if obj.contains(Vector2::new(x, y))? {
                            let id: i64 = unit.raw_get("unitId")?;
                            out.insert(id);
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Deterministic OLO* hub airport ids from objective zone `airbaseID` properties.
fn collect_hub_airport_ids_from_olo_airbase_props(
    base: &LoadedMiz,
) -> Result<HashSet<i64>> {
    let mut out: HashSet<i64> = HashSet::default();
    for zone in base.mission.triggers()? {
        let zone = zone?;
        let name = zone.name()?;
        if !name.starts_with("OLO") {
            continue;
        }
        for prop in zone.properties()? {
            let prop = prop?;
            if !prop.key.eq_ignore_ascii_case("airbaseID") {
                continue;
            }
            let raw = prop.value.trim();
            if raw.is_empty() {
                break;
            }
            if let Ok(id) = raw.parse::<i64>() {
                if id > 0 {
                    out.insert(id);
                }
            }
            break;
        }
    }
    Ok(out)
}

fn infer_hub_airport_ids_from_objectives(
    objectives: &[TriggerZone],
    objective_aircraft_by_side: &HashMap<StdString, HashMap<Side, HashSet<StdString>>>,
    base: &LoadedMiz,
) -> Result<HashSet<i64>> {
    fn row_aircraft_types_with_stock(row: &Table) -> Result<(Side, HashSet<StdString>)> {
        let coa: StdString = row.raw_get("coalition").unwrap_or_default();
        let side = match coa.to_ascii_lowercase().as_str() {
            "blue" => Side::Blue,
            "red" => Side::Red,
            _ => bail!("unsupported airport coalition {:?}", coa),
        };
        let mut out: HashSet<StdString> = HashSet::default();
        if let Ok(aircrafts) = row.raw_get::<_, Table>("aircrafts") {
            for cat in ["helicopters", "planes"] {
                let Ok(cat_tbl) = aircrafts.raw_get::<_, Table>(cat) else {
                    continue;
                };
                for pair in cat_tbl.clone().pairs::<StdString, Table>() {
                    let (unit_type, u) = pair?;
                    let amt = u.raw_get::<_, u32>("initialAmount").unwrap_or(0);
                    if amt > 0 {
                        out.insert(unit_type);
                    }
                }
            }
        }
        Ok((side, out))
    }

    let mut hub_objectives: HashSet<StdString> = HashSet::default();
    for obj in objectives {
        let zone_name = obj.inner.name()?;
        if zone_name.starts_with("OLO") {
            hub_objectives.insert(obj.objective_name.to_string());
        }
    }
    if hub_objectives.is_empty() {
        return Ok(HashSet::default());
    }

    let airports = base
        .warehouses
        .raw_get::<_, Table>("airports")
        .context("getting airports for OLO hub inference")?;
    let mut out: HashSet<i64> = HashSet::default();

    for pair in airports.clone().pairs::<i64, Table>() {
        let (id, row) = pair?;
        let Ok((side, row_types)) = row_aircraft_types_with_stock(&row) else {
            continue;
        };
        if row_types.is_empty() {
            continue;
        }

        let mut exact_hub = false;
        let mut hub_score = 0usize;
        let mut non_hub_score = 0usize;
        for (obj_name, by_side) in objective_aircraft_by_side {
            let Some(obj_types) = by_side.get(&side) else {
                continue;
            };
            let score = row_types.intersection(obj_types).count();
            if hub_objectives.contains(obj_name) {
                hub_score = hub_score.max(score);
                if *obj_types == row_types {
                    exact_hub = true;
                }
            } else {
                non_hub_score = non_hub_score.max(score);
            }
        }

        if exact_hub || (hub_score > 0 && hub_score > non_hub_score) {
            out.insert(id);
        }
    }
    Ok(out)
}

fn format_allowed_campaign_decades() -> std::string::String {
    campaign_cfg::ALLOWED_CAMPAIGN_DECADES.join(", ")
}

fn resolve_weapon_template_path(
    cfg: &MizCmd,
    campaign_overlay: Option<&campaign_cfg::CampaignWarehouseOverlay>,
) -> Result<PathBuf> {
    let Some(overlay) = campaign_overlay else {
        info!("campaign cfg not provided; using legacy --weapon path {:?}", cfg.weapon);
        return Ok(cfg.weapon.clone());
    };
    let Some(decade) = overlay.campaign_decade.as_deref() else {
        bail!(
            "campaign cfg {:?} is missing \"campaign_decade\". Set one of [{}]. \
             Expected weapon template filename: weapon<campaign_decade>.miz (e.g. weapon1980s.miz).",
            cfg.campaign_cfg.as_ref().unwrap_or(&cfg.weapon),
            format_allowed_campaign_decades()
        );
    };
    if !campaign_cfg::ALLOWED_CAMPAIGN_DECADES.contains(&decade) {
        bail!(
            "campaign cfg {:?} has unsupported campaign_decade={:?}. Allowed values: [{}]. \
             Also ensure weapon file is named weapon<campaign_decade>.miz.",
            cfg.campaign_cfg.as_ref().unwrap_or(&cfg.weapon),
            decade,
            format_allowed_campaign_decades()
        );
    }
    let expected_name = format!("weapon{decade}.miz");
    let expected_path = cfg.weapon.with_file_name(expected_name.clone());
    if !expected_path.exists() {
        bail!(
            "missing weapon template {:?}. Expected file name for campaign_decade {:?} is {:?}. \
             Allowed campaign_decade values: [{}]. \
             Fix: set \"campaign_decade\" correctly in mission CFG and place matching weapon<campaign_decade>.miz and warehouse<campaign_decade>.miz in the mission folder.",
            expected_path,
            decade,
            expected_name,
            format_allowed_campaign_decades()
        );
    }
    info!("campaign_decade {:?} -> weapon template {:?}", decade, expected_path);
    Ok(expected_path)
}

/// With `--campaign-cfg`, loads `warehouse<campaign_decade>.miz` beside the anchor path
/// (`--warehouse` if set, else the resolved weapon template path).
fn resolve_warehouse_template_path(
    cfg: &MizCmd,
    overlay: &campaign_cfg::CampaignWarehouseOverlay,
    weapon_template_path: &Path,
) -> Result<PathBuf> {
    let Some(decade) = overlay.campaign_decade.as_deref() else {
        bail!(
            "campaign cfg {:?} is missing \"campaign_decade\". Set one of [{}]. \
             Expected warehouse template filename: warehouse<campaign_decade>.miz (e.g. warehouse1980s.miz).",
            cfg.campaign_cfg.as_deref().unwrap_or(weapon_template_path),
            format_allowed_campaign_decades()
        );
    };
    if !campaign_cfg::ALLOWED_CAMPAIGN_DECADES.contains(&decade) {
        bail!(
            "campaign cfg {:?} has unsupported campaign_decade={:?}. Allowed values: [{}]. \
             Also ensure warehouse file is named warehouse<campaign_decade>.miz.",
            cfg.campaign_cfg.as_deref().unwrap_or(weapon_template_path),
            decade,
            format_allowed_campaign_decades()
        );
    }
    let anchor =
        cfg.warehouse.as_ref().map(|p| p.as_path()).unwrap_or(weapon_template_path);
    let expected_name = format!("warehouse{decade}.miz");
    let expected_path = anchor.with_file_name(expected_name.clone());
    if !expected_path.exists() {
        bail!(
            "missing warehouse template {:?}. Expected file name for campaign_decade {:?} is {:?}. \
             Allowed campaign_decade values: [{}]. \
             Fix: set \"campaign_decade\" correctly in mission CFG and place matching weapon<campaign_decade>.miz and warehouse<campaign_decade>.miz in the mission folder.",
            expected_path,
            decade,
            expected_name,
            format_allowed_campaign_decades()
        );
    }
    info!("campaign_decade {:?} -> warehouse template {:?}", decade, expected_path);
    Ok(expected_path)
}

fn find_deployable_covering_tisp_template<'a>(
    cfg: &'a Cfg,
    side: Side,
    template: &str,
) -> Option<&'a Deployable> {
    cfg.deployables.get(&side).into_iter().flatten().find(|d| {
        d.provides_tisp_ship_template(template)
    })
}

/// Drops `mission.triggers.zones` entries whose names are in `remove` (re-sequence array part).
fn remove_mission_trigger_zones_named(
    lua: &'static Lua,
    mission: &Miz<'_>,
    remove: &HashSet<StdString>,
) -> Result<()> {
    let triggers: Table = mission.raw_get("triggers")?;
    let zones: Table = triggers.raw_get("zones")?;
    let mut kept = Vec::<Table>::new();
    for z in zones.sequence_values::<Table>() {
        let z = z?;
        let name: String = z.raw_get("name")?;
        if remove.contains(name.as_str()) {
            continue;
        }
        kept.push(z);
    }
    let new_zones = lua.create_table()?;
    for (i, z) in kept.into_iter().enumerate() {
        new_zones.raw_set((i + 1) as i64, z)?;
    }
    triggers.raw_set("zones", new_zones)?;
    Ok(())
}

fn audit_tisp_initial_ship_zones(
    lua: &'static Lua,
    mission: &Miz<'_>,
    idx: &miz::MizIndex,
    campaign_cfg: Option<&Path>,
) -> Result<()> {
    const RED: &str = "\x1b[31m";
    const RESET: &str = "\x1b[0m";
    let mut malformed: Vec<String> = Vec::new();
    let mut rows: Vec<(StdString, StdString, u32)> = Vec::new();
    for zone in mission.triggers()? {
        let zone = zone?;
        let name = zone.name()?;
        if !starts_with_tisp_prefix(name.as_str()) {
            continue;
        }
        if let Some(p) = parse_tisp_zone_name(name.as_str()) {
            rows.push((
                StdString::from(p.template),
                StdString::from(p.full_name),
                p.instance_index,
            ));
        } else {
            malformed.push(name);
        }
    }
    if rows.is_empty() && malformed.is_empty() {
        return Ok(());
    }
    if !malformed.is_empty() {
        malformed.sort();
        for z in &malformed {
            eprintln!(
                "{RED}ERROR malformed TISP trigger zone name \"{z}\": expected {TISP_PREFIX}{{B|R}}ShipName with optional trailing -N (example: {TISP_PREFIX}BTarawa, {TISP_PREFIX}BFrigate, {TISP_PREFIX}BFrigate-1, {TISP_PREFIX}BFrigate-2).{RESET}"
            );
        }
        eprintln!(
            "{RED}Mission assembly was interrupted: the output .miz was not written and no mission files were copied.{RESET}"
        );
        bail!("malformed TISP trigger zone name(s)");
    }
    let cfg_path = match campaign_cfg {
        Some(p) => p,
        None => {
            eprintln!(
                "{RED}ERROR the base mission contains TISP* initial-ship placement zones but --campaign-cfg was not passed.{RESET}"
            );
            eprintln!(
                "{RED}Fix: pass the mission Fowl *_CFG JSON with --campaign-cfg so FowlTools can verify each TISP template against deployables (Group template + limit).{RESET}"
            );
            eprintln!(
                "{RED}Mission assembly was interrupted: the output .miz was not written and no mission files were copied.{RESET}"
            );
            bail!("TISP zones require --campaign-cfg");
        }
    };
    let cfg: Cfg = serde_json::from_reader(File::open(cfg_path).with_context(|| {
        format_compact!("opening campaign cfg for TISP audit {:?}", cfg_path)
    })?)
    .with_context(|| format_compact!("decoding campaign cfg for TISP audit {:?}", cfg_path))?;
    let mut templates: Vec<StdString> = rows.iter().map(|(t, _, _)| t.clone()).collect();
    templates.sort();
    templates.dedup();
    let mut to_remove: HashSet<StdString> = HashSet::default();
    for template in &templates {
        let side = match template.as_bytes().first() {
            Some(b'B') => Side::Blue,
            Some(b'R') => Side::Red,
            _ => bail!("internal: TISP template {:?}", template),
        };
        let Some(dep) = find_deployable_covering_tisp_template(&cfg, side, template.as_str())
        else {
            eprintln!(
                "{RED}ERROR TISP template {:?}: no deployables entry for {:?} covers ship template {:?} (Group, Objective.pad_templates, or legacy top-level \"template\").{RESET}",
                template, side, template
            );
            eprintln!(
                "{RED}Fix: add or adjust a deployable so one of those matches the ME ship group name.{RESET}"
            );
            eprintln!(
                "{RED}Mission assembly was interrupted: the output .miz was not written and no mission files were copied.{RESET}"
            );
            bail!("TISP template missing from CFG deployables");
        };
        mission
            .get_group_by_name(idx, GroupKind::Any, side, template.as_str())?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "missing ship template group {template:?} for {side:?} (ME group name must match the deployable Group template)"
                )
            })
            .with_context(|| format_compact!("TISP template group {:?}", template))?;
        let limit = dep.limit as usize;
        let mut slots: Vec<(u32, StdString)> = rows
            .iter()
            .filter(|(t, _, _)| t == template)
            .map(|(_, fname, inst)| (*inst, fname.clone()))
            .collect();
        slots.sort_by(|(ia, na), (ib, nb)| ia.cmp(ib).then_with(|| na.cmp(nb)));
        for (_, zname) in slots.into_iter().skip(limit) {
            to_remove.insert(zname);
        }
    }
    if !to_remove.is_empty() {
        let mut listed: Vec<&str> = to_remove.iter().map(|s| s.as_str()).collect();
        listed.sort_unstable();
        warn!(
            "FowlTools removed {} TISP* trigger zone(s) over CFG deployable limit (not used at runtime): {:?}",
            listed.len(),
            listed
        );
        remove_mission_trigger_zones_named(lua, mission, &to_remove)?;
    }
    Ok(())
}

fn validate_base_fowl_trigger_zone_names(mission: &Miz) -> Result<()> {
    for zone in mission.triggers()? {
        let zone = zone?;
        let name = zone.name()?;
        if !fowl_trigger_zone_name_valid(&name) {
            const RED: &str = "\x1b[31m";
            const RESET: &str = "\x1b[0m";
            eprintln!(
                "{RED}ERROR invalid trigger zone type code {name}, expected {}{RESET}",
                FOWL_TRIGGER_ZONE_EXPECTED_PREFIXES_DISPLAY
            );
            eprintln!(
                "Fix: open the mission you pass as --base in the DCS Mission Editor, find trigger zone \"{name}\", and rename it so the name starts with: O (Fowl objective), G (Fowl objective-group spawn), T (slot/template tooling, e.g. TTS*/TTD*/…), or SETTINGS- (FowlTools build toggles)."
            );
            eprintln!(
                "Mission assembly was interrupted: the output .miz was not written and no mission files were copied."
            );
            bail!("invalid Fowl trigger zone name");
        }
    }
    Ok(())
}

fn audit_objective_display_aliases(base: &LoadedMiz) -> Result<()> {
    let mut count = 0usize;
    for zone in base.mission.triggers()? {
        let zone = zone?;
        if zone.name()?.as_str() != SETTINGS_OBJECTIVE_ALIASES_ZONE {
            continue;
        }
        for prop in zone.properties()? {
            let prop = prop?;
            let key = prop.key.trim();
            let value = prop.value.trim();
            if key.is_empty() || value.is_empty() {
                continue;
            }
            count += 1;
        }
        info!(
            "SETTINGS-aliases: {count} internal id → display name mapping(s) (runtime map labels)"
        );
        break;
    }
    Ok(())
}

pub fn run(cfg: &MizCmd) -> Result<()> {
    let campaign_overlay: Option<campaign_cfg::CampaignWarehouseOverlay> = match cfg
        .campaign_cfg
        .as_ref()
    {
        None => None,
        Some(p) => {
            let w = campaign_cfg::load_overlay(p)
                .with_context(|| format!("loading campaign cfg {:?}", p))?;
            const YELLOW: &str = "\x1b[33m";
            const RESET: &str = "\x1b[0m";
            info!(
                    "campaign warehouse defaults (BDEFAULT/RDEFAULT weapons):\n{YELLOW}aa_missiles: {}\nag_missiles: {}\nag_rockets: {}\nag_bombs: {}\nag_guided_bombs: {}\nfueltanks: {}\nFueltanks_empty: {}\nmisc: {}{RESET}",
                    w.defaults.aa_missiles,
                    w.defaults.ag_missiles,
                    w.defaults.ag_rockets,
                    w.defaults.ag_bombs,
                    w.defaults.ag_guided_bombs,
                    w.defaults.fueltanks,
                    w.defaults.fueltanks_empty,
                    w.defaults.misc
                );
            if let Some(ref m) = w.warehouse_multipliers {
                info!("campaign warehouse multipliers from JSON: {:?}", m);
            }
            Some(w)
        }
    };
    let warehouse_defaults = campaign_overlay.as_ref().map(|o| &o.defaults);
    let wm = campaign_overlay.as_ref().and_then(|o| o.warehouse_multipliers.as_ref());
    let airbase_max = wm.and_then(|w| w.airbase_max).unwrap_or(cfg.warehouse_airbase_max);
    let hub_max = wm.and_then(|w| w.hub_max).unwrap_or(cfg.warehouse_hub_max);
    let fob_max = wm.and_then(|w| w.fob_max).unwrap_or(1);
    let farp_max = wm.and_then(|w| w.farp_max).unwrap_or(fob_max);
    let carrier_airbase_max = wm.and_then(|w| w.carrier_airbase_max).unwrap_or(1);

    let mut hub_airport_ids: HashSet<i64> = HashSet::default();
    if let Some(ref s) = cfg.warehouse_hub_ids {
        for part in s.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let id = part.parse::<i64>().with_context(|| {
                format_compact!("invalid --warehouse-hub-ids entry {part:?}")
            })?;
            hub_airport_ids.insert(id);
        }
    }
    let mut fob_warehouse_ids: HashSet<i64> = HashSet::default();
    if let Some(ref s) = cfg.warehouse_fob_ids {
        for part in s.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let id = part.parse::<i64>().with_context(|| {
                format_compact!("invalid --warehouse-fob-ids entry {part:?}")
            })?;
            fob_warehouse_ids.insert(id);
        }
    }
    let lua = Box::leak(Box::new(Lua::new()));
    lua.gc_stop();
    let lua = unsafe {
        LUA = lua;
        &*LUA
    };
    let mut base = LoadedMiz::new(lua, &cfg.base).context("loading base mission")?;
    validate_base_fowl_trigger_zone_names(&base.mission)
        .context("validating Fowl trigger zone names (must match runtime)")?;
    crate::discord_map_icons::validate_discord_map_zones(
        &base.mission,
        cfg.campaign_cfg.as_deref(),
    )
    .context("discord map ME corner zones")?;
    audit_objective_display_aliases(&base).context("SETTINGS-aliases zone")?;
    let base_idx = base
        .mission
        .index()
        .context("indexing base mission for TISP audit")?;
    audit_tisp_initial_ship_zones(lua, &base.mission, &base_idx, cfg.campaign_cfg.as_deref())
        .context("TISP initial-ship placement zones")?;
    let airbase_export_path = resolve_airbase_export_path(cfg)
        .context("resolving fowl_airbase_export JSON path")?;
    if let Some(ref p) = airbase_export_path {
        info!("using airbase export {}", p.display());
        warn_airbase_export_theatre_mismatch(&base.mission, p);
    }
    let apply_summary = apply_objective_airbase_ids_from_export(
        &mut base,
        airbase_export_path.as_deref(),
    )
        .context("applying objective airbaseID values from fowl_airbase_export-DCS.version.*.json")?;
    log_airbase_objective_zone_mapping(&base)
        .context("logging airbase ID / OAB-OLO zone mapping from zone properties")?;
    let (missing_airbase, invalid_airbase) = validate_objective_airbase_ids(&base)
        .context("validating objective airbaseID requirements (OAB*/OLO*)")?;
    let mut unresolved_surprising: Vec<StdString> = apply_summary
        .unresolved
        .iter()
        .filter(|n| !objective_zone_airbase_id_absent_is_expected(n.as_str()))
        .cloned()
        .collect();
    if !unresolved_surprising.is_empty() {
        unresolved_surprising.sort_unstable();
        unresolved_surprising.dedup();
    }
    let mut objectives = compile_objectives(&base).context("compiling objectives")?;
    validate_objective_zone_names(&objectives)
        .context("validating O* zone names (unique O+kind+owner+display stems)")?;
    validate_opr_zone_geometry(&objectives)
        .context("validating OPR* zones use quad (square) geometry in base.miz")?;
    validate_logistics_zones_not_neutral(&objectives)
        .context("validating OLO* zones are OLOB* or OLOR* (not OLON*)")?;
    validate_production_zone_counts(&objectives)
        .context("validating OPR* >= OLO* per coalition in base.miz")?;
    let factory_types: std::collections::HashSet<StdString> = campaign_overlay
        .as_ref()
        .map(|o| {
            o.production_factory_units
                .iter()
                .map(|s| StdString::from(s.as_str()))
                .collect()
        })
        .unwrap_or_default();
    validate_opr_zones_have_factory_statics(&base, &objectives, &factory_types)
        .context("validating each OPR* zone contains production_factory_units")?;
    apply_opr_zone_capacity_properties(&base, &objectives, &factory_types)
        .context("writing OPR* Capacity zone properties from factory statics")?;
    validate_opr_factory_static_counts(&base, &objectives, &factory_types)
        .context("checking OPR* factory static counts for overlap")?;
    let tzf_plane_fuel =
        compile_tzf_plane_fuel_zones(&base).context("compiling TZF plane fuel overlays")?;
    if !tzf_plane_fuel.is_empty() {
        info!(
            "TZF: {} overlay zone(s) — client aircraft inside get internal fuel stripped at build (remove overlays to keep weapon*.miz fuel)",
            tzf_plane_fuel.len(),
        );
    }
    validate_single_airbase_per_objective(&objectives, &base)
        .context("validating single airbase per objective")?;
    let mut hub_warehouse_ids = collect_hub_warehouse_ids_from_objectives(&objectives, &base)
        .context("collecting OLO* objective warehouse ids")?;
    let weapon_template_path =
        resolve_weapon_template_path(cfg, campaign_overlay.as_ref())
            .context("resolving weapon template by campaign_decade")?;
    let resolved_warehouse_path: Option<PathBuf> = if let Some(ref ov) = campaign_overlay
    {
        Some(
            resolve_warehouse_template_path(cfg, ov, weapon_template_path.as_path())
                .context("resolving warehouse template by campaign_decade")?,
        )
    } else if cfg.warehouse.is_some() {
        bail!(
                "--warehouse requires --campaign-cfg. FowlTools does not load warehouse.miz; \
                 use warehouse<campaign_decade>.miz (e.g. warehouse1980s.miz) with matching weapon<campaign_decade>.miz and \"campaign_decade\" in the Fowl *_CFG JSON."
            );
    } else {
        None
    };
    let weapon_bridge_path = if let Some(ref p) = cfg.weapon_bridge {
        if p.exists() {
            Some(p.clone())
        } else {
            warn!("--weapon-bridge path does not exist: {:?}", p);
            None
        }
    } else {
        let parent = weapon_template_path.parent();
        parent.and_then(weapon_bridge::resolve_auto_bridge_path)
    };
    let mut weapon_bridge_map: Option<weapon_bridge::WeaponBridgeMap> =
        match weapon_bridge_path.as_ref() {
            Some(p) => {
                let m = weapon_bridge::WeaponBridgeMap::load(p)
                    .with_context(|| format!("loading weapon bridge {}", p.display()))?;
                info!("weapon bridge: {} descriptors from {}", m.len(), p.display());
                Some(m)
            }
            None => {
                info!(
                "no weapon bridge JSON (--weapon-bridge, or fowl_weapon_bridge.json / fowl_weapon_bridge-DCS.version.*.json next to resolved weapon template); run Fowl_engine_export.lua in DCS Hooks first"
            );
                None
            }
        };
    let (vehicle_templates, droptank_ws_from_weapon_warehouses) = {
        let wep = LoadedMiz::new(lua, &weapon_template_path)
            .context("loading weapon template")?;
        (
            VehicleTemplates::new(&wep).context("loading templates")?,
            collect_droptank_ws_by_coalition_from_warehouses_root(&wep.warehouses)?,
        )
    };
    if let (Some(bridge_p), Some(ref mut wb)) =
        (weapon_bridge_path.as_ref(), weapon_bridge_map.as_mut())
    {
        let sidecar = vehicle_templates.build_fowl_weapon_payload_ws_file(wb);
        let out_path = bridge_p
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(weapon_bridge::FOWL_WEAPON_PAYLOAD_WS);
        sidecar
            .write(&out_path)
            .with_context(|| format!("write {}", out_path.display()))?;
        wb.reload_template_payload_ws(bridge_p).with_context(|| {
            format!("reload template payload ws after {}", out_path.display())
        })?;
        let n_pyl: usize = sidecar.pylon_ws_by_side.values().map(|m| m.len()).sum();
        let n_rst: usize = sidecar.restricted_ws_by_side.values().map(|m| m.len()).sum();
        info!(
            "wrote {} (template payload wsTypes: {} aircraft pylon map(s), {} aircraft restricted map(s))",
            out_path.display(),
            n_pyl,
            n_rst
        );
    }
    {
        let blue_vote = vehicle_templates.payload_weapon_descriptor_union(Side::Blue);
        let red_vote = vehicle_templates.payload_weapon_descriptor_union(Side::Red);
        info!(
            "payload weapon descriptors (restricted=blocked; allow if any pylons or restricted_count < mention_count): blue={} red={}",
            blue_vote.len(),
            red_vote.len()
        );
        if let Some(ref wb) = weapon_bridge_map {
            let blue = vehicle_templates
                .payload_warehouse_bridge_descriptor_keys(wb, Side::Blue);
            let red =
                vehicle_templates.payload_warehouse_bridge_descriptor_keys(wb, Side::Red);
            let rb = blue
                .iter()
                .filter(|s| {
                    !wb.ws_types_for_descriptor_or_key_substring(s.as_str()).is_empty()
                })
                .count();
            let rr = red
                .iter()
                .filter(|s| {
                    !wb.ws_types_for_descriptor_or_key_substring(s.as_str()).is_empty()
                })
                .count();
            info!(
                "warehouse bridge descriptor keys (payload vote + pylon-only fuel): blue={} red={}",
                blue.len(),
                red.len()
            );
            info!(
                "weapon bridge coverage: blue {}/{} red {}/{} strings map to ≥1 wsType (exact or substring)",
                rb,
                blue.len(),
                rr,
                red.len()
            );
        }
    }
    let warehouse_bundle: Option<WarehouseBundle> = match resolved_warehouse_path.as_ref()
    {
        None => None,
        Some(wh) => {
            let path = wh.clone();
            let loaded = LoadedMiz::new(lua, wh).context("loading warehouse template")?;
            let template = WarehouseTemplate::new(&loaded, cfg)
                .context("compiling warehouse template")?;
            Some(WarehouseBundle {
                path,
                loaded,
                template,
            })
        }
    };
    let carrier_pad_objectives = match cfg.campaign_cfg.as_ref() {
        Some(p) => {
            let fowl_cfg: Cfg = serde_json::from_reader(
                File::open(p).with_context(|| format!("opening campaign cfg {p:?}"))?,
            )
            .context("parsing campaign cfg for carrier pad_templates")?;
            let m = build_carrier_pad_objective_names(&fowl_cfg);
            if !m.is_empty() {
                info!(
                    "carrier pad -> objective map: {} deployable pad_template(s) from CFG",
                    m.len()
                );
            }
            m
        }
        None => HashMap::default(),
    };
    vehicle_templates.generate_slots(lua, &mut base).context("generating slots")?;
    vehicle_templates
        .apply(
            lua,
            &mut objectives,
            &mut base,
            &tzf_plane_fuel,
            &carrier_pad_objectives,
        )
        .context("applying vehicle templates")?;
    let ship_wh_for_late = collect_ship_warehouse_group_map(&base)?;
    set_warehouse_ships_late_activation(&mut base, &ship_wh_for_late)
        .context("warehouse ship lateActivation")?;
    let objective_aircraft_by_side = collect_objective_aircraft_by_side(
        &base,
        &objectives,
        &carrier_pad_objectives,
    )
    .context("collecting objective aircraft module map")?;
    info!(
        "objective module map prepared for {} objective(s)",
        objective_aircraft_by_side.len()
    );
    let dynamic_emit = vehicle_templates
        .emit_dynamic_spawn_templates(lua, &mut base)
        .context("emitting dynamic spawn templates")?;
    let obj_dyn_allow = build_objective_dyn_allow(&base, &dynamic_emit.dyn_templates)
        .context("building per-base objective dyn allow map")?;
    extend_hub_warehouse_ids_for_olo_logistics(&mut hub_warehouse_ids, &obj_dyn_allow, &base)
        .context("OLO* logistics hub warehouse ids for hub_max multiplier")?;
    sync_l10n_dictionary_sortie_stem_to_output_miz(&base, &cfg.output)
        .context("l10n: set dictionary[mission.sortie] to --output .miz stem (DCS Saved Games / Fowl files)")?;
    let s = serialize_to_lua("mission", Value::Table((&*base.mission).clone()))?;
    fs::write(&base.miz.files["mission"], &s).context("writing mission file")?;
    info!("wrote serialized mission to mission file.");
    let ship_wh_map = collect_ship_warehouse_group_map(&base)?;
    let naval_warehouse_ids: HashSet<i64> = ship_wh_map.keys().copied().collect();
    let olo_hub_airport_ids = collect_hub_airport_ids_from_olo_airbase_props(&base)
        .context("collecting OLO* hub airport ids from objective airbaseID props")?;
    hub_airport_ids.extend(olo_hub_airport_ids);
    let inferred_hub_airport_ids = infer_hub_airport_ids_from_objectives(
        &objectives,
        &objective_aircraft_by_side,
        &base,
    )
    .context("inferring OLO* hub airport ids from objective slot modules")?;
    hub_airport_ids.extend(inferred_hub_airport_ids);
    hub_warehouse_ids = hub_warehouse_ids
        .difference(&hub_airport_ids)
        .copied()
        .collect();
    let tpl_wh_tbl: Option<Table<'static>> = match warehouse_bundle.as_ref() {
        Some(wb) => Some(
            wb.loaded
                .warehouses
                .raw_get::<_, Table>("warehouses")
                .with_context(|| {
                    format_compact!(
                        "warehouse template `{}` warehouses.warehouses for DEP* FARP key detection",
                        wb.path.display()
                    )
                })?,
        ),
        None => None,
    };
    let base_wh_tbl = base
        .warehouses
        .raw_get::<_, Table>("warehouses")
        .context("--base warehouses.warehouses for DEP* FARP key detection")?;
    let mut dep_ww_candidate_tables: Vec<&Table<'static>> = Vec::new();
    if let Some(ref t) = tpl_wh_tbl {
        dep_ww_candidate_tables.push(t);
    }
    dep_ww_candidate_tables.push(&base_wh_tbl);

    let tpl_air_tbl: Option<Table<'static>> = warehouse_bundle
        .as_ref()
        .and_then(|wb| wb.loaded.warehouses.raw_get::<_, Table>("airports").ok());
    let base_air_tbl: Option<Table<'static>> =
        base.warehouses.raw_get::<_, Table>("airports").ok();
    let mut dep_air_candidate_tables: Vec<&Table<'static>> = Vec::new();
    if let Some(ref t) = tpl_air_tbl {
        dep_air_candidate_tables.push(t);
    }
    if let Some(ref t) = base_air_tbl {
        dep_air_candidate_tables.push(t);
    }

    let dep_farp_warehouse_ids = collect_dep_farp_warehouse_ids(
        &base,
        &dep_ww_candidate_tables,
        &dep_air_candidate_tables,
    )
    .context(
        "collecting DEP* (BDEPFARP*/RDEPFARP*/NDEPFARP*) dynamic FARP template warehouse ids",
    )?;
    let mult_cfg = WarehouseStockMultConfig {
        airbase_max,
        hub_max,
        fob_max,
        farp_max,
        carrier_airbase_max,
        hub_airport_ids,
        hub_warehouse_ids,
        fob_warehouse_ids,
        dep_farp_warehouse_ids,
        naval_warehouse_ids,
    };
    warn!(
        "warehouse stock multipliers: airbase_max={} hub_max={} fob_max={} farp_max={} carrier_airbase_max={}; hub airport keys {:?}; hub warehouse keys {:?}; fob warehouse keys {:?}; DEP FARP template keys {:?}; {} naval warehouse id(s)",
        mult_cfg.airbase_max,
        mult_cfg.hub_max,
        mult_cfg.fob_max,
        mult_cfg.farp_max,
        mult_cfg.carrier_airbase_max,
        mult_cfg.hub_airport_ids,
        mult_cfg.hub_warehouse_ids,
        mult_cfg.fob_warehouse_ids,
        mult_cfg.dep_farp_warehouse_ids,
        mult_cfg.naval_warehouse_ids.len()
    );
    let missing_default_warehouse_keys = campaign_overlay
        .as_ref()
        .map(|o| o.missing_default_warehouse_keys.clone())
        .unwrap_or_default();

    let mut warehouse_bundle = warehouse_bundle;
    let mut built_production_inventory: Option<(Table<'static>, Table<'static>)> = None;
    let mut fowl_from_warehouse = if let Some(wb) = warehouse_bundle.as_mut() {
        let bridge_gen = weapon_bridge_map.as_ref().map(|b| (&vehicle_templates, b));
        let (export, inventory_aircraft_orphans_cleared, built_blue, built_red) = wb
            .template
            .apply(
                lua,
                &cfg,
                &mut base,
                warehouse_defaults,
                bridge_gen,
                &vehicle_templates,
                &objective_aircraft_by_side,
                &droptank_ws_from_weapon_warehouses,
                &mult_cfg,
            )
            .context("applying warehouse template")?;
        built_production_inventory = Some((built_blue, built_red));

        if !inventory_aircraft_orphans_cleared.is_empty() {
            pack_warehouse_bundle_to_path(wb).with_context(|| {
                format_compact!(
                    "persist warehouse template after BINVENTORY/RINVENTORY aircraft orphan zeroing ({})",
                    wb.path.display()
                )
            })?;
            let campaign_decade = campaign_overlay
                .as_ref()
                .and_then(|o| o.campaign_decade.as_deref())
                .unwrap_or("?");
            print_inventory_aircraft_orphan_editor_notice(
                &inventory_aircraft_orphans_cleared,
                wb.path.as_path(),
                weapon_template_path.as_path(),
                campaign_decade,
            );
            bail!(
                "warehouse template {}: {} BINVENTORY/RINVENTORY aircraft row(s) have no matching airframe in weapon{}.miz; zeros were saved to the warehouse .miz; fix weapon template then rebuild (output mission .miz not written)",
                wb.path.display(),
                inventory_aircraft_orphans_cleared.len(),
                campaign_decade,
            );
        }

        pack_warehouse_bundle_to_path(wb).with_context(|| {
            format_compact!("repack warehouse template {}", wb.path.display())
        })?;
        info!(
            "repacked `{}`: BDEFAULT/RDEFAULT/BINVENTORY/RINVENTORY `weapons` from build (defaults filtered via weapon<campaign_decade>.miz when bridge loaded)",
            wb.path.display()
        );
        export
    } else {
        bfprotocols::fowl_miz_export::FowlMizExport::default()
    };
    if !ship_wh_map.is_empty() {
        let zone_names = collect_trigger_zone_names(&base)?;
        audit_naval_carrier_mission_rules(&ship_wh_map, &zone_names)
            .context("naval carrier Fowl mission rules")?;
    }
    if let Some(wb) = warehouse_bundle.as_ref() {
        let (blue_inv_id, red_inv_id) = production_inventory_unit_ids(&base, cfg)
            .context(
                "production BINVENTORY/RINVENTORY unitIds for dynamic warehouse prefill",
            )?;
        let dyn_farp_allow =
            build_dyn_farp_aircraft_allow(&base, &dynamic_emit.dyn_templates).context(
                "building TTDdynFARP allowlist for DEP dynamic FARP warehouse A/C stocks",
            )?;
        if !mult_cfg.dep_farp_warehouse_ids.is_empty() && dyn_farp_allow.is_none() {
            warn!(
                "DEP dynamic FARP template warehouse rows exist but trigger zone `{}` is missing; A/C allowlist pruning for those rows is skipped (full coalition inventory copy still applies).",
                TTD_DYN_FARP_POLICY_ZONE,
            );
        }
        let ship_wh_allow = build_ship_warehouse_aircraft_allow(
            &base,
            &dynamic_emit.dyn_templates,
            &ship_wh_map,
        )
        .context("building per-ship TTDN aircraft allowlists for naval warehouse rows")?;
        // Collect positions for all dynamic warehouse rows so per-zone filtering can find
        // their containing O* zone. Airport positions come from groups with airdromId;
        // FARP/FOB positions come from the unit with the matching unitId.
        let (airport_wids, farp_wids): (HashSet<i64>, HashSet<i64>) = {
            let airports = base
                .warehouses
                .raw_get::<_, Table>("airports")
                .unwrap_or_else(|_| lua.create_table().unwrap());
            let warehouses = base
                .warehouses
                .raw_get::<_, Table>("warehouses")
                .unwrap_or_else(|_| lua.create_table().unwrap());
            let a: HashSet<i64> = airports
                .clone()
                .pairs::<Value, Table>()
                .filter_map(|p| p.ok())
                .filter_map(|(k, _)| warehouse_lua_key_i64(k))
                .collect();
            let w: HashSet<i64> = warehouses
                .clone()
                .pairs::<Value, Table>()
                .filter_map(|p| p.ok())
                .filter_map(|(k, _)| warehouse_lua_key_i64(k))
                .collect();
            (a, w)
        };
        let mut airport_ids: Vec<i64> = airport_wids.iter().copied().collect();
        airport_ids.sort_unstable();
        info!(
            "airport warehouse ids detected: count={}, ids={:?}",
            airport_ids.len(),
            airport_ids
        );
        let mapped_airbase_ids: HashSet<i64> = obj_dyn_allow
            .iter()
            .filter_map(|o| o.airbase_id)
            .collect();
        let mut mapped_ids: Vec<i64> = mapped_airbase_ids.iter().copied().collect();
        mapped_ids.sort_unstable();
        info!(
            "O* zones with explicit airbaseID mapping: count={}, ids={:?}",
            mapped_ids.len(),
            mapped_ids
        );
        let mut warehouse_positions: HashMap<i64, Vector2> =
            collect_warehouse_unit_positions(&base, &farp_wids)
                .context("collecting FARP warehouse unit positions")?;
        let airport_positions =
            collect_airport_positions_from_groups(&base, &airport_wids)
                .context("collecting airport positions from groups")?;
        warehouse_positions.extend(airport_positions);
        patch_warehouse_dynamic_spawn_links(
            lua,
            &base.warehouses,
            &dynamic_emit,
            Some(&wb.template.blue_default),
            Some(&wb.template.red_default),
            Some(&wb.template.blue_inventory),
            Some(&wb.template.red_inventory),
            wb.template.blue_inventory_plus.as_ref(),
            wb.template.red_inventory_plus.as_ref(),
            Some(&wb.template.blue_default_plus),
            Some(&wb.template.red_default_plus),
            &wb.template.zone_plus_blue,
            &wb.template.zone_plus_red,
            &wb.template.zone_ws_inventory_blue,
            &wb.template.zone_ws_inventory_red,
            &wb.template.zone_default_plus_blue,
            &wb.template.zone_default_plus_red,
            &wb.template.zone_ws_default_blue,
            &wb.template.zone_ws_default_red,
            &mult_cfg,
            warehouse_defaults,
            &obj_dyn_allow,
            &warehouse_positions,
            dyn_farp_allow.as_ref(),
            Some(&ship_wh_allow),
            weapon_bridge_map.as_ref(),
            &vehicle_templates,
        )
        .context("patching warehouse linkDynTempl")?;
        apply_settings_dynamic_spawn_ground_flags(
            &base,
            &base.warehouses,
            &mult_cfg,
            &obj_dyn_allow,
            &warehouse_positions,
        )
        .context("applying SETTINGS-dynamic-spawn ground dynamicSpawn flags")?;
        apply_settings_dynamic_spawn_ttdn_naval_flags(
            &base,
            &base.warehouses,
            &dynamic_emit.ship_hull_by_wid,
        )
        .context("applying SETTINGS-dynamic-spawn-TTDN naval dynamicSpawn flags")?;
        let ai_template_stock = load_ai_template_stock_settings(&base)
            .context("loading SETTINGS-Ai template stock zones")?;
        if !ai_template_stock.is_empty() {
            apply_settings_ai_template_stock(
                lua,
                &base,
                &ai_template_stock,
                &obj_dyn_allow,
                &ship_wh_map,
                &dynamic_emit,
            )
            .context("applying SETTINGS-Ai warehouse stock")?;
        }
        if let Some(caps) = warehouse_defaults {
            if caps.has_any_nonzero_cap() {
                let mut skip = HashSet::default();
                skip.insert(blue_inv_id);
                skip.insert(red_inv_id);
                let blue_inv_skip = coalition_inventory_positive_weapon_ws(
                    &wb.template.blue_inventory,
                    wb.template.blue_inventory_plus.as_ref(),
                )
                .context("blue inventory wsTypes for cfg cap scale pass")?;
                let red_inv_skip = coalition_inventory_positive_weapon_ws(
                    &wb.template.red_inventory,
                    wb.template.red_inventory_plus.as_ref(),
                )
                .context("red inventory wsTypes for cfg cap scale pass")?;
                apply_weapon_cfg_cap_scale_pass(
                    &base.warehouses,
                    caps,
                    &mult_cfg,
                    &skip,
                    &blue_inv_skip,
                    &red_inv_skip,
                )
                .context("scaling default_warehouse_* caps by stock multiplier")?;
            }
        }
        if let Some((built_blue, built_red)) = built_production_inventory.as_ref() {
            let obj_dyn_allow = build_objective_dyn_allow(&base, &dynamic_emit.dyn_templates)
                .context("objective zones for stock export after warehouse patch")?;
            if let Some(br) = weapon_bridge_map.as_ref() {
                extend_objective_defaults_for_naval_hulls(
                    &mut fowl_from_warehouse.objective_defaults,
                    &base,
                    &ship_wh_map,
                    &dynamic_emit.dyn_templates,
                    br,
                )
                .context("naval hull objective_defaults for export")?;
            }
            let dep_wh_map = collect_dep_farp_warehouse_group_map(
                &base,
                &mult_cfg.dep_farp_warehouse_ids,
            )
            .context("DEP FARP pad map for objective_defaults export")?;
            if let (Some(br), Some(da)) = (
                weapon_bridge_map.as_ref(),
                dyn_farp_allow.as_ref(),
            ) {
                extend_objective_defaults_for_dep_farps(
                    &mut fowl_from_warehouse.objective_defaults,
                    &dep_wh_map,
                    da,
                    br,
                )
                .context("DEP FARP pad objective_defaults for export")?;
            }
            let mut objective_stock = build_objective_stock_export(
                lua,
                &base,
                &obj_dyn_allow,
                &mult_cfg,
                &wb.template,
                built_blue,
                built_red,
                &fowl_from_warehouse.objective_defaults,
            )
            .context("building per-objective warehouse stock export")?;
            merge_naval_ship_objective_stock_export(
                &mut objective_stock,
                &base,
                &ship_wh_map,
                &mult_cfg,
                &wb.template,
                built_blue,
                built_red,
                &fowl_from_warehouse.objective_defaults,
            )
            .context("merging naval ship objective_stock export")?;
            merge_dep_farp_objective_stock_export(
                &mut objective_stock,
                &base,
                &dep_wh_map,
                &mult_cfg,
                &wb.template,
                built_blue,
                built_red,
                &fowl_from_warehouse.objective_defaults,
            )
            .context("merging DEP FARP pad objective_stock export")?;
            merge_ai_template_stock_export(
                &mut objective_stock,
                &mut fowl_from_warehouse.ai_template_airframes,
                &ai_template_stock,
                &obj_dyn_allow,
                &ship_wh_map,
            );
            fowl_from_warehouse.objective_stock = objective_stock;
            info!(
                "fowl export objective_stock: written after warehouse patch ({} objectives)",
                fowl_from_warehouse.objective_stock.len()
            );
        }
    }

    if !missing_default_warehouse_keys.is_empty() {
        // PowerShell wrapper prints output line-by-line and recolors via tag matching,
        // which breaks multi-line colored output.
        // We emit a machine-readable marker and let the wrapper re-print it after SUCCESS.
        println!(
            "BFNEXT_MISSING_DEFAULT_WAREHOUSE_KEYS:{}",
            missing_default_warehouse_keys.join(",")
        );
    }
    if let (Some((built_blue, built_red)), Some(_wb)) =
        (built_production_inventory.as_ref(), warehouse_bundle.as_ref())
    {
        mirror_assembled_production_inventory(lua, &base, cfg, built_blue, built_red).context(
            "final mirror of built BINVENTORY/RINVENTORY into assembled mission",
        )?;
    }
    if warehouse_bundle.is_some() || !dynamic_emit.link_by_side_type.is_empty() {
        let s = serialize_to_lua("warehouses", Value::Table(base.warehouses.clone()))?;
        fs::write(&base.miz.files["warehouses"], &*s)
            .context("writing warehouse file")?;
        info!("wrote serialized warehouses to warehouse file.");
    }
    //replace options file
    /*
    let options_template = UnpackedMiz::new(&cfg.options).context("loading options template")?;
    let source_options_path = options_template.files.get("options").unwrap();
    let destination_options_path = base.miz.files.get("options").unwrap();
    fs::rename(source_options_path, destination_options_path)
        .context("replacing the options file")?;
    info!("replaced options file from {:?}", &cfg.options);
    */
    // By forcing the addition of modified base.miz - options file to the mission assembly
    let options_in_base =
        base.miz.files.get("mission").unwrap().parent().unwrap().join("options");
    if options_in_base.exists() {
        base.miz.files.insert("options".into(), options_in_base);
        info!("force-added base.miz-options from base folder to the final archive.");
    }
    crate::discord_map_icons::embed_into_miz(&base.miz.root, &mut base.miz.files, &cfg.base)
        .context("embedding discord map icons into mission")?;
    info!("saving finalized mission to {:?}", cfg.output);
    base.miz.pack(&cfg.output).context("repacking mission")?;
    let export_path = fowl_from_warehouse
        .write_next_to_miz(&cfg.output)
        .context("writing Fowl mission export JSON")?;
    info!("wrote Fowl mission export to {:?}", export_path);
    let (blue_objectives, red_objectives, neutral_objectives) =
        count_objectives_by_default_owner(&objectives);
    let (opr_blue, opr_red) = count_objective_kind_by_owner(&objectives, "PR");
    let (fact_blue, fact_red) =
        count_production_factories_by_coalition(&base, &objectives, &factory_types)
            .context("counting production factory statics per coalition")?;
    println!("Objectives total - Blue coalition: {blue_objectives}");
    println!("Objectives total - Red coalition: {red_objectives}");
    println!("Objectives total - Neutral coalition: {neutral_objectives}");
    println!(
        "Objectives production - Blue coalition: {opr_blue}  ( total number of production factories {fact_blue})"
    );
    println!(
        "Objectives production - Red coalition: {opr_red}  ( total number of production factories {fact_red})"
    );
    warn_airbase_objective_bindings_follow_up_summary(
        missing_airbase.as_slice(),
        invalid_airbase.as_slice(),
        unresolved_surprising.as_slice(),
    );
    Ok(())
}
