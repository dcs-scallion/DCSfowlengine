/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

use super::{as_tbl, cvt_err, unit::Unit, weapon::Weapon, LuaVec3, Position3, String};
use crate::{
    check_implements, record_perf, simple_enum, static_object::StaticObject, wrapped_table, LuaEnv,
    MizLua,
};
use anyhow::{anyhow, bail, Result};
use core::fmt;
use log::debug;
use mlua::{prelude::*, Value};
use serde_derive::{Deserialize, Serialize};
use std::{hash::Hash, marker::PhantomData, ops::Deref};

#[derive(Clone, Serialize, Deserialize)]
pub struct DcsOid<T> {
    pub(crate) id: u64,
    pub(crate) class: String,
    #[serde(skip)]
    pub(crate) t: PhantomData<T>,
}

impl<T> DcsOid<T> {
    pub fn erased(&self) -> DcsOid<ClassObject> {
        DcsOid {
            id: self.id,
            class: self.class.clone(),
            t: PhantomData,
        }
    }

    pub fn check_implements(&self, lua: MizLua, class: &str) -> Result<()> {
        let m = lua.inner().globals().raw_get(&**self.class)?;
        if !check_implements(&m, class) {
            bail!("{:?} is does not implement {class}", self)
        }
        Ok(())
    }
}

impl<T> fmt::Debug for DcsOid<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{ id: {}, class: {} }}", self.id, self.class)
    }
}

impl<T> Hash for DcsOid<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state)
    }
}

impl<T> PartialEq for DcsOid<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id.eq(&other.id)
    }
}

impl<T> Eq for DcsOid<T> {}

impl<T> PartialOrd for DcsOid<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.id.partial_cmp(&other.id)
    }
}

impl<T> Ord for DcsOid<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

#[derive(Debug, Clone)]
pub struct ClassObject;

pub trait DcsObject<'lua>: Sized + Deref<Target = mlua::Table<'lua>> {
    type Class: fmt::Debug + Clone;

    fn object_id(&self) -> Result<DcsOid<Self::Class>> {
        let id = self.raw_get("id_")?;
        let m = self
            .get_metatable()
            .ok_or_else(|| anyhow!("object with no metatable"))?;
        let class = m.raw_get("className_")?;
        Ok(DcsOid {
            id,
            class,
            t: PhantomData,
        })
    }

    fn change_instance(self, id: &DcsOid<Self::Class>) -> Result<Self>;
    fn change_instance_dyn<T>(self, id: &DcsOid<T>) -> Result<Self>;
    fn get_instance(lua: MizLua<'lua>, id: &DcsOid<Self::Class>) -> Result<Self>;
    fn get_instance_dyn<T>(lua: MizLua<'lua>, id: &DcsOid<T>) -> Result<Self>;
}

simple_enum!(ObjectCategory, u8, [
    Void => 0,
    Unit => 1,
    Weapon => 2,
    Static => 3,
    Base => 4,
    Scenery => 5,
    Cargo => 6
]);

wrapped_table!(Object, Some("Object"));

struct EventObjectDescriptor {
    id: u64,
    unit_type: Option<String>,
    coalition: Option<i64>,
}

fn event_object_class_candidates(desc: &EventObjectDescriptor) -> &'static [&'static str] {
    match desc.unit_type.as_deref() {
        Some(ut) if ut.starts_with("weapons.") => &["Weapon"],
        Some(ut) if ut.is_empty() && desc.coalition == Some(-1) => &["Weapon"],
        Some(ut) if ut.is_empty() => &["Unit", "Static", "Weapon"],
        Some(_) => &["Static", "Unit", "Weapon", "Scenery"],
        None if desc.coalition == Some(-1) => &["Weapon"],
        None => &["Static", "Unit", "Weapon", "Scenery"],
    }
}

fn resolve_event_object_descriptor<'lua>(
    lua: &'lua Lua,
    desc: &EventObjectDescriptor,
) -> Result<Object<'lua>> {
    let miz = MizLua(lua);
    for class in event_object_class_candidates(desc) {
        let oid = DcsOid {
            id: desc.id,
            class: (*class).into(),
            t: PhantomData,
        };
        if let Ok(obj) = Object::get_instance(miz, &oid) {
            debug!(
                "event object descriptor id={} unit_type={:?} resolved as {class}",
                desc.id, desc.unit_type
            );
            return Ok(obj);
        }
    }
    bail!("could not resolve event object descriptor id={}", desc.id)
}

