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

Fowl 2.0 warehouse template: static Invisible FARP names must include BDEFAULT and RDEFAULT (warehouse rows keyed by those unitIds). BINVENTORY / RINVENTORY unchanged. Airports and non-production hubs get the default row matching coalition (red|blue). When `fowl_weapon_bridge*.json` is present next to weapon.miz, BDEFAULT/RDEFAULT `weapons` are rebuilt once from weapon.miz payload strings (restricted+pylons) minus BINVENTORY/RINVENTORY wsTypes, then cloned to all hubs; optional --campaign-cfg sets initialAmount from default_warehouse_* (wsType heuristics). Without the bridge file, behaviour is clone template defaults + cfg counts only.

Weapon bridge (CLSID etc. -> wsType, Fowl engine 2.0): copy bftools/Fowl_engine_weapon_bridge_export.lua to Saved Games\DCS\Scripts\Hooks\, set OUTPUT_DIR to the scenario folder (same folder as weapon.miz). In Lua do not use a plain "C:\..." string — backslashes trigger escapes (\b = backspace). Use long brackets [[C:\...]] or forward slashes "C:/.../scenario". Run DCS once into 3D, then remove/rename the hook (DCS.setUserCallbacks). Produces fowl_weapon_bridge-DCS.version.<DCS_version>.json (version in filename and inside JSON for humans; or under Logs if OUTPUT_DIR empty). FowlTools.exe loads fowl_weapon_bridge.json if present, otherwise the newest fowl_weapon_bridge-DCS.version.*.json next to --weapon; or pass --weapon-bridge <path>.
