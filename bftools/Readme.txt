bftools.exe [-h | --help] [miz] 

$ ./bftools.exe miz --help
Usage: bftools.exe miz [OPTIONS] --output <OUTPUT> --base <BASE> --weapon <WEAPON> --options <OPTIONS>

Options:
  	--output <OUTPUT>                                  	the final miz file to output
  	--base <BASE>                                      	the base mission file
  	--weapon <WEAPON>                                  	the weapon template
  	--options <OPTIONS>                                	the options template
  	--warehouse <WAREHOUSE>                            	the warehouse template
  	--blue-production-template <BLUE_PRODUCTION_TEMPLATE>  [default: BINVENTORY]
  	--red-production-template <RED_PRODUCTION_TEMPLATE>	[default: RINVENTORY]
  -h, --help                                             	Print help
  
  
 EXAMPLE:
 $ cd ${HOME}/Saved Games/DCS.openbeta/Missions/SouthAtlantic

$ bftools.exe miz --output SouthAtlantic_final.miz --base SouthAtlantic_base.miz --weapon SouthAtlantic_weapons.miz --options SouthAtlantic_options.miz --warehouse SouthAtlantic_warehouse.miz

Fowl 2.0 warehouse template: static Invisible FARP names must include BDEFAULT and RDEFAULT (warehouse rows keyed by those unitIds). BINVENTORY / RINVENTORY unchanged. Optional Invisible FARP **BINVENTORY+** / **RINVENTORY+** warehouse rows (same `wsType` keys as main inventory): merged into B/RINVENTORY after coalition validation and pruning (stock overrides and new rows, `initialAmount>0` included in `_fowl_export.json`). Optional trigger zones **BINVENTORY+** / **RINVENTORY+** and **BDEFAULT+** / **RDEFAULT+** in the warehouse template mission support two row shapes: (1) **catalog module link** — **Key** = warehouse item label (display name), **Value** = airframe module type (e.g. `F-16C_50`); (2) **wsType row** — **Key** = `[l1,l2,l3,l4]` (spaces allowed), **Value** = `ALL` (stock on all coalition warehouses, exempt from TTD weapon prune), `FILTER` (stock only where base policy allows; default when only a numeric amount is set), a non-negative integer (stock amount), or a module type (allowlist link for that wsType). Keys or Values starting with `***` are ignored (editor comments on either column). FARP **+** rows with `initialAmount>0` for a wsType override zone amounts for that type; duplicate amounts across B/RINVENTORY+ and B/RDEFAULT+ zones: first zone wins. Quantities can still come from Invisible FARP **+** rows. Dynamic-spawn O* airports also keep default-zone wsTypes when pruning weapons for `include_dyn_slots` modules. Airports and non-production hubs get the default row matching coalition (red|blue). When `fowl_weapon_bridge*.json` is present next to weapon.miz, BDEFAULT/RDEFAULT `weapons` are rebuilt once from weapon.miz payload strings (restricted+pylons) minus BINVENTORY/RINVENTORY wsTypes, then cloned to all hubs; optional --campaign-cfg sets initialAmount from default_warehouse_* (wsType heuristics). Without the bridge file, behaviour is clone template defaults + cfg counts only (trigger-zone module links require the bridge).

Runtime export (weapon bridge only, Fowl engine 2.0): copy Fowl_engine_export.lua from the scenario folder (e.g. miz/Scenarios/80s/caucasus1987/) to Saved Games\DCS\Scripts\Hooks\, set OUTPUT_DIR to the scenario folder (same folder as weapon.miz / base.miz). In Lua do not use a plain "C:\..." string — backslashes trigger escapes (\b = backspace). Use long brackets [[C:\...]] or forward slashes "C:/.../scenario". Run DCS once into 3D, then remove/rename the hook (DCS.setUserCallbacks). Writes fowl_weapon_bridge-DCS.version.<DCS_version>.json into OUTPUT_DIR (or under Logs if OUTPUT_DIR empty). Airport warehouse list JSON comes from bflib admin command airbaseexport in mission (Saved Games), not from this hook. FowlTools.exe loads fowl_weapon_bridge.json if present, otherwise the newest fowl_weapon_bridge-DCS.version.*.json next to --weapon; or pass --weapon-bridge <path>.
