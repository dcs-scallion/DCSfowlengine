---@meta
-- LuaLS-only stubs: DCS hook / GUI symbols not in stock Lua 5.1.

---@class (partial) _G
---@field db table
---@field ED_FINAL_VERSION string|number|nil
---@field DCS_VERSION string|nil
---@field __DCS_VERSION__ string|nil

---@return table
function LoGetVersion() end
