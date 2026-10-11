# Design: Attrition dashboard identity, claim, squadrons

Status: **concept locked**; stages **A–H implemented** in code (ops truncate for A optional).  
Scope: Fowl 2.0 / Attrition multi-instance dashboard (`bfdb` + UI) and relation to DCSServerBot.  
Language: English (project docs).

---

## 1. Goals

- Players can fly Attrition and use the **public** dashboard **without Discord**.
- Discord is optional community / bot commands only — **not** a gate for Fowl identity or gated dashboard pages.
- Player stats source of truth: **bfdb** (from `bflib` `Stat::*`), not DCSServerBot mission/user stats.
- Primary identity: **web account (username + password) ↔ UCID** via in-game claim.
- Coalition (side) is **per Attrition server (instance)**; dashboard filters gated content by selected server + stored side.
- Squadrons (“letky”) are **global** across Attrition servers, owned by **bfdb**, not bot Userstats squadrons.

## 2. Non-goals

- Discord OAuth as player login (removed; existing Discord sessions are not migrated — everyone uses bfdb Login).
- Using DCSServerBot `/linkme` / `.linkme` for dashboard or Cockpit authorization.
- Syncing Fowl claim ↔ bot link in either direction.
- Requiring email for registration or password recovery.
- Coalition-restricted Discord channels as an access control model.
- Replacing DCSServerBot for DCS process control (start/stop/scheduler/update). Bot Postgres remains for bot ops (`players`, `bans`, `missions`, `nodes`, …).
- Reviving dissolved squadrons under the same id/name.

---

## 3. Identity diagram

```
                    ┌─────────────────────────────────────┐
                    │  Public dashboard (no login)          │
                    │  rankings, kills, public stats, …     │
                    └─────────────────────────────────────┘

DCS join (pick Blue/Red)
    → Fowl records UCID + display name + side @ instance
    → bfdb pilot row (per server side)

Register (username + password, no email)
    → web account (no UCID yet)
    → show claim code (TTL 2 days):  -linkme NNNN

DCS chat: -linkme NNNN
    → bflib → bfdb: bind account ↔ session UCID
    → gated + MY PROFILE + Cockpit allowed (with login)

Login (username + password)
    → session cookie
    → load UCID + per-server sides from bfdb
    → no need to be online in DCS

Optional (independent):
Discord /linkme → DCS .linkme TOKEN → bot Postgres only (bot slash/commands)
```

Two link systems (no sync):

| System | Discord | In-game | Store | Purpose |
|--------|---------|---------|-------|---------|
| **Fowl claim** | — | `-linkme <code>` | bfdb | Dashboard / Cockpit / squadrons |
| **Bot link** | `/linkme` | `.linkme <token>` (Attrition bot prefix `.`) | bot DB | Bot Discord features only |

---

## 4. Auth matrix (feature × requirement)

| Feature | Anonymous | Login only | Login + claim | Login + claim + side on selected server |
|---------|-----------|------------|---------------|----------------------------------------|
| Public rankings / pilots / kill feed / … | yes | yes | yes | yes |
| Register / Login / Forgot password | yes | — | — | — |
| MY PROFILE (account, claim status, side list, squadron UI) | — | — | yes | yes |
| Generate claim / recovery code UI | — | yes* | yes | yes |
| Cockpit (JTAC, EWR, …) | — | — | yes | (side if action is coalition-scoped) |
| Coalition-gated pages (e.g. TAC MAP) | — | — | — | yes |
| Create / join request / leave squadron | — | — | yes | yes |
| Admin (local username/password only) | — | admin role | — | — |

\* Recovery code: Forgot password flow (username, no password) issues a code bound to that account.

Server switcher (header): for each Attrition instance, show public pages always; lock coalition subsections if this UCID has never joined that instance (unknown side) or side does not match.

---

## 5. Flows

### 5.1 First join + claim

