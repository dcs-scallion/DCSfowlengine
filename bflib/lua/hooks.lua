-- Copyright 2024 Eric Stokes.

-- This file is part of bflib.

-- bflib is free software: you can redistribute it and/or modify it under
-- the terms of the GNU Affero Public License as published by the Free
-- Software Foundation, either version 3 of the License, or (at your
-- option) any later version.

-- bflib is distributed in the hope that it will be useful, but WITHOUT
-- ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
-- FITNESS FOR A PARTICULAR PURPOSE. See the GNU Affero Public License
-- for more details.

net.log("loading bflib.dll")

package.cpath = package.cpath .. ";" .. lfs.writedir() .. "\\Scripts\\?.dll"

local function file_exists(name)
    local f = io.open(name, "r")
    if f ~= nil then
        io.close(f)
        return true
    else
        return false
    end
end

local function copy_file(from, to)
    local fromf, fe = io.open(from, "rb")
    local tof, te = io.open(to, "wb")
    if fromf == nil then
        return fe
    end
    if tof == nil then
        return te
    end
    tof:write(fromf:read("*a"))
    fromf:close()
    tof:close()
    return nil
end

-- Next free archive: live.old0, live.old1, ... (max existing + 1).
local function next_old_path(live)
    local dir = live:match("^(.*)\\[^\\]+$") or "."
    local name = live:match("([^\\]+)$") or live
    local prefix = name .. ".old"
    local max_n = -1
    for fname in lfs.dir(dir) do
        if fname ~= "." and fname ~= ".." and fname:sub(1, #prefix) == prefix then
            local rest = fname:sub(#prefix + 1)
            local n = tonumber(rest)
            if n and tostring(n) == rest and n > max_n then
                max_n = n
            end
        end
    end
    return dir .. "\\" .. prefix .. tostring(max_n + 1)
end

local function promote_staged(live, staging)
    if not file_exists(staging) then
        return true
    end
    if file_exists(live) then
        local old = next_old_path(live)
        local ok, err = os.rename(live, old)
        if not ok then
            net.log("could not archive " .. live .. " -> " .. old .. ": " .. tostring(err))
            return false
        end
        net.log("archived " .. live .. " -> " .. old)
    end
    local ok, err = os.rename(staging, live)
    if ok then
        net.log("promoted " .. staging .. " -> " .. live)
        return true
    end
    -- Rename can fail across volumes; fall back to copy + remove staging.
    local ce = copy_file(staging, live)
    if ce ~= nil then
        net.log("could not promote " .. staging .. ": " .. tostring(ce) .. " / " .. tostring(err))
        return false
    end
    os.remove(staging)
    net.log("promoted (copy) " .. staging .. " -> " .. live)
    return true
end

local bflib_update_file = lfs.writedir() .. "\\Scripts\\_bflib.dll"
local bflib_dll = lfs.writedir() .. "\\Scripts\\bflib.dll"

if file_exists(bflib_update_file) then
    if not promote_staged(bflib_dll, bflib_update_file) then
        net.log("could not install updated bflib.dll")
    else
        net.log("installed updated bflib.dll")
    end
end

local bflib = require("bflib")
bflib.initHooks()
