/*
Copyright 2024 Eric Stokes.

This file is part of bflib.

bflib is free software: you can redistribute it and/or modify it under
the terms of the GNU Affero Public License as published by the Free
Software Foundation, either version 3 of the License, or (at your
option) any later version.

bflib is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE. See the GNU Affero Public License
for more details.
*/

use super::{objective::Objective, persisted::Persisted};
use crate::msgq::MsgQ;
use bfprotocols::{
    cfg::Cfg,
    db::objective::{ObjectiveId, ObjectiveKind},
};
use dcso3::{
    Color, LuaVec3, Vector2, Vector3,
    trigger::{LineType, MarkId, QuadSpec, SideFilter},
};
use fxhash::FxHashMap;
use log::{info, warn};
use spade::{
    DelaunayTriangulation, HasPosition, Point2, Triangulation,
    handles::VoronoiVertex::Inner,
};
use std::hash::{Hash, Hasher};

const FRONT_LINE_ALPHA: f32 = 0.75;
const MIN_SEGMENT_M: f64 = 500.;
const FRONT_SHAFT_HALF_WIDTH_M: f64 = 200.;
const BISECTOR_HALF_CAP_M: f64 = 35_000.;
const MAX_VORONOI_CHORD_M: f64 = 80_000.;
/// Max quad step along tactical front (must stay under [`MAX_VORONOI_CHORD_M`]).
const TACTICAL_QUAD_STEP_M: f64 = 70_000.;
const MAX_FOCUS_RADIUS_M: f64 = 20_000.;
const TOPO_NODE_SNAP_M: f64 = 1.;
/// Bevel length at sharp polyline joints (provisional F10 line until DCS B-spline).
const CHAMFER_M: f64 = 8_000.;
/// Interior angle sharper than ~30 deg gets a chamfer (cos threshold).
const CHAMFER_SHARP_COS: f64 = 0.866;

#[derive(Debug, Default)]
pub(super) struct FrontLine {
    marks: Vec<MarkId>,
    participant_count: usize,
    owner_revision: u64,
    segment_count: usize,
}

#[derive(Clone, Copy, Debug)]
struct SeedSite {
    id: ObjectiveId,
    pos: Point2<f64>,
}

impl HasPosition for SeedSite {
    type Scalar = f64;

    fn position(&self) -> Point2<f64> {
        self.pos
    }
}

