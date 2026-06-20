# AI air — handoff (Fowl 2.0, Caucasus1985-SARH)

Continuation doc for air AI work. Covers the full hub lifecycle effort; last session focus was **persist in-air resume**.

**Commit:** `001f7fd2` — `air Ai actions & persistence`  
**Reference mission:** `miz/Scenarios/80s/Caucasus1985-SARH/`

---

## Project goal (why we are doing this)

Replace Fowl 1.0 air AI (spawn in air, unlimited fuel, simple orbit) with Fowl 2.0:

- Cold spawn from allied hub (airfield / FOB / carrier), warehouse debit for fuel + weapons
- Waypoint orbit during mission; waypoint move via `-action *-waypoint`
- Real fuel; Fowl-controlled bingo RTB (not DCS default)
- RTB cycle: land → service → return to waypoint
- Commands: `-action rtb`, `start`, `status`, `rearm`
- `duration` in CFG: `null` = fly until destroyed; `Some(h)` = shutdown after landing
- **Persistence:** resume after mission reload — ground at hub **or** in air at last position (Fowl 1.0 behaviour restored)

---

## In this commit (summary)

| Area | File | What |
|------|------|------|
| Phase machine | `ai_air.rs` | Bootstrap, OnMission, RtbInbound, Servicing, Departing, duration shutdown, … |
| Hub / spawn | `ai_air.rs` | Parking, helipad, carrier deck slots; `spawn_ai_air_group(..., mode)` |
| Commands | `actions.rs`, `ai_air.rs` | rtb, start, status, rearm |
| Partial loadout | `ai_air.rs` | `AwaitingLaunch` hold + panel; `-action rearm` |
| DCS RTB off | `ai_air.rs` | `RtbOnBingo(false)`, `RtbOnOutOfAmmo(false)`; Fowl bingo after 120 s |
| Carrier hubs | `ai_air.rs` | Fixed-wing hub = any friendly obj with `airbase_by_oid` |
| Multi-unit DCS | `ai_air.rs` | One DCS group per airframe; missions on all `dcs_spawn_names` |
| Helipad spawn | `ai_air.rs` | Spawn on `slot.pos`, correct `helipadId` / `linkUnit` |
| Weapon flags | `dcso3/unit.rs` | `try_weapon_flags` (status tick no longer crashes on nil) |
| **Fuel persist** | `group.rs` | `SpawnedUnit.fuel_fraction` from `getFuel()` on position updates |
| **In-air resume** | `ai_air.rs`, `actions.rs` | `should_resume_airborne`, `PersistInAir` spawn path |
| **Ground resume fix** | `actions.rs` | `bootstrap_grounded = true` after ground persist bootstrap |
| **Waypoint regen** | `actions.rs` | `*Waypoint` spec kinds → `mission_kind` for orbit regen |
| CFG | `Caucasus1985-SARH_CFG` | `rearm` action added |

---

## Persist in-air resume (detail)

### In-air resume when ALL alive units pass

- Phase: `OnMission`, `RtbInbound`, or `Departing`
- Persisted AGL > **80 m** (`PERSIST_RESUME_MIN_AGL_M`) for every alive unit
- `Departing` → normalized to `OnMission` with orbit mission
- Mission pushed immediately in `respawn_action` (no bootstrap)
- Fuel from `fuel_fraction`; **no** warehouse rearm on in-air path

### Ground resume when

- Phase: `Bootstrap`, `Servicing`, `AwaitingLaunch`, `Refueling`, `ShutdownParked`, …
- OR any unit AGL ≤ 80 m (takeoff / landing transition)
- `OnMission` / `Departing` on ground → `Bootstrap` at hub parking

### Known limitation

- **Loadout not persisted** on in-air resume — ME template default weapons
- **`fuel_fraction`** only exists after at least one flight + save with this DLL (field is new)

### Log lines to verify

```
ai air {gid}: in-air persist resume (N unit(s))
ai air {gid} unit {name}: in-air resume baro {alt}m pos [...] fuel {pct}%
ai air {gid}: ground persist resume (fuel {pct}%)
ai air {gid}: airborne -> on-mission orbit (N wpts)   # ground bootstrap path only
ai air rtb {gid} -> hub "..." (bingo/auto)
ai air rtb {gid} -> hub "..." (explicit)
```

