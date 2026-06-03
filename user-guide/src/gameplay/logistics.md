# Logistics & Supply

The logistics system adds strategic depth to Fowl Engine. Understanding supply flows is key to sustained operations.

## Overview

Logistics simulates the supply chain required to maintain military operations. Without adequate supplies, objectives cannot function effectively.

## The Logistics System

### What is Logistics?

**Logistics tracks two main resources**:
1. **Equipment** (weapons, vehicles, parts)
2. **Fuel** (aviation fuel, vehicle fuel)

**Three levels of infrastructure**:
1. **Logistics (Logi)** - Infrastructure health
2. **Supply** - Equipment inventory level
3. **Fuel** - Fuel inventory level

## Supply Flow

### Logistics Hubs

**Central Distribution**:
- Logistics hubs are special objectives
- They distribute supplies to connected objectives
- Form the backbone of supply network

**Hub Connections**:
- Each hub connects to multiple objectives
- Supply flows automatically
- Captured objectives reconnect supply lines

### Supply Routes

Supplies flow from:
```
Logistics Hub → Frontline Objectives → FARPs
```

**Route Characteristics**:
- Automatic distribution every tick interval
- Prioritizes objectives with lowest supply
- When `virtual_resupply` is enabled, hub-to-objective delivery is scaled by distance (see below)
- Broken routes halt supply flow

### Production zones (OPR) and logistics hubs (OLO)

Fowl maps use trigger zone names `O…` in the mission (see [Objectives](./objectives.md)). Two kinds matter for the supply chain:

| Zone prefix | Kind | Role |
|-------------|------|------|
| **OPR\*** | Production | Factory area; output depends on linked production buildings |
| **OLO\*** | Logistics hub | Central warehouse; receives production and feeds connected objectives |

(\* `R` or `B` in the zone name is the default coalition: `OPR…` Red production, `OLOB…` Blue logistics hub, etc.)

**Two-step virtual pipeline** (when `virtual_resupply` is true in campaign CFG):

```
OPR (Production) → OLO* (Logistics hub) → objectives with warehouses
```

1. **OPR → OLO\*** — Each production zone is linked to the **nearest logistics hub of the same coalition**. The hub’s **Production** stat (0–100%) reflects how many factory buildings in the OPR zone are active. Periodic production into the hub inventory is scaled by that percentage (same rounding rule as delivery efficiency: at least 1 unit when base output is non-zero and Production > 0).

2. **OLO\* → objectives** — On each logistics tick, the hub distributes stock to objectives listed in its warehouse destination list. Each objective is supplied by the **nearest owned logistics hub** (`compute_supplier`). When virtual resupply is on, the **amount delivered** is multiplied by a **distance efficiency** (see next section).

When **Production** on an OPR zone is **0%**, it is treated as disconnected: no production feed to the hub and **no** OPR→OLO map line.

### Captured logistics hubs (occupied OLO)

A logistics hub is **occupied** when its **current owner** differs from the **default coalition in the ME zone name** (the `R` or `B` after `OLO` in the trigger zone, e.g. `OLOBTbilisi` is Blue by default; if Red holds it, that hub is occupied for Red).

| Rule | Normal OLO (`owner` matches zone letter) | Occupied OLO |
|------|------------------------------------------|--------------|
| OPR link / black feed line | Yes | **No** — no OPR production into the hub |
| Hub **Production** from factories | Yes | **No** (stays 0% from OPR) |
| Stock from `deliver_production` | Yes | **No** |
| **Inbound** resupply | OPR + virtual network | **Virtual only** from the **nearest normal** OLO of the **occupier**, same trigger as other objectives (**supply** or **fuel** &lt; 100%) |
| Inbound delivery efficiency | N/A (OPR path) | **100%** — no distance decay on that leg |
| **Outbound** to frontline objectives | Yes (decay applies) | **Yes** — same as a normal hub for the occupier |

**Re-capture** by the original coalition (`owner` matches the zone name again) restores normal OLO behaviour and OPR links.

