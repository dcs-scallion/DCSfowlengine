--[[
  Fowl engine 2.0 — export CLSID (a příbuzných klíčů) → wsType [l1,l2,l3,l4] z DCS.
  Při sestavení mise načte FowlTools.exe výstup (viz FowlTools --help):
  buď `fowl_weapon_bridge.json`, nebo nejnovější `fowl_weapon_bridge-DCS.version.<verze>.json`.

  Nasazení (jednorázově před exportem; pak hook vypni — DCS.setUserCallbacks přepisuje jiné hooky):
    1) Zkopíruj tento soubor do:
       Saved Games\DCS\Scripts\Hooks\
    2) Níže nastav OUTPUT_DIR na složku scenáře (tam kde leží weapon.miz).
    3) Spusť DCS, načti misi a vstup do 3D (spustí se onSimulationStart).
    4) Log: Saved Games\DCS\Logs\fowl_weapon_bridge_export.log
    5) JSON: OUTPUT_DIR\fowl_weapon_bridge-DCS.version.<verze_DCS>.json (verze v názvu = zdroj dat)
    6) Soubor z Hooks smaž nebo přejmenuj.

  Obsah JSON ≠ seznam povolené munice z warehouse*.miz: hook exportuje celý DCS strom db
  (CLSID → wsType), aby FowlTools uměl přeložit libovolný řetězec z payloadů. Zakázané položky
  z leteckých šablon (payload.restricted) se při sestavení vyhodnocují zvlášť — viz
  payload_weapon_descriptor_union / weapon bridge v bftools.

  _G.db se v misi může lišit od plné DB; hook běží v GUI stavu při startu simulace.
]]

