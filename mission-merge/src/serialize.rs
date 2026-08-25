use anyhow::{bail, Result};
use mlua::Value;
use std::fmt::{self, Display, Write as FmtWrite};

struct LuaSerVal<'a> {
    value: &'a Value<'a>,
    level: usize,
}

impl LuaSerVal<'_> {
    fn indent(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for _ in 0..self.level {
            write!(f, " ")?;
        }
        Ok(())
    }
}

fn escape_lua_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

impl Display for LuaSerVal<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.value {
            Value::Boolean(b) => write!(f, "{b}"),
            Value::Integer(i) => write!(f, "{i}"),
            Value::Nil => write!(f, "nil"),
            Value::Number(n) => write!(f, "{n}"),
            Value::String(s) => write!(f, "\"{}\"", escape_lua_string(&s.to_string_lossy())),
            Value::Table(tbl) => {
                write!(f, "\n")?;
                self.indent(f)?;
                write!(f, "{{\n")?;
                let mut seq_max: Option<i64> = None;
                if tbl.contains_key(1).unwrap_or(false) {
                    for (i, v) in tbl.clone().sequence_values::<Value>().enumerate() {
                        let i = (i + 1) as i64;
                        let v = v.map_err(|_| fmt::Error)?;
                        seq_max = Some(i);
                        let k = Value::Integer(i);
                        let kv = LuaSerVal {
                            value: &k,
                            level: self.level + 4,
                        };
                        let vv = LuaSerVal {
                            value: &v,
                            level: self.level + 4,
                        };
                        kv.indent(f)?;
                        if vv.value.is_table() {
                            write!(f, "[{kv}] = {vv}, -- end of [{kv}]\n")?;
                        } else {
                            write!(f, "[{kv}] = {vv},\n")?;
                        }
                    }
                }
                tbl.for_each::<Value, Value>(|k, v| {
                    if let Some(max) = seq_max {
                        if let Some(ki) = k.as_integer() {
                            if (1..=max).contains(&ki) {
                                return Ok(());
                            }
                        }
                    }
                    let kv = LuaSerVal {
                        value: &k,
                        level: self.level + 4,
                    };
                    let vv = LuaSerVal {
                        value: &v,
                        level: self.level + 4,
                    };
                    kv.indent(f).map_err(|e| mlua::Error::external(e))?;
                    if vv.value.is_table() {
                        write!(f, "[{kv}] = {vv}, -- end of [{kv}]\n")
                            .map_err(|e| mlua::Error::external(e))?;
                    } else {
                        write!(f, "[{kv}] = {vv},\n").map_err(|e| mlua::Error::external(e))?;
                    }
                    Ok(())
                })
                .map_err(|_| fmt::Error)?;
                self.indent(f)?;
                write!(f, "}}")
            }
            Value::Error(_)
            | Value::Function(_)
            | Value::LightUserData(_)
            | Value::Thread(_)
            | Value::UserData(_) => Err(fmt::Error),
        }
    }
}

pub fn serialize_mission(value: &Value) -> Result<String> {
    if matches!(
        value,
        Value::Error(_)
            | Value::Function(_)
            | Value::LightUserData(_)
            | Value::Thread(_)
            | Value::UserData(_)
    ) {
        bail!("mission table cannot be serialized");
    }
    let mut s = String::with_capacity(1024 * 1024);
    write!(
        s,
        "mission = {}",
        LuaSerVal {
            value,
            level: 0,
        }
    )?;
    if !s.ends_with('\n') {
        s.push('\n');
    }
    Ok(s)
}