**ME naming**: logistics zones must be **`OLOB*`** or **`OLOR*`** only — **`OLON*`** (neutral) is rejected at mission build and campaign start.

**F10 map**: occupied hubs show a **solid** line (same shaft width as hub→objective supply arrows, no arrowhead) from the nearest **normal** friendly OLO to the captured hub, in the **occupier’s** coalition colour at **50%** opacity. This is separate from the black OPR feed and the coloured supply arrows to objectives.

### Virtual resupply and distance decay

Campaign flag **`virtual_resupply`** (in `*_CFG`):

- **`true`** — Hub production runs automatically and hubs deliver to connected objectives using **virtual** resupply (no 3D convoys). Delivery amounts use distance decay below.
- **`false`** — Hub production still runs; **no** automatic hub-to-objective virtual delivery (intended for future physical supply routes). Distance decay is **not** applied (`100%` efficiency).

Distance is **hub center to objective center**, in kilometres (metres / 1000).

**Delivery efficiency** uses exponential decay with a floor (`virtual_resupply_decay` in CFG):

```
E(d) = f + (100 - f) * exp(-k * d)

k = -ln((r - f) / (100 - f)) / y
```

| CFG key | Meaning | Engine default |
|---------|---------|----------------|
| `reference_distance_km` (`y`) | Distance where efficiency equals `r` | 250 |
| `efficiency_at_reference_pct` (`r`) | Efficiency at `y` (whole percent, u8) | 25 |
| `efficiency_floor_pct` (`f`) | Minimum efficiency; distance alone never zeroes delivery | 3 |

Examples with defaults: ~96% at 8 km, 25% at 250 km, ~8% at 500 km, approaching 3% on very long links.

Efficiency is a whole percent (u8), cached per hub–objective pair and recomputed when routes or positions change.

### F10 map supply lines

Two **independent** line types are drawn on the F10 map (do not confuse them):

| Line | Route | Appearance | Meaning |
|------|-------|------------|---------|
| **Production feed** | OPR → OLO\* | Narrow **black** shaft (no arrowhead) | Factory link to the hub that receives OPR output |
| **Occupied hub link** | Normal OLO → occupied OLO | **Solid** shaft (supply-line width), coalition colour, 50% opacity | Captured hub fed virtually from nearest normal hub (100% on that leg) |
| **Supply connection** | OLO\* → objective | Coloured **arrow** (shaft + head) | Virtual resupply route to a warehouse objective |

**Production feed (OPR → OLO\*)**:
- Drawn only while OPR **Production > 0%** and a feed hub exists
- **Opacity** scales linearly with Production: full at 100%, hidden at 0%
- **Width** is half that of supply-connection shafts
- **No distance decay** on this segment — only the hub Production percentage matters

**Supply connections (OLO\* → objective)**:
- Drawn for each hub→destination pair in the supply network
- **Colour** follows delivery efficiency: **green** (100%) → **yellow** → **orange** (floor), matching the decay curve
- Arrow geometry is custom map markup (not DCS default arrow thickness)

Reading the map: a long **orange** arrow means a far objective still receives some virtual supply but at low efficiency; a **black** OPR→OLO line shows factories feeding the hub, brighter when Production is high.

### Decay curve figure (`virtual_resupply_decay.png`)

To visualise the default (or campaign-specific) decay curve, use the generator script shipped with the user guide:

- **Script:** `user-guide/scripts/generate_virtual_resupply_decay_graph.py`
- **Output file:** `virtual_resupply_decay.png` (written in the **current working directory**, overwrites if present)

**Typical use in a mission folder:**

1. Copy `generate_virtual_resupply_decay_graph.py` next to your `*_CFG`.
2. From that folder, run:

```powershell
py -3 generate_virtual_resupply_decay_graph.py
```

The script loads `virtual_resupply_decay` from the local `*_CFG` when found; otherwise it uses engine defaults (250 km / 25% / 3%). Requires Python 3 with `matplotlib` and `numpy`.