-- ==== KONFIGURACE — absolutní cesta ke složce scenáře (weapon.miz) ====
-- Nech prázdné → JSON půjde do Saved Games\DCS\Logs\ (viz log).
--
-- DŮLEŽITÉ (Lua): v obyčejných uvozovkách "\" začíná escape — "\b" je backspace,
-- "\f" je formfeed, atd. Cesta se pak rozpadne (viz log „can't open C:fnext-...“).
-- Bezpečné varianty:
--   local OUTPUT_DIR = [[C:\bfnext-pracovni_DYNAMIC_UPDATE\miz\Scenarios\80s\caucasus1987]]
--   local OUTPUT_DIR = "C:/bfnext-pracovni_DYNAMIC_UPDATE/miz/Scenarios/80s/caucasus1987"
local OUTPUT_DIR = [[C:\bfnext-pracovni_DYNAMIC_UPDATE\miz\Scenarios\80s\caucasus1987]]
-- =======================================================================

local LOG_NAME = "fowl_weapon_bridge_export.log"

local function version_like(s)
    if type(s) == "number" then
        s = tostring(s)
    end
    if type(s) ~= "string" then
        return nil
    end
    local v = s:match("%d+%.%d+%.%d+%.%d+")
    if v and v ~= "" then
        return v
    end
    return nil
end

local function read_file(path)
    local f = io.open(path, "r")
    if not f then
        return nil
    end
    local s = f:read("*a")
    f:close()
    return s
end

local function package_path_roots()
    local roots = {}
    local seen = {}
    local pp = package and package.path or ""
    for part in string.gmatch(pp, "([^;]+)") do
        local p = part:gsub("/", "\\")
        local root = p:match("^(.*)\\Scripts\\%?%.lua$")
        if root and root ~= "" and not seen[root] then
            seen[root] = true
            roots[#roots + 1] = root
        end
    end
    return roots
end

local function read_version_from_autoupdate_cfg()
    local candidates = {}
    local cwd = lfs.currentdir and lfs.currentdir() or nil
    if type(cwd) == "string" and cwd ~= "" then
        local c = cwd:gsub("/", "\\")
        candidates[#candidates + 1] = c .. "\\autoupdate.cfg"
        candidates[#candidates + 1] = c .. "\\..\\autoupdate.cfg"
        candidates[#candidates + 1] = c .. "\\..\\..\\autoupdate.cfg"
    end
    for _, root in ipairs(package_path_roots()) do
        candidates[#candidates + 1] = root .. "\\autoupdate.cfg"
    end
    local seen = {}
    for _, path in ipairs(candidates) do
        if not seen[path] then
            seen[path] = true
            local content = read_file(path)
            if content then
                local v = content:match('"version"%s*:%s*"([^"]+)"')
                    or content:match("'version'%s*:%s*'([^']+)'")
                    or content:match('"branch_version"%s*:%s*"([^"]+)"')
                v = version_like(v) or v
                if type(v) == "string" and v ~= "" then
                    return v
                end
            end
        end
    end
    return nil
end

local function dcs_version_string()
    if type(DCS) == "table" and type(DCS.getVersion) == "function" then
        local okVer, v = pcall(DCS.getVersion)
        local okBuild, b = pcall(DCS.getBuildNumber)
        if okVer then
            if type(v) == "table" then
                v = v.version or v.Version or v.productVersion or v.ProductVersion or v[1]
            end
            v = version_like(v) or v
            if type(v) == "string" and v ~= "" then
                if okBuild and b ~= nil and tostring(b) ~= "" then
                    return string.format("%s-build.%s", v, tostring(b))
                end
                return v
            end
        end
    end
    if type(LoGetVersion) == "function" then
        local ok, t = pcall(LoGetVersion)
        if ok and type(t) == "table" then
            local v = t.Version or t.version or t.ProductVersion or t.productVersion or t.Revision or t.FileVersion
            v = version_like(v) or v
            if type(v) == "string" and v ~= "" then
                return v
            end
        end
    end
    local globals = {
        _G and _G.ED_FINAL_VERSION or nil,
        _G and _G.DCS_VERSION or nil,
        _G and _G.__DCS_VERSION__ or nil,
    }
    for _, gv in ipairs(globals) do
        local v = version_like(gv) or gv
        if type(v) == "string" and v ~= "" then
            return v
        end
    end
    local cfgv = read_version_from_autoupdate_cfg()
    if cfgv then
        return cfgv
    end
    return "unknown"
end

local function filename_safe_version(s)
    s = tostring(s)
    s = s:gsub("[%c%z]", "_")
    s = s:gsub('[\\/:%*%?"<>|]', "_")
    s = s:gsub("^[_%.]+", "")
    if s == "" then
        s = "unknown"
    end
    return s
end

local function output_json_filename()
    return "fowl_weapon_bridge-DCS.version." .. filename_safe_version(dcs_version_string()) .. ".json"
end

local function log_line(msg)
    local p = lfs.writedir() .. "Logs/" .. LOG_NAME
    local f = io.open(p, "a")
    if not f then
        return
    end
    f:write(os.date("!%Y-%m-%dT%H:%M:%SZ ") .. tostring(msg) .. "\n")
    f:close()
end

local function notify_user(msg)
    log_line(msg)
    if type(trigger) == "table" and type(trigger.action) == "table" and type(trigger.action.outText) == "function" then
        pcall(trigger.action.outText, msg, 15)
        return
    end
    if type(net) == "table" and type(net.log) == "function" then
        pcall(net.log, msg)
        return
    end
    if type(env) == "table" and type(env.info) == "function" then
        pcall(env.info, msg)
    end
end

local function json_escape(s)
    s = tostring(s)
    s = s:gsub("\\", "\\\\")
    s = s:gsub('"', '\\"')
    s = s:gsub("\n", "\\n")
    s = s:gsub("\r", "\\r")
    return s
end

local ws_key
local add_alias
local collect_entry_aliases
local ws_for_clsid
local collect_ws_from_table

local function read_ws4(tbl)
    if type(tbl) ~= "table" then
        return nil
    end
    if not (tbl[1] and tbl[2] and tbl[3] and tbl[4]) then
        return nil
    end
    return {
        tonumber(tbl[1]) or 0,
        tonumber(tbl[2]) or 0,
        tonumber(tbl[3]) or 0,
        tonumber(tbl[4]) or 0,
    }
end

local function ws_from_entry(e)
    if type(e) ~= "table" then
        return nil, nil
    end
    local desc = type(e.desc) == "table" and e.desc or nil
    local candidates = {
        { "wsTypeOfWeapon", e.wsTypeOfWeapon },
        { "ws_type_of_weapon", e.ws_type_of_weapon },
        { "wsType", e.wsType },
        { "attribute", e.attribute },
        { "desc.wsType", desc and desc.wsType or nil },
        { "desc.attribute", desc and desc.attribute or nil },
    }
    for _, c in ipairs(candidates) do
        local ws = read_ws4(c[2])
        if ws then
            return ws, c[1]
        end
    end
    return nil, nil
end

local function collect_db_branch(branch, out, seen, depth, path, stats)
    if depth > 32 or type(branch) ~= "table" then
        return
    end
    if seen[branch] then
        return
    end
    seen[branch] = true
    for k, v in pairs(branch) do
        if type(v) == "table" then
            local key_s = tostring(k)
            local next_path = path .. "/" .. key_s
            local wst, ws_src = ws_from_entry(v)
            local clsid = v.CLSID or v.clsid
            if wst then
                stats.total_ws = stats.total_ws + 1
                if ws_src then
                    stats.ws_source_counts[ws_src] = (stats.ws_source_counts[ws_src] or 0) + 1
                end
                if wst[1] == 1 and wst[2] == 3 then
                    stats.fuel_ws = stats.fuel_ws + 1
                    if #stats.fuel_examples < 40 then
                        local tag = type(clsid) == "string" and clsid ~= "" and clsid or key_s
                        stats.fuel_examples[#stats.fuel_examples + 1] = string.format(
                            "%s -> [%d,%d,%d,%d] @ %s",
                            tostring(tag),
                            wst[1], wst[2], wst[3], wst[4],
                            next_path
                        )
                    end
                end
                if wst[1] == 4 and wst[2] == 5 then
                    stats.ws_4_5 = stats.ws_4_5 + 1
                end
            else
                local desc = type(v.desc) == "table" and v.desc or nil
                local category = v.category or (desc and desc.category or nil)
                if category == 5 or category == "5" then
                    stats.category5_no_ws = stats.category5_no_ws + 1
                    if #stats.category5_examples < 30 then
                        stats.category5_examples[#stats.category5_examples + 1] = next_path
                    end
                end
            end
            if wst then
                local aliases = {}
                collect_entry_aliases(v, key_s, next_path, aliases)
                for a in pairs(aliases) do
                    add_alias(out, a, wst, stats)
                end
            end
            collect_db_branch(v, out, seen, depth + 1, next_path, stats)
        end
    end
end

local function build_map()
    local out = {}
    local stats = {
        total_ws = 0,
        fuel_ws = 0,
        ws_4_5 = 0,
        fuel_examples = {},
        category5_no_ws = 0,
        category5_examples = {},
        ws_source_counts = {},
        alias_added = 0,
        alias_same = 0,
        alias_collisions = 0,
    }
    local db = _G.db
    if type(db) ~= "table" then
        log_line("ERROR: _G.db is not a table (sanitized environment?)")
        return out, stats
    end
    if type(db.Weapons) ~= "table" then
        log_line("WARN: db.Weapons missing")
    end
    collect_db_branch(db, out, {}, 0, "db", stats)
    return out, stats
end

local function collect_all_fueltank_ws(map)
    local wsset = {}
    for _, ws in pairs(map) do
        if type(ws) == "table" and ws[1] == 1 and ws[2] == 3 then
            local ws_key = table.concat(ws, ",")
            wsset[ws_key] = ws
        end
    end
    local out = {}
    for _, ws in pairs(wsset) do
        out[#out + 1] = ws
    end
    table.sort(out, function(a, b)
        if a[1] ~= b[1] then return a[1] < b[1] end
        if a[2] ~= b[2] then return a[2] < b[2] end
        if a[3] ~= b[3] then return a[3] < b[3] end
        return a[4] < b[4]
    end)
    return out
end

collect_ws_from_table = function(node, descriptor_map, out_ws, seen)
    if type(node) ~= "table" then
        return
    end
    if seen[node] then
        return
    end
    seen[node] = true
    local wst = select(1, ws_from_entry(node))
    if wst then
        out_ws[ws_key(wst)] = wst
    end
    for k, v in pairs(node) do
        if type(k) == "string" then
            local w = ws_for_clsid(descriptor_map, k)
            if w then
                out_ws[ws_key(w)] = w
            end
        end
        if (k == "CLSID" or k == "clsid") and type(v) == "string" and v ~= "" then
            local w = ws_for_clsid(descriptor_map, v)
            if w then
                out_ws[ws_key(w)] = w
            end
        end
        if type(v) == "table" then
            collect_ws_from_table(v, descriptor_map, out_ws, seen)
        end
    end
end

local function collect_clsids_from_table(node, out, seen)
    if type(node) ~= "table" then
        return
    end
    if seen[node] then
        return
    end
    seen[node] = true
    for k, v in pairs(node) do
        if type(k) == "string" and k:match("^%b{}$") then
            out[k] = true
        end
        if k == "CLSID" and type(v) == "string" and v ~= "" then
            out[v] = true
        end
        if k == "clsid" and type(v) == "string" and v ~= "" then
            out[v] = true
        end
        if type(v) == "table" then
            collect_clsids_from_table(v, out, seen)
        end
    end
end

local function aircraft_type_key(parent_key, node)
    if type(node) ~= "table" then
        return nil
    end
    local from_fields = node.type or node.Type or node.typeName or node.TypeName or node.Name
    if type(from_fields) == "string" and from_fields ~= "" then
        return from_fields
    end
    if type(parent_key) == "string" and parent_key ~= "" then
        return parent_key
    end
    return nil
end

ws_for_clsid = function(descriptor_map, clsid)
    if type(clsid) ~= "string" or clsid == "" then
        return nil
    end
    return descriptor_map[clsid] or descriptor_map["{" .. clsid .. "}"]
end

ws_key = function(ws)
    return table.concat(ws, ",")
end

add_alias = function(map, alias, ws, stats)
    if type(alias) ~= "string" then
        return
    end
    local k = alias:gsub("^%s+", ""):gsub("%s+$", "")
    if k == "" then
        return
    end
    local prev = map[k]
    if prev then
        if prev[1] == ws[1] and prev[2] == ws[2] and prev[3] == ws[3] and prev[4] == ws[4] then
            stats.alias_same = (stats.alias_same or 0) + 1
            return
        end
        stats.alias_collisions = (stats.alias_collisions or 0) + 1
        return
    end
    map[k] = ws
    stats.alias_added = (stats.alias_added or 0) + 1
end

collect_entry_aliases = function(entry, key_s, path, out)
    out[key_s] = true
    out[path] = true
    if type(key_s) == "string" and key_s:match("^%b{}$") then
        out[key_s:sub(2, #key_s - 1)] = true
    end
    if type(entry) ~= "table" then
        return
    end
    local function add_if_str(v)
        if type(v) == "string" and v ~= "" then
            out[v] = true
        end
    end
    local desc = type(entry.desc) == "table" and entry.desc or nil
    add_if_str(entry.CLSID)
    add_if_str(entry.clsid)
    add_if_str(entry.Name)
    add_if_str(entry.name)
    add_if_str(entry.DisplayName)
    add_if_str(entry.displayName)
    add_if_str(entry.user_name)
    add_if_str(entry.type)
    add_if_str(entry.Type)
    add_if_str(entry.typeName)
    add_if_str(entry.TypeName)
    add_if_str(entry.wsTypeAsString)
    if desc then
        add_if_str(desc.CLSID)
        add_if_str(desc.clsid)
        add_if_str(desc.Name)
        add_if_str(desc.name)
        add_if_str(desc.DisplayName)
        add_if_str(desc.displayName)
        add_if_str(desc.user_name)
        add_if_str(desc.typeName)
        add_if_str(desc.TypeName)
    end
end

local function collect_fueltank_by_aircraft(db, descriptor_map)
    local out = {}
    local seen_nodes = {}
    local function walk(node)
        if type(node) ~= "table" then
            return
        end
        if seen_nodes[node] then
            return
        end
        seen_nodes[node] = true
        for k, v in pairs(node) do
            if type(v) == "table" then
                if type(v.Pylons) == "table" then
                    local aircraft = aircraft_type_key(k, v)
                    if aircraft then
                        local clsids = {}
                        collect_clsids_from_table(v.Pylons, clsids, {})
                        local wsset = {}
                        for clsid in pairs(clsids) do
                            local ws = ws_for_clsid(descriptor_map, clsid)
                            if ws and ws[1] == 1 and ws[2] == 3 then
                                local ws_key = table.concat(ws, ",")
                                wsset[ws_key] = ws
                            end
                        end
                        if next(wsset) ~= nil then
                            local arr = out[aircraft] or {}
                            local merged = {}
                            for _, ws in ipairs(arr) do
                                merged[table.concat(ws, ",")] = ws
                            end
                            for _, ws in pairs(wsset) do
                                merged[table.concat(ws, ",")] = ws
                            end
                            arr = {}
                            for _, ws in pairs(merged) do
                                arr[#arr + 1] = ws
                            end
                            table.sort(arr, function(a, b)
                                if a[1] ~= b[1] then return a[1] < b[1] end
                                if a[2] ~= b[2] then return a[2] < b[2] end
                                if a[3] ~= b[3] then return a[3] < b[3] end
                                return a[4] < b[4]
                            end)
                            out[aircraft] = arr
                        end
                    end
                end
                walk(v)
            end
        end
    end
    walk(db)
    return out
end

local function collect_weapon_ws_by_aircraft(db, descriptor_map)
    local out = {}
    local seen_nodes = {}
    local function walk(node)
        if type(node) ~= "table" then
            return
        end
        if seen_nodes[node] then
            return
        end
        seen_nodes[node] = true
        for k, v in pairs(node) do
            if type(v) == "table" then
                if type(v.Pylons) == "table" then
                    local aircraft = aircraft_type_key(k, v)
                    if aircraft then
                        local wsset = {}
                        collect_ws_from_table(v.Pylons, descriptor_map, wsset, {})
                        if next(wsset) ~= nil then
                            local arr = out[aircraft] or {}
                            local merged = {}
                            for _, ws in ipairs(arr) do
                                merged[ws_key(ws)] = ws
                            end
                            for _, ws in pairs(wsset) do
                                merged[ws_key(ws)] = ws
                            end
                            arr = {}
                            for _, ws in pairs(merged) do
                                arr[#arr + 1] = ws
                            end
                            table.sort(arr, function(a, b)
                                if a[1] ~= b[1] then return a[1] < b[1] end
                                if a[2] ~= b[2] then return a[2] < b[2] end
                                if a[3] ~= b[3] then return a[3] < b[3] end
                                return a[4] < b[4]
                            end)
                            out[aircraft] = arr
                        end
                    end
                end
                walk(v)
            end
        end
    end
    walk(db)
    return out
end

local function collect_aircraft_by_ws(weapon_ws_by_aircraft)
    local by_ws = {}
    for aircraft, rows in pairs(weapon_ws_by_aircraft) do
        for _, ws in ipairs(rows) do
            local key = ws_key(ws)
            local s = by_ws[key]
            if not s then
                s = {}
                by_ws[key] = s
            end
            s[aircraft] = true
        end
    end
    local out = {}
    for key, set in pairs(by_ws) do
        local arr = {}
        for aircraft in pairs(set) do
            arr[#arr + 1] = aircraft
        end
        table.sort(arr)
        out[key] = arr
    end
    return out
end

local function write_json(path, map, all_fueltank_ws, fueltank_by_aircraft, weapon_ws_by_aircraft, aircraft_by_ws, dcs_ver)
    local keys = {}
    for k in pairs(map) do
        keys[#keys + 1] = k
    end
    table.sort(keys)
    local f, err = io.open(path, "w")
    if not f then
        log_line("ERROR: cannot open output " .. tostring(path) .. " " .. tostring(err))
        return false
    end
    f:write('{\n  "schema_version": 2,\n')
    f:write('  "dcs_version": "' .. json_escape(dcs_ver) .. '",\n')
    f:write('  "by_descriptor": {\n')
    for i, k in ipairs(keys) do
        local w = map[k]
        local comma = (i < #keys) and "," or ""
        f:write(string.format(
            '    "%s": [%d, %d, %d, %d]%s\n',
            json_escape(k),
            w[1],
            w[2],
            w[3],
            w[4],
            comma
        ))
    end
    f:write("  },\n")
    f:write('  "fueltank_all_ws": [')
    for i, ws in ipairs(all_fueltank_ws) do
        local comma = (i < #all_fueltank_ws) and ", " or ""
        f:write(string.format("[%d, %d, %d, %d]%s", ws[1], ws[2], ws[3], ws[4], comma))
    end
    f:write("],\n")
    f:write('  "fueltank_by_aircraft": {\n')
    local aircraft_keys = {}
    for k in pairs(fueltank_by_aircraft) do
        aircraft_keys[#aircraft_keys + 1] = k
    end
    table.sort(aircraft_keys)
    for i, aircraft in ipairs(aircraft_keys) do
        local rows = fueltank_by_aircraft[aircraft]
        f:write('    "' .. json_escape(aircraft) .. '": [')
        for j, ws in ipairs(rows) do
            local sep = (j < #rows) and ", " or ""
            f:write(string.format("[%d, %d, %d, %d]%s", ws[1], ws[2], ws[3], ws[4], sep))
        end
        local comma = (i < #aircraft_keys) and "," or ""
        f:write("]" .. comma .. "\n")
    end
    f:write("  },\n")
    f:write('  "weapon_ws_by_aircraft": {\n')
    local weapon_aircraft_keys = {}
    for k in pairs(weapon_ws_by_aircraft) do
        weapon_aircraft_keys[#weapon_aircraft_keys + 1] = k
    end
    table.sort(weapon_aircraft_keys)
    for i, aircraft in ipairs(weapon_aircraft_keys) do
        local rows = weapon_ws_by_aircraft[aircraft]
        f:write('    "' .. json_escape(aircraft) .. '": [')
        for j, ws in ipairs(rows) do
            local sep = (j < #rows) and ", " or ""
            f:write(string.format("[%d, %d, %d, %d]%s", ws[1], ws[2], ws[3], ws[4], sep))
        end
        local comma = (i < #weapon_aircraft_keys) and "," or ""
        f:write("]" .. comma .. "\n")
    end
    f:write("  },\n")
    f:write('  "aircraft_by_ws": {\n')
    local ws_keys = {}
    for k in pairs(aircraft_by_ws) do
        ws_keys[#ws_keys + 1] = k
    end
    table.sort(ws_keys)
    for i, k in ipairs(ws_keys) do
        local aircrafts = aircraft_by_ws[k]
        f:write('    "' .. json_escape(k) .. '": [')
        for j, aircraft in ipairs(aircrafts) do
            local sep = (j < #aircrafts) and ", " or ""
            f:write('"' .. json_escape(aircraft) .. '"' .. sep)
        end
        local comma = (i < #ws_keys) and "," or ""
        f:write("]" .. comma .. "\n")
    end
    f:write("  }\n")
    f:write("}\n")
    f:close()
    return true
end

local done = false
local handler = {}

function handler.onSimulationStart()
    if done then
        return
    end
    done = true
    notify_user("Fowl bridge export: started")
    log_line("Fowl_engine_weapon_bridge_export: onSimulationStart")
    if type(DCS) == "table" then
        local okVer, rawVer = pcall(DCS.getVersion)
        local okBuild, rawBuild = pcall(DCS.getBuildNumber)
        if okVer then
            if type(rawVer) == "table" then
                local vv = rawVer.version or rawVer.Version or rawVer.productVersion or rawVer.ProductVersion or rawVer[1]
                log_line("DCS.getVersion() table value: " .. tostring(vv))
            else
                log_line("DCS.getVersion(): " .. tostring(rawVer))
            end
        else
            log_line("DCS.getVersion() unavailable")
        end
        if okBuild then
            log_line("DCS.getBuildNumber(): " .. tostring(rawBuild))
        else
            log_line("DCS.getBuildNumber() unavailable")
        end
    end
    local ver = dcs_version_string()
    log_line("DCS version string: " .. ver)
    local OUTPUT_NAME = output_json_filename()
    local map, stats = build_map()
    local all_fueltank_ws = collect_all_fueltank_ws(map)
    local fueltank_by_aircraft = collect_fueltank_by_aircraft(_G.db, map)
    local weapon_ws_by_aircraft = collect_weapon_ws_by_aircraft(_G.db, map)
    local aircraft_by_ws = collect_aircraft_by_ws(weapon_ws_by_aircraft)
    local n = 0
    local n_aircraft = 0
    local n_weapon_aircraft = 0
    local n_ws_backrefs = 0
    for _ in pairs(fueltank_by_aircraft) do
        n_aircraft = n_aircraft + 1
    end
    for _ in pairs(weapon_ws_by_aircraft) do
        n_weapon_aircraft = n_weapon_aircraft + 1
    end
    for _ in pairs(aircraft_by_ws) do
        n_ws_backrefs = n_ws_backrefs + 1
    end
    for _ in pairs(map) do
        n = n + 1
    end
    log_line(
        "wsType diagnostics: total_ws_nodes=" .. tostring(stats.total_ws)
            .. " fuel_1_3_nodes=" .. tostring(stats.fuel_ws)
            .. " ws_4_5_nodes=" .. tostring(stats.ws_4_5)
            .. " category5_without_ws=" .. tostring(stats.category5_no_ws)
            .. " alias_added=" .. tostring(stats.alias_added or 0)
            .. " alias_same=" .. tostring(stats.alias_same or 0)
            .. " alias_collisions=" .. tostring(stats.alias_collisions or 0)
    )
    for src, nsrc in pairs(stats.ws_source_counts) do
        log_line("wsType source count: " .. tostring(src) .. "=" .. tostring(nsrc))
    end
    if stats.fuel_ws == 0 then
        notify_user("Fowl bridge export: no (1,3,*,*) in runtime db")
    else
        notify_user("Fowl bridge export: found (1,3,*,*) count=" .. tostring(stats.fuel_ws))
    end
    for i, ex in ipairs(stats.fuel_examples) do
        log_line("fuel wsType example " .. tostring(i) .. ": " .. ex)
    end
    for i, ex in ipairs(stats.category5_examples) do
        log_line("category5 without ws example " .. tostring(i) .. ": " .. ex)
    end
    log_line(
        "collected entries: " .. tostring(n)
            .. " universal_fueltank_ws=" .. tostring(#all_fueltank_ws)
            .. " aircraft_fueltank_maps=" .. tostring(n_aircraft)
            .. " aircraft_weapon_maps=" .. tostring(n_weapon_aircraft)
            .. " ws_aircraft_backrefs=" .. tostring(n_ws_backrefs)
    )
    local out_path
    if OUTPUT_DIR and OUTPUT_DIR ~= "" then
        local dir = OUTPUT_DIR:gsub("\\", "/"):gsub("/+$", "")
        out_path = (dir .. "/" .. OUTPUT_NAME):gsub("/", "\\")
    else
        out_path = (lfs.writedir() .. "Logs/" .. OUTPUT_NAME):gsub("/", "\\")
        log_line("OUTPUT_DIR empty: writing to Logs; set OUTPUT_DIR to copy JSON next to weapon.miz")
    end
    if write_json(out_path, map, all_fueltank_ws, fueltank_by_aircraft, weapon_ws_by_aircraft, aircraft_by_ws, ver) then
        log_line("wrote " .. out_path)
        notify_user("Fowl bridge export: done (entries=" .. tostring(n) .. ")")
    else
        notify_user("Fowl bridge export: failed (check fowl_weapon_bridge_export.log)")
    end
end

DCS.setUserCallbacks(handler)
