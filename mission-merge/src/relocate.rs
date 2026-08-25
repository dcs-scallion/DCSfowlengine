use anyhow::Result;
use mlua::{Table, Value};
use std::collections::HashSet;

const CLUSTER_GAP_M: f64 = 5_000.0;
const SLOT_MARGIN_M: f64 = 2_000.0;
/// Parking radius around dest map center (ME-selectable on all theatres).
const SAFE_HALF_M: f64 = 300_000.0;

pub struct RelocateStats {
    pub clusters_moved: usize,
    pub clusters_kept: usize,
}

struct Point {
    x: f64,
    y: f64,
}

struct BBox {
    min_x: f64,
    max_x: f64,
    min_y: f64,
    max_y: f64,
}

impl BBox {
    fn from_points(pts: &[Point]) -> Option<Self> {
        let mut it = pts.iter();
        let first = it.next()?;
        let mut b = Self {
            min_x: first.x,
            max_x: first.x,
            min_y: first.y,
            max_y: first.y,
        };
        for p in it {
            b.min_x = b.min_x.min(p.x);
            b.max_x = b.max_x.max(p.x);
            b.min_y = b.min_y.min(p.y);
            b.max_y = b.max_y.max(p.y);
        }
        Some(b)
    }

    fn width(&self) -> f64 {
        self.max_x - self.min_x
    }

    fn height(&self) -> f64 {
        self.max_y - self.min_y
    }

    fn center(&self) -> Point {
        Point {
            x: (self.min_x + self.max_x) * 0.5,
            y: (self.min_y + self.max_y) * 0.5,
        }
    }

    fn translated(&self, dx: f64, dy: f64) -> Self {
        Self {
            min_x: self.min_x + dx,
            max_x: self.max_x + dx,
            min_y: self.min_y + dy,
            max_y: self.max_y + dy,
        }
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.min_x < other.max_x
            && self.max_x > other.min_x
            && self.min_y < other.max_y
            && self.max_y > other.min_y
    }

    fn fully_inside(&self, vis: &BBox) -> bool {
        self.min_x >= vis.min_x
            && self.max_x <= vis.max_x
            && self.min_y >= vis.min_y
            && self.max_y <= vis.max_y
    }

    fn inflate(&self, m: f64) -> Self {
        Self {
            min_x: self.min_x - m,
            max_x: self.max_x + m,
            min_y: self.min_y - m,
            max_y: self.max_y + m,
        }
    }
}

enum ZoneShape {
    Circle { c: Point, r: f64 },
    Quad { verts: Vec<Point> },
}

impl ZoneShape {
    fn contains(&self, p: &Point) -> bool {
        match self {
            ZoneShape::Circle { c, r } => {
                let dx = p.x - c.x;
                let dy = p.y - c.y;
                dx * dx + dy * dy <= r * r
            }
            ZoneShape::Quad { verts } => point_in_polygon(p, verts),
        }
    }

    fn points(&self) -> Vec<Point> {
        match self {
            ZoneShape::Circle { c, r } => vec![
                Point {
                    x: c.x - r,
                    y: c.y - r,
                },
                Point {
                    x: c.x + r,
                    y: c.y + r,
                },
            ],
            ZoneShape::Quad { verts } => verts
                .iter()
                .map(|p| Point { x: p.x, y: p.y })
                .collect(),
        }
    }
}

struct Uf {
    p: Vec<usize>,
}

impl Uf {
    fn new(n: usize) -> Self {
        Self {
            p: (0..n).collect(),
        }
    }
    fn find(&mut self, x: usize) -> usize {
        let mut x = x;
        while self.p[x] != x {
            self.p[x] = self.p[self.p[x]];
            x = self.p[x];
        }
        x
    }
    fn union(&mut self, a: usize, b: usize) {
        let pa = self.find(a);
        let pb = self.find(b);
        if pa != pb {
            self.p[pa] = pb;
        }
    }
}