/// DCS 2.9.27+ may send initiator/target as plain tables without Object metatable.
pub(crate) fn optional_event_object<'lua>(
    lua: &'lua Lua,
    value: Value<'lua>,
) -> LuaResult<Option<Object<'lua>>> {
    match value {
        Value::Nil => Ok(None),
        Value::Table(tbl) => {
            if let Some(meta) = tbl.get_metatable() {
                if check_implements(&meta, "Object") {
                    return Ok(Some(Object::from_lua(Value::Table(tbl), lua)?));
                }
            }
            let id: u64 = match tbl.raw_get("id_") {
                Ok(id) => id,
                Err(_) => return Ok(None),
            };
            let desc = EventObjectDescriptor {
                id,
                unit_type: tbl.raw_get("unit_type").ok(),
                coalition: tbl.raw_get("coalition").ok(),
            };
            Ok(resolve_event_object_descriptor(lua, &desc).ok())
        }
        _ => Ok(None),
    }
}

impl<'lua> Object<'lua> {
    pub fn destroy(self) -> Result<()> {
        Ok(self.t.call_method("destroy", ())?)
    }

    pub fn get_category(&self) -> Result<ObjectCategory> {
        Ok(self.t.call_method("getCategory", ())?)
    }

    pub fn get_desc(&self) -> Result<mlua::Table<'lua>> {
        Ok(self.t.call_method("getDesc", ())?)
    }

    pub fn has_attribute(&self, attr: String) -> Result<bool> {
        Ok(self.t.call_method("hasAttribute", attr)?)
    }

    pub fn get_name(&self) -> Result<String> {
        Ok(self.t.call_method("getName", ())?)
    }

    pub fn get_type_name(&self) -> Result<String> {
        Ok(self.t.call_method("getTypeName", ())?)
    }

    pub fn get_point(&self) -> Result<LuaVec3> {
        Ok(record_perf!(get_point, self.t.call_method("getPoint", ())?))
    }

    pub fn get_position(&self) -> Result<Position3> {
        Ok(record_perf!(
            get_position,
            self.t.call_method("getPosition", ())?
        ))
    }

    pub fn get_velocity(&self) -> Result<LuaVec3> {
        Ok(record_perf!(
            get_velocity,
            self.t.call_method("getVelocity", ())?
        ))
    }

    pub fn in_air(&self) -> Result<bool> {
        Ok(self.t.call_method("inAir", ())?)
    }

    pub fn is_exist(&self) -> Result<bool> {
        Ok(self.t.call_method("isExist", ())?)
    }

    pub fn as_unit(&self) -> Result<Unit<'lua>> {
        Ok(Unit::from_lua(Value::Table(self.t.clone()), self.lua)?)
    }

    pub fn as_weapon(&self) -> Result<Weapon<'lua>> {
        Ok(Weapon::from_lua(Value::Table(self.t.clone()), self.lua)?)
    }

    pub fn as_static(&self) -> Result<StaticObject<'lua>> {
        Ok(StaticObject::from_lua(
            Value::Table(self.t.clone()),
            self.lua,
        )?)
    }
}

impl<'lua> DcsObject<'lua> for Object<'lua> {
    type Class = ClassObject;

    fn get_instance(lua: MizLua<'lua>, id: &DcsOid<Self::Class>) -> Result<Self> {
        let t = lua.inner().create_table()?;
        t.set_metatable(Some(lua.inner().globals().raw_get(&**id.class)?));
        t.raw_set("id_", id.id)?;
        let t = Object {
            t,
            lua: lua.inner(),
        };
        if !t.is_exist()? {
            bail!("{} is an invalid object", id.id)
        }
        Ok(t)
    }

    fn get_instance_dyn<T>(lua: MizLua<'lua>, id: &DcsOid<T>) -> Result<Self> {
        id.check_implements(lua, "Object")?;
        let id = DcsOid {
            id: id.id,
            class: id.class.clone(),
            t: PhantomData,
        };
        Self::get_instance(lua, &id)
    }

    fn change_instance(self, id: &DcsOid<Self::Class>) -> Result<Self> {
        self.raw_set("id_", id.id)?;
        if !self.is_exist()? {
            bail!("{} is an invalid object", id.id)
        }
        Ok(self)
    }

    fn change_instance_dyn<T>(self, id: &DcsOid<T>) -> Result<Self> {
        id.check_implements(MizLua(self.lua), "Object")?;
        self.t.raw_set("id_", id.id)?;
        if !self.is_exist()? {
            bail!("{} is an invalid object", id.id)
        }
        Ok(self)
    }
}
