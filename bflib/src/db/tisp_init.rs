use super::group::DeployKind;
use super::Db;
use crate::spawnctx::{SpawnCtx, SpawnLoc};
use anyhow::{anyhow, bail, Context, Result};
use bfprotocols::{
    cfg::{Cfg, Deployable, DeployableKind},
    stats::Stat,
    tisp::{parse_tisp_zone_name, starts_with_tisp_prefix, TISP_PREFIX},
};
use dcso3::{
    centroid2d,
    coalition::Side,
    env::miz::{GroupKind, Miz, MizIndex, TriggerZone, TriggerZoneTyp},
    land::{Land, SurfaceType},
    net::Ucid,
    LuaVec2, MizLua, Vector2,
};
use enumflags2::BitFlags;
use indexmap::IndexMap;

fn tisp_zone_sample_pos2(zone: &TriggerZone<'_>) -> Result<Vector2> {
    Ok(match zone.typ()? {
        TriggerZoneTyp::Circle { .. } => zone.pos()?,
        TriggerZoneTyp::Quad(pts) => centroid2d([pts.p0.0, pts.p1.0, pts.p2.0, pts.p3.0]),
    })
}

pub fn validate_tisp_zones_on_water(lua: MizLua, miz: &Miz) -> Result<()> {
    let land = Land::singleton(lua)?;
    const RED: &str = "\x1b[31m";
    const RESET: &str = "\x1b[0m";
    let mut on_land: Vec<String> = Vec::new();
    for zone in miz.triggers()? {
        let zone = zone?;
        let name = zone.name()?;
        if !starts_with_tisp_prefix(name.as_str()) {
            continue;
        }
        if parse_tisp_zone_name(name.as_str()).is_none() {
            bail!(
                "malformed TISP trigger zone {:?}: after {:?} expected B… or R… ship template, optional trailing -N (e.g. TISPBFrigate, TISPBFrigate-1)",
                name,
                TISP_PREFIX
            );
        }
        let p = tisp_zone_sample_pos2(&zone)?;
        let st = land.get_surface_type(LuaVec2(p))?;
        if !matches!(st, SurfaceType::Water | SurfaceType::ShallowWater) {
            on_land.push(name.to_string());
        }
    }
    if on_land.is_empty() {
        return Ok(());
    }
    on_land.sort();
    eprintln!(
        "{RED}ERROR TISP initial-ship zone(s) must be on water (DCS surface type Water or ShallowWater), not land:{RESET}"
    );
    for z in &on_land {
        eprintln!("{RED}  - \"{z}\"{RESET}");
    }
    eprintln!(
        "{RED}Fix: DCS Mission Editor → select each listed trigger zone → move it so the zone center sits on open water → save → rebuild the mission.{RESET}"
    );
    let listed = on_land
        .iter()
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "TISP initial-ship zone(s) not on water under zone center (open base.miz in ME, move these trigger zone centers to open water, rebuild): {listed}"
    );
}

fn find_group_deployable_for_template(
    cfg: &Cfg,
    side: Side,
    template: &str,
) -> Option<Deployable> {
    let list = cfg.deployables.get(&side)?;
    for d in list {
        if d.provides_tisp_ship_template(template) {
            return Some(d.clone());
        }
    }
    None
}

pub fn place_tisp_initial_ships(
    miz: &Miz,
    idx: &MizIndex,
    db: &mut Db,
    spctx: &SpawnCtx<'_>,
) -> Result<()> {
    let cfg_arc = db.ephemeral.cfg.clone();
    let cfg = cfg_arc.as_ref();
    let mut per_template: IndexMap<String, Vec<(u32, String, Vector2)>> = IndexMap::new();
    for zone in miz.triggers()? {
        let zone = zone?;
        let name = zone.name()?;
        let Some(parsed) = parse_tisp_zone_name(name.as_str()) else {
            continue;
        };
        let pos = tisp_zone_sample_pos2(&zone)?;
        per_template
            .entry(parsed.template.to_string())
            .or_default()
            .push((
                parsed.instance_index,
                parsed.full_name.to_string(),
                pos,
            ));
    }
    if per_template.is_empty() {
        return Ok(());
    }
    let mut keys: Vec<String> = per_template.keys().cloned().collect();
    keys.sort();
    for template in keys {
        let side = match template.as_bytes().first() {
            Some(b'B') => Side::Blue,
            Some(b'R') => Side::Red,
            _ => bail!("internal: bad TISP template {:?}", template),
        };
        let dep = find_group_deployable_for_template(cfg, side, template.as_str())
            .with_context(|| {
                format!(
                    "TISP zones reference ship template {:?} but no CFG deployables entry covers that ME group name for {:?} (Group template, Objective pad_templates, or legacy \"template\")",
                    template, side
                )
            })?;
        let slots = per_template
            .get_mut(&template)
            .expect("key from keys vec");
        slots.sort_by(|(ia, na, _), (ib, nb, _)| ia.cmp(ib).then_with(|| na.cmp(nb)));
        let limit = dep.limit as usize;
        miz.get_group_by_name(idx, GroupKind::Any, side, template.as_str())?
            .ok_or_else(|| {
                anyhow!(
                    "TISP needs ship template group {:?} on {:?} (ME group name must match deployable Group template)",
                    template,
                    side
                )
            })?;
        let dep_menu_key = dep.path.last().ok_or_else(|| {
            anyhow!(
                "TISP deployable for ME template {:?} has empty path (menu name)",
                template
            )
        })?;
        for (_, zone_name, pos) in slots.iter().take(limit) {
            let (n, _) = db.number_deployed(side, dep_menu_key.as_str()).with_context(|| {
                format!("TISP counting deployed instances for zone {:?}", zone_name)
            })?;
            if n >= dep.limit as usize {
                bail!(
                    "TISP: more initial-ship zones than deployable limit ({}) for {:?} on {:?}",
                    dep.limit,
                    dep_menu_key.as_str(),
                    side
                );
            }
            let lua = spctx.lua();
            match &dep.kind {
                DeployableKind::Objective(parts) => {
                    let oid = db
                        .add_farp(lua, spctx, idx, side, *pos, &dep, parts)
                        .with_context(|| format!("TISP add_farp for zone {:?}", zone_name))?;
                    db.ephemeral.stat(Stat::DeployFarp {
                        by: Ucid::default(),
                        oid,
                        deployable: dep_menu_key.clone(),
                    });
                }
                DeployableKind::Group {
                    template: group_tpl,
                } => {
                    let spawn_group_label: Option<String> = if dep.limit > 1 {
                        Some(if n == 0 {
                            group_tpl.to_string()
                        } else {
                            format!("{}-{}", group_tpl.as_str(), n)
                        })
                    } else {
                        None
                    };
                    db.add_and_queue_group(
                        spctx,
                        idx,
                        side,
                        SpawnLoc::AtPos {
                            pos: *pos,
                            offset_direction: Vector2::new(1., 0.),
                            group_heading: 0.,
                        },
                        group_tpl.as_str(),
                        DeployKind::Deployed {
                            player: Ucid::default(),
                            moved_by: None,
                            spec: dep.clone(),
                            cost_fraction: 1.,
                            origin: None,
                        },
                        BitFlags::empty(),
                        None,
                        spawn_group_label.as_deref(),
                        None,
                    )
                    .with_context(|| format!("TISP spawn for zone {:?}", zone_name))?;
                }
            }
        }
    }
    Ok(())
}