pub fn relocate_copied(
    dest: &Table,
    groups: &[Table],
    zones: &[Table],
) -> Result<RelocateStats> {
    let vis = visible_bounds(dest)?;
    let n_z = zones.len();
    let n_g = groups.len();
    let n = n_z + n_g;
    if n == 0 {
        return Ok(RelocateStats {
            clusters_moved: 0,
            clusters_kept: 0,
        });
    }

    let zone_shapes: Vec<Option<ZoneShape>> = zones.iter().map(zone_shape).collect();
    let group_pts: Vec<Vec<Point>> = groups.iter().map(group_points).collect();

    let mut uf = Uf::new(n);
    for (zi, shape) in zone_shapes.iter().enumerate() {
        let Some(shape) = shape else {
            continue;
        };
        for (gi, pts) in group_pts.iter().enumerate() {
            if pts.iter().any(|p| shape.contains(p)) {
                uf.union(zi, n_z + gi);
            }
        }
    }
    for i in 0..n {
        let Some(bi) = item_bbox(i, n_z, &zone_shapes, &group_pts) else {
            continue;
        };
        for j in (i + 1)..n {
            let Some(bj) = item_bbox(j, n_z, &zone_shapes, &group_pts) else {
                continue;
            };
            if bi.inflate(CLUSTER_GAP_M).overlaps(&bj) {
                uf.union(i, j);
            }
        }
    }

    let roots: Vec<usize> = (0..n).map(|i| uf.find(i)).collect();
    let uniq: HashSet<usize> = roots.iter().copied().collect();
    let mut clusters: Vec<Vec<usize>> = Vec::new();
    for r in uniq {
        clusters.push(
            (0..n)
                .filter(|&i| roots[i] == r)
                .collect(),
        );
    }
    for c in &mut clusters {
        c.sort_unstable();
    }
    clusters.sort_by_key(|c| c[0]);

    let mut occupied: Vec<BBox> = Vec::new();
    let mut to_place: Vec<(Vec<usize>, BBox)> = Vec::new();
    let mut kept = 0usize;
    for members in clusters {
        let mut pts = Vec::new();
        for &i in &members {
            if let Some(b) = item_bbox(i, n_z, &zone_shapes, &group_pts) {
                pts.push(Point {
                    x: b.min_x,
                    y: b.min_y,
                });
                pts.push(Point {
                    x: b.max_x,
                    y: b.max_y,
                });
            }
        }
        let Some(bbox) = BBox::from_points(&pts) else {
            continue;
        };
        if bbox.fully_inside(&vis) {
            occupied.push(bbox.inflate(SLOT_MARGIN_M));
            kept += 1;
        } else {
            to_place.push((members, bbox));
        }
    }

    to_place.sort_by(|a, b| {
        let aa = a.1.width() * a.1.height();
        let bb = b.1.width() * b.1.height();
        bb.partial_cmp(&aa).unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut moved = 0usize;
    for (members, bbox) in &to_place {
        let (dx, dy) = slot_for(bbox, &vis, &occupied);
        for &i in members {
            if i < n_z {
                translate_zone(&zones[i], dx, dy)?;
            } else {
                translate_group(&groups[i - n_z], dx, dy)?;
            }
        }
        occupied.push(bbox.translated(dx, dy).inflate(SLOT_MARGIN_M));
        moved += 1;
    }

    Ok(RelocateStats {
        clusters_moved: moved,
        clusters_kept: kept,
    })
}

fn visible_bounds(dest: &Table) -> Result<BBox> {
    let (cx, cy) = match dest.raw_get::<_, Table>("map") {
        Ok(map) => (
            num_of(&map, "centerX").unwrap_or(0.0),
            num_of(&map, "centerY").unwrap_or(0.0),
        ),
        Err(_) => (0.0, 0.0),
    };
    Ok(BBox {
        min_x: cx - SAFE_HALF_M,
        max_x: cx + SAFE_HALF_M,
        min_y: cy - SAFE_HALF_M,
        max_y: cy + SAFE_HALF_M,
    })
}

fn item_bbox(
    i: usize,
    n_z: usize,
    zone_shapes: &[Option<ZoneShape>],
    group_pts: &[Vec<Point>],
) -> Option<BBox> {
    if i < n_z {
        BBox::from_points(&zone_shapes[i].as_ref()?.points())
    } else {
        BBox::from_points(&group_pts[i - n_z])
    }
}

fn slot_for(bbox: &BBox, vis: &BBox, occupied: &[BBox]) -> (f64, f64) {
    let w = bbox.width().max(1.0);
    let h = bbox.height().max(1.0);
    let step_x = (w + SLOT_MARGIN_M).max(8_000.0);
    let step_y = (h + SLOT_MARGIN_M).max(8_000.0);
    let c = bbox.center();
    let vc = vis.center();
    for ring in 0..48 {
        for (col, row) in spiral_ring(ring) {
            let tx = vc.x + col as f64 * step_x;
            let ty = vc.y + row as f64 * step_y;
            let dx = tx - c.x;
            let dy = ty - c.y;
            let placed = bbox.translated(dx, dy);
            if !placed.fully_inside(vis) {
                continue;
            }
            if occupied.iter().any(|o| placed.overlaps(o)) {
                continue;
            }
            return (dx, dy);
        }
    }
    (vc.x - c.x, vc.y - c.y)
}

fn spiral_ring(ring: i32) -> Vec<(i32, i32)> {
    if ring == 0 {
        return vec![(0, 0)];
    }
    let mut out = Vec::with_capacity((ring as usize) * 8);
    for col in -ring..=ring {
        out.push((col, -ring));
    }
    for row in (-ring + 1)..=ring {
        out.push((ring, row));
    }
    for col in (-ring..ring).rev() {
        out.push((col, ring));
    }
    for row in ((-ring + 1)..ring).rev() {
        out.push((-ring, row));
    }
    out
}

fn zone_shape(zone: &Table) -> Option<ZoneShape> {
    let c = xy(zone)?;
    let typ = int_of_tbl(zone, "type").unwrap_or(0);
    if typ == 2 {
        let verts = vertices_of(zone)?;
        if verts.len() >= 3 {
            return Some(ZoneShape::Quad { verts });
        }
    }
    let r = num_of(zone, "radius").unwrap_or(0.0);
    Some(ZoneShape::Circle { c, r })
}

fn vertices_of(zone: &Table) -> Option<Vec<Point>> {
    let tbl: Table = zone.raw_get("verticies").ok()?;
    let mut out = Vec::new();
    for pair in tbl.pairs::<Value, Table>() {
        let (_, v) = pair.ok()?;
        out.push(xy(&v)?);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn group_points(group: &Table) -> Vec<Point> {
    let mut pts = Vec::new();
    collect_xy(group, &mut pts);
    pts
}

fn collect_xy(tbl: &Table, pts: &mut Vec<Point>) {
    if let Some(p) = xy(tbl) {
        pts.push(p);
    }
    if let Ok(pairs) = tbl.clone().pairs::<Value, Value>().collect::<mlua::Result<Vec<_>>>() {
        for (_, v) in pairs {
            if let Value::Table(child) = v {
                collect_xy(&child, pts);
            }
        }
    }
}

fn translate_group(group: &Table, dx: f64, dy: f64) -> Result<()> {
    translate_xy_tree(group, dx, dy)
}

fn translate_zone(zone: &Table, dx: f64, dy: f64) -> Result<()> {
    translate_xy_tree(zone, dx, dy)
}

fn translate_xy_tree(tbl: &Table, dx: f64, dy: f64) -> Result<()> {
    add_xy(tbl, dx, dy)?;
    let children: Vec<Table> = tbl
        .clone()
        .pairs::<Value, Value>()
        .filter_map(|r| r.ok())
        .filter_map(|(_, v)| match v {
            Value::Table(t) => Some(t),
            _ => None,
        })
        .collect();
    for child in children {
        translate_xy_tree(&child, dx, dy)?;
    }
    Ok(())
}

fn add_xy(t: &Table, dx: f64, dy: f64) -> Result<()> {
    if let Some(p) = xy(t) {
        t.raw_set("x", p.x + dx)?;
        t.raw_set("y", p.y + dy)?;
    }
    Ok(())
}

fn xy(t: &Table) -> Option<Point> {
    Some(Point {
        x: num_of(t, "x")?,
        y: num_of(t, "y")?,
    })
}

fn num_of(t: &Table, key: &str) -> Option<f64> {
    match t.raw_get::<_, Value>(key).ok()? {
        Value::Number(n) => Some(n),
        Value::Integer(i) => Some(i as f64),
        _ => None,
    }
}

fn int_of_tbl(t: &Table, key: &str) -> Option<i64> {
    match t.raw_get::<_, Value>(key).ok()? {
        Value::Integer(i) => Some(i),
        Value::Number(n) if n.fract() == 0.0 => Some(n as i64),
        _ => None,
    }
}

fn point_in_polygon(p: &Point, verts: &[Point]) -> bool {
    if verts.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = verts.len() - 1;
    for i in 0..verts.len() {
        let vi = &verts[i];
        let vj = &verts[j];
        if (vi.y > p.y) != (vj.y > p.y) {
            let int_x = vj.x + (vi.x - vj.x) * (p.y - vj.y) / (vi.y - vj.y);
            if p.x < int_x {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}
