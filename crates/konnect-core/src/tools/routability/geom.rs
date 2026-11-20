//! Plane geometry for the routability pre-check: obstacle shapes, their
//! intersection with an axis-parallel line (the section-crossing cuts), and a
//! scan-line raster built from the same intersections (the congestion grid
//! and the free-area sums), so the cut and the grid can never disagree about
//! what is blocked.

pub(crate) type Pt = (f64, f64);

/// Which way a cut line runs. `Horizontal` is the line `y = c` (positions
/// along it are x); `Vertical` is `x = c` (positions along it are y).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Axis {
    Horizontal,
    Vertical,
}

impl Axis {
    /// `(along, across)` coordinates of a point for this axis.
    pub(crate) fn split(self, (x, y): Pt) -> (f64, f64) {
        match self {
            Axis::Horizontal => (x, y),
            Axis::Vertical => (y, x),
        }
    }
}

/// An obstacle or region outline.
#[derive(Debug, Clone)]
pub(crate) enum Shape {
    /// A closed polygon, any shape (even-odd rule).
    Poly(Vec<Pt>),
    /// A track: the segment `a`–`b` swept by a disc of radius `r`.
    Capsule { a: Pt, b: Pt, r: f64 },
    Circle { c: Pt, r: f64 },
}

impl Shape {
    pub(crate) fn bbox(&self) -> (f64, f64, f64, f64) {
        match self {
            Shape::Poly(points) => points.iter().fold(
                (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY),
                |(x0, y0, x1, y1), &(x, y)| (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
            ),
            Shape::Capsule { a, b, r } => (a.0.min(b.0) - r, a.1.min(b.1) - r, a.0.max(b.0) + r, a.1.max(b.1) + r),
            Shape::Circle { c, r } => (c.0 - r, c.1 - r, c.0 + r, c.1 + r),
        }
    }

    pub(crate) fn contains(&self, p: Pt) -> bool {
        match self {
            Shape::Poly(points) => crate::tools::fanout::point_in_polygon(p, points),
            Shape::Capsule { a, b, r } => point_segment_distance(p, *a, *b) <= *r,
            Shape::Circle { c, r } => (p.0 - c.0).hypot(p.1 - c.1) <= *r,
        }
    }

    /// The intervals (along the line) where the line `across = c` of `axis`
    /// is inside the shape, sorted and disjoint.
    pub(crate) fn line_intervals(&self, axis: Axis, c: f64) -> Vec<(f64, f64)> {
        match self {
            Shape::Poly(points) => {
                let edges = points.iter().zip(points.iter().cycle().skip(1)).map(|(a, b)| (*a, *b));
                edge_soup_intervals(edges, axis, c)
            }
            Shape::Circle { c: centre, r } => circle_interval(axis.split(*centre), *r, c).into_iter().collect(),
            Shape::Capsule { a, b, r } => {
                // Convex: the union of the two end discs and the body is one
                // interval spanning all three.
                let (a2, b2) = (axis.split(*a), axis.split(*b));
                let mut parts: Vec<(f64, f64)> = Vec::new();
                parts.extend(circle_interval(a2, *r, c));
                parts.extend(circle_interval(b2, *r, c));
                let (dx, dy) = (b2.0 - a2.0, b2.1 - a2.1);
                let len = dx.hypot(dy);
                if len > 1e-12 {
                    let (nx, ny) = (-dy / len * r, dx / len * r);
                    let body = [(a2.0 + nx, a2.1 + ny), (b2.0 + nx, b2.1 + ny), (b2.0 - nx, b2.1 - ny), (a2.0 - nx, a2.1 - ny)];
                    let edges = (0..4).map(|i| (body[i], body[(i + 1) % 4]));
                    parts.extend(edge_soup_intervals(edges, Axis::Horizontal, c));
                }
                if parts.is_empty() {
                    return Vec::new();
                }
                let lo = parts.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
                let hi = parts.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
                vec![(lo, hi)]
            }
        }
    }
}

fn circle_interval((cx, cy): Pt, r: f64, c: f64) -> Option<(f64, f64)> {
    let dy = c - cy;
    (dy.abs() <= r).then(|| {
        let half = (r * r - dy * dy).max(0.0).sqrt();
        (cx - half, cx + half)
    })
}

/// Inside intervals of the line `across = c` against a set of closed loops
/// given as loose edges (even-odd rule). Works for a board outline made of
/// separate `gr_line`s as long as the loops close.
pub(crate) fn edge_soup_intervals(edges: impl Iterator<Item = (Pt, Pt)>, axis: Axis, c: f64) -> Vec<(f64, f64)> {
    let mut xs: Vec<f64> = Vec::new();
    for (p, q) in edges {
        let ((x1, y1), (x2, y2)) = (axis.split(p), axis.split(q));
        if (y1 > c) != (y2 > c) {
            xs.push(x1 + (c - y1) * (x2 - x1) / (y2 - y1));
        }
    }
    xs.sort_by(f64::total_cmp);
    xs.chunks(2).filter(|w| w.len() == 2 && w[1] > w[0]).map(|w| (w[0], w[1])).collect()
}

/// Merge overlapping intervals (input in any order).
pub(crate) fn merge(mut intervals: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    intervals.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<(f64, f64)> = Vec::new();
    for (lo, hi) in intervals {
        match out.last_mut() {
            Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
            _ => out.push((lo, hi)),
        }
    }
    out
}

/// `base` minus the union of `cuts` (both sorted, disjoint after `merge`).
pub(crate) fn subtract(base: &[(f64, f64)], cuts: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    for &(lo, hi) in base {
        let mut start = lo;
        for &(c0, c1) in cuts {
            if c1 <= start || c0 >= hi {
                continue;
            }
            if c0 > start {
                out.push((start, c0));
            }
            start = start.max(c1);
            if start >= hi {
                break;
            }
        }
        if start < hi {
            out.push((start, hi));
        }
    }
    out
}

pub(crate) fn point_segment_distance(p: Pt, a: Pt, b: Pt) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 == 0.0 { 0.0 } else { (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0) };
    (p.0 - a.0 - t * dx).hypot(p.1 - a.1 - t * dy)
}