1. Player joins Attrition instance N, picks coalition in DCS MP UI.  
2. Fowl stores UCID, display name (updated each connect), side @ N.  
3. No Fowl chat `blue`/`red` registration command required.  
4. Player Registers on dashboard → claim code shown in UI (TTL **2 days**).  
5. In DCS: `-linkme <code>` → account ↔ UCID.  
6. Gated sections and Cockpit require Login + claim thereafter.

### 5.2 Return visit (day 2+)

1. Login with username + password (browser may autofill).  
2. Session → account → UCID → sides from bfdb.  
3. **Not** required to be online on DCS for stored-side gated content.

### 5.3 Forgot password (no email)

1. Forgot password → enter **username**.  
2. UI shows **recovery** claim code (TTL 2 days), bound to that account.  
3. Online on Attrition → `-linkme <code>` with the **same UCID** already linked to that account.  
4. On UCID match → set new password on the **same** account.  
5. On UCID mismatch → reject.

### 5.4 Register + claim when UCID already linked (**2a**)

If a **new** account’s `-linkme` targets a UCID already bound to **another** account: **warn and stop** (“use Login / Forgot password”). No UCID transfer.

### 5.5 New DCS UCID (Steam → Standalone / new purchase)

- Treated as a **new** player (new UCID row).  
- Merge old → new only via Admin **UCID Account Merge** (existing tooling).  
- No automatic re-link by display name.

### 5.6 Side switch

- In-game (existing Fowl rules) **or** MY PROFILE: list of servers, current side, remaining **`side_switches`** per server CFG.  
- Authoritative write into Fowl/bfdb; respect CFG limits and slot/mission conflicts (implementation detail later).

---

## 6. Squadrons (letky)

| Rule | Decision |
|------|----------|
| Scope | Global across all Attrition servers |
| Membership key | **UCID** (survives password recovery on same UCID) |
| Concurrent membership | At most **one** active squadron |
| History | Unlimited rows: squadron id, joined_at, left_at |
| Create | MY PROFILE → founder = commander (CO) |
| Join | Request from list → CO approve/deny |
| Leave | Member leaves unilaterally (no CO confirm) |
| CO leave without transfer | Squadron **dissolved** (`dissolved_at`); history kept; **not** revivable |
| CO may | Rename; transfer command to another **member** |
| CO may not | Hard-delete squadron; revive dissolved squadron |
| Name uniqueness | Across **all** squadrons including dissolved (create + rename) |
| Pending requests on dissolve | **Auto-reject** |
| Icon | Optional **PNG** (incl. transparency), max **20 KB**, max **150×150 px**, **150 DPI** guideline |
| Stats | Show icon by members later; squadron-specific stats later; filter active vs dissolved later |
| Bot squadrons | Not source of truth; unused when dashboard owns squadron admin/stats |

---

## 7. UI changes (login)

Replace current Discord / Admin dual tab player entry with:

- **Register** / **Sign in** (username + password; no Discord icon or “Login with Discord”).  
- **Forgot password** (username → recovery code).  
- **Admin** local login kept **separate** (not Discord).  
- Header **Login** button: no Discord branding.
- Register usernames: unique in bfdb; ASCII `a–z`/`0–9`/`_`/`-` only; reserved exact names blocked case-insensitively (DCS default nicknames such as `Player` / `Spieler` / …, plus `admin`, `guest`, …). Non-ASCII defaults (e.g. `Hráč`) fail the ASCII rule.

Public pages remain usable logged out.

---

## 8. DCSServerBot (ops vs stats)

| Keep | Disable / ignore for Attrition player stats |
|------|-----------------------------------------------|
| Postgres (ops) | `missionstats` gathering (`enabled: false` or `persistence: false`) |
| Mission, GameMaster, Admin, Scheduler | `userstats` gathering if Discord leaderboards unused |
| RestAPI (`/servers`, instance control) for cockpit/restart/weather | Bot Discord highscores / kill feed as authority |
| Bot `/linkme` + `.linkme` for Discord-only | Cloud stats sync for player ladders |

Keep Userstats/MissionStats plugins **loaded** if RestAPI depends on them; disable **collection**, do not necessarily unload.

