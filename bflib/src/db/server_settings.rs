//! DCS dedicated server `Config/serverSettings.lua` (same instance as `writedir` / `Scripts/bflib.dll`).

use anyhow::{bail, Context, Result};
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

/// Public host for outbound links (`discord_map`); not `0.0.0.0` / loopback.
pub fn public_bind_host(bind_address: &str) -> Result<&str> {
    let host = bind_address.trim();
    if host.is_empty() {
        bail!("serverSettings.lua cfg.bind_address is required when discord_map is enabled");
    }
    if matches!(
        host,
        "0.0.0.0" | "::" | "[::]" | "127.0.0.1" | "::1" | "localhost"
    ) {
        bail!(
            "serverSettings.lua cfg.bind_address must be a routable server address, not {host}"
        );
    }
    Ok(host)
}

#[cfg(test)]
mod tests {
    use super::public_bind_host;

    #[test]
    fn public_bind_host_rejects_unusable() {
        assert!(public_bind_host("").is_err());
        assert!(public_bind_host("0.0.0.0").is_err());
        assert!(public_bind_host("127.0.0.1").is_err());
    }

    #[test]
    fn public_bind_host_accepts_public_ipv4() {
        assert_eq!(public_bind_host("135.181.77.146").unwrap(), "135.181.77.146");
    }
}