fn orient(a: Pt, b: Pt, c: Pt) -> f64 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}

/// Proper crossing of two segments: they cross at a point interior to both.
/// Touching, sharing an end point or overlapping collinearly is not a
/// crossing (two airwires meeting at a pad do not conflict).
pub(crate) fn proper_intersection(a: Pt, b: Pt, c: Pt, d: Pt) -> Option<Pt> {
    let scale = 1e-9 * (1.0 + a.0.abs() + a.1.abs()).powi(2);
    let (d1, d2) = (orient(c, d, a), orient(c, d, b));
    let (d3, d4) = (orient(a, b, c), orient(a, b, d));
    let strictly = |u: f64, v: f64| (u > scale && v < -scale) || (u < -scale && v > scale);
    if strictly(d1, d2) && strictly(d3, d4) {
        let t = d1 / (d1 - d2);
        Some((a.0 + t * (b.0 - a.0), a.1 + t * (b.1 - a.1)))
    } else {
        None
    }
}

/// A per-layer occupancy raster: each pixel is outside the board, blocked
/// by an obstacle, or free. Built row by row from [`Shape::line_intervals`]
/// at the pixel centres.
pub(crate) struct Raster {
    pub x0: f64,
    pub y0: f64,
    pub res: f64,
    pub nx: usize,
    pub ny: usize,
    /// Pixel inside the board outline.
    pub inside: Vec<bool>,
    /// Per layer: pixel blocked (only meaningful where `inside`).
    pub blocked: Vec<Vec<bool>>,
}

impl Raster {
    pub(crate) fn new(bbox: (f64, f64, f64, f64), res: f64, layers: usize) -> Self {
        let nx = (((bbox.2 - bbox.0) / res).ceil() as usize).max(1);
        let ny = (((bbox.3 - bbox.1) / res).ceil() as usize).max(1);
        Self { x0: bbox.0, y0: bbox.1, res, nx, ny, inside: vec![false; nx * ny], blocked: vec![vec![false; nx * ny]; layers] }
    }