Disk: truncate `missionstats` (+ optional `statistics`) + `VACUUM FULL` after disable.

---

## 9. Implementation stages (logical order)

Stages are sequential where noted; DCS can stay up except short `bflib` reload windows.  
No new CLI flags without explicit approval. Cockpit **features** unchanged — auth gate only in E/F.

| Stage | Deliverable | Depends on | DCS impact | Cutover / note |
|-------|-------------|------------|------------|----------------|
| **A – Bot stats off** | Disable missionstats (+ userstats gather if unused); optional DB truncate + `VACUUM FULL` | — | None (bot restart only) | Ops anytime; frees disk; no Fowl code |
| **B – Auth foundation** | Web accounts (username+password, no email); Register / Login / Forgot UI; sessions; remove Discord player OAuth + Discord Login branding; Admin stays local-only | — | None | **Hard cut:** Discord login dead (decision 6A). Public pages keep working |
| **C – Claim end-to-end** | Issue claim + recovery codes (TTL 2d); bfdb bind account↔UCID; bflib `-linkme`; rule **2a** (warn+stop); display name sync on connect (as today) | B | Short mission/instance reload for new `bflib.dll` | Until C live, Register shows code but in-game claim fails — deploy B+C close together or hide claim until C |
| **D – Per-server side + gates** | Persist side per instance from DCS join; server switcher; coalition-gated pages use stored side (TAC MAP etc. when enabled) | C | Likely `bflib` reload if side ingest changes | Public OK offline; gated needs Login+claim+side |
| **E – MY PROFILE** | Claim status, UCID, per-server sides, `side_switches` UI + authoritative switch (CFG limits) | C, D | Possible small `bflib` if in-game switch parity | Account centre for players |
| **F – Cockpit auth only** | Replace Discord+bot `require_linked_player` with session + bfdb claim (+ side if needed). **No Cockpit feature changes** | C | None (bfdb) | Same panel, new gate |
| **G – Squadrons** | Global letky: create/CO, join requests, leave, rename, transfer CO, dissolve on CO leave w/o transfer, unique names, PNG ≤20KB / 150×150, membership on UCID, pending auto-reject | C, E | None | After MY PROFILE shell exists |
| **H – Stats polish** | Squadron icon by pilots; squadron stats; filter active vs dissolved | G | None | Can slip; not blocking fly/claim |

### Suggested deploy bundling

1. **A** alone (ops).  
2. **B+C** in one release window (auth without claim is incomplete UX).  
3. **D+E** next (side + profile).  
4. **F** when Cockpit must leave Discord gate.  
5. **G** then **H**.

### Parallelism

- **A** parallel to anything.  
- UI wireframes for B/E/G while A runs.  
- **H** never blocks A–F.

---

## 10. Open for implementation design only

Not product forks — details when coding:

- Password hashing, session TTL, rate limits on claim/recovery codes.  
- Exact PNG validation (color type, animated PNG reject).  
- Conflict handling when side_switches exhausted while slotted.  
- Wireframes / copy for warn messages (2a, recovery).  
- Whether display-name override in MY PROFILE (deferred product idea).

---

## 11. Decision log (short)

| Topic | Decision |
|-------|----------|
| Discord for players | Not required; OAuth login removed |
| Email | Not required |
| Password | Required; browser autofill OK |
| Forgot password | Username → recovery code → `-linkme` + UCID match → new password |
| Claim code delivery | In dashboard UI, TTL 2 days |
| Dual link systems | Fowl `-linkme` vs bot `.linkme`; no sync |
| Side | DCS MP pick; per instance; dashboard by selected server |
| New UCID | New player; Admin merge only |
| Claim collision | 2a warn and stop |
| Admin auth | Local username/password only |
| Cockpit | Login + bfdb claim |
| Squadrons | Global; UCID membership; dissolve on CO leave w/o transfer; unique names forever; PNG limits as above |
| Bot stats | Not authority; disable gathering |
| Discord session migration | None (path A) |

---

*End of concept lock. Implement only after explicit approval of a phase.*