#[derive(Clone, Copy, Debug)]
struct Bbox {
    min: Vector2,
    max: Vector2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct NodeKey(i64, i64);

fn participates(obj: &Objective) -> bool {
    matches!(
        obj.kind,
        ObjectiveKind::Airbase | ObjectiveKind::Fob | ObjectiveKind::Logistics
    )
}

fn to_vec2(p: Point2<f64>) -> Vector2 {
    Vector2::new(p.x, p.y)
}

fn node_key(p: Vector2) -> NodeKey {
    NodeKey(
        (p.x / TOPO_NODE_SNAP_M).round() as i64,
        (p.y / TOPO_NODE_SNAP_M).round() as i64,
    )
}

fn near_contact(p: Vector2, focus: Vector2, span: f64) -> bool {
    na::distance(&p.into(), &focus.into()) <= span * 0.5 + MAX_FOCUS_RADIUS_M
}

fn quad_spec(start: Vector2, end: Vector2) -> QuadSpec {
    let dir = end - start;
    if dir.norm_squared() < 1e-6 {
        let p = LuaVec3(Vector3::new(start.x, 0., start.y));
        return QuadSpec {
            p0: p,
            p1: p,
            p2: p,
            p3: p,
            color: Color::orange(FRONT_LINE_ALPHA),
            fill_color: Color::orange(FRONT_LINE_ALPHA),
            line_type: LineType::NoLine,
            read_only: true,
        };
    }
    let dir = dir.normalize();
    let perp = Vector2::new(-dir.y, dir.x);
    let hw = FRONT_SHAFT_HALF_WIDTH_M;
    let v3 = |p: Vector2| LuaVec3(Vector3::new(p.x, 0., p.y));
    QuadSpec {
        p0: v3(start + perp * hw),
        p1: v3(start - perp * hw),
        p2: v3(end - perp * hw),
        p3: v3(end + perp * hw),
        color: Color::orange(FRONT_LINE_ALPHA),
        fill_color: Color::orange(FRONT_LINE_ALPHA),
        line_type: LineType::NoLine,
        read_only: true,
    }
}

fn collect_sites(persisted: &Persisted) -> Vec<SeedSite> {
    persisted
        .objectives
        .into_iter()
        .filter(|(_, obj)| participates(obj))
        .map(|(id, obj)| {
            let pos = obj.zone.pos();
            SeedSite {
                id: *id,
                pos: Point2::new(pos.x, pos.y),
            }
        })
        .collect()
}

fn bbox_from_sites(sites: &[SeedSite]) -> Bbox {
    let margin = if sites.is_empty() {
        60_000.
    } else {
        let mut min_x = f64::INFINITY;
        let mut min_y = f64::INFINITY;
        let mut max_x = f64::NEG_INFINITY;
        let mut max_y = f64::NEG_INFINITY;
        for site in sites {
            min_x = min_x.min(site.pos.x);
            min_y = min_y.min(site.pos.y);
            max_x = max_x.max(site.pos.x);
            max_y = max_y.max(site.pos.y);
        }
        let span = (max_x - min_x).abs().max((max_y - min_y).abs());
        (span * 0.15).clamp(30_000., 80_000.)
    };
    let mut min_x = f64::INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for site in sites {
        min_x = min_x.min(site.pos.x);
        min_y = min_y.min(site.pos.y);
        max_x = max_x.max(site.pos.x);
        max_y = max_y.max(site.pos.y);
    }
    Bbox {
        min: Vector2::new(min_x - margin, min_y - margin),
        max: Vector2::new(max_x + margin, max_y + margin),
    }
}

fn contact_midpoint(a: &Objective, b: &Objective) -> Vector2 {
    (a.zone.pos() + b.zone.pos()) * 0.5
}

fn local_bisector_segment(a: &Objective, b: &Objective) -> Option<(Vector2, Vector2)> {
    let p0 = a.zone.pos();
    let p1 = b.zone.pos();
    let delta = p1 - p0;
    let len = delta.norm();
    if len < MIN_SEGMENT_M {
        return None;
    }
    let mid = (p0 + p1) * 0.5;
    let perp = Vector2::new(-delta.y, delta.x).normalize();
    let half = (len * 0.45).min(BISECTOR_HALF_CAP_M);
    Some((mid + perp * half, mid - perp * half))
}

fn cap_segment_on_axis(
    start: Vector2,
    end: Vector2,
    focus: Vector2,
    half_len: f64,
) -> Option<(Vector2, Vector2)> {
    let axis = end - start;
    let axis_len = axis.norm();
    if axis_len < MIN_SEGMENT_M {
        return None;
    }
    let axis = axis / axis_len;
    let t = (focus - start).dot(&axis);
    let half = half_len.min(axis_len * 0.5);
    Some((start + axis * (t - half), start + axis * (t + half)))
}

fn clip_segment(p0: Vector2, p1: Vector2, bbox: &Bbox) -> Option<(Vector2, Vector2)> {
    clip_parametric(p0, p1 - p0, bbox, 0., 1.)
}

fn clip_parametric(
    origin: Vector2,
    dir: Vector2,
    bbox: &Bbox,
    t_lo: f64,
    t_hi: f64,
) -> Option<(Vector2, Vector2)> {
    let inv = Vector2::new(
        if dir.x.abs() < 1e-15 {
            f64::INFINITY
        } else {
            1. / dir.x
        },
        if dir.y.abs() < 1e-15 {
            f64::INFINITY
        } else {
            1. / dir.y
        },
    );
    let mut t0 = t_lo;
    let mut t1 = t_hi;
    for i in 0..2 {
        let (o, d, inv_d, min, max) = if i == 0 {
            (origin.x, dir.x, inv.x, bbox.min.x, bbox.max.x)
        } else {
            (origin.y, dir.y, inv.y, bbox.min.y, bbox.max.y)
        };
        if d.abs() < 1e-15 {
            if o < min || o > max {
                return None;
            }
        } else {
            let mut near = (min - o) * inv_d;
            let mut far = (max - o) * inv_d;
            if near > far {
                std::mem::swap(&mut near, &mut far);
            }
            t0 = t0.max(near);
            t1 = t1.min(far);
            if t0 > t1 {
                return None;
            }
        }
    }
    if t1 - t0 < MIN_SEGMENT_M {
        return None;
    }
    Some((origin + dir * t0, origin + dir * t1))
}

/// Finite inner-inner Voronoi edge segment near the seed-pair contact (for topology stitch).
fn contested_voronoi_topo_segment(
    oa: &Objective,
    ob: &Objective,
    vedge: spade::handles::UndirectedVoronoiEdge<'_, SeedSite, (), (), ()>,
    bbox: &Bbox,
) -> Option<(Vector2, Vector2)> {
    let focus = contact_midpoint(oa, ob);
    let span = (oa.zone.pos() - ob.zone.pos()).norm();
    let half = (span * 0.45).min(BISECTOR_HALF_CAP_M);
    let [Inner(from), Inner(to)] = vedge.vertices() else {
        return None;
    };
    let start = to_vec2(from.circumcenter());
    let end = to_vec2(to.circumcenter());
    let chord = na::distance(&start.into(), &end.into());
    if chord < MIN_SEGMENT_M || chord > MAX_VORONOI_CHORD_M {
        return None;
    }
    let (start, end) = clip_segment(start, end, bbox)?;
    let (start, end) = cap_segment_on_axis(start, end, focus, half)?;
    if (end - start).norm() < MIN_SEGMENT_M {
        return None;
    }
    if !near_contact(start, focus, span) || !near_contact(end, focus, span) {
        return None;
    }
    Some((start, end))
}

fn extend_chain(
    chain: &mut Vec<Vector2>,
    from: NodeKey,
    adj: &FxHashMap<NodeKey, Vec<(usize, NodeKey, Vector2)>>,
    used: &mut [bool],
    forward: bool,
) {
    let mut at = from;
    loop {
        let Some((edge_idx, next_key, next_pos)) = adj.get(&at).and_then(|neighbors| {
            neighbors
                .iter()
                .find(|(idx, _, _)| !used[*idx])
                .copied()
        }) else {
            break;
        };
        used[edge_idx] = true;
        if forward {
            chain.push(next_pos);
        } else {
            chain.insert(0, next_pos);
        }
        at = next_key;
    }
}

fn stitch_polylines(segments: &[(Vector2, Vector2)]) -> Vec<Vec<Vector2>> {
    let mut adj: FxHashMap<NodeKey, Vec<(usize, NodeKey, Vector2)>> = FxHashMap::default();
    for (i, (a, b)) in segments.iter().enumerate() {
        let ka = node_key(*a);
        let kb = node_key(*b);
        adj.entry(ka).or_default().push((i, kb, *b));
        adj.entry(kb).or_default().push((i, ka, *a));
    }

    let mut used = vec![false; segments.len()];
    let mut polylines = Vec::new();

    for i in 0..segments.len() {
        if used[i] {
            continue;
        }
        used[i] = true;
        let (a, b) = segments[i];
        let mut chain = vec![a, b];
        extend_chain(&mut chain, node_key(b), &adj, &mut used, true);
        extend_chain(&mut chain, node_key(a), &adj, &mut used, false);
        if chain.len() >= 2 {
            polylines.push(chain);
        }
    }

    polylines
}

#[derive(Clone, Copy, Debug)]
struct TacticalEdge {
    a: ObjectiveId,
    b: ObjectiveId,
    p0: Vector2,
    p1: Vector2,
}

fn segment_midpoint(p0: Vector2, p1: Vector2) -> Vector2 {
    (p0 + p1) * 0.5
}

/// Endpoint farther from `away` (outer cap at chain ends).
fn outer_endpoint(p0: Vector2, p1: Vector2, away: Vector2) -> Vector2 {
    if na::distance(&p0.into(), &away.into()) >= na::distance(&p1.into(), &away.into()) {
        p0
    } else {
        p1
    }
}

/// Dominant axis through bisector midpoints (west → east on Caucasus-style fronts).
fn front_sort_axis(mids: &[Vector2]) -> Vector2 {
    let n = mids.len() as f64;
    let c = mids.iter().fold(Vector2::zeros(), |a, m| a + m) / n;
    let mut sxx = 0.0;
    let mut syy = 0.0;
    let mut sxy = 0.0;
    for m in mids {
        let d = m - c;
        sxx += d.x * d.x;
        syy += d.y * d.y;
        sxy += d.x * d.y;
    }
    let trace = sxx + syy;
    let det = sxx * syy - sxy * sxy;
    let lam = trace * 0.5 + (trace * trace * 0.25 - det).max(0.).sqrt();
    let mut axis = if sxy.abs() > 1e-6 {
        Vector2::new(lam - syy, sxy)
    } else if sxx >= syy {
        Vector2::new(1., 0.)
    } else {
        Vector2::new(0., 1.)
    };
    if axis.norm_squared() < 1e-6 {
        axis = Vector2::new(1., 0.);
    } else {
        axis = axis.normalize();
    }
    if axis.x < 0. {
        axis = -axis;
    }
    axis
}

/// Order bisector ticks along the front arc (projection on principal axis).
fn order_tactical_edges_spatial(edges: &[TacticalEdge]) -> Vec<usize> {
    let n = edges.len();
    if n <= 1 {
        return (0..n).collect();
    }
    let mids: Vec<Vector2> = edges
        .iter()
        .map(|e| segment_midpoint(e.p0, e.p1))
        .collect();
    let axis = front_sort_axis(&mids);
    let c = mids.iter().fold(Vector2::zeros(), |a, m| a + m) / n as f64;
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&i, &j| {
        let pi = (mids[i] - c).dot(&axis);
        let pj = (mids[j] - c).dot(&axis);
        pi.partial_cmp(&pj)
            .unwrap()
            .then_with(|| mids[i].y.partial_cmp(&mids[j].y).unwrap())
    });
    order
}

