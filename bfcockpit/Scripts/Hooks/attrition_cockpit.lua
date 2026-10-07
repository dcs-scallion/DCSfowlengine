-- Attrition Control — Fowl Engine 2.0 in-game cockpit overlay (DCS Hooks).
--
-- Copyright (c) 2026 Robo76. All rights reserved.
--
-- PROPRIETARY SOFTWARE — NOT OPEN SOURCE
--
-- Author: Robo76 (individual author; not a company).
--
-- This software may be used only by the operator of the DCS server on which
-- it is deployed, unless Robo76 grants written permission otherwise.
--
-- Contact for licensing: Discord private message to Robo76.
--
-- Independent Attrition rewrite. Not derived from a licensed Vector Strike
-- cockpit. Companion UI: bfweb/LICENSE.cockpit. See also bfcockpit/LICENSE.
--
-- Install: Saved Games\DCS\Scripts\Hooks\attrition_cockpit.lua
-- Config:  Saved Games\DCS\Config\AttritionCockpit.lua (written on first run)
--
-- DCS dxgui Window/WebViewWidget have no LuaLS stubs; calls are guarded with pcall.
---@diagnostic disable: undefined-field, need-check-nil

local ATTRITION_COCKPIT_VERSION = "1.0.32"

local net = require('net')

local function logmsg(msg)
    net.log("ATTRITION_COCKPIT: " .. tostring(msg))
end

logmsg("loading version " .. ATTRITION_COCKPIT_VERSION)

local function try_require(name)
    local ok, mod = pcall(require, name)
    if not ok then
        logmsg("FATAL: require('" .. name .. "') failed: " .. tostring(mod))
        return nil
    end
    return mod
end

local dxgui = try_require('dxgui')
local Window = try_require('Window')
local WebViewWidget = try_require('WebViewWidget')

if not (dxgui and Window and WebViewWidget) then
    logmsg("FATAL: dxgui modules missing, overlay disabled")
    return
end

local CONFIG_PATH = lfs.writedir() .. "Config\\AttritionCockpit.lua"

local DEFAULTS = {
    -- Public dashboard origin that serves /cockpit (Caddy + bfweb). Not bfdb :8880.
    url = "https://stats.attrition.cz/cockpit",
    opacity = 0.75,
    open_on_radio_menu = true,
    open_on_start = true,
    -- Overlay only on Attrition DCS hosts. Match net.get_server_host() against
    -- allowed_hosts (IP / host / host:port, or suffix like ".attrition.cz"),
    -- and/or server name from net.get_server_settings() against substrings
    -- (LAN joins often use 192.168.x.x instead of the public IP).
    allowed_hosts = {
        "78.80.144.178",
        ".attrition.cz",
    },
    allowed_name_substrings = {
        "attrition",
    },
    hotkeys = {
        toggle = "Ctrl+Shift+J",
        hide = "Escape",
        opacity_up = "Ctrl+Shift+O",
        opacity_down = "Ctrl+Shift+L",
        click_through = "Ctrl+Shift+T",
    },
    vr = "auto",
    scale = nil,
    window = { x = nil, y = nil, w = nil, h = nil },
    -- Resize is mouse-only (corner drag). Sleep minimizes to the title bar.
    debug = false,
}

-- Below this, CEF/WebView tends to corrupt layout / crash DCS.
local MIN_W = 420
local MIN_H = 280
local MAX_W = 2400
local MAX_H = 1600
-- Ignore size callbacks while we call setBounds ourselves.
local programmatic_resize = false
-- Last awake size (survives sleep shrink).
local normal_w, normal_h
-- Last WebView client size (skip redundant setBounds — CEF crashes if spammed).
local last_wv_w, last_wv_h
-- Title-bar-only "sleep": hide WebView + shrink chrome; keep CEF page alive.
-- (Unload/0×0/black-page on sleep corrupted CEF after wake — dead clicks/scroll.)
local TITLE_H = 20
local slept = false

local cfg = {}

local function deep_copy(t)
    if type(t) ~= 'table' then return t end
    local out = {}
    for k, v in pairs(t) do out[k] = deep_copy(v) end
    return out
end

local function apply_defaults(into, defaults)
    for k, v in pairs(defaults) do
        if type(v) == 'table' then
            if type(into[k]) ~= 'table' then into[k] = {} end
            apply_defaults(into[k], v)
        elseif into[k] == nil then
            into[k] = v
        end
    end