    fn row_y(&self, j: usize) -> f64 {
        self.y0 + (j as f64 + 0.5) * self.res
    }

    /// Mark the columns of row `j` whose centres fall inside `intervals`.
    fn fill_row(nx: usize, x0: f64, res: f64, row: &mut [bool], intervals: &[(f64, f64)]) {
        for &(lo, hi) in intervals {
            let i0 = ((lo - x0) / res - 0.5).ceil().max(0.0) as usize;
            let i1 = ((hi - x0) / res - 0.5).floor();
            if i1 < 0.0 {
                continue;
            }
            let i1 = (i1 as usize).min(nx.saturating_sub(1));
            for cell in row.iter_mut().take(i1 + 1).skip(i0) {
                *cell = true;
            }
        }
    }

    pub(crate) fn mark_board(&mut self, edges: &[(Pt, Pt)]) {
        for j in 0..self.ny {
            let y = self.row_y(j);
            let intervals = edge_soup_intervals(edges.iter().copied(), Axis::Horizontal, y);
            let (nx, x0, res) = (self.nx, self.x0, self.res);
            Self::fill_row(nx, x0, res, &mut self.inside[j * nx..(j + 1) * nx], &intervals);
        }
    }

    pub(crate) fn mark_obstacle(&mut self, shape: &Shape, layers: &[usize]) {
        let (_, y0, _, y1) = shape.bbox();
        let j0 = ((y0 - self.y0) / self.res - 0.5).ceil().max(0.0) as usize;
        let j1 = ((y1 - self.y0) / self.res - 0.5).floor();
        if j1 < 0.0 {
            return;
        }
        let j1 = (j1 as usize).min(self.ny.saturating_sub(1));
        let (nx, x0, res) = (self.nx, self.x0, self.res);
        for j in j0..=j1 {
            let intervals = shape.line_intervals(Axis::Horizontal, self.row_y(j));
            if intervals.is_empty() {
                continue;
            }
            for &layer in layers {
                Self::fill_row(nx, x0, res, &mut self.blocked[layer][j * nx..(j + 1) * nx], &intervals);
            }
        }
    }

    /// Free area (mm²) of `layer` inside the axis-aligned box.
    pub(crate) fn free_area_in(&self, layer: usize, (bx0, by0, bx1, by1): (f64, f64, f64, f64)) -> f64 {
        let (i0, i1, j0, j1) = self.index_range((bx0, by0, bx1, by1));
        let mut count = 0usize;
        for j in j0..j1 {
            for i in i0..i1 {
                let k = j * self.nx + i;
                if self.inside[k] && !self.blocked[layer][k] {
                    count += 1;
                }
            }
        }
        count as f64 * self.res * self.res
    }

    /// Board area (mm²) inside the axis-aligned box.
    pub(crate) fn inside_area_in(&self, (bx0, by0, bx1, by1): (f64, f64, f64, f64)) -> f64 {
        let (i0, i1, j0, j1) = self.index_range((bx0, by0, bx1, by1));
        let mut count = 0usize;
        for j in j0..j1 {
            for i in i0..i1 {
                if self.inside[j * self.nx + i] {
                    count += 1;
                }
            }
        }
        count as f64 * self.res * self.res
    }

    fn index_range(&self, (bx0, by0, bx1, by1): (f64, f64, f64, f64)) -> (usize, usize, usize, usize) {
        let clamp_x = |x: f64| (((x - self.x0) / self.res).round().max(0.0) as usize).min(self.nx);
        let clamp_y = |y: f64| (((y - self.y0) / self.res).round().max(0.0) as usize).min(self.ny);
        (clamp_x(bx0), clamp_x(bx1), clamp_y(by0), clamp_y(by1))
    }

    pub(crate) fn total_free_area(&self, layer: usize) -> f64 {
        self.inside.iter().zip(&self.blocked[layer]).filter(|(inside, blocked)| **inside && !**blocked).count() as f64
            * self.res
            * self.res
    }

    pub(crate) fn total_inside_area(&self) -> f64 {
        self.inside.iter().filter(|inside| **inside).count() as f64 * self.res * self.res
    }
}
