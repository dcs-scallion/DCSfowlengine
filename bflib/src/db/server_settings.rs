//! DCS dedicated server `Config/serverSettings.lua` (same instance as `writedir` / `Scripts/bflib.dll`).

use anyhow::{Context, Result};
use dcso3::{lfs::Lfs, LuaEnv, MizLua};
use log::warn;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServerSettings {
    pub bind_address: String,
    pub port: String,
    pub name: String,
}

pub fn load_server_settings(lua: MizLua) -> ServerSettings {
    match try_load_server_settings(lua) {
        Ok(s) => s,
        Err(e) => {
            warn!("serverSettings.lua: {e:#}");
            ServerSettings::default()
        }
    }
}

fn try_load_server_settings(lua: MizLua) -> Result<ServerSettings> {
    let writedir = Lfs::singleton(lua)?.writedir()?;
    let path = PathBuf::from(writedir.as_str()).join("Config").join("serverSettings.lua");
    if !path.is_file() {
        anyhow::bail!("missing {:?}", path);
    }
    let path_lua = path.to_string_lossy().replace('\\', "/");
    let chunk = format!(
        r#"
local path = "{path_lua}"
local ok = pcall(function() dofile(path) end)
if not ok or type(cfg) ~= "table" then
  return "", "", ""
end
local function s(v)
  if v == nil then return "" end
  return tostring(v)
end
return s(cfg.bind_address), s(cfg.port), s(cfg.name)
"#
    );
    let (bind_address, port, name): (String, String, String) = lua
        .inner()
        .load(&chunk)
        .eval()
        .with_context(|| format!("eval serverSettings.lua {:?}", path))?;
    Ok(ServerSettings {
        bind_address,
        port,
        name,
    })
}
