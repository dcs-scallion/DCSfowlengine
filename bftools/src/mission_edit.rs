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
use bfprotocols::miz_trigger::{
    fowl_trigger_zone_name_valid, FOWL_TRIGGER_ZONE_EXPECTED_PREFIXES_DISPLAY,
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
use rand::Rng;
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
        let pos = self.inner.pos()?;
        match self.inner.typ()? {
            TriggerZoneTyp::Quad(q) => Ok(q.contains(LuaVec2(pos))),
            TriggerZoneTyp::Circle { radius } => {
                Ok(radius >= na::distance(&v.into(), &pos.into()))
            }
        }
    }
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
        for (_, file_path) in &self.files {
            if file_path.is_dir() {
                continue;
            }
            let mut file = File::open(file_path)
                .with_context(|| format_compact!("opening file {:?}", file_path))?;
            let relative_path =
                file_path.strip_prefix(&self.root).with_context(|| {
                    format_compact!("stripping {:?} from file {file_path:?}", self.root)
                })?;
            zip_writer
                .start_file(relative_path.to_string_lossy(), FileOptions::default())
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
                        if k.is_integer() && k.as_integer().unwrap() <= max {
                            return Ok(());
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
                        // Template missing: e.g. TTS* / TTD* disabled in SETTINGS-*-slots-creation,
                        // so the template name was never registered. Skip instead of failing the build.
                        warn!(
                            "skipping property {:?} -> '{}' (template not loaded — likely disabled in SETTINGS-*-slots-creation)",
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
                                .or_default() += prop.value.parse::<usize>()?;
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

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum SlotType {
    Plane,
    Helicopter,
}

/// Emitted dynamic-spawn template groups sort late in ME lists (`zz`…).
const DYNAMIC_TEMPLATE_GROUP_PREFIX: &str = "zzDT-";
/// Older missions / weapon.miz may still use `DT-`; strip and match-load only.
const LEGACY_DYNAMIC_TEMPLATE_PREFIX: &str = "DT-";
/// North offset (meters, +DCS `y`) for template mirrors in `weapon.miz`.
const WEAPON_DT_MIRROR_OFFSET_NORTH_M: f64 = 1000.0;

#[inline]
fn is_dynamic_template_group_name(name: &str) -> bool {
    name.starts_with(DYNAMIC_TEMPLATE_GROUP_PREFIX)
        || name.starts_with(LEGACY_DYNAMIC_TEMPLATE_PREFIX)
}

fn gen_dynamic_template_slot_password_10_digits() -> StdString {
    let mut rng = rand::thread_rng();
    (0..10)
        .map(|_| char::from_digit(rng.gen_range(0..10), 10).unwrap())
        .collect::<StdString>()
}

fn apply_dynamic_template_group_visibility(
    group: &Group<'_>,
    slot_kind: SlotType,
) -> Result<()> {
    match slot_kind {
        SlotType::Plane => {
            group.raw_set("hidden", true)?;
            group.raw_set("hiddenOnPlanner", true)?;
            group.raw_set("hiddenOnMFD", true)?;
        }
        SlotType::Helicopter => {
            group.raw_set("hidden", true)?;
        }
    }
    Ok(())
}

fn offset_air_group_north_m(group: &Group<'_>, delta_y: f64) -> Result<()> {
    let p = group.pos()?;
    group.set_pos(na::Vector2::new(p.x, p.y + delta_y))?;
    for u in group.units()? {
        let u = u?;
        let up = u.pos()?;
        u.set_pos(na::Vector2::new(up.x, up.y + delta_y))?;
    }
    let route = group.route()?;
    let mut new_pts = Vec::new();
    for pt in route.points()? {
        let mut pt = pt?;
        pt.pos.0.y += delta_y;
        new_pts.push(pt);
    }
    route.set_points(new_pts)?;
    Ok(())
}

fn strip_dt_prefixed_groups_from_cjtf_air(
    lua: &'static Lua,
    mission: &Miz,
) -> Result<()> {
    for side in [Side::Blue, Side::Red] {
        let cname = match side {
            Side::Blue => Country::CJTF_BLUE,
            Side::Red => Country::CJTF_RED,
            Side::Neutral => continue,
        };
        let coa = mission.coalition(side)?;
        let Some(country) = coa.country(cname)? else {
            continue;
        };
        for category in ["plane", "helicopter"] {
            strip_dt_from_plane_or_heli(lua, &country, category)?;
        }
    }
    Ok(())
}

fn strip_dt_from_plane_or_heli(
    lua: &'static Lua,
    country: &MizCountry<'_>,
    category: &str,
) -> Result<()> {
    let seq: Sequence<Group> = match category {
        "plane" => country.planes()?,
        "helicopter" => country.helicopters()?,
        _ => bail!("strip_dt_from_plane_or_heli: bad category"),
    };
    if seq.len() == 0 {
        return Ok(());
    }
    let new_groups = lua.create_table()?;
    for g in seq {
        let g = g?;
        if is_dynamic_template_group_name(&g.name()?) {
            continue;
        }
        new_groups.push(g)?;
    }
    let cat_tbl: Table = country.raw_get(category)?;
    cat_tbl.raw_set("group", new_groups)?;
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

fn sync_dt_mirror_groups_into_weapon_miz(
    lua: &'static Lua,
    weapon_path: &Path,
    base: &LoadedMiz,
) -> Result<()> {
    let weapon_ld = LoadedMiz::new(lua, weapon_path).with_context(|| {
        format_compact!("loading weapon.miz for DT mirror: {weapon_path:?}")
    })?;
    strip_dt_prefixed_groups_from_cjtf_air(lua, &weapon_ld.mission)?;
    for side in [Side::Blue, Side::Red] {
        let cname = match side {
            Side::Blue => Country::CJTF_BLUE,
            Side::Red => Country::CJTF_RED,
            Side::Neutral => unreachable!(),
        };
        let base_coa = base.mission.coalition(side)?;
        let Some(base_country) = base_coa.country(cname)? else {
            continue;
        };
        for (slot_kind, seq) in [
            (SlotType::Plane, base_country.planes()?),
            (SlotType::Helicopter, base_country.helicopters()?),
        ] {
            for g in seq {
                let g = g?;
                let name = g.name()?;
                if !is_dynamic_template_group_name(&name) {
                    continue;
                }
                let tmpl: Group<'static> = g.deep_clone(lua)?;
                offset_air_group_north_m(&tmpl, WEAPON_DT_MIRROR_OFFSET_NORTH_M)?;
                apply_dynamic_template_group_visibility(&tmpl, slot_kind)?;
                push_client_air_group_to_cjtf(
                    lua,
                    &weapon_ld.mission,
                    side,
                    slot_kind,
                    tmpl,
                )?;
            }
        }
    }
    let s = serialize_to_lua("mission", Value::Table((&*weapon_ld.mission).clone()))?;
    fs::write(&weapon_ld.miz.files["mission"], &s)
        .with_context(|| format_compact!("writing weapon.miz mission (DT mirror)"))?;
    info!(
        "wrote zzDT-* / legacy DT-* mirror groups (+{:.0} m north, hidden) to {}",
        WEAPON_DT_MIRROR_OFFSET_NORTH_M,
        weapon_path.display()
    );
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

    /// Patch existing Lua waypoint tables in place (preserves DCS-only fields `IntoLua` might drop).
    /// First point: `TakeOffParking` (ramp) for planes, `TakeOffGround` for helis; rest: `Turning Point`.
    fn patch_dt_route_points_lua_tables(
        grp: &Group<'static>,
        slot_kind: SlotType,
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
            let typ = if i == 1 {
                match slot_kind {
                    SlotType::Plane => "TakeOffParking",
                    SlotType::Helicopter => "TakeOffGround",
                }
            } else {
                "Turning Point"
            };
            p.raw_set("type", typ)?;
            if i == 1 {
                p.raw_set("airdromId", Value::Nil)?;
                p.raw_set("helipadId", Value::Nil)?;
                p.raw_set("linkUnit", Value::Nil)?;
                p.raw_set("timeReFuAr", Value::Nil)?;
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
                            if let Ok(w) = unit.raw_get::<_, Table>("payload") {
                                payload_all.entry(side).or_default().push(w.clone());
                                payload_variants
                                    .entry(side)
                                    .or_default()
                                    .entry(unit_type.clone())
                                    .or_default()
                                    .push(w.clone());
                                payload.entry(side).or_default().insert(unit_type, w);
                            }
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
                        let unit_type: String =
                            unit.raw_get("type").context("getting units")?;
                        match st {
                            SlotType::Helicopter => {
                                helicopter_slots.entry(side).or_default()
                            }
                            SlotType::Plane => plane_slots.entry(side).or_default(),
                        }
                        .insert(unit_type.clone(), group.clone());
                        info!("adding payload template: {unit_type}");
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
                            prop_aircraft
                                .entry(side)
                                .or_default()
                                .insert(unit_type.clone(), w);
                        }
                        if let Ok(w) = unit.raw_get("Radio") {
                            radio.entry(side).or_default().insert(unit_type.clone(), w);
                        }
                        if let Ok(v) = unit.raw_get("frequency") {
                            frequency.entry(side).or_default().insert(unit_type, v);
                        }
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
        fn set_dl_mizuid(unit: &Table) -> Result<()> {
            if let Ok(Some(dl)) = unit.raw_get::<_, Option<Table>>("datalinks") {
                let uid = unit.raw_get::<_, i64>("unitId")?;
                let mut ok = false;
                if let Ok(ownship) = dl.raw_get_path::<Table>(&path![
                    "Link16",
                    "network",
                    "teamMembers",
                    1
                ]) {
                    ownship.raw_set("missionUnitId", uid)?;
                    ok = true;
                }
                if let Ok(presets) = dl
                    .raw_get_path::<Sequence<Table>>(&path!["IDM", "network", "presets"])
                {
                    for preset in presets {
                        let preset = preset?;
                        if let Ok(ownship) =
                            preset.raw_get_path::<Table>(&path!["members", 1])
                        {
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
        for zone in base.mission.triggers()? {
            let zone = zone?;
            let name = zone.name()?;
            if !name.starts_with("TS") {
                continue;
            }
            let spec = SlotSpec::new(
                &templates,
                zone.properties()?,
                false,
                INCLUDE_STATIC_SLOT_KEYS,
            )?;
            for (side, slots) in &spec.slots {
                let mut posgen: Box<dyn PosGenerator> = match zone.typ()? {
                    TriggerZoneTyp::Quad(quad) => Box::new(SlotGrid::new(
                        name.clone(),
                        quad,
                        spec.margin,
                        spec.spacing,
                    )?),
                    TriggerZoneTyp::Circle { radius } => Box::new(SlotRadial::new(
                        name.clone(),
                        radius,
                        zone.pos()?,
                        spec.margin,
                        spec.spacing,
                    )?),
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
                        route.set_points({
                            let mut first = true;
                            route
                                .points()?
                                .into_iter()
                                .map(|p| {
                                    let mut p = p?;
                                    if first {
                                        let is_naval = spec
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
                        for u in tmpl.units()? {
                            let u = u?;
                            if u.skill()? != Skill::Client {
                                bail!("slot templates must be set to Client skill level")
                            }
                            u.set_id(uid)?;
                            u.set_heading(posgen.azumith())?;
                            u.set_pos(pos)?;
                            set_dl_mizuid(&u)
                                .with_context(|| format_compact!("unit {u:?}"))?;
                            uid.next();
                        }
                        gid.next();
                        seq.push(tmpl)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Emits `zzDT-<type>-<side>` groups (`dynSpawnTemplate` + warehouse `linkDynTempl`).
    ///
    /// Appended like static slots in ME lists (prefix `zz` sorts them late). Mirrors go to `weapon.miz`
    /// (+1 km north, hidden). Runtime uses `dynSpawnTemplate` + warehouse links on the **base** mission.
    fn emit_dynamic_spawn_templates(
        &self,
        lua: &'static Lua,
        base: &mut LoadedMiz,
    ) -> Result<DynamicSpawnEmit> {
        // NOTE: DT_* templates used to re-stamp datalink missionUnitId fields here.
        // We intentionally strip `datalinks` from DT_* now (to keep templates small),
        // so this is no longer needed.

        let idx = base.mission.index()?;
        let dynamic_creation_settings =
            Self::load_zone_creation_settings(base, "SETTINGS-dynamic-slots-creation")?;
        let mut uid = idx.max_uid();
        let mut gid = idx.max_gid();
        uid.next();
        gid.next();
        // NOTE: We no longer stamp Link16 STN into DT_* templates (to keep them minimal).

        // Optional dynamic template filters from trigger zones:
        // - TTD*  = land dynamic template definitions
        // - TTDN* = naval dynamic template definitions
        // (both currently emit Turning Point route types)
        let mut dyn_templates: HashMap<String, SlotSpec> = HashMap::default();
        for zone in base.mission.triggers()? {
            let zone = zone?;
            let name = zone.name()?;
            if let Some(s) = name.strip_prefix("TTDN") {
                if !Self::zone_enabled_by_settings(&dynamic_creation_settings, &name) {
                    continue;
                }
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
                if !Self::zone_enabled_by_settings(&dynamic_creation_settings, &name) {
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
        // Land policy: enabled zones named `TTD*` except `TTDN*`. Naval: enabled `TTDN*` only.
        // If no zones of a kind exist, that side of the policy is "open" (all types get links where applicable).
        let mut land_allowed_set: HashSet<(Side, String)> = HashSet::default();
        let mut naval_allowed_set: HashSet<(Side, String)> = HashSet::default();
        let mut have_land_policy_zones = false;
        let mut have_naval_policy_zones = false;
        for zone in base.mission.triggers()? {
            let zone = zone?;
            let name = zone.name()?;
            if name.starts_with("TTDN") {
                if !Self::zone_enabled_by_settings(&dynamic_creation_settings, &name) {
                    continue;
                }
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
                if !Self::zone_enabled_by_settings(&dynamic_creation_settings, &name) {
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
                    let include = match (&land_allow, &naval_allow) {
                        (Some(l), Some(n)) => {
                            l.contains(&(side, unit_type.clone()))
                                || n.contains(&(side, unit_type.clone()))
                        }
                        _ => true,
                    };
                    if !include {
                        continue;
                    }
                    specs.push((side, SlotType::Plane, unit_type.clone(), g.clone()));
                }
            }
            if let Some(m) = self.helicopter_slots.get(&side) {
                for (unit_type, g) in m {
                    let include = match (&land_allow, &naval_allow) {
                        (Some(l), Some(n)) => {
                            l.contains(&(side, unit_type.clone()))
                                || n.contains(&(side, unit_type.clone()))
                        }
                        _ => true,
                    };
                    if !include {
                        continue;
                    }
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

        // Dynamic templates are created off-map, so we cannot reuse their pylons directly.
        // However, DCS expects `payload.pylons` to be present for weapon selection UI.
        // Here we capture pylons from already-generated static slots (same `Side` + `unit_type`)
        // and later merge them into the dynamic templates' payload (keeping `restricted`).
        let mut wanted_types: HashMap<Side, HashSet<String>> = HashMap::default();
        for (side, _, unit_type, _) in &specs {
            wanted_types.entry(*side).or_default().insert(unit_type.clone());
        }
        let mut pylons_by_side_type: HashMap<(Side, String), Table<'static>> =
            HashMap::default();
        for side in [Side::Red, Side::Blue] {
            if let Some(wanted) = wanted_types.get(&side) {
                let coa = base.mission.coalition(side)?;
                for country in coa.raw_get::<_, Table>("country")?.pairs::<Value, Table>()
                {
                    let country = country?.1;
                    for group in vehicle(&country, "plane")?
                        .chain(vehicle(&country, "helicopter")?)
                    {
                        let group = group?;
                        for unit in
                            group.raw_get::<_, Table>("units")?.pairs::<Value, Table>()
                        {
                            let unit = unit?.1;
                            let unit_type: String = unit.raw_get("type")?;
                            if !wanted.contains(&unit_type) {
                                continue;
                            }
                            if pylons_by_side_type
                                .contains_key(&(side, unit_type.clone()))
                            {
                                continue;
                            }
                            let payload: Table<'static> = match unit.raw_get("payload") {
                                Ok(p) => p,
                                Err(_) => continue,
                            };
                            let pylons: Table<'static> = match payload.raw_get("pylons") {
                                Ok(p) => p,
                                Err(_) => continue,
                            };
                            // Use a robust non-empty heuristic.
                            // `payload.pylons` may use different key types depending on source.
                            let has_any_pylons =
                                pylons.clone().pairs::<Value, Value>().next().is_some();
                            if has_any_pylons {
                                pylons_by_side_type.insert((side, unit_type), pylons);
                            }
                        }
                    }
                }
            }
        }

        let mut link_by_side_type: HashMap<(Side, String), GroupId> = HashMap::new();
        let mut emitted_names: HashSet<String> = HashSet::new();
        let slot_password = gen_dynamic_template_slot_password_10_digits();
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
            if from_weapon_dt {
                info!(
                    "{}{unit_type}-{}: using route from weapon.miz template",
                    DYNAMIC_TEMPLATE_GROUP_PREFIX,
                    side.to_str()
                );
            } else {
                // Synthesized from slot template: set types on existing Lua waypoint tables (ME strings).
                Self::patch_dt_route_points_lua_tables(&tmpl, slot_kind)?;
            }

            tmpl.set_name(group_name.clone())?;
            tmpl.set_id(gid)?;

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
                u.raw_set("password", slot_password.clone())?;

                // Keep DT_* units small. These templates are only used to populate the
                // dynamic-spawn payload UI; large avionics/radio/datalink blobs can make
                // Mission Editor enumeration extremely slow.
                u.raw_set("datalinks", Value::Nil)?;
                u.raw_set("Radio", Value::Nil)?;
                u.raw_set("AddPropAircraft", Value::Nil)?;

                // Apply full weapon payload + coalition-specific radio so dynamic spawns
                // behave the same as statically generated slots.
                if let Some(w) =
                    Self::table_for_side_or_opposite(&self.payload, side, &unit_type)
                {
                    let payload_tbl = w.deep_clone(lua)?;
                    let pylons = pylons_by_side_type
                        .get(&(side, unit_type.clone()))
                        .or_else(|| {
                            pylons_by_side_type.get(&(side.opposite(), unit_type.clone()))
                        });
                    if let Some(pylons) = pylons {
                        payload_tbl.raw_set("pylons", pylons.deep_clone(lua)?)?;
                    }
                    u.set("payload", payload_tbl)?;
                }

                if let Some(v) = self.frequency.get(&side).and_then(|t| t.get(&unit_type))
                {
                    u.set("frequency", v.deep_clone(lua)?)?;
                }

                // If a src template contained an unknown datalink pattern, we no longer
                // care here because we stripped datalinks above.
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

        Ok(DynamicSpawnEmit { link_by_side_type, land_allow, naval_allow })
    }

    fn apply(
        &self,
        lua: &Lua,
        objectives: &mut Vec<TriggerZone>,
        base: &mut LoadedMiz,
    ) -> Result<()> {
        let mut slots: HashMap<String, HashMap<String, usize>> = HashMap::default();
        let mut replace_count: HashMap<String, isize> = HashMap::new();
        let mut stn = 1u64;
        //apply weapon/APA templates to mission table in self
        info!("replacing slots with template payloads");
        for (side, coa) in
            Side::ALL.into_iter().map(|side| (side, base.mission.coalition(side)))
        {
            let coa = coa?;
            for country in coa.raw_get::<_, Table>("country")?.pairs::<Value, Table>() {
                let country = country?.1;
                for group in vehicle(&country, "plane").context("getting planes")?.chain(
                    vehicle(&country, "helicopter").context("getting helicopters")?,
                ) {
                    let group = group.context("getting group")?;
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
                        match Self::table_for_side_or_opposite(
                            &self.payload,
                            side,
                            &unit_type,
                        ) {
                            Some(w) => unit.set("payload", w.deep_clone(lua)?)?,
                            None => {
                                if !unit.contains_key("payload")? {
                                    warn!("no payload table for {side}/{unit_type}");
                                }
                            }
                        }
                        let stn_string = match Self::table_for_side_or_opposite(
                            &self.prop_aircraft,
                            side,
                            &unit_type,
                        ) {
                            None => String::from(""),
                            Some(tmpl) => {
                                let tmpl = tmpl.deep_clone(lua)?;
                                let stn = if tmpl.contains_key("STN_L16")? {
                                    tmpl.raw_set(
                                        "STN_L16",
                                        String::from(format_compact!("{:005o}", stn)),
                                    )?;
                                    let s = String::from(format_compact!(
                                        " STN#{:005o}",
                                        stn
                                    ));
                                    stn += 1;
                                    s
                                } else {
                                    String::from("")
                                };
                                unit.set("AddPropAircraft", tmpl)?;
                                stn
                            }
                        };
                        // Radio presets are coalition-specific; do not fall back to opposite side.
                        if let Some(w) =
                            self.radio.get(&side).and_then(|t| t.get(&unit_type))
                        {
                            unit.set("Radio", w.deep_clone(lua)?)?
                        }
                        if let Some(v) =
                            self.frequency.get(&side).and_then(|t| t.get(&unit_type))
                        {
                            unit.set("frequency", v.deep_clone(lua)?)?
                        }
                        increment_key(&mut replace_count, &unit_type);
                        let x = unit.get("x")?;
                        let y = unit.get("y")?;
                        let mut found = false;
                        for trigger_zone in &mut *objectives {
                            if trigger_zone.contains(Vector2::new(x, y))? {
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
            info!("replaced {amount} radio/payloads for {unit_type}");
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

struct WarehouseTemplate {
    blue_inventory: Table<'static>,
    red_inventory: Table<'static>,
    blue_default: Table<'static>,
    red_default: Table<'static>,
    blue_default_plus: Table<'static>,
    red_default_plus: Table<'static>,
    blue_all_fueltanks: Table<'static>,
    red_all_fueltanks: Table<'static>,
    blue_default_fueltanks: Table<'static>,
    red_default_fueltanks: Table<'static>,
}

/// Keeps `LoadedMiz` alive so template warehouse tables remain valid for optional write-back pack.
struct WarehouseBundle {
    path: PathBuf,
    loaded: LoadedMiz,
    template: WarehouseTemplate,
}

/// Red/blue rows get BDEFAULT/RDEFAULT; neutral is left as in base.miz (e.g. civilian airfields).
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
    carrier_airbase_max: u32,
    hub_airport_ids: HashSet<i64>,
    fob_warehouse_ids: HashSet<i64>,
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
        if self.naval_warehouse_ids.contains(&id) {
            self.carrier_airbase_max.max(1)
        } else if self.fob_warehouse_ids.contains(&id) {
            self.fob_max.max(1)
        } else {
            self.airbase_max.max(1)
        }
    }

    fn mult_dynamic_row(&self, id: i64, is_airports_table: bool) -> u32 {
        if is_airports_table {
            self.mult_airport(id)
        } else {
            self.mult_warehouse_row(id)
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

fn merge_liquids_from_inventory_template(
    dst: &Table,
    src: &Table,
    lua: &Lua,
) -> Result<()> {
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

fn apply_weapon_cfg_cap_scale_pass(
    warehouses_root: &Table,
    caps: &campaign_cfg::WarehouseDefaultsFromCfg,
    mult_cfg: &WarehouseStockMultConfig,
    skip_ids: &HashSet<i64>,
) -> Result<()> {
    fn one_table(
        tbl: &Table,
        caps: &campaign_cfg::WarehouseDefaultsFromCfg,
        mult_cfg: &WarehouseStockMultConfig,
        skip_ids: &HashSet<i64>,
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
            campaign_cfg::scale_weapon_amounts_matching_cfg_cap(&row, caps, mult)?;
        }
        Ok(())
    }
    let airports =
        warehouses_root.raw_get::<_, Table>("airports").context("scale pass airports")?;
    let warehouses = warehouses_root
        .raw_get::<_, Table>("warehouses")
        .context("scale pass warehouses")?;
    one_table(&airports, caps, mult_cfg, skip_ids, true)?;
    one_table(&warehouses, caps, mult_cfg, skip_ids, false)?;
    Ok(())
}

/// `Some(&set)` only when non-empty. An empty set must not act as allowlist (would remove every row).
fn warehouse_allowlist_for_filter(
    opt: &Option<HashSet<[i32; 4]>>,
) -> Option<&HashSet<[i32; 4]>> {
    opt.as_ref().filter(|s| !s.is_empty())
}

/// Drops `row.weapons` whose `wsType` is in `strip_ws`; if `allowed_ws` is set, keeps only rows in allowlist.
fn prune_warehouse_weapons_row(
    lua: &Lua,
    row: &Table,
    strip_ws: &HashSet<[i32; 4]>,
    allowed_ws: Option<&HashSet<[i32; 4]>>,
    log_label: &str,
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
        Ok(Self {
            blue_inventory: warehouses
                .raw_get(blue_inventory_id)
                .context("getting blue inventory")?,
            red_inventory: warehouses
                .raw_get(red_inventory_id)
                .context("getting red inventory")?,
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
        lua: &Lua,
        cfg: &MizCmd,
        base: &mut LoadedMiz,
        warehouse_caps: Option<&campaign_cfg::WarehouseDefaultsFromCfg>,
        bridge_gen: Option<(&VehicleTemplates, &weapon_bridge::WeaponBridgeMap)>,
        objective_aircraft_by_side: &HashMap<
            StdString,
            HashMap<Side, HashSet<StdString>>,
        >,
        _droptank_ws_from_weapon_warehouses: &(HashSet<[i32; 4]>, HashSet<[i32; 4]>),
        mult_cfg: &WarehouseStockMultConfig,
    ) -> Result<bfprotocols::fowl_miz_export::FowlMizExport> {
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
                format_compact!("{label}: write-back set weapons on template row")
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
                    info!(
                        "{row_name}: zeroed forbidden weapon wsType [{}, {}, {}, {}] amount {}",
                        ws[0], ws[1], ws[2], ws[3], cur
                    );
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

        fn preserve_dynamic_flags(
            lua: &Lua,
            new_row: &Table,
            old_row: &Table,
        ) -> Result<()> {
            let dynamic_spawn =
                old_row.raw_get::<_, bool>("dynamicSpawn").unwrap_or(false);
            let dynamic_cargo =
                old_row.raw_get::<_, bool>("dynamicCargo").unwrap_or(false);
            new_row.raw_set("dynamicSpawn", dynamic_spawn)?;
            new_row.raw_set("dynamicCargo", dynamic_cargo)?;
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
            Ok(())
        }

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
            // B/RDEFAULT: per-airframe `template_restricted` + pylon evidence. B/RINVENTORY: DCS
            // mount set for coalition `weapon*.miz` types only (no `payload.restricted` cull; AIM-54 etc.).
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
                    for ws in br.weapon_ws_for_aircraft_key_only(unit_type) {
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
            let mut fowl_b = blue_default_allowlist.clone();
            fowl_b.extend(blue_inventory_allowlist.iter().copied());
            blue_fowl_export_union = Some(br.expand_ws_alias_family(&fowl_b));
            let mut fowl_r = red_default_allowlist.clone();
            fowl_r.extend(red_inventory_allowlist.iter().copied());
            red_fowl_export_union = Some(br.expand_ws_alias_family(&fowl_r));
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
        )?;
        prune_warehouse_weapons_row(
            lua,
            &red_master,
            &red_strip_ws,
            warehouse_allowlist_for_filter(&red_allowed_ws),
            "RDEFAULT",
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
            let is_dynamic = old_row.raw_get::<_, bool>("dynamicSpawn").unwrap_or(false);
            if is_dynamic {
                // Dynamic airport rows are prefilled later from filtered BINVENTORY/RINVENTORY.
                continue;
            }
            let Some(side) = warehouse_side_for_default_apply(&old_row)
                .with_context(|| format_compact!("airport warehouse {id}"))?
            else {
                warn!(
                    "airport warehouse {id}: coalition neutral — skipping BDEFAULT/RDEFAULT (keeping base row)"
                );
                continue;
            };
            let src = match side {
                Side::Blue => &blue_master,
                Side::Red => &red_master,
                Side::Neutral => unreachable!("filtered above"),
            };
            let new_row = src.deep_clone(lua)?;
            preserve_dynamic_flags(lua, &new_row, &old_row)?;
            let inv_tpl = match side {
                Side::Blue => &self.blue_inventory,
                Side::Red => &self.red_inventory,
                Side::Neutral => unreachable!(),
            };
            merge_liquids_from_inventory_template(&new_row, inv_tpl, lua)?;
            if let Some(caps) = warehouse_caps {
                if caps.has_any_nonzero_cap() && !is_dynamic {
                    let m = mult_cfg.mult_airport(id);
                    campaign_cfg::fill_zero_weapon_amounts_from_cfg(&new_row, caps, m)
                        .with_context(|| {
                            format_compact!("fill zero weapons airport {id}")
                        })?;
                    match side {
                        Side::Blue => {
                            prune_warehouse_weapons_row(
                                lua,
                                &new_row,
                                &blue_strip_ws,
                                warehouse_allowlist_for_filter(&blue_allowed_ws),
                                "airport post-fill BDEFAULT filter",
                            )?;
                            zero_default_weapons_present_in_positive_inventory(
                                &new_row,
                                &self.blue_inventory,
                                blue_inventory_positive_block_ws.as_ref(),
                                "airport post-fill BDEFAULT de-dup",
                                "BINVENTORY",
                            )?;
                        }
                        Side::Red => {
                            prune_warehouse_weapons_row(
                                lua,
                                &new_row,
                                &red_strip_ws,
                                warehouse_allowlist_for_filter(&red_allowed_ws),
                                "airport post-fill RDEFAULT filter",
                            )?;
                            zero_default_weapons_present_in_positive_inventory(
                                &new_row,
                                &self.red_inventory,
                                red_inventory_positive_block_ws.as_ref(),
                                "airport post-fill RDEFAULT de-dup",
                                "RINVENTORY",
                            )?;
                        }
                        Side::Neutral => unreachable!("filtered above"),
                    }
                }
            }
            airports
                .set(id, new_row)
                .with_context(|| format_compact!("setting airport {id}"))?;
        }
        for id in whids {
            let old_row = warehouses
                .raw_get::<_, Table>(id)
                .with_context(|| format_compact!("getting warehouse {id}"))?;
            let Some(side) = warehouse_side_for_default_apply(&old_row)
                .with_context(|| format_compact!("warehouse {id}"))?
            else {
                warn!(
                    "warehouse {id}: coalition neutral — skipping BDEFAULT/RDEFAULT (keeping base row)"
                );
                continue;
            };
            let src = match side {
                Side::Blue => &blue_master,
                Side::Red => &red_master,
                Side::Neutral => unreachable!("filtered above"),
            };
            let new_row = src.deep_clone(lua)?;
            let is_dynamic = old_row.raw_get::<_, bool>("dynamicSpawn").unwrap_or(false);
            preserve_dynamic_flags(lua, &new_row, &old_row)?;
            let inv_tpl = match side {
                Side::Blue => &self.blue_inventory,
                Side::Red => &self.red_inventory,
                Side::Neutral => unreachable!(),
            };
            merge_liquids_from_inventory_template(&new_row, inv_tpl, lua)?;
            if let Some(caps) = warehouse_caps {
                if caps.has_any_nonzero_cap() && !is_dynamic {
                    let m = mult_cfg.mult_warehouse_row(id);
                    campaign_cfg::fill_zero_weapon_amounts_from_cfg(&new_row, caps, m)
                        .with_context(|| {
                            format_compact!("fill zero weapons warehouse {id}")
                        })?;
                    match side {
                        Side::Blue => {
                            prune_warehouse_weapons_row(
                                lua,
                                &new_row,
                                &blue_strip_ws,
                                warehouse_allowlist_for_filter(&blue_allowed_ws),
                                "warehouse post-fill BDEFAULT filter",
                            )?;
                            zero_default_weapons_present_in_positive_inventory(
                                &new_row,
                                &self.blue_inventory,
                                blue_inventory_positive_block_ws.as_ref(),
                                "warehouse post-fill BDEFAULT de-dup",
                                "BINVENTORY",
                            )?;
                        }
                        Side::Red => {
                            prune_warehouse_weapons_row(
                                lua,
                                &new_row,
                                &red_strip_ws,
                                warehouse_allowlist_for_filter(&red_allowed_ws),
                                "warehouse post-fill RDEFAULT filter",
                            )?;
                            zero_default_weapons_present_in_positive_inventory(
                                &new_row,
                                &self.red_inventory,
                                red_inventory_positive_block_ws.as_ref(),
                                "warehouse post-fill RDEFAULT de-dup",
                                "RINVENTORY",
                            )?;
                        }
                        Side::Neutral => unreachable!("filtered above"),
                    }
                }
            }
            warehouses
                .set(id, new_row)
                .with_context(|| format_compact!("setting warehouse {id}"))?
        }
        let old_red_inventory = warehouses
            .raw_get::<_, Table>(red_inventory)
            .context("getting current red inventory")?;
        let new_red_inventory = self.red_inventory.deep_clone(lua)?;
        preserve_dynamic_flags(lua, &new_red_inventory, &old_red_inventory)?;
        validate_inventory_weapons(
            &new_red_inventory,
            warehouse_allowlist_for_filter(&red_inventory_allowed_ws),
            "RINVENTORY",
        )?;
        prune_warehouse_weapons_row(
            lua,
            &new_red_inventory,
            &red_strip_ws,
            warehouse_allowlist_for_filter(&red_inventory_allowed_ws),
            "RINVENTORY",
        )?;
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
        preserve_dynamic_flags(lua, &new_blue_inventory, &old_blue_inventory)?;
        validate_inventory_weapons(
            &new_blue_inventory,
            warehouse_allowlist_for_filter(&blue_inventory_allowed_ws),
            "BINVENTORY",
        )?;
        prune_warehouse_weapons_row(
            lua,
            &new_blue_inventory,
            &blue_strip_ws,
            warehouse_allowlist_for_filter(&blue_inventory_allowed_ws),
            "BINVENTORY",
        )?;
        log_agm65_diag("after_inventory_finalize", "BINVENTORY", &new_blue_inventory)?;
        // bflib: union of B/RDEFAULT and B/RINVENTORY legal wsTypes (alias-expanded).
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
        base.warehouses.set("airports", airports)?;
        base.warehouses.set("warehouses", warehouses)?;
        if cfg.write_back_warehouse_defaults {
            if !weapon_bridge_used {
                warn!(
                    "--write-back-warehouse-defaults: weapon bridge missing, keep template BDEFAULT/RDEFAULT unchanged"
                );
            } else {
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
            }
            copy_weapons_subtable(
                lua,
                &self.blue_inventory,
                &new_blue_inventory,
                "BINVENTORY template",
            )?;
            copy_weapons_subtable(
                lua,
                &self.red_inventory,
                &new_red_inventory,
                "RINVENTORY template",
            )?;
        }
        Ok(bfprotocols::fowl_miz_export::FowlMizExport {
            schema_version: 3,
            weapon_bridge_used,
            blue_weapon_ws: blue_weapon_export,
            red_weapon_ws: red_weapon_export,
            objective_defaults,
        })
    }
}

/// Emitted `DT_*` templates and allow-lists for where each type may offer dynamic spawn.
struct DynamicSpawnEmit {
    link_by_side_type: HashMap<(Side, String), GroupId>,
    /// `None` if no enabled `TTD*` zones (excluding `TTDN*`) → land airports / FARPs allow every emitted type.
    land_allow: Option<HashSet<(Side, String)>>,
    /// `None` if no enabled `TTDN*` zones → ship warehouses allow every emitted type.
    naval_allow: Option<HashSet<(Side, String)>>,
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

/// Wire `linkDynTempl` to emitted dynamic template group ids (`zzDT-*`, `dynSpawnTemplate`).
/// Scales BINVENTORY/RINVENTORY `initialAmount` like Fowl `WarehouseConfig::capacity`
/// (airport hub vs airbase; `warehouses` naval vs FOB vs airbase).
///
/// `TTD*` (but not `TTDN*`) allow-lists apply to **airports** and non-ship `warehouses` (e.g. FARP).
/// `TTDN*` allow-lists apply only to **ship** warehouses (unitId keys present in both mission ships and `warehouses`).
///
/// --- Planned carrier-specific pipeline (per-ship, not global naval union) ---
/// 1. For each ship warehouse (coalition side), prefill `aircrafts` rows from `BINVENTORY` / `RINVENTORY`
///    (same shape as today’s template merge).
/// 2. Set `linkDynTempl` only for rows with non-zero stock and a matching dynamic template group for `(side, type)`.
/// 3. For warehouse id tied to ship template key `K` (e.g. group name `RKuznecow` → zones `TTSNRKuznecow`,
///    `TTDNRKuznecow`), set `initialAmount = 0` and `linkDynTempl = 0` for any aircraft type not listed in
///    the merged slot specs from those zones (static ∪ dynamic allowlist for that hull only).
///    SETTINGS-* filters become redundant for carriers if every hull always has matching `TTSN*` + `TTDN*`.
/// If ground bases still misbehave, consider generalising this 3-step pattern to airports / FARPs.
///
/// Carrier naming / zones are validated earlier by `audit_naval_carrier_mission_rules` (build fails if invalid).
fn patch_warehouse_dynamic_spawn_links(
    lua: &Lua,
    warehouses_root: &Table<'static>,
    emit: &DynamicSpawnEmit,
    blue_inventory: Option<&Table<'static>>,
    red_inventory: Option<&Table<'static>>,
    mult_cfg: &WarehouseStockMultConfig,
    warehouse_caps: Option<&campaign_cfg::WarehouseDefaultsFromCfg>,
) -> Result<()> {
    fn patch_table(
        lua: &Lua,
        tbl: &Table<'static>,
        emit: &DynamicSpawnEmit,
        blue_inventory: Option<&Table<'static>>,
        red_inventory: Option<&Table<'static>>,
        mult_cfg: &WarehouseStockMultConfig,
        is_airports_table: bool,
        warehouse_caps: Option<&campaign_cfg::WarehouseDefaultsFromCfg>,
    ) -> Result<()> {
        fn copy_initial_amounts_scaled(
            lua: &Lua,
            dst_row: &Table<'static>,
            src_row: &Table<'static>,
            mult: u32,
            warehouse_caps: Option<&campaign_cfg::WarehouseDefaultsFromCfg>,
        ) -> Result<()> {
            // Prefill dynamic warehouses: same base counts as BINVENTORY/RINVENTORY, scaled like Fowl capacity.
            if let (Ok(dst_aircrafts), Ok(src_aircrafts)) = (
                dst_row.raw_get::<_, Table>("aircrafts"),
                src_row.raw_get::<_, Table>("aircrafts"),
            ) {
                for cat in ["helicopters", "planes"] {
                    let (Ok(dst_cat), Ok(src_cat)) = (
                        dst_aircrafts.raw_get::<_, Table>(cat),
                        src_aircrafts.raw_get::<_, Table>(cat),
                    ) else {
                        continue;
                    };
                    for pair in dst_cat.clone().pairs::<String, Table>() {
                        let (unit_type, dst_unit) = pair?;
                        let Ok(src_unit) = src_cat.raw_get::<_, Table>(unit_type.clone())
                        else {
                            continue;
                        };
                        let Ok(src_amt) = src_unit.raw_get::<_, u32>("initialAmount")
                        else {
                            continue;
                        };
                        dst_unit
                            .raw_set("initialAmount", src_amt.saturating_mul(mult))?;
                    }
                }
            }

            if let Ok(src_weapons) = src_row.raw_get::<_, Table>("weapons") {
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
            Ok(())
        }

        for pair in tbl.clone().pairs::<Value, Table>() {
            let (k, wh) = pair?;
            let Some(wid) = warehouse_lua_key_i64(k) else {
                continue;
            };
            if !wh.raw_get::<_, bool>("dynamicSpawn").unwrap_or(false) {
                continue;
            }
            let mult = mult_cfg.mult_dynamic_row(wid, is_airports_table);
            let coa: String = wh.raw_get("coalition")?;
            let side = match coa.to_lowercase().as_str() {
                "red" => Side::Red,
                "blue" => Side::Blue,
                _ => continue,
            };
            let inv = match side {
                Side::Blue => blue_inventory,
                Side::Red => red_inventory,
                Side::Neutral => None,
            };
            if let Some(inv) = inv {
                copy_initial_amounts_scaled(lua, &wh, inv, mult, warehouse_caps)?;
            }
            let aircrafts: Table = wh.raw_get("aircrafts")?;
            let use_naval_filter = mult_cfg.naval_warehouse_ids.contains(&wid);
            for cat in ["helicopters", "planes"] {
                let Ok(cat_tbl) = aircrafts.raw_get::<_, Table>(cat) else {
                    continue;
                };
                for pair in cat_tbl.pairs::<String, Table>() {
                    let (unit_type, row) = pair?;
                    let link = emit
                        .link_by_side_type
                        .get(&(side, unit_type.clone()))
                        .map(|gid| gid.inner())
                        .unwrap_or(0);
                    let allowed = if use_naval_filter {
                        emit.naval_allow
                            .as_ref()
                            .map_or(true, |s| s.contains(&(side, unit_type.clone())))
                    } else {
                        emit.land_allow
                            .as_ref()
                            .map_or(true, |s| s.contains(&(side, unit_type.clone())))
                    };
                    row.raw_set(
                        "linkDynTempl",
                        if allowed && link != 0 { link } else { 0 },
                    )?;
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
        blue_inventory,
        red_inventory,
        mult_cfg,
        true,
        warehouse_caps,
    )
    .context("patching airport linkDynTempl")?;

    let warehouses = warehouses_root
        .raw_get::<_, Table>("warehouses")
        .context("getting warehouses")?;
    patch_table(
        lua,
        &warehouses,
        emit,
        blue_inventory,
        red_inventory,
        mult_cfg,
        false,
        warehouse_caps,
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

fn collect_objective_aircraft_by_side(
    base: &LoadedMiz,
    objectives: &[TriggerZone],
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
                    let mut found = false;
                    for obj in objectives {
                        if obj.contains(Vector2::new(x, y))? {
                            out.entry(obj.objective_name.to_string())
                                .or_default()
                                .entry(side)
                                .or_default()
                                .insert(unit_type.to_string());
                            found = true;
                            break;
                        }
                    }
                    if !found {
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

fn validate_single_airbase_per_objective(
    objectives: &[TriggerZone],
    base: &LoadedMiz,
) -> Result<()> {
    // Objective zones must map to exactly one airbase/warehouse hub to keep supply logic unambiguous.
    let mut errors: Vec<std::string::String> = vec![];

    for obj in objectives {
        let mut airbases: Vec<std::string::String> = vec![];
        for (side, coa) in
            Side::ALL.into_iter().map(|side| (side, base.mission.coalition(side)))
        {
            let _ = side;
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
                        let x: f64 = unit.raw_get("x")?;
                        let y: f64 = unit.raw_get("y")?;
                        if obj.contains(Vector2::new(x, y))? {
                            airbases.push(name.to_string());
                        }
                    }
                }
            }
        }

        if airbases.len() > 1 {
            airbases.sort();
            airbases.dedup();
            errors.push(format!(
                "objective {} has multiple airbases inside the trigger zone: {}",
                obj.objective_name,
                airbases.join(", ")
            ));
        }
    }

    if !errors.is_empty() {
        bail!("{}", errors.join("\n"));
    }
    Ok(())
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
    let mut objectives = compile_objectives(&base).context("compiling objectives")?;
    validate_single_airbase_per_objective(&objectives, &base)
        .context("validating single airbase per objective")?;
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
    if cfg.write_back_warehouse_defaults && resolved_warehouse_path.is_none() {
        bail!(
            "--write-back-warehouse-defaults requires --campaign-cfg and warehouse<campaign_decade>.miz beside the weapon template"
        );
    }
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
                "no weapon bridge JSON (--weapon-bridge, or fowl_weapon_bridge.json / fowl_weapon_bridge-DCS.version.*.json next to resolved weapon template); run Fowl_engine_weapon_bridge_export.lua in DCS Hooks first"
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
            Some(WarehouseBundle { path, loaded, template })
        }
    };
    vehicle_templates.generate_slots(lua, &mut base).context("generating slots")?;
    vehicle_templates
        .apply(lua, &mut objectives, &mut base)
        .context("applying vehicle templates")?;
    let objective_aircraft_by_side =
        collect_objective_aircraft_by_side(&base, &objectives)
            .context("collecting objective aircraft module map")?;
    info!(
        "objective module map prepared for {} objective(s)",
        objective_aircraft_by_side.len()
    );
    let dynamic_emit = vehicle_templates
        .emit_dynamic_spawn_templates(lua, &mut base)
        .context("emitting dynamic spawn templates")?;
    if weapon_template_path != cfg.base {
        sync_dt_mirror_groups_into_weapon_miz(lua, &weapon_template_path, &base)
            .context("syncing zzDT-* mirror groups into weapon.miz")?;
    } else {
        warn!("--weapon equals --base; skipping zzDT-* mirror write to weapon.miz");
    }
    sync_l10n_dictionary_sortie_stem_to_output_miz(&base, &cfg.output)
        .context("l10n: set dictionary[mission.sortie] to --output .miz stem (DCS Saved Games / Fowl files)")?;
    let s = serialize_to_lua("mission", Value::Table((&*base.mission).clone()))?;
    fs::write(&base.miz.files["mission"], &s).context("writing mission file")?;
    info!("wrote serialized mission to mission file.");
    let ship_wh_map = collect_ship_warehouse_group_map(&base)?;
    let naval_warehouse_ids: HashSet<i64> = ship_wh_map.keys().copied().collect();
    let mult_cfg = WarehouseStockMultConfig {
        airbase_max,
        hub_max,
        fob_max,
        carrier_airbase_max,
        hub_airport_ids,
        fob_warehouse_ids,
        naval_warehouse_ids,
    };
    warn!(
        "warehouse stock multipliers: airbase_max={} hub_max={} fob_max={} carrier_airbase_max={}; hub airport keys {:?}; fob warehouse keys {:?}; {} naval warehouse id(s)",
        mult_cfg.airbase_max,
        mult_cfg.hub_max,
        mult_cfg.fob_max,
        mult_cfg.carrier_airbase_max,
        mult_cfg.hub_airport_ids,
        mult_cfg.fob_warehouse_ids,
        mult_cfg.naval_warehouse_ids.len()
    );
    let missing_default_warehouse_keys = campaign_overlay
        .as_ref()
        .map(|o| o.missing_default_warehouse_keys.clone())
        .unwrap_or_default();

    let fowl_from_warehouse = if let Some(wb) = warehouse_bundle.as_ref() {
        let bridge_gen = weapon_bridge_map.as_ref().map(|b| (&vehicle_templates, b));
        let export = wb
            .template
            .apply(
                lua,
                &cfg,
                &mut base,
                warehouse_defaults,
                bridge_gen,
                &objective_aircraft_by_side,
                &droptank_ws_from_weapon_warehouses,
                &mult_cfg,
            )
            .context("applying warehouse template")?;
        if cfg.write_back_warehouse_defaults && weapon_bridge_map.is_some() {
            let wh_file = wb
                .loaded
                .miz
                .files
                .get("warehouses")
                .context("warehouse template miz missing warehouses file entry")?;
            let s = serialize_to_lua(
                "warehouses",
                Value::Table(wb.loaded.warehouses.clone()),
            )?;
            fs::write(wh_file, &*s)
                .context("write-back: serializing template warehouses")?;
            wb.loaded.miz.pack(&wb.path).with_context(|| {
                format_compact!(
                    "write-back: repacking warehouse template {}",
                    wb.path.display()
                )
            })?;
            info!(
                "write-back: updated BDEFAULT/RDEFAULT/BINVENTORY/RINVENTORY `weapons` in `{}` (mirror of build; edit policy in weapon template, not ME here)",
                wb.path.display()
            );
        }
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
        let airports_tbl: Table<'static> = base
            .warehouses
            .raw_get("airports")
            .context("getting airports for dynamic warehouse prefill")?;
        let warehouses_tbl: Table<'static> = base
            .warehouses
            .raw_get("warehouses")
            .context("getting warehouses for dynamic warehouse prefill")?;
        let blue_inv_row: Table<'static> = airports_tbl
            .raw_get(blue_inv_id)
            .or_else(|_| warehouses_tbl.raw_get(blue_inv_id))
            .with_context(|| {
                format_compact!("getting filtered BINVENTORY row {blue_inv_id}")
            })?;
        let red_inv_row: Table<'static> = airports_tbl
            .raw_get(red_inv_id)
            .or_else(|_| warehouses_tbl.raw_get(red_inv_id))
            .with_context(|| {
                format_compact!("getting filtered RINVENTORY row {red_inv_id}")
            })?;
        patch_warehouse_dynamic_spawn_links(
            lua,
            &base.warehouses,
            &dynamic_emit,
            Some(&blue_inv_row),
            Some(&red_inv_row),
            &mult_cfg,
            warehouse_defaults,
        )
        .context("patching warehouse linkDynTempl")?;
        if let Some(caps) = warehouse_defaults {
            if caps.has_any_nonzero_cap() {
                let mut skip = HashSet::default();
                skip.insert(blue_inv_id);
                skip.insert(red_inv_id);
                apply_weapon_cfg_cap_scale_pass(&base.warehouses, caps, &mult_cfg, &skip)
                    .context("scaling default_warehouse_* caps by stock multiplier")?;
            }
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
    info!("saving finalized mission to {:?}", cfg.output);
    base.miz.pack(&cfg.output).context("repacking mission")?;
    let export_path = fowl_from_warehouse
        .write_next_to_miz(&cfg.output)
        .context("writing Fowl mission export JSON")?;
    info!("wrote Fowl mission export to {:?}", export_path);
    Ok(())
}