---

## DCS test plan (do first)

1. `. .\setup-build.ps1` → `cargo build --release --package=bflib` → copy `bflib.dll`
2. Copy `Caucasus1985-SARH_fowl_export.json` next to CFG in Saved Games if needed
3. **In-air persist:** deploy drone + fighters, orbit 5+ min, note fuel/pos, exit, reload  
   → expect last positions + orbit; log `in-air persist resume`
4. **Ground persist:** RTB to hub, park / Servicing, reload  
   → hub parking, bootstrap, **not** in-air
5. **Transition:** reload during takeoff (low AGL) → ground bootstrap
6. **status:** `-action status <gid>` — fuel % + stores (see open items if no panel)
7. **Bingo:** fighters/CALCM hold orbit; after bingo → log `bingo/auto`, Fowl RTB
8. **Explicit RTB:** `-action rtb <gid>` on mark at distant base → log `explicit`; check Tacview landing airfield
9. **AWACS on carrier:** `-action awacs-small` near carrier → hub name Naval …
10. **Partial loadout:** exhaust warehouse → `AwaitingLaunch` + `partial loadout:` panel → `rearm` / `start`
11. Tacview + `C:\Users\Robo\Saved Games\DCS\Logs\bfnext.txt`

---

## Still open (priority)

| P | Issue | Notes |
|---|--------|------|
| 1 | **Test in-air persist** | Implemented in `001f7fd2`; not yet confirmed in DCS after user tests |
| 2 | **`status` panel off-slot** | `issue_status` uses `panel_to_player` only — invisible in GCI/observer; use `panel_to_side` fallback |
| 3 | **Explicit RTB wrong airfield** | `issue_rtb` logs explicit hub; verify `rtb_inbound_route` / `LandingReFuAr` lands on chosen base not nearest |
| 4 | **False servicing after spawn** | `on ground at hub -> servicing` right after ground persist (fighters 1509 in old log) |
| 5 | **Post-RTB orbit ingress** | After service, low flight toward objective centre not waypoint |
| 6 | **CALCM bomber** | Orbit height, cruise tasks, JTAC launch — re-verify after multi-unit fix |
| 7 | **Attack heli** | FOB spawn + helipad assignment — re-verify player cannot take same pad |
| 8 | **Duration shutdown** | Multi-ship: all landed before park/off; delete persist only after DCS despawn |
| 9 | **`racetrack_leg_m` CFG** | AWACS/tanker use hardcoded 30 km legs (large “circles” on map) |
| 10 | **Loadout persist** | Optional future work for in-air resume |

---

## Key code entry points

| Topic | Location |
|-------|----------|
| Persist reload | `actions.rs` — `respawn_action` (~635) |
| In-air gate | `ai_air.rs` — `should_resume_airborne`, `PERSIST_RESUME_MIN_AGL_M` |
| Spawn modes | `ai_air.rs` — `AiAirPersistSpawn`, `spawn_ai_air_group` |
| Orbit regen | `actions.rs` — `regenerate_ai_air_mission(..., resume_airborne)` |
| RTB | `ai_air.rs` — `issue_rtb`, `rtb_inbound_route`, `landing_hub_mission_point` |
| Bingo tick | `ai_air.rs` — `advance_ai_air`, `BINGO_FUEL_FRAC`, `ON_MISSION_BINGO_MIN` |
| Status | `ai_air.rs` — `issue_status` |
| Rearm / hold | `ai_air.rs` — `try_rearm_from_template`, `issue_rearm`, `AwaitingLaunch` |
| Fuel save | `group.rs` — `update_unit_positions` → `fuel_fraction` |

---

## Fowl 1.0 reference (`c:\GitHub\bfnext`)

- Spawn: `SpawnLoc::InAir`; waypoint in `loc.pos`
- Persist: respawn at `unit.pos` + immediate orbit mission
- No hub cycle, unlimited fuel on many types
- Useful when validating “resume where they left off” behaviour

---

## User notes (`air Ai.docx`)

- Restart showed only marks → fixed hub respawn; in-air resume added later
- Kh-101 vs Kh-555 was template/warehouse mismatch, not arm logic bug
- Large drone “circles” on other engine likely race-track 30 km (AWACS) or high `speed` (Circle) — no radius in DCS Lua API