/// Split long polyline spans so connector quads are not dropped by max chord.
fn densify_polyline(pts: &[Vector2], max_step: f64) -> Vec<Vector2> {
    if pts.len() < 2 {
        return pts.to_vec();
    }
    let mut out = vec![pts[0]];
    for w in pts.windows(2) {
        let a = w[0];
        let b = w[1];
        let len = (b - a).norm();
        if len <= max_step {
            out.push(b);
            continue;
        }
        let steps = (len / max_step).ceil() as u32;
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            out.push(a + (b - a) * t);
        }
    }
    out
}

/// Provisional front polyline: outer endpoints at ends, segment midpoints in between.
fn polyline_via_midpoints(edges: &[TacticalEdge], order: &[usize]) -> Vec<Vector2> {
    match order.len() {
        0 => Vec::new(),
        1 => {
            let e = &edges[order[0]];
            let mid = segment_midpoint(e.p0, e.p1);
            vec![e.p0, mid, e.p1]
        }
        _ => {
            let mids: Vec<Vector2> = order
                .iter()
                .map(|&i| {
                    let e = &edges[i];
                    segment_midpoint(e.p0, e.p1)
                })
                .collect();
            let first = &edges[order[0]];
            let last = &edges[order[order.len() - 1]];
            let start = outer_endpoint(first.p0, first.p1, mids[1]);
            let end = outer_endpoint(last.p0, last.p1, mids[mids.len() - 2]);
            let mut pts = Vec::with_capacity(mids.len() + 2);
            pts.push(start);
            pts.extend(mids);
            pts.push(end);
            pts
        }
    }
}