A reference render is kept in the repo at `user-guide/src/figures/virtual_resupply_decay.png` (regenerate locally after CFG changes).

**Example CFG block:**

```json
"virtual_resupply": true,
"virtual_resupply_decay": {
  "reference_distance_km": 250,
  "efficiency_at_reference_pct": 25,
  "efficiency_floor_pct": 3
}
```

Omit keys to keep engine defaults; set `virtual_resupply` to `false` to disable virtual hub delivery and distance decay.

### Supply Ticks

The system runs on a **tick cycle**:

**Tick Frequency** (PG Tempest):
- Every **10 minutes**
- Supplies distribute during each tick
- Automatic process, no player action needed
- Full delivery cycle: **24 ticks** (4 hours)

**During Each Tick**:
1. System assesses all objectives
2. Calculates supply needs
3. Distributes from logistics hubs
4. Updates objective supply levels

## Supply & Fuel Levels

### Supply Percentage

Represents equipment and munitions:

- **100%**: Fully stocked
- **75-99%**: Good condition
- **50-74%**: Adequate supplies
- **25-49%**: Low stocks
- **0-24%**: Critical shortage

**Effects of Low Supply**:
- Reduced repair speeds
- Limited deployments available
- Decreased operational tempo
- Warehouse capacity reduced

### Fuel Percentage

Represents aviation fuel stocks:

- **100%**: Full fuel reserves
- **75-99%**: Good fuel stocks
- **50-74%**: Adequate fuel
- **25-49%**: Low fuel
- **0-24%**: Fuel emergency

**Effects of Low Fuel**:
- Aircraft cannot rearm
- Helicopter operations limited
- May prevent takeoffs
- Logistics vehicles affected

## Supply Priorities

### Automatic Distribution

The system prioritizes:
1. **Lowest supply first** - Most desperate get priority
2. **Connected objectives** - Must have supply route
3. **Available inventory** - Hub must have supplies

**Example Priority**:
```
Objective A: 20% supply → Gets first priority
Objective B: 45% supply → Gets second priority  
Objective C: 80% supply → Gets last priority
```

## Logistics Infrastructure (Logi)

### What is Logi?

Logi represents the physical infrastructure:
- Buildings
- Roads and railways
- Communications
- Support facilities

### Logi Percentage

- **100%**: Perfect condition
- **75-99%**: Minor damage
- **50-74%**: Moderate damage
- **25-49%**: Heavy damage
- **1-24%**: Critical damage
- **0%**: Destroyed (objective capturable!)

### Logi Effects

**High Logi (75-100%)**:
- Fast repair times
- Efficient supply processing
- Normal operations

**Medium Logi (25-74%)**:
- Slower operations
- Reduced efficiency
- Still functional

**Low Logi (1-24%)**:
- Severely impaired
- Very slow repairs
- Minimal functionality

**Zero Logi (0%)**:
- **OBJECTIVE CAN BE CAPTURED**
- No supply processing
- No repairs possible
- Critical vulnerability

## Repairing Logistics

### Natural Repair

Logistics gradually repair over time:
- Automatic process
- Slow regeneration
- Requires some supply availability

**Repair Rate**:
- Typically 1-5% per tick
- Server-configured
- Requires positive supply level

### Manual Repair

Players can expedite repairs:

**Via Actions Menu**:
```
F10 → Actions → Repair (or Repair-Fast) → [Select Objective]
```

**Requirements**:
- Costs **100 points** (helo) or **200 points** (fast fixed-wing)
- Must own the objective
- Repair crate must be delivered to objective

**Benefits**:
- Immediate logi increase (one step)
- Prevents capture vulnerability
- Strategic investment
- Fast option gets there quicker

## Warehouse System

### Equipment Inventory

Objectives store equipment in warehouses:

**Types of Equipment**:
- Aircraft and helicopters
- Tanks and armored vehicles
- Artillery systems
- Infantry units
- Support equipment

