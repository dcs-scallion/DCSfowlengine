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
    MizLua,
    coalition::Side,
    land::{Land, SurfaceType},
    Color, LuaVec3, Vector2, Vector3,
    trigger::{LineType, MarkId, QuadSpec, SideFilter},
};
use fxhash::FxHashMap;
use log::info;
use serde_derive::{Deserialize, Serialize};
use std::{
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

const MIN_SEGMENT_M: f64 = 500.;
const FRONT_LINE_HALF_WIDTH_M: f64 = 320.;
const FRONT_LINE_ALPHA: f32 = 0.35;
const MAX_DRAW_CHORD_M: f64 = 80_000.;
const MAX_LINE_STEP_M: f64 = 70_000.;
const TOPO_NODE_SNAP_M: f64 = 1.;
const GRAPH_NODE_SNAP_M: f64 = 500.;
const GRID_CELL_M: f64 = 2_500.;
const MAX_GRID_CELLS: usize = 220;
/// Inset from draw bbox so marks stay inside the mission hull.
const MAP_CLIP_INSET_M: f64 = 500.;
/// Corner turn: |dot| above this => straight, no corner fill.
const CORNER_STRAIGHT_DOT: f64 = 0.95;
/// Corner turn: dot below this => direction reversal (yellow); no diagonal fill.
const CORNER_REVERSE_DOT: f64 = -0.01;

#[derive(Debug, Default)]
pub(super) struct FrontLine {
    marks: Vec<MarkId>,
    participant_count: usize,
    owner_revision: u64,
    segment_count: usize,
    water_grid: Option<WaterGridMask>,
}

#[derive(Clone, Copy, Debug)]
struct Site {
    pos: Vector2,
    owner: Side,
}

#[derive(Clone, Copy, Debug)]
struct Bbox {
    min: Vector2,
    max: Vector2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct NodeKey(i64, i64);

/// Which coalition owns the cell on each side of a grid wall.
#[derive(Clone, Copy, Debug)]
enum WallAxis {
    /// Vertical wall between cell `(i,j)` and `(i+1,j)`; `left` owns the west cell.
    Vertical { left: Side },
    /// Horizontal wall between `(i,j)` and `(i,j+1)`; `bottom` owns the south cell.
    Horizontal { bottom: Side },
}

/// Grid wall between opposing coalition cells.
#[derive(Clone, Copy, Debug)]
struct WallEdge {
    a: Vector2,
    b: Vector2,
    axis: WallAxis,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WaterGridExport {
    pub schema_version: u32,
    pub theatre: String,
    pub front_line_grid_size_meters: f64,
    pub min_x: f64,
    pub min_y: f64,
    #[serde(default)]
    pub cell_w: f64,
    #[serde(default)]
    pub cell_h: f64,
    pub nx: usize,
    pub ny: usize,
    pub cells: Vec<u8>,
}

#[derive(Debug, Clone)]
struct WaterGridMask {
    min_x: f64,
    min_y: f64,
    cell_w: f64,
    cell_h: f64,
    nx: usize,
    ny: usize,
    cells: Vec<u8>,
}

impl WaterGridMask {
    fn from_export(doc: WaterGridExport) -> Option<Self> {
        if doc.nx == 0 || doc.ny == 0 || doc.cells.len() != doc.nx * doc.ny {
            return None;
        }
        Some(Self {
            min_x: doc.min_x,
            min_y: doc.min_y,
            cell_w: if doc.cell_w > 0. {
                doc.cell_w
            } else {
                doc.front_line_grid_size_meters
            },
            cell_h: if doc.cell_h > 0. {
                doc.cell_h
            } else {
                doc.front_line_grid_size_meters
            },
            nx: doc.nx,
            ny: doc.ny,
            cells: doc.cells,
        })
    }

    fn is_land_at(&self, p: Vector2) -> bool {
        if self.cell_w <= 0. || self.cell_h <= 0. {
            return true;
        }
        let ix = ((p.x - self.min_x) / self.cell_w).floor() as isize;
        let iy = ((p.y - self.min_y) / self.cell_h).floor() as isize;
        if ix < 0 || iy < 0 || ix >= self.nx as isize || iy >= self.ny as isize {
            return true;
        }
        let idx = ix as usize + iy as usize * self.nx;
        self.cells.get(idx).copied().unwrap_or(1) != 0
    }
}

/// Unit vector from the wall into the cell owned by `coalition`.
fn inward_into_coalition(edge: &WallEdge, coalition: Side) -> Vector2 {
    match edge.axis {
        WallAxis::Vertical { left } => {
            if coalition == left {
                Vector2::new(-1., 0.)
            } else {
                Vector2::new(1., 0.)
            }
        }
        WallAxis::Horizontal { bottom } => {
            if coalition == bottom {
                Vector2::new(0., -1.)
            } else {
                Vector2::new(0., 1.)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CornerKind {
    Straight,
    Convex,
    Reverse,
}

fn participates(obj: &Objective) -> bool {
    matches!(
        obj.kind,
        ObjectiveKind::Airbase | ObjectiveKind::Fob | ObjectiveKind::Logistics
    )
}

fn collect_sites(persisted: &Persisted) -> Vec<Site> {
    persisted
        .objectives
        .into_iter()
        .filter(|(_, obj)| participates(obj))
        .map(|(_, obj)| Site {
            pos: obj.zone.pos(),
            owner: obj.owner,
        })
        .collect()
}

fn bbox_from_sites(sites: &[Site]) -> Bbox {
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

fn clip_bbox(bbox: &Bbox) -> Bbox {
    Bbox {
        min: Vector2::new(
            bbox.min.x + MAP_CLIP_INSET_M,
            bbox.min.y + MAP_CLIP_INSET_M,
        ),
        max: Vector2::new(
            bbox.max.x - MAP_CLIP_INSET_M,
            bbox.max.y - MAP_CLIP_INSET_M,
        ),
    }
}

fn clamp_point(p: Vector2, clip: &Bbox) -> Vector2 {
    if clip.min.x > clip.max.x || clip.min.y > clip.max.y {
        return p;
    }
    Vector2::new(
        p.x.clamp(clip.min.x, clip.max.x),
        p.y.clamp(clip.min.y, clip.max.y),
    )
}

fn clip_segment_to_bbox(p0: Vector2, p1: Vector2, clip: &Bbox) -> Option<(Vector2, Vector2)> {
    if clip.min.x > clip.max.x || clip.min.y > clip.max.y {
        return None;
    }
    let mut t0 = 0_f64;
    let mut t1 = 1_f64;
    let dx = p1.x - p0.x;
    let dy = p1.y - p0.y;
    for (p, d, lo, hi) in [
        (p0.x, dx, clip.min.x, clip.max.x),
        (p0.y, dy, clip.min.y, clip.max.y),
    ] {
        if d.abs() < 1e-12 {
            if p < lo || p > hi {
                return None;
            }
        } else {
            let mut t_lo = (lo - p) / d;
            let mut t_hi = (hi - p) / d;
            if t_lo > t_hi {
                std::mem::swap(&mut t_lo, &mut t_hi);
            }
            t0 = t0.max(t_lo);
            t1 = t1.min(t_hi);
            if t0 > t1 {
                return None;
            }
        }
    }
    if t1 - t0 < 1e-6 {
        return None;
    }
    Some((p0 + Vector2::new(dx, dy) * t0, p0 + Vector2::new(dx, dy) * t1))
}

fn grid_dims(bbox: &Bbox, cell_m: f64) -> (usize, usize, f64, f64) {
    let span_x = (bbox.max.x - bbox.min.x).max(cell_m);
    let span_y = (bbox.max.y - bbox.min.y).max(cell_m);
    let nx = (span_x / cell_m).ceil() as usize;
    let ny = (span_y / cell_m).ceil() as usize;
    let cell_w = span_x / nx as f64;
    let cell_h = span_y / ny as f64;
    (nx.max(1), ny.max(1), cell_w, cell_h)
}

fn nearest_owner(pos: Vector2, sites: &[Site]) -> Side {
    let mut best: Option<(Side, f64)> = None;
    for site in sites {
        let d2 = (site.pos - pos).norm_squared();
        if best.map_or(true, |(_, bd)| d2 < bd) {
            best = Some((site.owner, d2));
        }
    }
    best.map(|(o, _)| o).unwrap_or(Side::Neutral)
}

fn is_front_wall(a: Side, b: Side) -> bool {
    matches!(
        (a, b),
        (Side::Red, Side::Blue) | (Side::Blue, Side::Red)
    )
}

fn fill_owner_grid(
    bbox: &Bbox,
    sites: &[Site],
    nx: usize,
    ny: usize,
    cell_w: f64,
    cell_h: f64,
) -> Vec<Side> {
    let mut grid = vec![Side::Neutral; nx * ny];
    for j in 0..ny {
        for i in 0..nx {
            let cx = bbox.min.x + (i as f64 + 0.5) * cell_w;
            let cy = bbox.min.y + (j as f64 + 0.5) * cell_h;
            grid[i + j * nx] = nearest_owner(Vector2::new(cx, cy), sites);
        }
    }
    grid
}

fn grid_wall_edges(
    bbox: &Bbox,
    grid: &[Side],
    nx: usize,
    ny: usize,
    cell_w: f64,
    cell_h: f64,
) -> Vec<WallEdge> {
    let idx = |i: usize, j: usize| grid[i + j * nx];
    let mut walls = Vec::new();
    for j in 0..ny {
        for i in 0..nx {
            let o = idx(i, j);
            if i + 1 < nx {
                let o2 = idx(i + 1, j);
                if is_front_wall(o, o2) {
                    let x = bbox.min.x + (i + 1) as f64 * cell_w;
                    let y0 = bbox.min.y + j as f64 * cell_h;
                    let y1 = bbox.min.y + (j + 1) as f64 * cell_h;
                    walls.push(WallEdge {
                        a: Vector2::new(x, y0),
                        b: Vector2::new(x, y1),
                        axis: WallAxis::Vertical { left: o },
                    });
                }
            }
            if j + 1 < ny {
                let o2 = idx(i, j + 1);
                if is_front_wall(o, o2) {
                    let y = bbox.min.y + (j + 1) as f64 * cell_h;
                    let x0 = bbox.min.x + i as f64 * cell_w;
                    let x1 = bbox.min.x + (i + 1) as f64 * cell_w;
                    walls.push(WallEdge {
                        a: Vector2::new(x0, y),
                        b: Vector2::new(x1, y),
                        axis: WallAxis::Horizontal { bottom: o },
                    });
                }
            }
        }
    }
    walls
}

fn graph_node_key(p: Vector2) -> NodeKey {
    NodeKey(
        (p.x / GRAPH_NODE_SNAP_M).round() as i64,
        (p.y / GRAPH_NODE_SNAP_M).round() as i64,
    )
}

fn pick_next_edge(
    at: NodeKey,
    prev: Vector2,
    nodes: &FxHashMap<NodeKey, Vector2>,
    adj: &FxHashMap<NodeKey, Vec<(usize, NodeKey)>>,
    used: &[bool],
) -> Option<(usize, NodeKey)> {
    let at_pos = nodes.get(&at)?;
    let incoming = at_pos - prev;
    let in_len = incoming.norm();
    let in_dir = if in_len > 1e-3 {
        incoming / in_len
    } else {
        Vector2::zeros()
    };
    let mut best: Option<(usize, NodeKey, f64)> = None;
    for &(idx, next_k) in adj.get(&at)? {
        if used[idx] {
            continue;
        }
        let next_pos = nodes.get(&next_k)?;
        let outgoing = *next_pos - at_pos;
        let out_len = outgoing.norm();
        if out_len < 1e-3 {
            continue;
        }
        let score = if in_len > 1e-3 {
            in_dir.dot(&(outgoing / out_len))
        } else {
            0.
        };
        if best.map_or(true, |(_, _, s)| score > s) {
            best = Some((idx, next_k, score));
        }
    }
    best.map(|(idx, next_k, _)| (idx, next_k))
}

fn extend_chain_turn(
    chain: &mut Vec<Vector2>,
    at: NodeKey,
    prev: Vector2,
    nodes: &FxHashMap<NodeKey, Vector2>,
    adj: &FxHashMap<NodeKey, Vec<(usize, NodeKey)>>,
    used: &mut [bool],
    forward: bool,
) {
    let mut at = at;
    let mut prev = prev;
    loop {
        let Some((edge_idx, next_k)) = pick_next_edge(at, prev, nodes, adj, used) else {
            break;
        };
        used[edge_idx] = true;
        let next_pos = nodes[&next_k];
        if forward {
            chain.push(next_pos);
        } else {
            chain.insert(0, next_pos);
        }
        prev = nodes[&at];
        at = next_k;
    }
}

fn stitch_polylines(segments: &[(Vector2, Vector2)]) -> Vec<Vec<Vector2>> {
    if segments.is_empty() {
        return Vec::new();
    }
    let mut nodes: FxHashMap<NodeKey, Vector2> = FxHashMap::default();
    for &(a, b) in segments {
        nodes.entry(graph_node_key(a)).or_insert(a);
        nodes.entry(graph_node_key(b)).or_insert(b);
    }

    let mut adj: FxHashMap<NodeKey, Vec<(usize, NodeKey)>> = FxHashMap::default();
    for (i, (a, b)) in segments.iter().enumerate() {
        let ka = graph_node_key(*a);
        let kb = graph_node_key(*b);
        adj.entry(ka).or_default().push((i, kb));
        adj.entry(kb).or_default().push((i, ka));
    }

    let mut used = vec![false; segments.len()];
    let mut polylines = Vec::new();

    let mut endpoints: Vec<NodeKey> = nodes
        .keys()
        .copied()
        .filter(|k| {
            adj.get(k)
                .map(|list| list.iter().filter(|(idx, _)| !used[*idx]).count())
                .unwrap_or(0)
                == 1
        })
        .collect();
    endpoints.sort_by_key(|k| {
        let p = nodes[k];
        (p.x.to_bits(), p.y.to_bits())
    });

    for start in endpoints {
        while let Some((edge_idx, _)) = adj.get(&start).and_then(|list| {
            list.iter().find(|(idx, _)| !used[*idx]).copied()
        }) {
            if used[edge_idx] {
                break;
            }
            used[edge_idx] = true;
            let (a, b) = segments[edge_idx];
            let ka = graph_node_key(a);
            let kb = graph_node_key(b);
            let pa = nodes[&ka];
            let pb = nodes[&kb];
            let mut chain = vec![pa, pb];
            extend_chain_turn(&mut chain, kb, pa, &nodes, &adj, &mut used, true);
            extend_chain_turn(&mut chain, ka, pb, &nodes, &adj, &mut used, false);
            if chain.len() >= 2 {
                polylines.push(chain);
            }
        }
    }

    for i in 0..segments.len() {
        if used[i] {
            continue;
        }
        used[i] = true;
        let (a, b) = segments[i];
        let ka = graph_node_key(a);
        let kb = graph_node_key(b);
        let pa = nodes[&ka];
        let pb = nodes[&kb];
        let mut chain = vec![pa, pb];
        extend_chain_turn(&mut chain, kb, pa, &nodes, &adj, &mut used, true);
        extend_chain_turn(&mut chain, ka, pb, &nodes, &adj, &mut used, false);
        if chain.len() >= 2 {
            polylines.push(chain);
        }
    }

    polylines
}

fn find_wall_edge<'a>(edges: &'a [WallEdge], p0: Vector2, p1: Vector2) -> Option<&'a WallEdge> {
    let k0 = graph_node_key(p0);
    let k1 = graph_node_key(p1);
    edges.iter().find(|e| {
        (graph_node_key(e.a) == k0 && graph_node_key(e.b) == k1)
            || (graph_node_key(e.a) == k1 && graph_node_key(e.b) == k0)
    })
}

fn unit_dir(a: Vector2, b: Vector2) -> Vector2 {
    let d = b - a;
    let n = d.norm();
    if n < 1e-6 {
        Vector2::zeros()
    } else {
        d / n
    }
}

fn corner_kind(d1: Vector2, d2: Vector2) -> CornerKind {
    let n1 = d1.norm();
    let n2 = d2.norm();
    if n1 < 1e-6 || n2 < 1e-6 {
        return CornerKind::Straight;
    }
    let dot = (d1 / n1).dot(&(d2 / n2));
    if dot < CORNER_REVERSE_DOT {
        CornerKind::Reverse
    } else if dot > CORNER_STRAIGHT_DOT {
        CornerKind::Straight
    } else {
        CornerKind::Convex
    }
}

fn wall_endpoints(edge: &WallEdge, forward: bool) -> (Vector2, Vector2) {
    if forward {
        (edge.a, edge.b)
    } else {
        (edge.b, edge.a)
    }
}

fn ribbon_line_for_coalition(
    edge: &WallEdge,
    forward: bool,
    sep: f64,
    coalition: Side,
) -> (Vector2, Vector2) {
    let (a, b) = wall_endpoints(edge, forward);
    let off = inward_into_coalition(edge, coalition) * sep;
    (a + off, b + off)
}

fn vec2_to_v3(p: Vector2) -> LuaVec3 {
    LuaVec3(Vector3::new(p.x, 0., p.y))
}

fn side_color(side: Side) -> Color {
    match side {
        Side::Red => Color::red(FRONT_LINE_ALPHA),
        Side::Blue => Color::blue(FRONT_LINE_ALPHA),
        Side::Neutral => Color::black(FRONT_LINE_ALPHA),
    }
}

fn quad_colored(start: Vector2, end: Vector2, color: Color) -> QuadSpec {
    let dir = end - start;
    if dir.norm_squared() < 1e-6 {
        let p = vec2_to_v3(start);
        return QuadSpec {
            p0: p,
            p1: p,
            p2: p,
            p3: p,
            color,
            fill_color: color,
            line_type: LineType::NoLine,
            read_only: true,
        };
    }
    let dir = dir.normalize();
    let perp = Vector2::new(-dir.y, dir.x);
    let hw = FRONT_LINE_HALF_WIDTH_M;
    let v3 = |p: Vector2| vec2_to_v3(p);
    QuadSpec {
        p0: v3(start + perp * hw),
        p1: v3(start - perp * hw),
        p2: v3(end - perp * hw),
        p3: v3(end + perp * hw),
        color,
        fill_color: color,
        line_type: LineType::NoLine,
        read_only: true,
    }
}

fn push_clipped_segment(
    out: &mut Vec<QuadSpec>,
    start: Vector2,
    end: Vector2,
    color: Color,
    clip: &Bbox,
    water_grid: Option<&WaterGridMask>,
) {
    let start = clamp_point(start, clip);
    let end = clamp_point(end, clip);
    let Some((start, end)) = clip_segment_to_bbox(start, end, clip) else {
        return;
    };
    let len = (end - start).norm();
    if len < MIN_SEGMENT_M {
        return;
    }
    let dir = (end - start) / len;
    let parts = if len > MAX_DRAW_CHORD_M {
        (len / MAX_LINE_STEP_M).ceil() as usize
    } else {
        1
    }
    .max(1);
    for i in 0..parts {
        let t0 = i as f64 / parts as f64;
        let t1 = (i + 1) as f64 / parts as f64;
        let s = start + dir * (len * t0);
        let e = start + dir * (len * t1);
        if (e - s).norm() < MIN_SEGMENT_M {
            continue;
        }
        if let Some(mask) = water_grid {
            let mid = (s + e) * 0.5;
            if !mask.is_land_at(mid) {
                continue;
            }
        }
        if (e - s).norm() > MAX_DRAW_CHORD_M {
            continue;
        }
        out.push(quad_colored(s, e, color));
    }
}

fn ribbon_quads_for_chain(
    out: &mut Vec<QuadSpec>,
    chain: &[Vector2],
    edges: &[WallEdge],
    sep: f64,
    clip: &Bbox,
    water_grid: Option<&WaterGridMask>,
) {
    if chain.len() < 2 {
        return;
    }
    let mut edge_data: Vec<(WallEdge, bool)> = Vec::new();
    for w in chain.windows(2) {
        let Some(e) = find_wall_edge(edges, w[0], w[1]) else {
            return;
        };
        let forward = graph_node_key(e.a) == graph_node_key(w[0]);
        edge_data.push((*e, forward));
    }

    for coalition in [Side::Red, Side::Blue] {
        let color = side_color(coalition);
        let mut merged: Vec<(Vector2, Vector2)> = Vec::new();
        for (edge, forward) in &edge_data {
            let (seg_start, seg_end) = ribbon_line_for_coalition(edge, *forward, sep, coalition);
            if let Some((cur_start, cur_end)) = merged.last_mut() {
                let prev_dir = unit_dir(*cur_start, *cur_end);
                let next_dir = unit_dir(seg_start, seg_end);
                let colinear = prev_dir.dot(&next_dir) > 0.999
                    && (prev_dir.x * next_dir.y - prev_dir.y * next_dir.x).abs() < 1e-6;
                let connected = (*cur_end - seg_start).norm() < 1e-3;
                if colinear && connected {
                    *cur_end = seg_end;
                    continue;
                }
            }
            merged.push((seg_start, seg_end));
        }
        for (start, end) in merged {
            push_clipped_segment(out, start, end, color, clip, water_grid);
        }
    }
}

fn build_front_quads(
    sites: &[Site],
    bbox: &Bbox,
    grid_size_m: f64,
    water_grid: Option<&WaterGridMask>,
) -> (Vec<QuadSpec>, usize, usize, f64, usize, usize, usize) {
    let clip = clip_bbox(bbox);
    let mut cell_m = grid_size_m;
    let (mut nx, mut ny, _, _) = grid_dims(bbox, cell_m);
    while nx > MAX_GRID_CELLS || ny > MAX_GRID_CELLS {
        cell_m *= 1.25;
        (nx, ny, _, _) = grid_dims(bbox, cell_m);
    }
    let (_, _, cell_w, cell_h) = grid_dims(bbox, cell_m);
    let sep = (cell_w.min(cell_h) * 0.22_f64).clamp(200_f64, 900_f64);

    let grid = fill_owner_grid(bbox, sites, nx, ny, cell_w, cell_h);
    let wall_edges = grid_wall_edges(bbox, &grid, nx, ny, cell_w, cell_h);
    let wall_count = wall_edges.len();

    let seg_pairs: Vec<(Vector2, Vector2)> = wall_edges.iter().map(|e| (e.a, e.b)).collect();
    let chains = stitch_polylines(&seg_pairs);

    let mut specs = Vec::new();
    for chain in &chains {
        if chain.len() < 2 {
            continue;
        }
        ribbon_quads_for_chain(&mut specs, chain, &wall_edges, sep, &clip, water_grid);
    }

    let chain_count = chains.len();
    let quad_count = specs.len();

    info!(
        "front line: grid {}x{} @ {:.0}m, sep {:.0}m, {} wall(s), {} chain(s), {} quad(s)",
        nx, ny, cell_m, sep, wall_count, chain_count, quad_count
    );

    (specs, nx, ny, cell_m, wall_count, chain_count, quad_count)
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

fn grid_size_label_meters(cell_m: f64) -> String {
    if (cell_m - cell_m.round()).abs() < 1e-6 {
        format!("{:.0}", cell_m)
    } else {
        format!("{:.1}", cell_m).replace('.', "p")
    }
}

pub(crate) fn water_grid_export_file_name(cell_m: f64, theatre: &str) -> String {
    format!(
        "fowl_water_grid_{}_export_{}.json",
        grid_size_label_meters(cell_m),
        theatre
    )
}

pub(crate) fn water_grid_export_path(state_path: &Path, cell_m: f64, theatre: &str) -> PathBuf {
    state_path.with_file_name(water_grid_export_file_name(cell_m, theatre))
}

pub(crate) fn export_water_grid(
    cfg: &Cfg,
    persisted: &Persisted,
    state_path: &Path,
    theatre: &str,
    lua: MizLua,
) -> anyhow::Result<PathBuf> {
    let sites = collect_sites(persisted);
    if sites.is_empty() {
        anyhow::bail!("no objectives available for water grid scan");
    }
    let bbox = bbox_from_sites(&sites);
    let cell_m = cfg.front_line_grid_size_meters;
    if cell_m <= 0. {
        anyhow::bail!("front_line_grid_size_meters must be > 0");
    }
    let (nx, ny, cell_w, cell_h) = grid_dims(&bbox, cell_m);
    let land = Land::singleton(lua)?;
    let mut cells = Vec::with_capacity(nx * ny);
    for j in 0..ny {
        for i in 0..nx {
            let x = bbox.min.x + (i as f64 + 0.5) * cell_w;
            let y = bbox.min.y + (j as f64 + 0.5) * cell_h;
            let st = land.get_surface_type(dcso3::LuaVec2(Vector2::new(x, y)))?;
            let is_land = !matches!(st, SurfaceType::Water | SurfaceType::ShallowWater);
            cells.push(if is_land { 1 } else { 0 });
        }
    }
    let doc = WaterGridExport {
        schema_version: 1,
        theatre: theatre.to_string(),
        front_line_grid_size_meters: cell_m,
        min_x: bbox.min.x,
        min_y: bbox.min.y,
        cell_w,
        cell_h,
        nx,
        ny,
        cells,
    };
    let out = water_grid_export_path(state_path, cell_m, theatre);
    std::fs::write(&out, serde_json::to_string_pretty(&doc)?)?;
    Ok(out)
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
        let (want, _nx, _ny, _cell_m, seg_count, _chains, _quads) =
            build_front_quads(
                &sites,
                &bbox,
                cfg.front_line_grid_size_meters,
                self.water_grid.as_ref(),
            );
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

    pub fn load_water_grid_from_file(&mut self, state_path: &Path, theatre: &str, cfg: &Cfg) {
        let path = water_grid_export_path(state_path, cfg.front_line_grid_size_meters, theatre);
        let loaded = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<WaterGridExport>(&raw).ok())
            .and_then(|doc| {
                if doc.theatre != theatre {
                    return None;
                }
                if (doc.front_line_grid_size_meters - cfg.front_line_grid_size_meters).abs() > 1e-6 {
                    return None;
                }
                WaterGridMask::from_export(doc)
            });
        if loaded.is_some() {
            info!("front line: loaded water grid from {:?}", path);
        }
        self.water_grid = loaded;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::persisted::Persisted;
    use std::{fs::File, path::Path};

    #[test]
    fn corner_kind_reverse_detected() {
        assert_eq!(
            corner_kind(Vector2::new(1., 0.), Vector2::new(-1., 0.)),
            CornerKind::Reverse
        );
    }

    #[test]
    fn corner_kind_convex_detected() {
        assert_eq!(
            corner_kind(Vector2::new(1., 0.), Vector2::new(0., 1.)),
            CornerKind::Convex
        );
    }

    #[test]
    fn convex_corner_keeps_sharp_right_angle() {
        let sep = 400.;
        let clip = clip_bbox(&Bbox {
            min: Vector2::new(0., 0.),
            max: Vector2::new(50_000., 50_000.),
        });
        let e0 = WallEdge {
            a: Vector2::new(0., 0.),
            b: Vector2::new(10_000., 0.),
            axis: WallAxis::Horizontal { bottom: Side::Red },
        };
        let e1 = WallEdge {
            a: Vector2::new(10_000., 0.),
            b: Vector2::new(10_000., 8_000.),
            axis: WallAxis::Vertical { left: Side::Red },
        };
        let chain = vec![
            Vector2::new(0., 0.),
            Vector2::new(10_000., 0.),
            Vector2::new(10_000., 8_000.),
        ];
        let edges = [e0, e1];
        let mut specs = Vec::new();
        ribbon_quads_for_chain(&mut specs, &chain, &edges, sep, &clip, None);
        assert_eq!(
            specs.len(),
            4,
            "no chamfer bridge: two sharp legs per coalition"
        );
    }

    #[test]
    fn ribbon_colors_follow_cell_owners_not_map_north() {
        let sep = 500.;
        let bbox = Bbox {
            min: Vector2::new(0., 0.),
            max: Vector2::new(30_000., 30_000.),
        };
        let clip = clip_bbox(&bbox);

        // Red south, blue north (editor flipped layout).
        let south = [
            Site {
                pos: Vector2::new(15_000., 5_000.),
                owner: Side::Red,
            },
            Site {
                pos: Vector2::new(15_000., 25_000.),
                owner: Side::Blue,
            },
        ];
        let grid_s = fill_owner_grid(&bbox, &south, 12, 12, 2_500., 2_500.);
        let walls_s = grid_wall_edges(&bbox, &grid_s, 12, 12, 2_500., 2_500.);
        let horiz_s = walls_s
            .iter()
            .find(|e| matches!(e.axis, WallAxis::Horizontal { .. }))
            .expect("horizontal wall");
        let (r0, r1) = ribbon_line_for_coalition(horiz_s, true, sep, Side::Red);
        let (b0, b1) = ribbon_line_for_coalition(horiz_s, true, sep, Side::Blue);
        let ry = (r0.y + r1.y) * 0.5;
        let by = (b0.y + b1.y) * 0.5;
        assert!(
            ry < by,
            "red ribbon must sit on red (southern) side of wall, got red_y={ry} blue_y={by}"
        );
        let mid = horiz_s.a.y;
        let to_red = south[0].pos - Vector2::new(mid, ry);
        let to_blue = south[1].pos - Vector2::new(mid, by);
        let in_red = inward_into_coalition(horiz_s, Side::Red);
        let in_blue = inward_into_coalition(horiz_s, Side::Blue);
        assert!(to_red.x * in_red.x + to_red.y * in_red.y > 0.);
        assert!(to_blue.x * in_blue.x + to_blue.y * in_blue.y > 0.);

        // Red north, blue south.
        let north = [
            Site {
                pos: Vector2::new(15_000., 25_000.),
                owner: Side::Red,
            },
            Site {
                pos: Vector2::new(15_000., 5_000.),
                owner: Side::Blue,
            },
        ];
        let grid_n = fill_owner_grid(&bbox, &north, 12, 12, 2_500., 2_500.);
        let walls_n = grid_wall_edges(&bbox, &grid_n, 12, 12, 2_500., 2_500.);
        let horiz_n = walls_n
            .iter()
            .find(|e| matches!(e.axis, WallAxis::Horizontal { .. }))
            .expect("horizontal wall");
        let (r0, r1) = ribbon_line_for_coalition(horiz_n, true, sep, Side::Red);
        let (b0, b1) = ribbon_line_for_coalition(horiz_n, true, sep, Side::Blue);
        let ry = (r0.y + r1.y) * 0.5;
        let by = (b0.y + b1.y) * 0.5;
        assert!(
            ry > by,
            "red ribbon must sit on red (northern) side of wall, got red_y={ry} blue_y={by}"
        );
        let _ = clip;
    }

    #[test]
    fn grid_wall_between_two_sides() {
        let sites = [
            Site {
                pos: Vector2::new(0., 0.),
                owner: Side::Red,
            },
            Site {
                pos: Vector2::new(20_000., 0.),
                owner: Side::Blue,
            },
        ];
        let bbox = bbox_from_sites(&sites);
        let (specs, _, _, _, wall_count, chains, _) =
            build_front_quads(&sites, &bbox, GRID_CELL_M, None);
        assert!(wall_count >= 1);
        assert!(chains >= 1);
        assert!(specs.len() >= 2, "red + blue segments");
    }

    #[test]
    fn clip_keeps_segment_inside_box() {
        let bbox = Bbox {
            min: Vector2::new(0., 0.),
            max: Vector2::new(100_000., 100_000.),
        };
        let clip = clip_bbox(&bbox);
        let seg = clip_segment_to_bbox(
            Vector2::new(-10_000., 50_000.),
            Vector2::new(110_000., 50_000.),
            &clip,
        );
        assert!(seg.is_some());
        let (a, b) = seg.unwrap();
        assert!(a.x >= clip.min.x);
        assert!(b.x <= clip.max.x);
    }

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
        let bbox = bbox_from_sites(&sites);
        let (specs, _nx, _ny, _cell, wall_count, chains, _) =
            build_front_quads(&sites, &bbox, GRID_CELL_M, None);
        assert!(wall_count >= 10, "grid walls on caucasus save, got {wall_count}");
        assert!(chains >= 1);
        assert!(specs.len() >= 20, "ribbon quads, got {}", specs.len());
    }
}