fn tactical_front_polylines(edges: &[TacticalEdge]) -> Vec<Vec<Vector2>> {
    if edges.is_empty() {
        return Vec::new();
    }
    let order = order_tactical_edges_spatial(edges);
    let poly = densify_polyline(
        &chamfer_polyline(&polyline_via_midpoints(edges, &order)),
        TACTICAL_QUAD_STEP_M,
    );
    if poly.len() >= 2 {
        vec![poly]
    } else {
        Vec::new()
    }
}

/// Replace sharp vertices with a short bevel so quads do not form a miter spike.
fn chamfer_polyline(pts: &[Vector2]) -> Vec<Vector2> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let mut out = vec![pts[0]];
    for i in 1..pts.len() - 1 {
        let a = pts[i - 1];
        let b = pts[i];
        let c = pts[i + 1];
        let v1 = b - a;
        let v2 = c - b;
        let l1 = v1.norm();
        let l2 = v2.norm();
        if l1 < MIN_SEGMENT_M || l2 < MIN_SEGMENT_M {
            out.push(b);
            continue;
        }
        let u1 = v1 / l1;
        let u2 = v2 / l2;
        let dot = u1.dot(&u2).clamp(-1., 1.);
        if dot > CHAMFER_SHARP_COS {
            out.push(b);
            continue;
        }
        let d = CHAMFER_M.min(l1 * 0.45).min(l2 * 0.45);
        if d * 2. < l1.min(l2) {
            out.push(b - u1 * d);
            out.push(b + u2 * d);
        } else {
            out.push(b);
        }
    }
    out.push(*pts.last().unwrap());
    out
}