end

local function serialize(value, indent)
    indent = indent or ""
    local t = type(value)
    if t == 'string' then
        return string.format("%q", value)
    elseif t == 'number' or t == 'boolean' then
        return tostring(value)
    elseif t == 'table' then
        local inner = indent .. "    "
        local keys = {}
        for k in pairs(value) do keys[#keys + 1] = k end
        table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
        local parts = { "{\n" }
        for _, k in ipairs(keys) do
            local key
            if type(k) == "number" and k == math.floor(k) then
                key = "[" .. tostring(k) .. "]"
            else
                key = "[" .. string.format("%q", tostring(k)) .. "]"
            end
            parts[#parts + 1] = inner .. key .. " = "
                .. serialize(value[k], inner) .. ",\n"
        end
        parts[#parts + 1] = indent .. "}"
        return table.concat(parts)
    end
    return "nil"
end

-- Config saves used to quote integer keys as ["1"], which breaks ipairs.
local function string_list(t)
    local out = {}
    if type(t) ~= "table" then return out end
    for _, v in pairs(t) do
        if type(v) == "string" and v ~= "" then
            out[#out + 1] = v
        end
    end
    table.sort(out)
    return out
end

local function load_config()
    cfg = deep_copy(DEFAULTS)
    local chunk, err = loadfile(CONFIG_PATH)
    if not chunk then
        local msg = tostring(err)
        if not (string.find(msg, "no file", 1, true)
                or string.find(msg, "No such file", 1, true)
                or string.find(msg, "cannot open", 1, true)) then
            logmsg("config load failed, using defaults: " .. msg)
        end
        return false
    end
    local env = {}
    setfenv(chunk, env)
    local ok, run_err = pcall(chunk)
    if not ok then
        logmsg("config errored, using defaults: " .. tostring(run_err))
        return false
    end
    local user = env.cockpit or env.AttritionCockpit or env.cfg
    if type(user) ~= 'table' then
        logmsg("config has no cockpit table, using defaults")
        return false
    end
    for k, v in pairs(user) do cfg[k] = deep_copy(v) end
    apply_defaults(cfg, DEFAULTS)
    cfg.allowed_hosts = string_list(cfg.allowed_hosts)
    cfg.allowed_name_substrings = string_list(cfg.allowed_name_substrings)
    if #cfg.allowed_hosts == 0 then
        cfg.allowed_hosts = deep_copy(DEFAULTS.allowed_hosts)
    end
    if #cfg.allowed_name_substrings == 0 then
        cfg.allowed_name_substrings = deep_copy(DEFAULTS.allowed_name_substrings)
    end
    logmsg("config loaded from " .. CONFIG_PATH)
    return true
end

local function save_config()
    local ok, err = pcall(function()
        local f, open_err = io.open(CONFIG_PATH, "w")
        if not f then error(tostring(open_err)) end
        f:write("-- Attrition cockpit overlay settings.\n")
        f:write("-- Safe to edit. Window geometry/opacity update automatically.\n\n")
        -- PascalCase root avoids LuaLS "lowercase-global" on this file.
        f:write("AttritionCockpit = " .. serialize(cfg) .. "\n")
        f:close()
    end)
    if not ok then logmsg("could not write config: " .. tostring(err)) end
end

load_config()
-- Legacy compact mode removed — sleep is the only minimize path.
cfg.compacted = nil
cfg.compact_size = nil
-- Reload is toolbar-only (no Ctrl+Alt+R).
if type(cfg.hotkeys) == "table" then
    cfg.hotkeys.reload = nil
end

local function detect_vr()
    if cfg.vr == "on" then return true end
    if cfg.vr == "off" then return false end
    local chunk = loadfile(lfs.writedir() .. "Config\\options.lua")
    if not chunk then return false end
    local env = {}
    setfenv(chunk, env)
    if not pcall(chunk) then return false end
    local vr = env.options and env.options.VR
    if type(vr) ~= 'table' then return false end
    return vr.enable == true or vr.enabled == true
end

local IS_VR = detect_vr()
local UI_SCALE = tonumber(cfg.scale) or (IS_VR and 1.35 or 1.0)
logmsg("VR " .. (IS_VR and "on" or "off") .. ", scale " .. tostring(UI_SCALE))

local function urlencode(s)
    return (string.gsub(tostring(s or ""), "([^%w%-%.%_%~])", function(c)
        return string.format("%%%02X", string.byte(c))
    end))
end

local window = nil
local webview = nil
-- Always-visible 1×1 sink: Window hotkeys die when the main overlay is hidden.
local hotkey_sink = nil
local sink_hotkeys_bound = false
local hide_hotkey_bound = false
local visible = false
local click_through = false
local dirty_geom = false
-- True after CEF browser exists (cefLoadUrl before this is a no-op).
local browser_ready = false
-- True after a load was issued against a ready browser (or page finished).
local page_alive = false
local page_loading = false
-- show() before browserCreated — load as soon as the browser exists.
local pending_load = false
local load_webview_url -- forward decl (hotkeys / show)
local toggle_sleep
local enter_sleep
local wake_from_sleep
local apply_webview_bounds
local show -- forward (toggle from sink before create_window)

local function current_server_host()
    local ok, host = pcall(function()
        return net.get_server_host()
    end)
    if ok and host ~= nil and tostring(host) ~= "" then
        return tostring(host)
    end
    return nil
end

local function current_server_name()
    local ok, settings = pcall(function()
        return net.get_server_settings()
    end)
    if ok and type(settings) == "table" and settings.name ~= nil then
        return tostring(settings.name)
    end
    return nil
end

local function build_url()
    local base = tostring(cfg.url or DEFAULTS.url)
    local sep = string.find(base, "?", 1, true) and "&" or "?"
    local pid = 0
    local ok, id = pcall(net.get_my_player_id)
    if ok and type(id) == 'number' then pid = id end
    local o = tonumber(cfg.opacity) or DEFAULTS.opacity
    if o < 0.15 then o = 0.15 end
    if o > 1 then o = 1 end
    local toggle = ((cfg.hotkeys or {}).toggle)
        or ((DEFAULTS.hotkeys or {}).toggle)
        or "Ctrl+Shift+J"
    -- bfdb routes cockpit RPC via instances.json dcs_server_name (?server=).
    local server = current_server_name()
    local server_q = ""
    if server and server ~= "" then
        server_q = "&server=" .. urlencode(server)
    end
    return base
        .. sep .. "playerid=" .. tostring(pid)
        .. server_q
        .. "&plugin=" .. urlencode(ATTRITION_COCKPIT_VERSION)
        .. (IS_VR and "&vr=1" or "")
        .. "&scale=" .. tostring(UI_SCALE)
        -- Widget setOpacity blanks CEF; page applies this via CSS instead.
        .. "&opacity=" .. string.format("%.2f", o)
        -- Footer shows the live toggle binding from AttritionCockpit.lua.
        .. "&toggle=" .. urlencode(tostring(toggle))
        -- CEF caches /cockpit HTML hard; change query so each load gets fresh assets.
        .. "&_cb=" .. tostring(os.time())
end

local function host_is_allowed(host)
    if not host then return false end
    local host_l = string.lower(host)
    local ip = string.match(host, "^(%d+%.%d+%.%d+%.%d+)") or string.match(host, "^([^:]+)")
    ip = ip and string.lower(ip) or host_l
    local list = cfg.allowed_hosts
    if type(list) ~= "table" then list = DEFAULTS.allowed_hosts end
    for _, entry in ipairs(list) do
        local e = string.lower(tostring(entry or ""))
        if e ~= "" then
            if string.sub(e, 1, 1) == "." then
                if string.sub(host_l, -#e) == e or string.sub(ip, -#e) == e then
                    return true
                end
            elseif host_l == e or ip == e then
                return true
            elseif string.find(host_l, e .. ":", 1, true) == 1 then
                return true
            end
        end
    end
    return false
end

local function name_is_allowed(name)
    if not name or name == "" then return false end
    local nl = string.lower(name)
    local list = cfg.allowed_name_substrings
    if type(list) ~= "table" then list = DEFAULTS.allowed_name_substrings end
    for _, sub in ipairs(list) do
        local s = string.lower(tostring(sub or ""))
        if s ~= "" and string.find(nl, s, 1, true) then
            return true
        end
    end
    return false
end

-- True only on Attrition (public IP / hostname, or server name containing "Attrition").
local function is_attrition_server()
    local host = current_server_host()
    local name = current_server_name()
    if host_is_allowed(host) then return true end
    if name_is_allowed(name) then return true end
    return false
end

local function set_window_size_limits(min_w, min_h)
    if not window then return end
    pcall(function()
        local skin = window:getSkin()
        local params = skin and skin.skinData and skin.skinData.params
        if not params then return end
        params.minSize = { horz = min_w, vert = min_h }
        params.maxSize = { horz = MAX_W, vert = MAX_H }
        window:setSkin(skin)
    end)
end

local function apply_window_skin()
    if not window then return end
    -- Transparent body (CSS page alpha shows the map) + dxgui minSize so
    -- resize cannot go below CEF-safe dims. Live Lua clamp+setBounds fights
    -- the drag every frame and corrupts/crashes WebView.
    pcall(function()
        local skin = window:getSkin()
        if not skin or not skin.skinData then return end
        local released = skin.skinData.states and skin.skinData.states.released
        local bkg = released and released[1] and released[1].bkg
        if bkg then
            bkg.center_center = "0x00000000"
        end
        local params = skin.skinData.params
        if params then
            params.minSize = { horz = MIN_W, vert = MIN_H }
            params.maxSize = { horz = MAX_W, vert = MAX_H }
        end
        window:setSkin(skin)
    end)
end

local function apply_opacity()
    if not window then return end
    local o = tonumber(cfg.opacity) or 0.92
    if o < 0.15 then o = 0.15 end
    if o > 1 then o = 1 end
    cfg.opacity = o
    -- dxgui WidgetSetOpacity on a Window that hosts CEF blanks the page
    -- (edQuery/page-loaded still work; only the texture is empty). Keep
    -- widgets at 1; transparency is &opacity= on the /cockpit URL + CSS.
    pcall(function() window:setOpacity(1) end)
    if webview then
        pcall(function() webview:setOpacity(1) end)
    end
    logmsg("opacity=" .. tostring(o) .. " (CSS via URL; widgets=1)")
end

local function clamp_dim(w, h)
    w = math.floor(tonumber(w) or MIN_W)
    h = math.floor(tonumber(h) or MIN_H)
    if w < MIN_W then w = MIN_W end
    if h < MIN_H then h = MIN_H end
    if w > MAX_W then w = MAX_W end
    if h > MAX_H then h = MAX_H end
    return w, h
end

local function remember_geom()
    if not window or slept then return end
    local ok, x, y, w, h = pcall(function()
        local bx, by = window:getPosition()
        local bw, bh = window:getSize()
        return bx, by, bw, bh
    end)
    if ok and w and h then
        w, h = clamp_dim(w, h)
        cfg.window = { x = x, y = y, w = w, h = h }
        dirty_geom = true
    end
end

local function flush_geom()
    if dirty_geom then
        save_config()
        dirty_geom = false
    end
end

local function ensure_hotkey_sink()
    if hotkey_sink or not Window then return end
    hotkey_sink = Window.new()
    pcall(function() hotkey_sink:setText("") end)
    pcall(function() hotkey_sink:setBounds(-200, -200, 1, 1) end)
    pcall(function() hotkey_sink:setTransparentForUserInput(true) end)
    pcall(function() hotkey_sink:setVisible(true) end)
end

local function on_toggle_hotkey()
    if slept then
        wake_from_sleep()
    elseif visible then
        hide()
    else
        show()
    end
end

local function bind_hide_hotkey_on_main()
    if not window or hide_hotkey_bound then return end
    local hide_key = (cfg.hotkeys or {}).hide
    if type(hide_key) ~= "string" or hide_key == "" then return end
    -- Escape only while the overlay is up — do not steal Esc from DCS when hidden.
    local ok = pcall(function()
        window:addHotKeyCallback(hide_key, function() hide() end)
    end)
    if ok then hide_hotkey_bound = true end
end

local function register_hotkeys()
    ensure_hotkey_sink()
    if not sink_hotkeys_bound and hotkey_sink then
        sink_hotkeys_bound = true
        local hk = cfg.hotkeys or {}
        local map = {
            { hk.toggle, on_toggle_hotkey },
            { hk.opacity_up, function()
                cfg.opacity = math.min(1, (tonumber(cfg.opacity) or 0.92) + 0.05)
                apply_opacity(); save_config()
                if visible and not slept then load_webview_url() end
            end },
            { hk.opacity_down, function()
                cfg.opacity = math.max(0.15, (tonumber(cfg.opacity) or 0.92) - 0.05)
                apply_opacity(); save_config()
                if visible and not slept then load_webview_url() end
            end },
            { hk.click_through, function()
                click_through = not click_through
                pcall(function()
                    if window and window.setTransparentForUserInput then
                        window:setTransparentForUserInput(click_through)
                    elseif window and window.setTransparentForMouse then
                        window:setTransparentForMouse(click_through)
                    end
                end)
                logmsg("click_through=" .. tostring(click_through))
            end },
        }
        for _, item in ipairs(map) do
            if type(item[1]) == "string" and item[1] ~= "" then
                pcall(function()
                    hotkey_sink:addHotKeyCallback(item[1], item[2])
                end)
            end
        end
        logmsg("hotkey sink armed (toggle survives hide)")
    end
    bind_hide_hotkey_on_main()
end

-- DCS CEF API (dxgui/bind/WebViewWidget.lua): cefLoadUrl, not setUrl.
-- Must run only after browserCreated — earlier calls appear to succeed but load nothing.
load_webview_url = function()
    if not webview then return end
    if not browser_ready then
        pending_load = true
        logmsg("cefLoadUrl deferred (browser not ready)")
        return
    end
    pending_load = false
    page_loading = true
    local url = build_url()
    local ok, err = pcall(function()
        webview:cefLoadUrl(url)
    end)
    if ok then
        page_alive = true
        logmsg("cefLoadUrl " .. url)
    else
        page_loading = false
        page_alive = false
        logmsg("cefLoadUrl failed: " .. tostring(err))
    end
end

local function reveal_webview()
    if not webview or slept then return end
    pcall(function() webview:setVisible(true) end)
    apply_webview_bounds()
end

apply_webview_bounds = function()
    if not (window and webview) or slept then return end
    -- Client rect = area below the title; outer getSize stretches CEF and
    -- desyncs hit-testing (clicks miss, scroll dies).
    local w, h
    local ok = pcall(function()
        w, h = window:getClientRectSize()
    end)
    if not (ok and w and h and w > 1 and h > 1) then
        ok = pcall(function()
            w, h = window:getSize()
        end)
    end
    if not (ok and w and h and w > 1 and h > 1) then return end
    w, h = math.floor(w), math.floor(h)
    if w == last_wv_w and h == last_wv_h then return end
    last_wv_w, last_wv_h = w, h
    pcall(function() webview:setBounds(0, 0, w, h) end)
end

local function window_xywh()
    local ok, x, y, w, h = pcall(function()
        return window:getBounds()
    end)
    if ok and w then return x, y, w, h end
    local x2, y2 = 0, 0
    pcall(function() x2, y2 = window:getPosition() end)
    local w2, h2 = 400, 600
    pcall(function() w2, h2 = window:getSize() end)
    return x2, y2, w2, h2
end

local function default_normal_size()
    local sw, sh = 1920, 1080
    pcall(function()
        local w, h = dxgui.GetScreenSize()
        if w and h then sw, sh = w, h end
    end)
    return clamp_dim(math.floor(sw * (IS_VR and 0.55 or 0.32)), math.floor(sh * (IS_VR and 0.70 or 0.62)))
end

local function sleep_title_height()
    local h = TITLE_H
    if not window then return h end
    pcall(function()
        local skin = window:getSkin()
        local hh = skin and skin.skinData and skin.skinData.params and skin.skinData.params.headerHeight
        if type(hh) == "number" and hh >= 16 and hh <= 40 then
            h = math.floor(hh)
        end
    end)
    return h
end

enter_sleep = function()
    if not window or slept then return end
    local x, y, w, h = window_xywh()
    w, h = clamp_dim(w, h)
    normal_w, normal_h = w, h
    cfg.window = { x = x, y = y, w = w, h = h }
    local aw = normal_w or w
    if aw < MIN_W then aw = MIN_W end
    slept = true
    -- Hide only — keep CEF size + document. Black-page/0×0 broke wake.
    if webview then
        pcall(function() webview:setVisible(false) end)
    end
    local th = sleep_title_height()
    set_window_size_limits(MIN_W, th)
    programmatic_resize = true
    pcall(function() window:setResizable(false) end)
    pcall(function() window:setBounds(x, y, aw, th) end)
    programmatic_resize = false
    logmsg("sleep on (title bar only, h=" .. tostring(th) .. ", page kept)")
    dirty_geom = true
    flush_geom()
end

wake_from_sleep = function()
    if not window or not slept then return end
    slept = false
    local x, y = window_xywh()
    local nw = normal_w or (cfg.window and cfg.window.w)
    local nh = normal_h or (cfg.window and cfg.window.h)
    if not nw or not nh then
        nw, nh = default_normal_size()
    else
        nw, nh = clamp_dim(nw, nh)
    end
    set_window_size_limits(MIN_W, MIN_H)
    programmatic_resize = true
    pcall(function() window:setBounds(x, y, nw, nh) end)
    pcall(function() window:setResizable(true) end)
    last_wv_w, last_wv_h = nil, nil
    if webview then
        apply_webview_bounds()
        reveal_webview()
        -- Reload only if the document was lost; normal sleep keeps it.
        if not page_alive and not page_loading then
            load_webview_url()
        end
    end
    programmatic_resize = false
    logmsg("sleep off -> " .. tostring(nw) .. "x" .. tostring(nh)
        .. " page_alive=" .. tostring(page_alive))
    dirty_geom = true
    flush_geom()
end

toggle_sleep = function()
    if slept then wake_from_sleep() else enter_sleep() end
end

local function handle_cef_query(queryId, jsonRequest)
    local req = tostring(jsonRequest or "")
    logmsg("edQuery: " .. req:sub(1, 120))
    if string.find(req, "toggle_sleep", 1, true) then
        pcall(function() webview:edQuerySuccess(queryId, '{"ok":true,"slept":true}') end)
        enter_sleep()
        return
    end
    if string.find(req, "get_sleep", 1, true) then
        pcall(function() webview:edQuerySuccess(queryId, '{"ok":true,"slept":' .. tostring(slept) .. '}') end)
        return
    end
    if string.find(req, "get_hotkeys", 1, true) then
        local toggle = ((cfg.hotkeys or {}).toggle)
            or ((DEFAULTS.hotkeys or {}).toggle)
            or "Ctrl+Shift+J"
        local payload = '{"ok":true,"toggle":"' .. tostring(toggle):gsub('\\', '\\\\'):gsub('"', '\\"') .. '"}'
        pcall(function() webview:edQuerySuccess(queryId, payload) end)
        return
    end
    if string.find(req, '"method":"reload"', 1, true) then
        pcall(function() webview:edQuerySuccess(queryId, '{"ok":true}') end)
        load_webview_url()
        return
    end
    pcall(function() webview:edQueryFailure(queryId, -1, "unknown method") end)
end

local function create_window()
    if window then return end
    local sw, sh = 1920, 1080
    pcall(function()
        local w, h = dxgui.GetScreenSize()
        if w and h then sw, sh = w, h end
    end)

    local ww = (cfg.window and cfg.window.w) or math.floor(sw * (IS_VR and 0.55 or 0.32))
    local wh = (cfg.window and cfg.window.h) or math.floor(sh * (IS_VR and 0.70 or 0.62))
    ww, wh = clamp_dim(ww, wh)
    normal_w, normal_h = ww, wh
    local wx = (cfg.window and cfg.window.x) or math.floor((sw - ww) / 2)
    local wy = (cfg.window and cfg.window.y) or math.floor((sh - wh) / 2)
    -- Drop legacy compact keys if still present in AttritionCockpit.lua.
    cfg.compacted = nil
    cfg.compact_size = nil

    window = Window.new()
    -- Leading spaces: default caption skin is left-aligned and sits tight to the edge.
    window:setText("  Attrition Control")
    window:setBounds(wx, wy, ww, wh)
    pcall(function() window:setDraggable(true) end)
    pcall(function() window:setResizable(true) end)
    apply_window_skin()
    pcall(function()
        window:addCloseCallback(function()
            visible = false
            if slept then
                -- Keep slept; next show() will wake.
            end
            flush_geom()
        end)
    end)
    -- Variant A: double-click title / window → sleep (title bar only) or wake.
    pcall(function()
        window:addMouseDoubleDownCallback(function()
            toggle_sleep()
        end)
    end)
    apply_opacity()

    webview = WebViewWidget.new()
    last_wv_w, last_wv_h = nil, nil
    window:insertWidget(webview)
    apply_webview_bounds()

    pcall(function()
        webview:onMountFailed(function()
            logmsg("FATAL: WebView mount failed")
        end)
    end)
    pcall(function()
        webview:onPageLoaded(function()
            page_loading = false
            page_alive = true
            logmsg("page loaded")
            if visible and not slept then
                reveal_webview()
            end
        end)
    end)
    -- DCS CEF bridge is window.edQuery in the page (not cefQuery).
    pcall(function()
        webview:edQueryCallback(function(queryId, jsonRequest, _persistent)
            handle_cef_query(queryId, jsonRequest)
        end)
    end)
    browser_ready = false
    page_alive = false
    page_loading = false
    pending_load = false
    local browser_cb_registered = false
    local reg_ok = pcall(function()
        webview:browserCreated(function()
            browser_ready = true
            -- Fresh CEF instance — previous navigation is gone.
            page_alive = false
            page_loading = false
            logmsg("browserCreated")
            if slept then return end
            if visible or pending_load then
                load_webview_url()
                if visible then reveal_webview() end
            end
        end)
        browser_cb_registered = true
    end)
    if not (reg_ok and browser_cb_registered) then
        -- Older builds without browserCreated: best-effort immediate load.
        browser_ready = true
        logmsg("browserCreated API missing — loading immediately")
    end

    register_hotkeys()

    pcall(function()
        window:addSizeCallback(function()
            if slept then return end
            -- Never setBounds(window) here — fighting the drag spam-resizes CEF.
            apply_webview_bounds()
            if programmatic_resize then return end
            remember_geom()
            local _, _, w, h = window_xywh()
            if w and h then
                normal_w, normal_h = clamp_dim(w, h)
            end
        end)
    end)
    pcall(function()
        window:addMoveCallback(function()
            if programmatic_resize then return end
            if slept then
                local x, y = window_xywh()
                if cfg.window then
                    cfg.window.x = x
                    cfg.window.y = y
                end
                dirty_geom = true
                return
            end
            remember_geom()
        end)
    end)

    window:setVisible(false)
    visible = false
    logmsg("window created, url=" .. build_url())
end

function show()
    if not is_attrition_server() then
        logmsg("show blocked — not Attrition (host="
            .. tostring(current_server_host()) .. " name=" .. tostring(current_server_name()) .. ")")
        hide()
        return
    end
    create_window()
    if not window then return end
    -- Sleep is sticky: radio menu / slot changes must not wake/reload.
    if slept then
        window:setVisible(true)
        visible = true
        apply_opacity()
        return
    end
    visible = true
    window:setVisible(true)
    apply_opacity()
    if browser_ready then
        if not page_alive and not page_loading then
            load_webview_url()
        end
        reveal_webview()
    else
        -- browserCreated will load once CEF is ready.
        pending_load = true
        logmsg("show waiting for browserCreated")
    end
end

function hide()
    if not window then return end
    -- Keep the page in memory (theme + React state). Only sleep blanks CEF.
    window:setVisible(false)
    visible = false
    flush_geom()
end

local handler = {}

function handler.onNetConnect(_playerId)
    local host = current_server_host()
    local name = current_server_name()
    logmsg("onNetConnect host=" .. tostring(host) .. " name=" .. tostring(name))
    if not is_attrition_server() then
        hide()
        logmsg("overlay disabled — not an Attrition server")
        return
    end
    register_hotkeys()
    if cfg.open_on_start then
        show()
    end
end

function handler.onNetDisconnect()
    logmsg("onNetDisconnect — hiding overlay")
    hide()
end

function handler.onMissionLoadEnd()
    if not is_attrition_server() then
        hide()
        logmsg("onMissionLoadEnd — not Attrition (host="
            .. tostring(current_server_host()) .. " name=" .. tostring(current_server_name()) .. ")")
        return
    end
    if cfg.open_on_start then
        show()
    end
end

function handler.onShowRadioMenu()
    -- Slot/unslot re-opens the radio menu. Never wake sleep; never reload CEF.
    if slept then return end
    if not is_attrition_server() then return end
    if not cfg.open_on_radio_menu then return end
    if not window then
        show()
        return
    end
    -- Window already exists: only reveal chrome + live page (no cefLoadUrl).
    visible = true
    pcall(function() window:setVisible(true) end)
    apply_opacity()
    if page_alive then
        reveal_webview()
    end
end

function handler.onSimulationStop()
    flush_geom()
    if window then
        pcall(function() window:setVisible(false) end)
    end
    visible = false
end

DCS.setUserCallbacks(handler)
logmsg("hooks registered")

if not loadfile(CONFIG_PATH) then
    save_config()
end