**Capacity**:
- Each objective has maximum capacity
- Varies by objective type
- Display format: `stored / capacity`

### Liquid Inventory

Fuel stored separately:

**Liquid Types**:
- Jet fuel (aircraft)
- Aviation gasoline (props)
- Diesel (vehicles)

## Supply Strategies

### Offensive Strategy

**Attacking Enemy Supply**:
1. **Target logistics hubs** - Cut off multiple objectives
2. **Interdict supply routes** - Attack connecting objectives
3. **Reduce frontline supply** - Weaken enemy operations

**Maintaining Your Supply**:
1. **Protect logistics hubs** - Heavy air defense
2. **Secure supply routes** - Defend connecting objectives
3. **Keep logi above 0%** - Prevents captures

### Defensive Strategy

**Supply Line Defense**:
- Deploy SAMs at logistics hubs
- Maintain CAP over critical objectives
- Repair logi quickly when damaged
- Keep fuel reserves high

**Emergency Response**:
- If logi falls to 0%, immediate priority repair
- Rush fighters to defend against capture
- Deploy ground units to contest zone

## Reading Supply Information

### F10 Map Markers

Typical format:
```
Musa Airbase
Health: 85
Logi: 42
Supply: 75
Fuel: 100
Points: 0
```

- **Health**: 85 - Facility condition
- **Logi**: 42 - Infrastructure (safe from capture, above 0)
- **Supply**: 75 - Equipment stocks (good level)
- **Fuel**: 100 - Fuel stocks (full)
- **Points**: 0 - Capture point value

**Note**: Values are whole numbers 0-100.

### In-Game Notifications

System messages for supply events:
- "Objective supply critical" - Below 25%
- "Objective fuel emergency" - Below 25%
- "Logistics damaged" - Logi falling
- "Objective capturable" - Logi at 0%

## Logistics Transfers

### Manual Transfers

Admin or special actions can transfer supplies:

**Command**:
```
-admin transfer <from-objective> <to-objective>
```

**Use Cases**:
- Emergency supply to starved objective
- Balancing supply distribution
- Preparing for major operations

**Restrictions**:
- Requires admin privileges (for `-admin` variant)
- Limited by warehouse capacity
- Both objectives must be owned

## Advanced Topics

### Supply Line Optimization

**Efficient Network**:
- Capture objectives in logical order
- Maintain control of connecting objectives
- Don't overextend supply lines

**Example Bad Strategy**:
```
Hub → A → (enemy) → B → Front
```
Objective B is cut off!

**Example Good Strategy**:
```
Hub → A → B → Front
```
Clear supply line maintained.

### Logistics as Weapon

**Starve Enemy Objectives**:
1. Identify their logistics hubs
2. Strike them repeatedly
3. Target connecting objectives
4. Wait for supply depletion
5. Attack when weakened

**Siege Warfare**:
- Surround enemy objective
- Cut off supply routes
- Wait for supplies to deplete
- Capture when logistics fail

### Supply Consumption

Different operations consume supplies:

**High Consumption**:
- Repairing damaged aircraft
- Deploying heavy armor
- Sustained combat operations
- Large-scale actions

**Low Consumption**:
- CAP flights
- Basic repairs
- Small unit deployments

## Troubleshooting

### "Why is my objective low on supply?"

Possible causes:
- Logistics hub captured by enemy
- Supply route broken
- High consumption rate
- Insufficient tick intervals passed
- Objective far from its supplying hub (low virtual resupply efficiency when `virtual_resupply` is on)

**Solution**:
- Check supply route integrity
- Protect logistics hubs
- Wait for next supply tick
- Reduce unnecessary deployments
- Prefer nearer hubs or capture intermediate objectives to shorten supply links

### "Logi won't repair"

Possible causes:
- Supply level too low
- Recent damage faster than repair
- Server settings

**Solution**:
- Wait for supply delivery
- Use manual repair action
- Defend objective from attacks

## Next Steps

Learn about the [Points and Lives System](./points-and-lives.md) to understand how resources are earned and used!