fn polylines_to_quads(polylines: &[Vec<Vector2>]) -> Vec<QuadSpec> {
    let mut specs = Vec::new();
    for chain in polylines {
        for w in chain.windows(2) {
            let len = na::distance(&w[0].into(), &w[1].into());
            if len >= MIN_SEGMENT_M && len <= MAX_VORONOI_CHORD_M {
                specs.push(quad_spec(w[0], w[1]));
            }
        }
    }
    specs
}

/// Asymmetric tactical pair (one side sees the other as nearest, but not vice versa)
/// is kept only if the pair distance is not more than this factor of the other side's
/// actual nearest-enemy distance. Avoids "stranded" ticks across a mostly empty quadrant.
/// Tuned so that only clearly stranded contacts (ratio ~2x and above) are dropped.
const TACTICAL_DIST_RATIO: f64 = 1.7;

/// For each participating objective, its nearest enemy among participating objectives:
/// `(enemy_id, distance)`.
fn nearest_enemy_map(persisted: &Persisted) -> FxHashMap<ObjectiveId, (ObjectiveId, f64)> {
    let mut map = FxHashMap::default();
    for (id, obj) in &persisted.objectives {
        if !participates(obj) {
            continue;
        }
        let pos = obj.zone.pos();
        let owner = obj.owner;
        let best = persisted
            .objectives
            .into_iter()
            .filter(|(_, other)| participates(other) && other.owner != owner)
            .map(|(eid, other)| (eid, (other.zone.pos() - pos).norm_squared()))
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        if let Some((enemy_id, d2)) = best {
            map.insert(*id, (*enemy_id, d2.sqrt()));
        }
    }
    map
}

/// True when (a, b) is a usable front contact: either mutual nearest enemies,
/// or an asymmetric pair where the pair distance is comparable to the other side's
/// real nearest-enemy distance (rejects "stranded" cross-map ticks).
fn is_tactical_contact(
    a: ObjectiveId,
    b: ObjectiveId,
    pair_dist: f64,
    nearest: &FxHashMap<ObjectiveId, (ObjectiveId, f64)>,
) -> bool {
    let a_sees_b = nearest.get(&a).map(|(id, _)| *id) == Some(b);
    let b_sees_a = nearest.get(&b).map(|(id, _)| *id) == Some(a);
    if a_sees_b && b_sees_a {
        return true;
    }
    if !a_sees_b && !b_sees_a {
        return false;
    }
    let other_actual = if a_sees_b {
        nearest.get(&b).map(|(_, d)| *d).unwrap_or(pair_dist)
    } else {
        nearest.get(&a).map(|(_, d)| *d).unwrap_or(pair_dist)
    };
    pair_dist <= other_actual * TACTICAL_DIST_RATIO
}

fn owner_revision(persisted: &Persisted) -> u64 {
    let mut ids: Vec<ObjectiveId> = persisted
        .objectives
        .into_iter()
        .filter(|(_, obj)| participates(obj))
        .map(|(id, _)| *id)
        .collect();
    ids.sort_unstable();
    let mut hasher = fxhash::FxHasher::default();
    for id in ids {
        id.hash(&mut hasher);
        if let Some(obj) = persisted.objectives.get(&id) {
            obj.owner.hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn build_front_quads(
    persisted: &Persisted,
    tri: &DelaunayTriangulation<SeedSite>,
    bbox: &Bbox,
) -> (Vec<QuadSpec>, usize, usize) {
    let nearest = nearest_enemy_map(persisted);
    let mut contested = 0u32;
    let mut tactical = 0u32;
    let mut topo_segments = Vec::new();
    let mut tactical_edges = Vec::new();

    for vedge in tri.undirected_voronoi_edges() {
        let de = vedge.as_delaunay_edge();
        let [v0, v1] = de.vertices();
        let Some(oa) = persisted.objectives.get(&v0.data().id) else {
            continue;
        };
        let Some(ob) = persisted.objectives.get(&v1.data().id) else {
            continue;
        };
        if oa.owner == ob.owner {
            continue;
        }
        contested += 1;
        let pair_dist = (oa.zone.pos() - ob.zone.pos()).norm();
        if !is_tactical_contact(oa.id, ob.id, pair_dist, &nearest) {
            continue;
        }
        tactical += 1;
        if let Some(seg) = contested_voronoi_topo_segment(oa, ob, vedge, bbox) {
            topo_segments.push(seg);
        } else if let Some((p0, p1)) = local_bisector_segment(oa, ob) {
            tactical_edges.push(TacticalEdge {
                a: oa.id,
                b: ob.id,
                p0,
                p1,
            });
        }
    }

    let mut polylines: Vec<Vec<Vector2>> = stitch_polylines(&topo_segments)
        .into_iter()
        .map(|c| chamfer_polyline(&c))
        .collect();
    polylines.extend(tactical_front_polylines(&tactical_edges));

    let chain_count = polylines.len();
    let specs = polylines_to_quads(&polylines);
    let quad_count = specs.len();

    info!(
        "front line: {} contested, {} tactical, {} topo + {} bisector edge(s), {} chain(s), {} quad(s)",
        contested,
        tactical,
        topo_segments.len(),
        tactical_edges.len(),
        chain_count,
        quad_count
    );

    (specs, chain_count, quad_count)
}

impl FrontLine {
    fn clear(&mut self, msgq: &mut MsgQ) {
        for id in self.marks.drain(..) {
            msgq.delete_mark(id);
        }
        self.participant_count = 0;
        self.owner_revision = 0;
        self.segment_count = 0;
    }

    pub fn sync(&mut self, cfg: &Cfg, persisted: &Persisted, msgq: &mut MsgQ) {
        if !cfg.front_line {
            if !self.marks.is_empty() || self.participant_count > 0 {
                self.clear(msgq);
            }
            return;
        }

        let sites = collect_sites(persisted);
        let participant_count = sites.len();
        if participant_count < 2 {
            if !self.marks.is_empty() || self.participant_count > 0 {
                self.clear(msgq);
            }
            return;
        }

        let revision = owner_revision(persisted);
        let bbox = bbox_from_sites(&sites);
        let tri = match DelaunayTriangulation::<SeedSite>::bulk_load(sites.clone()) {
            Ok(t) => t,
            Err(e) => {
                warn!("front line: could not build Delaunay triangulation: {e:?}");
                return;
            }
        };
        let (want, _chains, seg_count) = build_front_quads(persisted, &tri, &bbox);
        if revision == self.owner_revision
            && participant_count == self.participant_count
            && seg_count == self.segment_count
            && !self.marks.is_empty()
        {
            return;
        }

        for id in self.marks.drain(..) {
            msgq.delete_mark(id);
        }
        for spec in want {
            let id = MarkId::new();
            msgq.quad_to_all(SideFilter::All, id, spec, None);
            self.marks.push(id);
        }

        self.participant_count = participant_count;
        self.owner_revision = revision;
        self.segment_count = seg_count;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::persisted::Persisted;
    use std::{fs::File, path::Path};

    #[test]
    fn caucasus_save_front_line() {
        let path = Path::new(r"C:\Users\Robo\Saved Games\DCS\Rust_Fowl_Engine_2.0_Caucasus1985-SARH");
        if !path.exists() {
            return;
        }
        let file = File::open(path).unwrap();
        let file = zstd::stream::Decoder::new(file).unwrap();
        let persisted: Persisted = serde_json::from_reader(file).unwrap();
        let sites = collect_sites(&persisted);
        let tri = DelaunayTriangulation::<SeedSite>::bulk_load(sites.clone()).unwrap();
        let bbox = bbox_from_sites(&sites);
        let (specs, chains, _) = build_front_quads(&persisted, &tri, &bbox);
        eprintln!("caucasus front: {} quad(s), {} chain(s)", specs.len(), chains);
        assert!(
            specs.len() < 20,
            "tactical filter must drop some Delaunay-only contests, got {}",
            specs.len()
        );
        assert!(chains >= 1, "expected at least one stitched front chain");
        assert!(specs.len() >= 4, "expected drawable front segments");
    }

    #[test]
    fn spatial_order_follows_front_axis() {
        let id = ObjectiveId::from(1);
        let edges = vec![
            TacticalEdge {
                a: id,
                b: ObjectiveId::from(2),
                p0: Vector2::new(300_000., 0.),
                p1: Vector2::new(310_000., 0.),
            },
            TacticalEdge {
                a: id,
                b: ObjectiveId::from(3),
                p0: Vector2::new(0., 0.),
                p1: Vector2::new(10_000., 0.),
            },
            TacticalEdge {
                a: id,
                b: ObjectiveId::from(4),
                p0: Vector2::new(100_000., 0.),
                p1: Vector2::new(110_000., 0.),
            },
        ];
        let order = order_tactical_edges_spatial(&edges);
        assert_eq!(order, vec![1, 2, 0]);
    }

    #[test]
    fn polyline_via_midpoints_uses_outer_caps() {
        let a = ObjectiveId::from(1);
        let b = ObjectiveId::from(2);
        let c = ObjectiveId::from(3);
        let edges = vec![
            TacticalEdge {
                a,
                b,
                p0: Vector2::new(0., 0.),
                p1: Vector2::new(100_000., 0.),
            },
            TacticalEdge {
                a: b,
                b: c,
                p0: Vector2::new(200_000., 0.),
                p1: Vector2::new(300_000., 0.),
            },
        ];
        let pts = polyline_via_midpoints(&edges, &[0, 1]);
        assert_eq!(pts.len(), 4);
        assert!((pts[0].x - 0.).abs() < 1.);
        assert!((pts[1].x - 50_000.).abs() < 1.);
        assert!((pts[2].x - 250_000.).abs() < 1.);
        assert!((pts[3].x - 300_000.).abs() < 1.);
    }

    #[test]
    fn densify_splits_long_span() {
        let pts = vec![Vector2::new(0., 0.), Vector2::new(150_000., 0.)];
        let out = densify_polyline(&pts, 70_000.);
        assert!(out.len() >= 3);
        for w in out.windows(2) {
            assert!(na::distance(&w[0].into(), &w[1].into()) <= 70_000. + 1.);
        }
    }

    #[test]
    fn chamfer_replaces_sharp_corner() {
        let pts = vec![
            Vector2::new(0., 0.),
            Vector2::new(10_000., 0.),
            Vector2::new(10_000., 10_000.),
        ];
        let out = chamfer_polyline(&pts);
        assert!(out.len() >= 4, "sharp 90 deg corner should be beveled");
    }

    #[test]
    fn tactical_contact_mutual_and_asymmetric() {
        let a = ObjectiveId::from(1);
        let b = ObjectiveId::from(2);
        let c = ObjectiveId::from(3);
        let mut map: FxHashMap<ObjectiveId, (ObjectiveId, f64)> = FxHashMap::default();
        // a's nearest is b at 10 km; b's nearest is c at 5 km (a is "stranded")
        map.insert(a, (b, 10_000.));
        map.insert(b, (c, 5_000.));
        map.insert(c, (b, 5_000.));
        // mutual b<->c
        assert!(is_tactical_contact(b, c, 5_000., &map));
        assert!(is_tactical_contact(c, b, 5_000., &map));
        // asymmetric a->b at 10 km vs b's actual 5 km: ratio 2.0 > 1.35 → drop
        assert!(!is_tactical_contact(a, b, 10_000., &map));
        // asymmetric a->b at 6 km vs b's actual 5 km: ratio 1.2 < 1.35 → keep
        assert!(is_tactical_contact(a, b, 6_000., &map));
        // a and c neither sees other → drop
        assert!(!is_tactical_contact(a, c, 50_000., &map));
    }
}
