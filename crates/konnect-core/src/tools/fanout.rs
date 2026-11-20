//! Plane fan-out planning: for each surface-mount pad on a plane net, a short
//! stub and a through via (or, on a large pad, a grid of vias inside the pad)
//! that drop it to the net's plane.
//!
//! This is the step that lets a board with inner planes be autorouted without
//! routing the plane nets as traces: once every SMD power pad reaches its
//! plane, the plane connects them, and the autorouter only sees signal nets.
//!
//! Pure geometry over a board snapshot, so it can be planned, inspected and
//! tested without KiCad; the tool applies the plan over IPC.
//!
//! What a via site must satisfy:
//! - inside the board outline (the real Edge.Cuts lines and arcs, not their
//!   bounding box) and the edge clearance away from it;
//! - inside a zone of the same net on an inner layer, so it reaches the plane;
//! - clear of every other net's pads (as boxes), tracks, arcs and vias on
//!   any layer, since a through via spans them all;
//! - outside every keep-out rule area, board-level or inside a footprint,
//!   for the whole via circle, not just its centre.
//!
//! Pads are their bounding boxes, so a site can be rejected that a precise
//! shape would allow; never the reverse. DRC follows every apply.

use konnect_sexp::bounds::{pad_bbox, Bbox};
use konnect_sexp::SexpNode;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

type Point = (f64, f64);

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FanoutOptions {
    pub via_size_mm: f64,
    pub via_drill_mm: f64,
    pub track_width_mm: f64,
    pub clearance_mm: f64,
    pub edge_clearance_mm: f64,
    pub max_stub_mm: f64,
    /// A pad whose smaller side is at least this gets vias inside the pad
    /// (exposed pads, regulator tabs) instead of one stub via.
    pub large_pad_mm: f64,
}

impl Default for FanoutOptions {
    fn default() -> Self {
        Self {
            via_size_mm: 0.6,
            via_drill_mm: 0.3,
            track_width_mm: 0.3,
            clearance_mm: 0.2,
            edge_clearance_mm: 0.5,
            max_stub_mm: 2.5,
            large_pad_mm: 2.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct PlannedFanout {
    pub reference: String,
    pub pad: String,
    pub net: String,
    /// The pad's copper layer, where the stub runs.
    pub layer: String,
    pub pad_x: f64,
    pub pad_y: f64,
    pub via_x: f64,
    pub via_y: f64,
    /// 0 for a via inside a large pad: no stub is drawn.
    pub stub_mm: f64,
    pub in_pad: bool,
    /// false for a stub-only tie to a same-net large pad of the same part
    /// (a QFN ground pin to its exposed pad): the stub ends at
    /// (`via_x`, `via_y`) inside that pad and no via is placed.
    pub via: bool,
    pub via_size_mm: f64,
    pub via_drill_mm: f64,
    pub track_width_mm: f64,
    /// Low-inductance mode (Pass 24): where the via sits against the pad
    /// (`side`, `end`, `corner`, `in_pad`, `outward`), or `shared` for a
    /// stub to an IC pin's via.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<&'static str>,
    /// Pad edge to via copper edge, and pad edge to drill edge, mm.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via_gap_mm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hole_gap_mm: Option<f64>,
    /// `REF.PAD` of the IC pin whose via this pad shares (`via` is false).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shares_via_of: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct SkippedPad {
    pub reference: String,
    pub pad: String,
    pub net: String,
    pub reason: String,
}

/// Skip reason for a pad that already reaches its plane; callers test for it.
pub(crate) const ALREADY_REACHES_PLANE: &str = "already reaches its plane through a via";

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct FanoutPlan {
    pub nets: Vec<String>,
    /// What "reaches the plane" was judged against.
    pub plane_source: &'static str,
    pub fanouts: Vec<PlannedFanout>,
    pub skipped: Vec<SkippedPad>,
    /// Through-hole pads on the nets: they reach every layer already.
    pub through_hole_pads: usize,
    /// `legacy` or `low_inductance`.
    pub mode: &'static str,
    /// Low-inductance mode: one decision per pad on the nets, SMD and
    /// through-hole, so nothing is skipped silently.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pads: Vec<PadDecision>,
}

/// What the low-inductance planner decided for one pad, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct PadDecision {
    /// `REF.PAD`; numbers can repeat, so `pad_uuid` and `at` identify it.
    pub pad: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pad_uuid: Option<String>,
    pub at: [f64; 2],
    pub net: String,
    pub layer: String,
    /// `via` (own via against the pad), `via_in_pad` (declared filled and
    /// capped), `via_grid` (large pad), `share` (an IC pin's via), `tie` (to
    /// the part's exposed pad), `kept` (already passes `plane_connection`),
    /// `exception` (declared in the intent), `through_hole`, `not_planned`
    /// (the judge's doubt is not one a via fixes) or `no_site`.
    pub action: &'static str,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<[f64; 2]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<&'static str>,
    /// Connecting trace outside the pad and the via annulus, mm.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stub_mm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via_gap_mm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hole_gap_mm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shares_via_of: Option<String>,
    /// Two-pad parts: how the part's other pad was treated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symmetry: Option<String>,
}

impl FanoutPlan {
    /// Every SMD pad of the plan's nets already reaches its plane.
    pub(crate) fn all_pads_reach_planes(&self) -> bool {
        self.fanouts.is_empty() && self.skipped.iter().all(|skip| skip.reason == ALREADY_REACHES_PLANE)
    }
}

/// Nets with a zone on an inner copper layer: the planes a pad can drop to.
pub(crate) fn plane_nets(tree: &SexpNode) -> Vec<String> {
    let geometry = BoardGeometry::read(tree);
    geometry.planes.keys().cloned().collect()
}

fn is_inner(layer: &str) -> bool {
    layer.starts_with("In") && layer.ends_with(".Cu")
}

// ─── Board geometry ─────────────────────────────────────────────────────────

struct Keepout {
    polygon: Vec<Point>,
    no_vias: bool,
    no_tracks: bool,
    layers: Vec<String>,
}

struct BoardGeometry {
    /// Edge.Cuts as straight segments (arcs and circles sampled).
    edges: Vec<(Point, Point)>,
    /// Same-net zone outlines on inner layers, per net.
    planes: BTreeMap<String, Vec<Vec<Point>>>,
    /// The zones' filled copper on inner layers, per net, when the file
    /// holds a fill: voids, islands and higher-priority zones are cut out.
    fills: BTreeMap<String, Vec<Vec<Point>>>,
    keepouts: Vec<Keepout>,
    /// The plane copper each via must land on (fill, else outline), indexed.
    plane_index: BTreeMap<String, Vec<PolyIndex>>,
}

/// A polygon indexed for many point queries: edges by horizontal band for
/// the even-odd test, and its solid edges (fracture slits left out, see
/// [`solid_edges`]) by grid cell for the clearance test.
struct PolyIndex {
    edges: Vec<(Point, Point)>,
    y0: f64,
    band: f64,
    bands: Vec<Vec<u32>>,
    cell: f64,
    grid: std::collections::HashMap<(i64, i64), Vec<u32>>,
    solid: Vec<(Point, Point)>,
    bbox: Bbox,
}

impl PolyIndex {
    fn new(polygon: &[Point]) -> Self {
        let edges: Vec<(Point, Point)> = polygon_edges(polygon).collect();
        let solid: Vec<(Point, Point)> = solid_edges(polygon).collect();
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for p in polygon {
            x0 = x0.min(p.0);
            y0 = y0.min(p.1);
            x1 = x1.max(p.0);
            y1 = y1.max(p.1);
        }
        let band = 0.5;
        let n = (((y1 - y0) / band).ceil() as usize).max(1) + 1;
        let mut bands = vec![Vec::new(); n];
        for (i, (a, b)) in edges.iter().enumerate() {
            let lo = ((a.1.min(b.1) - y0) / band).floor().max(0.0) as usize;
            let hi = (((a.1.max(b.1) - y0) / band).floor().max(0.0) as usize).min(n - 1);
            for slot in bands.iter_mut().take(hi + 1).skip(lo) {
                slot.push(i as u32);
            }
        }
        let cell = 1.0;
        let mut grid: std::collections::HashMap<(i64, i64), Vec<u32>> = std::collections::HashMap::new();
        for (i, (a, b)) in solid.iter().enumerate() {
            let (cx0, cx1) = ((a.0.min(b.0) / cell).floor() as i64, (a.0.max(b.0) / cell).floor() as i64);
            let (cy0, cy1) = ((a.1.min(b.1) / cell).floor() as i64, (a.1.max(b.1) / cell).floor() as i64);
            for cx in cx0..=cx1 {
                for cy in cy0..=cy1 {
                    grid.entry((cx, cy)).or_default().push(i as u32);
                }
            }
        }
        Self { edges, y0, band, bands, cell, grid, solid, bbox: (x0, y0, x1, y1) }
    }

    fn contains(&self, (x, y): Point) -> bool {
        let (bx0, by0, bx1, by1) = self.bbox;
        if x < bx0 || x > bx1 || y < by0 || y > by1 {
            return false;
        }
        let slot = (((y - self.y0) / self.band).floor().max(0.0) as usize).min(self.bands.len() - 1);
        let mut inside = false;
        for &i in &self.bands[slot] {
            let ((xi, yi), (xj, yj)) = self.edges[i as usize];
            if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
                inside = !inside;
            }
        }
        inside
    }

    /// Every solid edge is at least `r` from `p`.
    fn clear_of_edges(&self, p: Point, r: f64) -> bool {
        let (cx0, cx1) = (((p.0 - r) / self.cell).floor() as i64, ((p.0 + r) / self.cell).floor() as i64);
        let (cy0, cy1) = (((p.1 - r) / self.cell).floor() as i64, ((p.1 + r) / self.cell).floor() as i64);
        for cx in cx0..=cx1 {
            for cy in cy0..=cy1 {
                if let Some(list) = self.grid.get(&(cx, cy)) {
                    for &i in list {
                        let (a, b) = self.solid[i as usize];
                        if point_segment_distance(p, a, b) < r - EPS {
                            return false;
                        }
                    }
                }
            }
        }
        true
    }
}

fn point(node: &SexpNode, tag: &str) -> Option<Point> {
    let child = node.find(tag)?;
    Some((child.get_f64(1)?, child.get_f64(2)?))
}

/// Points along the arc through `start`, `mid`, `end` (inclusive).
fn arc_points(start: Point, mid: Point, end: Point, steps: usize) -> Vec<Point> {
    let (ax, ay) = start;
    let (bx, by) = mid;
    let (cx, cy) = end;
    let d = 2.0 * (ax * (by - cy) + bx * (cy - ay) + cx * (ay - by));
    if d.abs() < 1e-12 {
        return vec![start, end];
    }
    let ux = ((ax * ax + ay * ay) * (by - cy) + (bx * bx + by * by) * (cy - ay) + (cx * cx + cy * cy) * (ay - by)) / d;
    let uy = ((ax * ax + ay * ay) * (cx - bx) + (bx * bx + by * by) * (ax - cx) + (cx * cx + cy * cy) * (bx - ax)) / d;
    let r = (ax - ux).hypot(ay - uy);
    let angle = |(x, y): Point| (y - uy).atan2(x - ux);
    let (a0, am, a1) = (angle(start), angle(mid), angle(end));
    let tau = std::f64::consts::TAU;
    // Sweep from a0 through am to a1, in whichever direction contains am.
    let ccw = |from: f64, to: f64| (to - from).rem_euclid(tau);
    let sweep = if ccw(a0, am) <= ccw(a0, a1) { ccw(a0, a1) } else { -ccw(a1, a0) };
    (0..=steps)
        .map(|i| {
            let a = a0 + sweep * i as f64 / steps as f64;
            (ux + r * a.cos(), uy + r * a.sin())
        })
        .collect()
}

fn polygon_points(node: &SexpNode) -> Vec<Point> {
    node.find("polygon")
        .and_then(|polygon| polygon.find("pts"))
        .or_else(|| node.find("pts"))
        .map(|pts| pts.find_all("xy").iter().filter_map(|xy| Some((xy.get_f64(1)?, xy.get_f64(2)?))).collect())
        .unwrap_or_default()
}

fn zone_layers(zone: &SexpNode) -> Vec<String> {
    let mut layers = Vec::new();
    for tag in ["layers", "layer"] {
        if let Some(node) = zone.find(tag) {
            layers.extend(node.children().unwrap_or_default().iter().skip(1).filter_map(SexpNode::as_str).map(str::to_string));
        }
    }
    layers
}

impl BoardGeometry {
    fn read(tree: &SexpNode) -> Self {
        let mut edges = Vec::new();
        for node in tree.children().unwrap_or_default() {
            if node.find_str("layer") != Some("Edge.Cuts") {
                continue;
            }
            match node.head() {
                Some("gr_line") => {
                    if let (Some(a), Some(b)) = (point(node, "start"), point(node, "end")) {
                        edges.push((a, b));
                    }
                }
                Some("gr_arc") => {
                    if let (Some(a), Some(m), Some(b)) = (point(node, "start"), point(node, "mid"), point(node, "end")) {
                        edges.extend(arc_points(a, m, b, 16).windows(2).map(|w| (w[0], w[1])));
                    }
                }
                Some("gr_rect") => {
                    if let (Some((x0, y0)), Some((x1, y1))) = (point(node, "start"), point(node, "end")) {
                        let c = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
                        edges.extend((0..4).map(|i| (c[i], c[(i + 1) % 4])));
                    }
                }
                Some("gr_circle") => {
                    if let (Some((cx, cy)), Some((ex, ey))) = (point(node, "center"), point(node, "end")) {
                        let r = (ex - cx).hypot(ey - cy);
                        let p: Vec<Point> = (0..=32)
                            .map(|i| {
                                let a = std::f64::consts::TAU * i as f64 / 32.0;
                                (cx + r * a.cos(), cy + r * a.sin())
                            })
                            .collect();
                        edges.extend(p.windows(2).map(|w| (w[0], w[1])));
                    }
                }
                Some("gr_poly") => {
                    let p = polygon_points(node);
                    if p.len() >= 3 {
                        edges.extend((0..p.len()).map(|i| (p[i], p[(i + 1) % p.len()])));
                    }
                }
                _ => {}
            }
        }

        let mut planes: BTreeMap<String, Vec<Vec<Point>>> = BTreeMap::new();
        let mut fills: BTreeMap<String, Vec<Vec<Point>>> = BTreeMap::new();
        let mut keepouts = Vec::new();
        let mut zone_nodes: Vec<&SexpNode> = tree.find_all("zone");
        for footprint in konnect_sexp::board::footprints(tree) {
            zone_nodes.extend(footprint.find_all("zone"));
        }
        for zone in zone_nodes {
            let polygon = polygon_points(zone);
            if polygon.len() < 3 {
                continue;
            }
            let layers = zone_layers(zone);
            if let Some(keepout) = zone.find("keepout") {
                let not_allowed = |tag: &str| keepout.find(tag).and_then(|n| n.get(1)).and_then(SexpNode::as_str) == Some("not_allowed");
                keepouts.push(Keepout { polygon, no_vias: not_allowed("vias"), no_tracks: not_allowed("tracks"), layers });
                continue;
            }
            let net = zone.find("net").and_then(konnect_sexp::net::net_name).filter(|n| !n.is_empty());
            if let Some(net) = net {
                if layers.iter().any(|layer| is_inner(layer)) {
                    planes.entry(net.to_string()).or_default().push(polygon);
                }
                for fill in zone.find_all("filled_polygon") {
                    if fill.find_str("layer").is_some_and(is_inner) {
                        let points = polygon_points(fill);
                        if points.len() >= 3 {
                            fills.entry(net.to_string()).or_default().push(points);
                        }
                    }
                }
            }
        }
        let mut plane_index = BTreeMap::new();
        for net in planes.keys().chain(fills.keys()) {
            if let Some(polys) = fills.get(net).or_else(|| planes.get(net)) {
                plane_index.entry(net.clone()).or_insert_with(|| polys.iter().map(|p| PolyIndex::new(p)).collect());
            }
        }
        Self { edges, planes, fills, keepouts, plane_index }
    }

    fn inside_board(&self, p: Point, margin: f64) -> bool {
        if self.edges.is_empty() {
            return false;
        }
        // Ray cast to +x over the edge segments; no loop chaining needed.
        let mut inside = false;
        for &((x1, y1), (x2, y2)) in &self.edges {
            if (y1 > p.1) != (y2 > p.1) && p.0 < (x2 - x1) * (p.1 - y1) / (y2 - y1) + x1 {
                inside = !inside;
            }
        }
        inside && self.edges.iter().all(|&(a, b)| point_segment_distance(p, a, b) >= margin)
    }

    fn reaches_plane(&self, net: &str, p: Point) -> bool {
        self.reaches_plane_with(net, p, 0.0)
    }

    /// A via of radius `r` at `p` lands wholly on the net's plane copper:
    /// inside the fill when the file has one (else the outline) and at least
    /// `r` from its edges. KiCad's fills are fractured into single outlines,
    /// so the even-odd test and edge distance hold for them.
    fn reaches_plane_with(&self, net: &str, p: Point, r: f64) -> bool {
        self.plane_index.get(net).is_some_and(|zones| zones.iter().any(|zone| zone.contains(p) && (r <= 0.0 || zone.clear_of_edges(p, r))))
    }

    fn plane_source(&self, nets: &[String]) -> &'static str {
        if nets.iter().all(|n| self.fills.contains_key(n)) {
            "filled zones (refill after moving parts so the fill is current)"
        } else {
            "zone outlines: the file holds no fill for some plane nets, so voids and islands are not seen"
        }
    }

    /// Does a circle at `p` (radius `r`) avoid every via keep-out?
    fn via_allowed(&self, p: Point, r: f64) -> bool {
        self.keepouts.iter().filter(|k| k.no_vias).all(|k| !circle_hits_polygon(p, r, &k.polygon))
    }

    /// Does a stub from `a` to `b` (half width `w`) on `layer` avoid every track keep-out?
    fn track_allowed(&self, a: Point, b: Point, w: f64, layer: &str) -> bool {
        self.keepouts
            .iter()
            .filter(|k| k.no_tracks && k.layers.iter().any(|l| l == layer || l == "*.Cu" || l == "F&B.Cu"))
            .all(|k| !segment_hits_polygon(a, b, w, &k.polygon))
    }
}

// ─── Pads and obstacles ─────────────────────────────────────────────────────

struct PadInfo {
    reference: String,
    number: String,
    net: Option<String>,
    center: Point,
    /// Board-space angle of the pad and its size in its own frame.
    angle: f64,
    size: (f64, f64),
    bbox: Bbox,
    smd: bool,
    layer: &'static str,
    footprint_center: Point,
    /// The footprint's rotation in degrees and the pad's KiCad UUID.
    footprint_rotation: f64,
    uuid: Option<String>,
    /// KiCad pad shape (`rect`, `circle`, `oval`, `roundrect`, …) and the
    /// roundrect corner ratio.
    shape: String,
    roundrect_ratio: f64,
}

impl PadInfo {
    /// The pad's true outline in board coordinates (circle, oval and
    /// roundrect sampled; other shapes as their rectangle). Convex.
    fn outline(&self) -> Vec<Point> {
        let (w, h) = (self.size.0 / 2.0, self.size.1 / 2.0);
        let arc = |cx: f64, cy: f64, r: f64, from: f64, steps: usize| -> Vec<Point> {
            (0..=steps).map(|i| {
                let a = from + std::f64::consts::FRAC_PI_2 * i as f64 / steps as f64;
                (cx + r * a.cos(), cy + r * a.sin())
            }).collect()
        };
        // Corner radius r: rounded rectangle with quarter arcs at the corners.
        let rounded = |r: f64| -> Vec<Point> {
            let r = r.min(w).min(h).max(0.0);
            let half_pi = std::f64::consts::FRAC_PI_2;
            let mut pts = Vec::new();
            pts.extend(arc(w - r, h - r, r, 0.0, 6));
            pts.extend(arc(-w + r, h - r, r, half_pi, 6));
            pts.extend(arc(-w + r, -h + r, r, 2.0 * half_pi, 6));
            pts.extend(arc(w - r, -h + r, r, 3.0 * half_pi, 6));
            pts
        };
        let local: Vec<Point> = match self.shape.as_str() {
            "circle" => rounded(w.min(h)),
            "oval" => rounded(w.min(h)),
            "roundrect" => rounded(self.roundrect_ratio * 2.0 * w.min(h)),
            _ => vec![(-w, -h), (w, -h), (w, h), (-w, h)],
        };
        local.into_iter().map(|(x, y)| konnect_sexp::geometry::transform_pad(x, y, self.center.0, self.center.1, self.angle)).collect()
    }

    /// The pad's rectangle in board coordinates, rotation included.
    fn corners(&self) -> [Point; 4] {
        let (w, h) = (self.size.0 / 2.0, self.size.1 / 2.0);
        [(-w, -h), (w, -h), (w, h), (-w, h)]
            .map(|(x, y)| konnect_sexp::geometry::transform_pad(x, y, self.center.0, self.center.1, self.angle))
    }
}

fn collect_pads(tree: &SexpNode) -> Vec<PadInfo> {
    let mut pads = Vec::new();
    for footprint in konnect_sexp::board::footprints(tree) {
        let Some(at) = footprint.find("at") else { continue };
        let (Some(fx), Some(fy)) = (at.get_f64(1), at.get_f64(2)) else { continue };
        let rot = at.get_f64(3).unwrap_or(0.0);
        let back = footprint.find_str("layer") == Some("B.Cu");
        let reference = footprint
            .find_all("property")
            .into_iter()
            .find(|p| p.get(1).and_then(SexpNode::as_str) == Some("Reference"))
            .and_then(|p| p.get(2))
            .and_then(SexpNode::as_str)
            .unwrap_or("")
            .to_string();
        for pad in footprint.find_all("pad") {
            // Paste- or mask-only pads (KiCad's exposed-pad paste windows)
            // carry no copper: neither an obstacle nor a fan-out source.
            let copper = pad
                .find("layers")
                .and_then(SexpNode::children)
                .unwrap_or_default()
                .iter()
                .filter_map(SexpNode::as_str)
                .any(|layer| layer.ends_with(".Cu"));
            if !copper {
                continue;
            }
            let Some(bbox) = pad_bbox(pad, fx, fy, rot) else { continue };
            let Some(pad_at) = pad.find("at") else { continue };
            let (Some(px), Some(py)) = (pad_at.get_f64(1), pad_at.get_f64(2)) else { continue };
            let center = konnect_sexp::geometry::transform_pad(px, py, fx, fy, rot);
            let size = pad.find("size").and_then(|s| Some((s.get_f64(1)?, s.get_f64(2)?))).unwrap_or((0.0, 0.0));
            let kind = pad.get(2).and_then(SexpNode::as_str).unwrap_or("");
            let net = pad
                .find("net")
                .and_then(konnect_sexp::net::net_name)
                .filter(|name| !name.is_empty())
                .map(str::to_string);
            pads.push(PadInfo {
                reference: reference.clone(),
                number: pad.get(1).and_then(SexpNode::as_str).unwrap_or("").to_string(),
                net,
                center,
                angle: pad_at.get_f64(3).unwrap_or(0.0),
                size,
                bbox,
                smd: kind == "smd",
                layer: if back { "B.Cu" } else { "F.Cu" },
                footprint_center: (fx, fy),
                footprint_rotation: rot,
                uuid: pad.find_str("uuid").or_else(|| pad.find_str("tstamp")).map(str::to_string),
                shape: pad.get(3).and_then(SexpNode::as_str).unwrap_or("rect").to_string(),
                roundrect_ratio: pad.find_f64("roundrect_rratio").unwrap_or(0.25),
            });
        }
    }
    pads
}

/// An obstacle of some net (`None`: no net, an obstacle for every net).
enum Obstacle {
    /// A pad as its true (rotated) rectangle, with its part's reference:
    /// another part's pad is an obstacle even on the same net (Pass 24: a
    /// via inside a neighbour's same-net pad made a daisy chain).
    /// `center` tells the pad itself from its part's other pads; `large`
    /// (an exposed pad) is the only same-part, same-net pad a site may
    /// enter (review M1).
    Poly { corners: [Point; 4], net: Option<String>, layers: Layers, reference: String, center: Point, large: bool },
    Segment { a: Point, b: Point, half_width: f64, net: Option<String>, layer: String },
    Circle { center: Point, radius: f64, net: Option<String> },
}

#[derive(Clone)]
enum Layers {
    All,
    One(&'static str),
}

fn pad_obstacle(pad: &PadInfo) -> Obstacle {
    Obstacle::Poly {
        corners: pad.corners(),
        net: pad.net.clone(),
        layers: if pad.smd { Layers::One(pad.layer) } else { Layers::All },
        reference: pad.reference.clone(),
        center: pad.center,
        large: pad.size.0.min(pad.size.1) >= FanoutOptions::default().large_pad_mm,
    }
}

/// Same-net copper used to follow a pad to its plane.
struct NetCopper {
    segments: Vec<(Point, Point, f64)>,
    vias: Vec<(Point, f64)>,
}

fn routed_copper(tree: &SexpNode) -> (Vec<Obstacle>, BTreeMap<String, NetCopper>) {
    let mut obstacles = Vec::new();
    let mut by_net: BTreeMap<String, NetCopper> = BTreeMap::new();
    for track in konnect_sexp::board::tracks(tree).items {
        if let Some(net) = track.net.clone() {
            by_net.entry(net).or_insert_with(|| NetCopper { segments: vec![], vias: vec![] }).segments.push((
                (track.x1, track.y1),
                (track.x2, track.y2),
                track.width,
            ));
        }
        obstacles.push(Obstacle::Segment {
            a: (track.x1, track.y1),
            b: (track.x2, track.y2),
            half_width: track.width / 2.0,
            net: track.net,
            layer: track.layer,
        });
    }
    // Routed arcs: `tracks` reads only straight segments.
    for arc in tree.find_all("arc") {
        let (Some(s), Some(m), Some(e)) = (point(arc, "start"), point(arc, "mid"), point(arc, "end")) else { continue };
        let width = arc.find_f64("width").unwrap_or(0.2);
        let layer = arc.find_str("layer").unwrap_or("").to_string();
        let net = arc.find("net").and_then(konnect_sexp::net::net_name).filter(|n| !n.is_empty()).map(str::to_string);
        let points = arc_points(s, m, e, 8);
        for w in points.windows(2) {
            if let Some(net) = &net {
                by_net.entry(net.clone()).or_insert_with(|| NetCopper { segments: vec![], vias: vec![] }).segments.push((w[0], w[1], width));
            }
            obstacles.push(Obstacle::Segment { a: w[0], b: w[1], half_width: width / 2.0, net: net.clone(), layer: layer.clone() });
        }
    }
    for via in konnect_sexp::board::vias(tree).items {
        if let Some(net) = &via.net {
            by_net.entry(net.clone()).or_insert_with(|| NetCopper { segments: vec![], vias: vec![] }).vias.push(((via.x, via.y), via.size / 2.0));
        }
        obstacles.push(Obstacle::Circle { center: (via.x, via.y), radius: via.size / 2.0, net: via.net });
    }
    (obstacles, by_net)
}

/// Follow same-net tracks from the pad; true when a via that sits inside
/// the net's plane is reached (or sits on the pad itself).
fn pad_reaches_plane(pad: &PadInfo, net: &str, copper: Option<&NetCopper>, geometry: &BoardGeometry) -> bool {
    let Some(copper) = copper else { return false };
    let on_pad = |p: Point, slack: f64| point_rect_distance(p, pad.bbox) <= slack;
    let good_via = |center: Point| geometry.reaches_plane(net, center);
    if copper.vias.iter().any(|&(c, r)| on_pad(c, r) && good_via(c)) {
        return true;
    }
    let mut frontier: Vec<Point> = Vec::new();
    let mut used = vec![false; copper.segments.len()];
    for (i, &(a, b, w)) in copper.segments.iter().enumerate() {
        if on_pad(a, w / 2.0) {
            frontier.push(b);
            used[i] = true;
        } else if on_pad(b, w / 2.0) {
            frontier.push(a);
            used[i] = true;
        }
    }
    let mut steps = 0;
    while let Some(p) = frontier.pop() {
        steps += 1;
        if steps > 10_000 {
            break;
        }
        if copper.vias.iter().any(|&(c, r)| (c.0 - p.0).hypot(c.1 - p.1) <= r && good_via(c)) {
            return true;
        }
        for (i, &(a, b, w)) in copper.segments.iter().enumerate() {
            if used[i] {
                continue;
            }
            let tol = w / 2.0;
            if (a.0 - p.0).hypot(a.1 - p.1) <= tol {
                used[i] = true;
                frontier.push(b);
            } else if (b.0 - p.0).hypot(b.1 - p.1) <= tol {
                used[i] = true;
                frontier.push(a);
            }
        }
    }
    false
}

// ─── Planning ───────────────────────────────────────────────────────────────

pub(crate) fn plan(
    tree: &SexpNode,
    nets: &[String],
    options_for: &dyn Fn(&str) -> FanoutOptions,
) -> FanoutPlan {
    let wanted: BTreeSet<&str> = nets.iter().map(String::as_str).collect();
    let geometry = BoardGeometry::read(tree);
    let pads = collect_pads(tree);
    let (mut obstacles, copper) = routed_copper(tree);
    for pad in &pads {
        obstacles.push(pad_obstacle(pad));
    }

    let mut result = FanoutPlan { nets: nets.to_vec(), plane_source: geometry.plane_source(nets), mode: "legacy", ..FanoutPlan::default() };
    // Large pads first, so small pads beside them can tie to their vias.
    let mut order: Vec<&PadInfo> = pads.iter().collect();
    order.sort_by_key(|pad| {
        let o = options_for(pad.net.as_deref().unwrap_or(""));
        pad.size.0.min(pad.size.1) < o.large_pad_mm
    });
    let mut large_with_vias: Vec<(&PadInfo, String)> = Vec::new();
    for pad in order {
        let Some(net) = pad.net.as_deref().filter(|net| wanted.contains(net)) else { continue };
        let options = options_for(net);
        let skip = |reason: &str| SkippedPad {
            reference: pad.reference.clone(),
            pad: pad.number.clone(),
            net: net.to_string(),
            reason: reason.to_string(),
        };
        if !pad.smd {
            result.through_hole_pads += 1;
            continue;
        }
        if pad_reaches_plane(pad, net, copper.get(net), &geometry) {
            result.skipped.push(skip(ALREADY_REACHES_PLANE));
            continue;
        }
        if !geometry.planes.contains_key(net) {
            result.skipped.push(skip("the net has no zone on an inner layer"));
            continue;
        }
        let planned = |via: Point, stub: f64, in_pad: bool| PlannedFanout {
            reference: pad.reference.clone(),
            pad: pad.number.clone(),
            net: net.to_string(),
            layer: pad.layer.to_string(),
            pad_x: round4(pad.center.0),
            pad_y: round4(pad.center.1),
            via_x: round4(via.0),
            via_y: round4(via.1),
            stub_mm: round4(stub),
            in_pad,
            via: true,
            via_size_mm: options.via_size_mm,
            via_drill_mm: options.via_drill_mm,
            track_width_mm: options.track_width_mm,
            placement: None,
            via_gap_mm: None,
            hole_gap_mm: None,
            shares_via_of: None,
        };
        if pad.size.0.min(pad.size.1) >= options.large_pad_mm {
            let sites = in_pad_sites(pad, net, &obstacles, &geometry, &options);
            if sites.is_empty() {
                result.skipped.push(skip("large pad: no via site inside it reaches the plane"));
                continue;
            }
            for site in sites {
                obstacles.push(Obstacle::Circle { center: site, radius: options.via_size_mm / 2.0, net: Some(net.to_string()) });
                result.fanouts.push(planned(site, 0.0, true));
            }
            large_with_vias.push((pad, net.to_string()));
            continue;
        }
        match best_site(pad, net, &obstacles, &geometry, &options) {
            Some((via, stub)) => {
                obstacles.push(Obstacle::Circle { center: via, radius: options.via_size_mm / 2.0, net: Some(net.to_string()) });
                obstacles.push(Obstacle::Segment {
                    a: pad.center,
                    b: via,
                    half_width: options.track_width_mm / 2.0,
                    net: Some(net.to_string()),
                    layer: pad.layer.to_string(),
                });
                result.fanouts.push(planned(via, stub, false));
            }
            None => match tie_to_large_pad(pad, net, &large_with_vias, &obstacles, &geometry, &options) {
                Some(end) => {
                    obstacles.push(Obstacle::Segment {
                        a: pad.center,
                        b: end,
                        half_width: options.track_width_mm.min(pad.size.0.min(pad.size.1)) / 2.0,
                        net: Some(net.to_string()),
                        layer: pad.layer.to_string(),
                    });
                    let stub = (end.0 - pad.center.0).hypot(end.1 - pad.center.1);
                    let mut tie = planned(end, stub, false);
                    tie.via = false;
                    tie.track_width_mm = options.track_width_mm.min(pad.size.0.min(pad.size.1));
                    result.fanouts.push(tie);
                }
                None => result.skipped.push(skip(&format!(
                    "no via site within {} mm clears every other net and lands in the plane",
                    options.max_stub_mm
                ))),
            },
        }
    }
    result
}

// ─── Low-inductance planning (Pass 24) ──────────────────────────────────────
//
// Every SMD pad on a plane net gets its own via against the pad, unless the
// `plane_connection` judge already passes it or the intent excepts it:
// - against the pad, not in it: the drill edge `hole_gap_mm` (0.25 mm) from
//   the pad edge (Eurocircuits, tombstoning: "move the Via Hole at least
//   0.25mm from the edge of the component pad to the edge of the Via Hole"
//   [docdb eurocircuits-tombstoning]), or the nearest legal ring beyond;
// - on the side of the pad that keeps the loop small: beside the pads of a
//   two-pad part rather than at their ends ("Via placement at the sides of
//   the pads decreases the mounting's overall parasitic inductance by
//   increasing the mutual inductive coupling of one via to the other",
//   UG483 p.24 [docdb xilinx-ug483-7series-pcb]), the two vias as close
//   together as possible ("Locate the two capacitor vias as close together
//   as possible" [docdb learnemc-decoupling-widely-spaced-planes]), and on
//   the side facing the IC pin the part serves;
// - in the pad only when the caller declares filled and capped vias, and
//   then for both pads of a two-pad part;
// - a capacitor pad shares an IC pin's via only when the plane regime
//   allows it (`pwr.decoupling_via_sharing_with_device`): same side, planes
//   > 0.5 mm apart, the via adjacent to the pad, never another capacitor's.

/// One pad's identity across the judge and the planner: its KiCad UUID,
/// else its name and centre. Pad numbers repeat (a SOT-223's pin 2 and tab
/// 2, a switch's 1, 1, 2, 2), so `REF.PAD` alone collapses them (Pass 24
/// review H1).
pub(crate) fn pad_identity(name: &str, uuid: Option<&str>, center: Point) -> String {
    match uuid.filter(|u| !u.is_empty()) {
        Some(u) => format!("uuid:{u}"),
        None => {
            let r = |v: f64| (v * 1000.0).round() / 1000.0 + 0.0;
            format!("{name}@{:.3},{:.3}", r(center.0), r(center.1))
        }
    }
}

fn identity_of(p: &PadInfo) -> String {
    pad_identity(&format!("{}.{}", p.reference, p.number), p.uuid.as_deref(), p.center)
}

/// The judge's verdict on one pad of the board as it is.
#[derive(Debug, Clone, Default)]
pub(crate) struct JudgeVerdict {
    /// `REF.PAD`.
    pub pad: String,
    pub status: String,
    pub connection: String,
    pub detail: String,
    /// The way down the judge assigned, when it is a via.
    pub via: Option<Point>,
    pub stub_mm: Option<f64>,
    /// Exception kind and source, for an excepted pad.
    pub exception: Option<(String, String)>,
}

/// A plane net's decoupling regime and connecting-trace limit (the judge's).
#[derive(Debug, Clone)]
pub(crate) struct NetRegime {
    /// `closely_spaced`, `widely_spaced` or `unknown`.
    pub regime: String,
    pub spacing_mm: Option<f64>,
    pub note: String,
    pub max_stub_mm: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct LowInductance {
    /// Pad edge to drill edge, mm.
    pub hole_gap_mm: f64,
    /// The caller declares filled and capped vias: small pads take the via in the pad.
    pub via_in_pad: bool,
    /// `share_with_ic: auto`.
    pub share_with_ic: bool,
    /// A shared IC via must sit within this of the capacitor pad (copper
    /// edge to pad edge). `None`: butted against the pad, no further than
    /// the own-via placement would put it (drill edge `hole_gap_mm` from the
    /// pad, plus one 0.05 mm search step), so no real trace (review L8).
    pub share_max_gap_mm: Option<f64>,
    /// No via under another part's body on the pad's side (its courtyard
    /// less the courtyard excess, [`courtyard_margin`]).
    pub avoid_courtyards: bool,
    pub nets: BTreeMap<String, NetRegime>,
    /// Keyed by ([`pad_identity`], net).
    pub verdicts: BTreeMap<(String, String), JudgeVerdict>,
}

impl Default for LowInductance {
    fn default() -> Self {
        Self {
            hole_gap_mm: 0.25,
            via_in_pad: false,
            share_with_ic: true,
            share_max_gap_mm: None,
            avoid_courtyards: true,
            nets: BTreeMap::new(),
            verdicts: BTreeMap::new(),
        }
    }
}

/// Without a stack-up model the judge falls back to 1.0 mm; so does this.
const FALLBACK_STUB_MM: f64 = 1.0;

/// Part kind by reference, as the judge reads it.
pub(crate) fn ref_kind(reference: &str) -> &'static str {
    let starts = |p: &str| reference.strip_prefix(p).is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()));
    if starts("C") {
        "capacitor"
    } else if starts("U") || starts("IC") || starts("Q") {
        "device"
    } else if starts("J") || starts("CN") || starts("P") || starts("X") {
        "connector"
    } else {
        "other"
    }
}

/// Another part's courtyard outline in board coordinates, as segments (the
/// footprint's `F.CrtYd`/`B.CrtYd` lines, rectangles, polygons, circles and
/// arcs). Real outlines, not a bounding box: a QFP's courtyard leaves its
/// corners free, where decoupling capacitors sit.
struct Courtyard {
    reference: String,
    edges: Vec<(Point, Point)>,
    bbox: Bbox,
    back: bool,
    /// How far the courtyard stands off the body ([`courtyard_margin`]).
    margin: f64,
}

/// The courtyard excess around a part's body, by its kind: IPC-7351B
/// nominal (density level B) 0.15 mm for two-terminal chips smaller than
/// 0603 and 0.25 mm otherwise [docdb kicad-fpgen-ipc7351b-two-terminal,
/// kicad-fpgen-ipc-definitions]; the KiCad library's 0.5 mm for connectors
/// and 1 mm for BGAs [docdb kicad-fpgen-config-klcv3]. Smaller than 0603:
/// two pads and a courtyard no longer than 2.4 mm (0402: 1.82 mm; 0603:
/// 2.96 mm in KiCad's library).
fn courtyard_margin(reference: &str, footprint_name: &str, pads: usize, bbox: Bbox) -> f64 {
    let long = (bbox.2 - bbox.0).max(bbox.3 - bbox.1);
    if footprint_name.to_ascii_uppercase().contains("BGA") {
        1.0
    } else if ref_kind(reference) == "connector" && !is_frequency_part(footprint_name) {
        // A crystal or oscillator referenced X* is a two-pad/four-pad body,
        // not a connector: it keeps the standard 0.25 mm offset.
        0.5
    } else if pads == 2 && long < 2.4 {
        0.15
    } else {
        0.25
    }
}

/// Crystal, oscillator or resonator footprints (the `X*` prefix is shared
/// with connectors in `ref_kind`).
fn is_frequency_part(footprint_name: &str) -> bool {
    let name = footprint_name.to_ascii_lowercase();
    ["crystal", "oscillator", "resonator"].iter().any(|k| name.contains(k))
}

impl Courtyard {
    fn inside(&self, (x, y): Point) -> bool {
        let mut inside = false;
        for &((xi, yi), (xj, yj)) in &self.edges {
            if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
                inside = !inside;
            }
        }
        inside
    }

    /// Does a circle at `p` (radius `r`) reach the body: the courtyard less
    /// its `margin`?
    fn circle_reaches_body(&self, p: Point, r: f64, margin: f64) -> bool {
        if point_rect_distance(p, self.bbox) > r {
            return false;
        }
        let d = self.edges.iter().map(|&(a, b)| point_segment_distance(p, a, b)).fold(f64::INFINITY, f64::min);
        if self.inside(p) { d + r > margin + EPS } else { r - d > margin + EPS }
    }
}

fn courtyards(tree: &SexpNode) -> Vec<Courtyard> {
    let mut out = Vec::new();
    for footprint in konnect_sexp::board::footprints(tree) {
        let Some(at) = footprint.find("at") else { continue };
        let (Some(fx), Some(fy)) = (at.get_f64(1), at.get_f64(2)) else { continue };
        let rot = at.get_f64(3).unwrap_or(0.0);
        let back = footprint.find_str("layer") == Some("B.Cu");
        let reference = footprint
            .find_all("property")
            .into_iter()
            .find(|p| p.get(1).and_then(SexpNode::as_str) == Some("Reference"))
            .and_then(|p| p.get(2))
            .and_then(SexpNode::as_str)
            .unwrap_or("")
            .to_string();
        let board = |(x, y): Point| konnect_sexp::geometry::transform_pad(x, y, fx, fy, rot);
        let mut edges: Vec<(Point, Point)> = Vec::new();
        // The footprint's own side only: a footprint drawing both would read
        // as two nested outlines, hollow to the even-odd test (review L5).
        let own = if back { "B.CrtYd" } else { "F.CrtYd" };
        for shape in footprint.children().unwrap_or_default() {
            if shape.find_str("layer") != Some(own) {
                continue;
            }
            let local: Vec<(Point, Point)> = match shape.head() {
                Some("fp_line") => match (point(shape, "start"), point(shape, "end")) {
                    (Some(a), Some(b)) => vec![(a, b)],
                    _ => vec![],
                },
                Some("fp_rect") => match (point(shape, "start"), point(shape, "end")) {
                    (Some((x0, y0)), Some((x1, y1))) => {
                        let c = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
                        (0..4).map(|i| (c[i], c[(i + 1) % 4])).collect()
                    }
                    _ => vec![],
                },
                Some("fp_poly") => {
                    let p = polygon_points(shape);
                    (0..p.len()).map(|i| (p[i], p[(i + 1) % p.len()])).collect()
                }
                Some("fp_arc") => match (point(shape, "start"), point(shape, "mid"), point(shape, "end")) {
                    (Some(a), Some(m), Some(b)) => arc_points(a, m, b, 12).windows(2).map(|w| (w[0], w[1])).collect(),
                    _ => vec![],
                },
                Some("fp_circle") => match (point(shape, "center"), point(shape, "end")) {
                    (Some((cx, cy)), Some((ex, ey))) => {
                        let r = (ex - cx).hypot(ey - cy);
                        let p: Vec<Point> = (0..=24).map(|i| {
                            let a = std::f64::consts::TAU * i as f64 / 24.0;
                            (cx + r * a.cos(), cy + r * a.sin())
                        }).collect();
                        p.windows(2).map(|w| (w[0], w[1])).collect()
                    }
                    _ => vec![],
                },
                _ => vec![],
            };
            edges.extend(local.into_iter().map(|(a, b)| (board(a), board(b))));
        }
        if edges.is_empty() {
            continue;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for &(a, b) in &edges {
            for p in [a, b] {
                x0 = x0.min(p.0);
                y0 = y0.min(p.1);
                x1 = x1.max(p.0);
                y1 = y1.max(p.1);
            }
        }
        let name = footprint.get(1).and_then(SexpNode::as_str).unwrap_or("");
        let pads = footprint.find_all("pad").len();
        let bbox = (x0, y0, x1, y1);
        out.push(Courtyard { margin: courtyard_margin(&reference, name, pads, bbox), reference, edges, bbox, back });
    }
    out
}

/// A legal via site against a pad.
#[derive(Debug, Clone, Copy)]
struct Site {
    at: Point,
    /// Connecting trace outside the pad and the via annulus (pad centre to via centre drawn).
    stub: f64,
    via_gap: f64,
    hole_gap: f64,
    placement: &'static str,
    cost: f64,
}

/// What the site search needs to know about the pad's part.
struct PartContext {
    /// Unit vector from this pad towards the part's other pad (two-pad parts).
    axis: Option<Point>,
    /// The point the loop closes through: the other pad's planned via, or its centre.
    partner: Option<Point>,
    /// The IC pin the part serves (nearest device pad on its nets, same side).
    ic: Option<Point>,
    /// The part's other pad (for the under-body exclusion).
    other_pad: Option<[Point; 4]>,
    half_w: f64,
}

/// Distance from the pad centre to where a ray towards `p` leaves the pad
/// rectangle.
fn exit_distance(pad: &PadInfo, p: Point) -> f64 {
    let (dx, dy) = (p.0 - pad.center.0, p.1 - pad.center.1);
    let len = dx.hypot(dy);
    if len < 1e-9 {
        return 0.0;
    }
    // Into the pad frame (inverse of transform_pad's rotation).
    let a = pad.angle.to_radians();
    let (ux, uy) = (dx / len, dy / len);
    let (lx, ly) = (ux * a.cos() - uy * a.sin(), ux * a.sin() + uy * a.cos());
    let (w, h) = (pad.size.0 / 2.0, pad.size.1 / 2.0);
    let tx = if lx.abs() > 1e-12 { w / lx.abs() } else { f64::INFINITY };
    let ty = if ly.abs() > 1e-12 { h / ly.abs() } else { f64::INFINITY };
    tx.min(ty).min(len)
}

/// Points at distance `r` around the pad rectangle (an offset outline with
/// round corners), about every `step` mm, in board coordinates.
fn ring_points(pad: &PadInfo, r: f64, step: f64) -> Vec<Point> {
    let (w, h) = (pad.size.0 / 2.0, pad.size.1 / 2.0);
    let mut local = Vec::new();
    let line = |from: Point, to: Point, out: &mut Vec<Point>| {
        let n = (((to.0 - from.0).hypot(to.1 - from.1)) / step).ceil().max(1.0) as usize;
        for i in 0..n {
            let t = i as f64 / n as f64;
            out.push((from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t));
        }
    };
    let arc_steps = ((r * std::f64::consts::FRAC_PI_2) / step).ceil().max(2.0) as usize;
    let corners = [(w, h, 0.0), (-w, h, 1.0), (-w, -h, 2.0), (w, -h, 3.0)];
    for (i, &(cx, cy, q)) in corners.iter().enumerate() {
        let from = q * std::f64::consts::FRAC_PI_2;
        for k in 0..arc_steps {
            let a = from + std::f64::consts::FRAC_PI_2 * k as f64 / arc_steps as f64;
            local.push((cx + r * a.cos(), cy + r * a.sin()));
        }
        let end = from + std::f64::consts::FRAC_PI_2;
        let (nx, ny, _) = corners[(i + 1) % 4];
        line((cx + r * end.cos(), cy + r * end.sin()), (nx + r * end.cos(), ny + r * end.sin()), &mut local);
    }
    local.into_iter().map(|(x, y)| konnect_sexp::geometry::transform_pad(x, y, pad.center.0, pad.center.1, pad.angle)).collect()
}

/// Where the site sits relative to the pad and the part's axis.
fn placement_class(pad: &PadInfo, site: Point, part: &PartContext) -> &'static str {
    let corners = pad.corners();
    let Some((ax, ay)) = part.axis else { return "outward" };
    let (nx, ny) = (-ay, ax);
    let proj = |p: Point, (vx, vy): Point| (p.0 - pad.center.0) * vx + (p.1 - pad.center.1) * vy;
    let ea = corners.iter().map(|&c| proj(c, (ax, ay)).abs()).fold(0.0, f64::max);
    let en = corners.iter().map(|&c| proj(c, (nx, ny)).abs()).fold(0.0, f64::max);
    let (pa, pn) = (proj(site, (ax, ay)), proj(site, (nx, ny)).abs());
    // Beside the pad or the part's body (not past its outer end): side.
    match (pn > en + 1e-6, pa < -ea - 1e-6) {
        (true, false) => "side",
        (true, true) => "corner",
        (false, true) => "end",
        (false, false) => "inner",
    }
}

/// Under the part's own body: between its two pads, within their width.
fn under_own_body(pad: &PadInfo, site: Point, via_r: f64, part: &PartContext) -> bool {
    let (Some((ax, ay)), Some(other)) = (part.axis, part.other_pad) else { return false };
    let (nx, ny) = (-ay, ax);
    let proj = |p: Point, (vx, vy): Point| (p.0 - pad.center.0) * vx + (p.1 - pad.center.1) * vy;
    let mine = pad.corners();
    let inner_mine = mine.iter().map(|&c| proj(c, (ax, ay))).fold(f64::MIN, f64::max);
    let inner_other = other.iter().map(|&c| proj(c, (ax, ay))).fold(f64::MAX, f64::min);
    let en = mine.iter().chain(other.iter()).map(|&c| proj(c, (nx, ny)).abs()).fold(0.0, f64::max);
    let (pa, pn) = (proj(site, (ax, ay)), proj(site, (nx, ny)).abs());
    pa > inner_mine && pa < inner_other && pn < en + via_r
}

/// Why candidate sites were refused, counted for the `no_site` reason.
#[derive(Default)]
struct Refusals(BTreeMap<String, usize>);

impl Refusals {
    fn add(&mut self, why: String) {
        *self.0.entry(why).or_default() += 1;
    }
    fn summary(&self) -> String {
        let mut v: Vec<(&String, &usize)> = self.0.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        v.iter().take(4).map(|(k, n)| format!("{k} ({n})")).collect::<Vec<_>>().join(", ")
    }
}

/// An obstacle named for the refusal summary.
fn name_blocker(obstacle: &Obstacle, net: &str) -> String {
    match obstacle {
        Obstacle::Poly { reference, net: owner, .. } => {
            if owner.as_deref() == Some(net) { format!("{reference}'s same-net pad") } else { format!("{reference}'s pad ({})", owner.as_deref().unwrap_or("no net")) }
        }
        Obstacle::Segment { net: owner, layer, .. } => format!("track of {} on {layer}", owner.as_deref().unwrap_or("no net")),
        Obstacle::Circle { net: owner, .. } => {
            if owner.as_deref() == Some(net) { "drill spacing to a same-net via".to_string() } else { format!("via of {}", owner.as_deref().unwrap_or("no net")) }
        }
    }
}

/// The first obstacle a site (and its stub from the pad centre) hits.
#[allow(clippy::too_many_arguments)]
fn blocker<'a>(site: Point, pad: &PadInfo, net: &str, obstacles: &[&'a Obstacle], via_r: f64, half_w: f64, clear: f64, via_site: bool) -> Option<&'a Obstacle> {
    obstacles.iter().copied().find(|o| !site_is_clear(site, pad, net, std::slice::from_ref(*o), via_r, half_w, clear, via_site))
}


#[allow(clippy::too_many_arguments)]
fn li_site(
    pad: &PadInfo,
    net: &str,
    part: &PartContext,
    obstacles: &[&Obstacle],
    geometry: &BoardGeometry,
    options: &FanoutOptions,
    li: &LowInductance,
    limit: f64,
    yards: &[&Courtyard],
    refusals: &mut Refusals,
) -> Option<Site> {
    let via_r = options.via_size_mm / 2.0;
    let drill_r = options.via_drill_mm / 2.0;
    let half_w = part.half_w;
    // The ring: centre this far from the pad rectangle.
    let r0 = (li.hole_gap_mm + drill_r).max(via_r);
    let step = 0.05;
    let outward = {
        let (dx, dy) = (pad.center.0 - pad.footprint_center.0, pad.center.1 - pad.footprint_center.1);
        let length = dx.hypot(dy);
        if length > 1e-6 { (dx / length, dy / length) } else { (0.0, 0.0) }
    };
    let mut best: Option<Site> = None;
    let mut first_legal: Option<f64> = None;
    let mut k = 0;
    loop {
        let r = r0 + step * k as f64;
        k += 1;
        let via_gap = r - via_r;
        if via_gap > limit + 1e-9 {
            break;
        }
        if let Some(g) = first_legal {
            // 2 × stub dominates the cost: rings past this cannot win.
            if via_gap > g + 0.3 {
                break;
            }
        }
        for site in ring_points(pad, r, step) {
            let centre = (site.0 - pad.center.0).hypot(site.1 - pad.center.1);
            if centre > options.max_stub_mm + 1e-9 {
                refusals.add(format!("beyond max_stub {} mm (centre to centre)", options.max_stub_mm));
                continue;
            }
            if under_own_body(pad, site, via_r, part) {
                continue;
            }
            if !geometry.inside_board(site, options.edge_clearance_mm + via_r) {
                refusals.add("board edge".into());
                continue;
            }
            if !geometry.reaches_plane_with(net, site, via_r) {
                refusals.add("off the plane fill".into());
                continue;
            }
            if !geometry.via_allowed(site, via_r) || !geometry.track_allowed(pad.center, site, half_w, pad.layer) {
                refusals.add("keep-out".into());
                continue;
            }
            if let Some(y) = yards.iter().find(|y| y.reference != pad.reference && (y.back == (pad.layer == "B.Cu")) && y.circle_reaches_body(site, via_r, y.margin)) {
                refusals.add(format!("under {}'s body (courtyard less {} mm)", y.reference, y.margin));
                continue;
            }
            if let Some(hit) = blocker(site, pad, net, obstacles, via_r, half_w, options.clearance_mm, true) {
                refusals.add(name_blocker(hit, net));
                continue;
            }
            let stub = (centre - exit_distance(pad, site) - via_r).max(0.0);
            if stub > limit + 1e-9 {
                continue;
            }
            let placement = placement_class(pad, site, part);
            let cost = match part.partner {
                Some(p) => {
                    2.0 * stub
                        + (site.0 - p.0).hypot(site.1 - p.1)
                        + part.ic.map_or(0.0, |ic| 0.3 * (site.0 - ic.0).hypot(site.1 - ic.1))
                }
                None => {
                    let dir = ((site.0 - pad.center.0) / centre.max(1e-9), (site.1 - pad.center.1) / centre.max(1e-9));
                    2.0 * stub - 0.3 * (dir.0 * outward.0 + dir.1 * outward.1)
                }
            };
            let hole_gap = point_rect_distance_rot(site, pad) - drill_r;
            let real_gap = point_rect_distance_rot(site, pad) - via_r;
            if best.is_none_or(|b| cost < b.cost - 1e-9) {
                best = Some(Site { at: site, stub, via_gap: real_gap.max(0.0), hole_gap, placement, cost });
            }
            first_legal.get_or_insert(via_gap);
        }
    }
    best
}

/// Distance from `p` to the pad's rotated rectangle (0 inside).
fn point_rect_distance_rot(p: Point, pad: &PadInfo) -> f64 {
    point_polygon_distance(p, &pad.corners())
}

/// `site_is_clear` over a pre-filtered obstacle list.
#[allow(clippy::too_many_arguments)]
fn site_is_clear_in(site: Point, pad: &PadInfo, net: &str, obstacles: &[&Obstacle], via_r: f64, half_w: f64, clear: f64, via_site: bool) -> bool {
    blocker(site, pad, net, obstacles, via_r, half_w, clear, via_site).is_none()
}

/// The obstacles that can matter within `reach` of the pad.
fn nearby<'a>(obstacles: &'a [Obstacle], pad: &PadInfo, reach: f64) -> Vec<&'a Obstacle> {
    let (x0, y0, x1, y1) = pad.bbox;
    let (x0, y0, x1, y1) = (x0 - reach, y0 - reach, x1 + reach, y1 + reach);
    let inside = |p: Point, m: f64| p.0 >= x0 - m && p.0 <= x1 + m && p.1 >= y0 - m && p.1 <= y1 + m;
    obstacles
        .iter()
        .filter(|o| match o {
            Obstacle::Poly { corners, .. } => {
                let (mut a, mut b, mut c, mut d) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
                for p in corners {
                    a = a.min(p.0);
                    b = b.min(p.1);
                    c = c.max(p.0);
                    d = d.max(p.1);
                }
                !(c < x0 || a > x1 || d < y0 || b > y1)
            }
            Obstacle::Segment { a, b, half_width, .. } => {
                let (sx0, sy0, sx1, sy1) = (a.0.min(b.0) - half_width, a.1.min(b.1) - half_width, a.0.max(b.0) + half_width, a.1.max(b.1) + half_width);
                !(sx1 < x0 || sx0 > x1 || sy1 < y0 || sy0 > y1)
            }
            Obstacle::Circle { center, radius, .. } => inside(*center, *radius),
        })
        .collect()
}

/// The IC pin a pad's part serves: the nearest device pad on the same side
/// whose net is one of the part's nets, within 15 mm.
fn serving_ic(pad: &PadInfo, part_nets: &[&str], pads: &[PadInfo]) -> Option<Point> {
    pads.iter()
        .filter(|p| ref_kind(&p.reference) == "device" && p.layer == pad.layer && p.net.as_deref().is_some_and(|n| part_nets.contains(&n)))
        .map(|p| (p.center, (p.center.0 - pad.center.0).hypot(p.center.1 - pad.center.1)))
        .filter(|(_, d)| *d <= 15.0)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(c, _)| c)
}

pub(crate) fn plan_low_inductance(
    tree: &SexpNode,
    nets: &[String],
    options_for: &dyn Fn(&str) -> FanoutOptions,
    li: &LowInductance,
) -> FanoutPlan {
    let wanted: BTreeSet<&str> = nets.iter().map(String::as_str).collect();
    let geometry = BoardGeometry::read(tree);
    let pads = collect_pads(tree);
    let yards_all = courtyards(tree);
    let yards: Vec<&Courtyard> = if li.avoid_courtyards { yards_all.iter().collect() } else { Vec::new() };
    let (mut obstacles, copper) = routed_copper(tree);
    for pad in &pads {
        obstacles.push(pad_obstacle(pad));
    }
    let mut result = FanoutPlan { nets: nets.to_vec(), plane_source: geometry.plane_source(nets), mode: "low_inductance", ..FanoutPlan::default() };
    let name = |p: &PadInfo| format!("{}.{}", p.reference, p.number);
    let pads_of = |r: &str| pads.iter().filter(|p| p.reference == r).count();

    // Large pads, then device pins (whose vias capacitors may share), then the rest.
    let mut order: Vec<usize> = (0..pads.len()).collect();
    order.sort_by_key(|&i| {
        let pad = &pads[i];
        let o = options_for(pad.net.as_deref().unwrap_or(""));
        if pad.size.0.min(pad.size.1) >= o.large_pad_mm {
            0
        } else if ref_kind(&pad.reference) == "device" {
            1
        } else {
            2
        }
    });
    let mut large_with_vias: Vec<(&PadInfo, String)> = Vec::new();
    // Device-owned vias a capacitor may share: (position, size, drill, owner pad index, net).
    let mut device_vias: Vec<(Point, f64, f64, usize, String)> = Vec::new();
    for (i, pad) in pads.iter().enumerate() {
        let Some(net) = pad.net.as_deref() else { continue };
        if ref_kind(&pad.reference) != "device" {
            continue;
        }
        if let Some(v) = li.verdicts.get(&(identity_of(pad), net.to_string())) {
            if v.status == "pass" && matches!(v.connection.as_str(), "via" | "via_in_pad") {
                if let Some(at) = v.via {
                    if let Some(&(_, r)) = copper.get(net).and_then(|c| c.vias.iter().find(|(c, _)| (c.0 - at.0).hypot(c.1 - at.1) < 0.01)) {
                        device_vias.push((at, 2.0 * r, 0.0, i, net.to_string()));
                    }
                }
            }
        }
    }
    // Vias some capacitor already uses (by the judge, or shared in this plan).
    let mut cap_used: Vec<Point> = li
        .verdicts
        .iter()
        .filter(|(_, v)| ref_kind(v.pad.split('.').next().unwrap_or("")) == "capacitor" && v.status != "fail")
        .filter_map(|(_, v)| v.via)
        .collect();
    // Planned own vias per pad index (for the partner of a two-pad part).
    let mut planned_via: BTreeMap<usize, Point> = BTreeMap::new();

    for &i in &order {
        let pad = &pads[i];
        let Some(net) = pad.net.as_deref().filter(|net| wanted.contains(net)) else { continue };
        let options = options_for(net);
        let pname = name(pad);
        let mut decision = PadDecision {
            pad: pname.clone(),
            pad_uuid: pad.uuid.clone(),
            at: [round4(pad.center.0), round4(pad.center.1)],
            net: net.to_string(),
            layer: pad.layer.to_string(),
            action: "no_site",
            reason: String::new(),
            source: None,
            via: None,
            placement: None,
            stub_mm: None,
            via_gap_mm: None,
            hole_gap_mm: None,
            shares_via_of: None,
            symmetry: None,
        };
        let skip = |reason: &str| SkippedPad { reference: pad.reference.clone(), pad: pad.number.clone(), net: net.to_string(), reason: reason.to_string() };
        if !pad.smd {
            result.through_hole_pads += 1;
            decision.action = "through_hole";
            decision.reason = "through-hole pad: it reaches every layer, the plane included".into();
            result.pads.push(decision);
            continue;
        }
        let verdict = li.verdicts.get(&(identity_of(pad), net.to_string()));
        if let Some(v) = verdict {
            if let Some((kind, source)) = &v.exception {
                decision.action = "exception";
                decision.reason = format!("{kind} exception declared in the intent ({}): {}", v.status, v.detail);
                decision.source = Some(source.clone());
                result.skipped.push(skip(&decision.reason));
                result.pads.push(decision);
                continue;
            }
            let shared = matches!(v.connection.as_str(), "shared_via_ok" | "shared_pad_ok");
            if v.status == "pass" && (li.share_with_ic || !shared) {
                decision.action = "kept";
                decision.reason = format!("already passes plane_connection ({}): {}", v.connection, v.detail);
                decision.stub_mm = v.stub_mm;
                decision.via = v.via.map(|p| [round4(p.0), round4(p.1)]);
                result.skipped.push(skip(&decision.reason));
                result.pads.push(decision);
                continue;
            }
            if v.status == "unverified" && !matches!(v.connection.as_str(), "none" | "shared_via" | "shared_pad") {
                decision.action = "not_planned";
                decision.reason = format!("plane_connection is unverified for a reason a new via does not settle ({}): {}", v.connection, v.detail);
                result.skipped.push(skip(&decision.reason));
                result.pads.push(decision);
                continue;
            }
        } else if pad_reaches_plane(pad, net, copper.get(net), &geometry) {
            decision.action = "kept";
            decision.reason = format!("{ALREADY_REACHES_PLANE} (the net is not one plane_connection judges)");
            result.skipped.push(skip(ALREADY_REACHES_PLANE));
            result.pads.push(decision);
            continue;
        }
        if !geometry.planes.contains_key(net) {
            decision.reason = "the net has no zone on an inner layer".into();
            result.skipped.push(skip(&decision.reason));
            result.pads.push(decision);
            continue;
        }
        let regime = li.nets.get(net);
        let limit = regime.map_or(FALLBACK_STUB_MM, |r| r.max_stub_mm);
        let mut planned = PlannedFanout {
            reference: pad.reference.clone(),
            pad: pad.number.clone(),
            net: net.to_string(),
            layer: pad.layer.to_string(),
            pad_x: round4(pad.center.0),
            pad_y: round4(pad.center.1),
            via_x: 0.0,
            via_y: 0.0,
            stub_mm: 0.0,
            in_pad: false,
            via: true,
            via_size_mm: options.via_size_mm,
            via_drill_mm: options.via_drill_mm,
            track_width_mm: options.track_width_mm,
            placement: None,
            via_gap_mm: None,
            hole_gap_mm: None,
            shares_via_of: None,
        };
        let via_r = options.via_size_mm / 2.0;
        if pad.size.0.min(pad.size.1) >= options.large_pad_mm {
            let sites = in_pad_sites(pad, net, &obstacles, &geometry, &options);
            if sites.is_empty() {
                decision.reason = "large pad: no via site inside it reaches the plane and clears other copper".into();
                result.skipped.push(skip(&decision.reason));
                result.pads.push(decision);
                continue;
            }
            for site in &sites {
                obstacles.push(Obstacle::Circle { center: *site, radius: via_r, net: Some(net.to_string()) });
                let mut f = planned.clone();
                (f.via_x, f.via_y, f.in_pad, f.placement) = (round4(site.0), round4(site.1), true, Some("in_pad"));
                result.fanouts.push(f);
            }
            if ref_kind(&pad.reference) == "device" {
                device_vias.extend(sites.iter().map(|s| (*s, options.via_size_mm, options.via_drill_mm, i, net.to_string())));
            }
            large_with_vias.push((pad, net.to_string()));
            decision.action = "via_grid";
            decision.reason = format!("{} vias inside the {:.2} mm pad (exposed or thermal pad)", sites.len(), pad.size.0.min(pad.size.1));
            decision.via = sites.first().map(|p| [round4(p.0), round4(p.1)]);
            decision.stub_mm = Some(0.0);
            result.pads.push(decision);
            continue;
        }

        // The part's context: its other pad (two-pad parts), the partner
        // point the loop closes through, and the IC pin it serves.
        let two_pad = pads_of(&pad.reference) == 2;
        let other = two_pad.then(|| pads.iter().enumerate().find(|(j, p)| *j != i && p.reference == pad.reference)).flatten();
        let other_net = other.and_then(|(_, p)| p.net.as_deref());
        // Same stub width on both pads of a two-pad part (Eurocircuits:
        // "make the track width to each pad of the component the same width").
        let mut width = options.track_width_mm;
        if let Some(on) = other_net.filter(|n| wanted.contains(n)) {
            width = width.min(options_for(on).track_width_mm);
        }
        width = width.min(pad.size.0.min(pad.size.1));
        planned.track_width_mm = width;
        let part_nets: Vec<&str> = [Some(net), other_net].into_iter().flatten().collect();
        let part = PartContext {
            axis: other.map(|(_, o)| {
                let (dx, dy) = (o.center.0 - pad.center.0, o.center.1 - pad.center.1);
                let l = dx.hypot(dy).max(1e-9);
                (dx / l, dy / l)
            }),
            partner: other.map(|(j, o)| planned_via.get(&j).copied().unwrap_or(o.center)),
            ic: if two_pad { serving_ic(pad, &part_nets, &pads) } else { None },
            other_pad: other.map(|(_, o)| o.corners()),
            half_w: width / 2.0,
        };
        let reach = options.max_stub_mm.max(limit + options.via_size_mm) + options.via_size_mm + options.clearance_mm + 0.5;
        let near = nearby(&obstacles, pad, reach);

        // 1. Share the IC pin's via, where the regime allows it.
        let can_share = li.share_with_ic && ref_kind(&pad.reference) == "capacitor";
        let mut share_note: Option<String> = None;
        if can_share {
            match regime.map(|r| r.regime.as_str()) {
                Some("widely_spaced") => {
                    let drill_r = options.via_drill_mm / 2.0;
                    let best = device_vias
                        .iter()
                        .filter(|(_, _, _, owner, n)| n == net && pads[*owner].layer == pad.layer)
                        .filter(|(at, ..)| !cap_used.iter().any(|u| (u.0 - at.0).hypot(u.1 - at.1) < 0.01))
                        .filter_map(|&(at, size, drill, owner, _)| {
                            let d = point_rect_distance_rot(at, pad);
                            let gap = d - size / 2.0;
                            let hole = d - if drill > 0.0 { drill / 2.0 } else { drill_r };
                            let centre = (at.0 - pad.center.0).hypot(at.1 - pad.center.1);
                            let stub = (centre - exit_distance(pad, at) - size / 2.0).max(0.0);
                            let drill_of = if drill > 0.0 { drill / 2.0 } else { drill_r };
                            let max_gap = li.share_max_gap_mm.unwrap_or(((li.hole_gap_mm + drill_of) - size / 2.0).max(0.0) + 0.05);
                            let ok = gap <= max_gap.min(limit) + 1e-9
                                && (li.via_in_pad || hole >= li.hole_gap_mm - 1e-9)
                                && geometry.track_allowed(pad.center, at, width / 2.0, pad.layer)
                                && site_is_clear_in(at, pad, net, &near, 0.0, width / 2.0, options.clearance_mm, false)
                                && !segment_touches_pad(pad.center, at, width / 2.0, &pads[owner], options.clearance_mm);
                            ok.then_some((at, gap.max(0.0), hole, stub, owner))
                        })
                        .min_by(|a, b| a.1.total_cmp(&b.1));
                    if best.is_none() {
                        // Say why an IC via right there was not taken: the other side.
                        if let Some(&(_, _, _, owner, _)) = device_vias.iter().find(|(at, size, _, owner, n)| {
                            n == net && pads[*owner].layer != pad.layer && point_rect_distance_rot(*at, pad) - size / 2.0 <= li.share_max_gap_mm.unwrap_or(0.5)
                        }) {
                            share_note = Some(format!(
                                "not shared with {}: it is on the other side ({}): \"Decoupling capacitors should never share a via with an active device when they are located on opposite sides of the board\" [docdb learnemc-decoupling-widely-spaced-planes]",
                                name(&pads[owner]),
                                pads[owner].layer
                            ));
                        }
                    }
                    if let Some((at, gap, hole, stub, owner)) = best {
                        let of = name(&pads[owner]);
                        let spacing = regime.and_then(|r| r.spacing_mm).map_or("?".to_string(), |s| format!("{s:.3}"));
                        planned.via = false;
                        (planned.via_x, planned.via_y) = (round4(at.0), round4(at.1));
                        planned.stub_mm = round4((at.0 - pad.center.0).hypot(at.1 - pad.center.1));
                        planned.placement = Some("shared");
                        planned.via_gap_mm = Some(round4(gap));
                        planned.hole_gap_mm = Some(round4(hole));
                        planned.shares_via_of = Some(of.clone());
                        obstacles.push(Obstacle::Segment { a: pad.center, b: at, half_width: width / 2.0, net: Some(net.to_string()), layer: pad.layer.to_string() });
                        cap_used.push(at);
                        result.fanouts.push(planned);
                        decision.action = "share";
                        decision.reason = format!(
                            "shares {of}'s via: same side ({}), planes {spacing} mm apart (> 0.5 mm): \"If the decoupling capacitor can be located near enough to the active device to share the same via, this is optimal\" [docdb learnemc-decoupling-widely-spaced-planes]; the via is {:.2} mm from the pad edge",
                            pad.layer, gap
                        );
                        decision.source = Some("learnemc-decoupling-widely-spaced-planes".into());
                        decision.via = Some([round4(at.0), round4(at.1)]);
                        decision.stub_mm = Some(round4(stub));
                        decision.via_gap_mm = Some(round4(gap));
                        decision.hole_gap_mm = Some(round4(hole));
                        decision.placement = Some("shared");
                        decision.shares_via_of = Some(of);
                        result.pads.push(decision);
                        continue;
                    }
                }
                Some("closely_spaced") => {
                    share_note = Some(format!(
                        "not shared with an IC pin: planes {} mm apart (≤ 0.5 mm): \"Do not let high-frequency decoupling capacitors share a via with an active device\" [docdb learnemc-decoupling-closely-spaced-planes]",
                        regime.and_then(|r| r.spacing_mm).map_or("?".to_string(), |s| format!("{s:.3}"))
                    ))
                }
                _ => {
                    share_note = Some(format!(
                        "not shared with an IC pin: the plane spacing is unknown ({}), so sharing could be optimal (> 0.5 mm) or harmful (≤ 0.5 mm)",
                        regime.map_or("the net is not one plane_connection judges".to_string(), |r| r.note.clone())
                    ))
                }
            }
        }

        // 2. In the pad, when the caller declares filled and capped vias.
        if li.via_in_pad {
            let site = pad.center;
            if geometry.reaches_plane_with(net, site, via_r)
                && geometry.via_allowed(site, via_r)
                && site_is_clear_in(site, pad, net, &near, via_r, 0.0, options.clearance_mm, true)
            {
                obstacles.push(Obstacle::Circle { center: site, radius: via_r, net: Some(net.to_string()) });
                (planned.via_x, planned.via_y, planned.in_pad, planned.placement) = (round4(site.0), round4(site.1), true, Some("in_pad"));
                planned.via_gap_mm = Some(0.0);
                result.fanouts.push(planned);
                planned_via.insert(i, site);
                decision.action = "via_in_pad";
                decision.reason = "via in the pad (declared filled and capped): UG483 p.30 lists via-in-pad as \"an option for reducing parasitic inductance\" [docdb xilinx-ug483-7series-pcb]".into();
                decision.source = Some("xilinx-ug483-7series-pcb".into());
                decision.via = Some([round4(site.0), round4(site.1)]);
                decision.stub_mm = Some(0.0);
                decision.via_gap_mm = Some(0.0);
                decision.placement = Some("in_pad");
                result.pads.push(decision);
                continue;
            }
        }

        // 3. Own via against the pad.
        let mut refusals = Refusals::default();
        let yards_near: Vec<&Courtyard> = yards.iter().copied().filter(|y| point_rect_distance(pad.center, y.bbox) <= reach).collect();
        if let Some(site) = li_site(pad, net, &part, &near, &geometry, &options, li, limit, &yards_near, &mut refusals) {
            obstacles.push(Obstacle::Circle { center: site.at, radius: via_r, net: Some(net.to_string()) });
            obstacles.push(Obstacle::Segment { a: pad.center, b: site.at, half_width: width / 2.0, net: Some(net.to_string()), layer: pad.layer.to_string() });
            (planned.via_x, planned.via_y) = (round4(site.at.0), round4(site.at.1));
            planned.stub_mm = round4((site.at.0 - pad.center.0).hypot(site.at.1 - pad.center.1));
            planned.placement = Some(site.placement);
            planned.via_gap_mm = Some(round4(site.via_gap));
            planned.hole_gap_mm = Some(round4(site.hole_gap));
            if ref_kind(&pad.reference) == "device" {
                device_vias.push((site.at, options.via_size_mm, options.via_drill_mm, i, net.to_string()));
            }
            planned_via.insert(i, site.at);
            result.fanouts.push(planned);
            decision.action = "via";
            let why = match site.placement {
                "side" => "beside the pad: side vias couple more strongly than end vias and lower the mounting inductance (UG483 p.24) [docdb xilinx-ug483-7series-pcb]",
                "end" => "at the pad's end: no legal side site was as good (UG483 p.24 prefers the sides) [docdb xilinx-ug483-7series-pcb]",
                "corner" => "at the pad's corner, towards the part's other pad: closest legal site to it [docdb learnemc-decoupling-widely-spaced-planes]",
                "inner" => "towards the part's other pad [docdb learnemc-decoupling-widely-spaced-planes]",
                _ => "outward from the part, against the pad [docdb xilinx-ug483-7series-pcb]",
            };
            decision.reason = format!(
                "own via {:.2} mm from the pad edge (drill edge {:.2} mm; ≥ {} mm per Eurocircuits [docdb eurocircuits-tombstoning]), {:.2} mm connecting trace (≤ {limit} mm); {why}{}",
                site.via_gap,
                site.hole_gap,
                li.hole_gap_mm,
                site.stub,
                share_note.as_ref().map(|n| format!("; {n}")).unwrap_or_default()
            );
            decision.source = Some("xilinx-ug483-7series-pcb; eurocircuits-tombstoning".into());
            decision.via = Some([round4(site.at.0), round4(site.at.1)]);
            decision.stub_mm = Some(round4(site.stub));
            decision.via_gap_mm = Some(round4(site.via_gap));
            decision.hole_gap_mm = Some(round4(site.hole_gap));
            decision.placement = Some(site.placement);
            result.pads.push(decision);
            continue;
        }

        // 4. A stub into the part's own exposed pad (a QFN ground pin).
        if let Some(end) = tie_to_large_pad(pad, net, &large_with_vias, &obstacles, &geometry, &options) {
            let w = options.track_width_mm.min(pad.size.0.min(pad.size.1));
            obstacles.push(Obstacle::Segment { a: pad.center, b: end, half_width: w / 2.0, net: Some(net.to_string()), layer: pad.layer.to_string() });
            let stub = (end.0 - pad.center.0).hypot(end.1 - pad.center.1);
            (planned.via_x, planned.via_y, planned.stub_mm, planned.via, planned.track_width_mm) = (round4(end.0), round4(end.1), round4(stub), false, w);
            planned.placement = Some("tie");
            result.fanouts.push(planned);
            decision.action = "tie";
            decision.reason = format!("no own via site within {limit} mm; tied to {}'s exposed pad, whose vias reach the plane", pad.reference);
            decision.stub_mm = Some(round4(stub));
            result.pads.push(decision);
            continue;
        }
        decision.reason = format!(
            "no via site against the pad within {limit} mm (pad edge to via edge) clears other copper, keep-outs{} and lands on the plane; refused sites: {}{}",
            if li.avoid_courtyards { ", other parts' bodies" } else { "" },
            refusals.summary(),
            share_note.as_ref().map(|n| format!("; {n}")).unwrap_or_default()
        );
        result.skipped.push(skip(&decision.reason));
        result.pads.push(decision);
    }

    // Two-pad parts: report how the other pad was treated (tombstoning symmetry).
    let actions: BTreeMap<String, (String, &'static str, Option<f64>, f64)> = result
        .pads
        .iter()
        .map(|d| (d.pad.clone(), (d.net.clone(), d.action, d.hole_gap_mm, 0.0)))
        .collect();
    let widths: BTreeMap<String, f64> = result.fanouts.iter().filter(|f| f.via || f.shares_via_of.is_some()).map(|f| (format!("{}.{}", f.reference, f.pad), f.track_width_mm)).collect();
    for d in result.pads.iter_mut() {
        let Some((reference, _)) = d.pad.split_once('.') else { continue };
        if pads_of(reference) != 2 || !matches!(d.action, "via" | "via_in_pad" | "share" | "no_site") {
            continue;
        }
        let Some(other) = pads.iter().find(|p| p.reference == reference && format!("{}.{}", p.reference, p.number) != d.pad) else { continue };
        let oname = format!("{}.{}", other.reference, other.number);
        d.symmetry = Some(match actions.get(&oname) {
            Some((_, a, gap, _)) => {
                let w = |n: &str| widths.get(n).map_or("-".to_string(), |w| format!("{w}"));
                let fine = matches!((d.action, *a), ("via" | "share", "via" | "share") | ("via_in_pad", "via_in_pad"));
                format!(
                    "{}: {oname} {a}{}; stub widths {} / {} mm",
                    if fine { "symmetric" } else { "check" },
                    gap.map(|g| format!(" (drill edge {g:.2} mm)")).unwrap_or_default(),
                    w(&d.pad),
                    w(&oname)
                )
            }
            None => format!("{oname} is not on a plane net (net {}): only this pad drops to a plane", other.net.as_deref().unwrap_or("none")),
        });
    }
    result
}

/// Does a stub from `a` to `b` (half width `w`) come within `clear` of `pad`?
fn segment_touches_pad(a: Point, b: Point, w: f64, pad: &PadInfo, clear: f64) -> bool {
    segment_polygon_distance(a, b, &pad.corners()) < w + clear - EPS
}

/// The board text with a plan's items added, for the judge to read (the
/// dry run's self-check). Zones are not refilled; new same-net vias sit
/// inside the existing fill by construction.
pub(crate) fn board_with_plan(snapshot: &str, plan: &FanoutPlan) -> String {
    let mut items = String::new();
    let q = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    for f in &plan.fanouts {
        if f.via {
            items.push_str(&format!(
                "\t(via (at {} {}) (size {}) (drill {}) (layers \"F.Cu\" \"B.Cu\") (net \"{}\"))\n",
                f.via_x, f.via_y, f.via_size_mm, f.via_drill_mm, q(&f.net)
            ));
        }
        if !f.in_pad {
            items.push_str(&format!(
                "\t(segment (start {} {}) (end {} {}) (width {}) (layer \"{}\") (net \"{}\"))\n",
                f.pad_x, f.pad_y, f.via_x, f.via_y, f.track_width_mm, q(&f.layer), q(&f.net)
            ));
        }
    }
    match snapshot.rfind(')') {
        Some(end) => format!("{}{}{}", &snapshot[..end], items, &snapshot[end..]),
        None => snapshot.to_string(),
    }
}

impl LowInductance {
    /// Take the judge's per-pad verdicts and per-net regime on the board as it is.
    pub(crate) fn absorb(&mut self, verdicts: &[crate::tools::design_intent::PlaneVerdict]) {
        for v in verdicts {
            let m = &v.measured;
            let via = m["target"]["via"].as_array().and_then(|a| Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?)));
            let exception = m.get("exception").filter(|e| e.is_object()).map(|e| {
                (e["kind"].as_str().unwrap_or("").to_string(), e["source"].as_str().unwrap_or("").to_string())
            });
            let at = m["at"].as_array().and_then(|a| Some((a.first()?.as_f64()?, a.get(1)?.as_f64()?))).unwrap_or((f64::NAN, f64::NAN));
            self.verdicts.insert(
                (pad_identity(&v.pad, m["pad_uuid"].as_str(), at), v.net.clone()),
                JudgeVerdict {
                    pad: v.pad.clone(),
                    status: v.status.clone(),
                    connection: m["connection"].as_str().unwrap_or("").to_string(),
                    detail: v.detail.clone(),
                    via,
                    stub_mm: m["stub_mm"].as_f64(),
                    exception,
                },
            );
            let r = &v.required;
            if let Some(limit) = r["max_stub_mm"].as_f64() {
                self.nets.entry(v.net.clone()).or_insert_with(|| NetRegime {
                    regime: r["regime"].as_str().unwrap_or("unknown").to_string(),
                    spacing_mm: r["plane_spacing_mm"].as_f64(),
                    note: r["regime_note"].as_str().unwrap_or("").to_string(),
                    max_stub_mm: if limit.is_finite() && limit > 0.0 { limit } else { FALLBACK_STUB_MM },
                });
            }
        }
    }
}


/// Plan a fan-out with the judge: `plane_connection` on the board as it is
/// decides which pads already pass, which are excepted and each net's
/// regime; `legacy` keeps the pre-Pass-24 planner.
pub(crate) fn plan_with_judge(
    tree: &SexpNode,
    nets: &[String],
    options_for: &dyn Fn(&str) -> FanoutOptions,
    legacy: bool,
    settings: &LowInductance,
    intent: &crate::tools::design_intent::DesignIntent,
) -> (FanoutPlan, Vec<crate::tools::design_intent::PlaneVerdict>) {
    let before = crate::tools::design_intent::plane_verdicts(tree, intent);
    if legacy {
        return (plan(tree, nets, options_for), before);
    }
    let mut li = settings.clone();
    li.absorb(&before);
    (plan_low_inductance(tree, nets, options_for, &li), before)
}

/// The judge's per-pad verdicts on `nets`, decision-ready: status, the
/// connecting trace and via it measured, sharing, and its reason.
pub(crate) fn self_check(verdicts: &[crate::tools::design_intent::PlaneVerdict], nets: &[String]) -> serde_json::Value {
    let mine: Vec<&crate::tools::design_intent::PlaneVerdict> = verdicts.iter().filter(|v| nets.contains(&v.net)).collect();
    let mut summary: BTreeMap<&str, usize> = BTreeMap::new();
    for v in &mine {
        *summary.entry(v.status.as_str()).or_default() += 1;
    }
    let pads: Vec<serde_json::Value> = mine
        .iter()
        .map(|v| {
            let m = &v.measured;
            let mut row = serde_json::json!({
                "pad": v.pad,
                "pad_uuid": m["pad_uuid"],
                "at": m["at"],
                "net": v.net,
                "status": v.status,
                "connection": m["connection"],
                "stub_mm": m["stub_mm"],
                "via": m["target"]["via"],
                "detail": v.detail,
            });
            for key in ["via_shared_with", "warnings", "exception"] {
                if let Some(x) = m.get(key) {
                    row[key] = x.clone();
                }
            }
            row
        })
        .collect();
    let failing = mine.iter().filter(|v| v.status == "fail" || v.status == "excepted_fail").count();
    serde_json::json!({
        "judge": "plane_connection, as check_design_intent runs it (intent exceptions and limits included)",
        "summary": summary,
        "failing": failing,
        "pads": pads,
    })
}

/// A stub from `pad` into a same-net large pad of the same footprint that
/// already has vias to the plane (a QFN ground pin to its exposed pad).
/// The stub is no wider than the pin, so it fits a fine-pitch row.
fn tie_to_large_pad(
    pad: &PadInfo,
    net: &str,
    large: &[(&PadInfo, String)],
    obstacles: &[Obstacle],
    geometry: &BoardGeometry,
    options: &FanoutOptions,
) -> Option<Point> {
    let half_w = options.track_width_mm.min(pad.size.0.min(pad.size.1)) / 2.0;
    large
        .iter()
        .filter(|(target, target_net)| target_net == net && target.reference == pad.reference)
        .filter_map(|(target, _)| {
            // The nearest point of the target, nudged half a width inside it.
            let corners = target.corners();
            let nearest = polygon_edges(&corners)
                .map(|(a, b)| closest_point(pad.center, a, b))
                .min_by(|p, q| {
                    let d = |x: &Point| (x.0 - pad.center.0).hypot(x.1 - pad.center.1);
                    d(p).total_cmp(&d(q))
                })?;
            let (dx, dy) = (target.center.0 - nearest.0, target.center.1 - nearest.1);
            let length = dx.hypot(dy).max(1e-9);
            let end = (nearest.0 + dx / length * half_w, nearest.1 + dy / length * half_w);
            let reach = (end.0 - pad.center.0).hypot(end.1 - pad.center.1);
            (reach <= options.max_stub_mm
                && geometry.track_allowed(pad.center, end, half_w, pad.layer)
                && site_is_clear(end, pad, net, obstacles, half_w, half_w, options.clearance_mm, false))
                .then_some((end, reach))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(end, _)| end)
}

fn closest_point(p: Point, a: Point, b: Point) -> Point {
    let (vx, vy) = (b.0 - a.0, b.1 - a.1);
    let length2 = vx * vx + vy * vy;
    let t = if length2 == 0.0 { 0.0 } else { (((p.0 - a.0) * vx + (p.1 - a.1) * vy) / length2).clamp(0.0, 1.0) };
    (a.0 + t * vx, a.1 + t * vy)
}

/// A grid of vias inside a large pad, in the pad's own (rotated) frame.
fn in_pad_sites(pad: &PadInfo, net: &str, obstacles: &[Obstacle], geometry: &BoardGeometry, options: &FanoutOptions) -> Vec<Point> {
    let via_r = options.via_size_mm / 2.0;
    let inset = via_r + 0.15;
    let pitch = options.via_size_mm + 0.5;
    let axis = |extent: f64| -> Vec<f64> {
        let usable = extent - 2.0 * inset;
        if usable < 0.0 {
            return vec![];
        }
        let n = ((usable / pitch).floor() as usize + 1).min(3);
        let span = (n - 1) as f64 * pitch;
        (0..n).map(|i| -span / 2.0 + i as f64 * pitch).collect()
    };
    let mut sites = Vec::new();
    for dx in axis(pad.size.0) {
        for dy in axis(pad.size.1) {
            let site = konnect_sexp::geometry::transform_pad(dx, dy, pad.center.0, pad.center.1, pad.angle);
            // A through via in the pad spans every layer: other-net copper on
            // any of them (a B.Cu track under an exposed pad) must be clear.
            if geometry.inside_board(site, options.edge_clearance_mm + via_r)
                && geometry.reaches_plane_with(net, site, via_r)
                && geometry.via_allowed(site, via_r)
                && site_is_clear(site, pad, net, obstacles, via_r, 0.0, options.clearance_mm, true)
            {
                sites.push(site);
            }
        }
    }
    sites
}

/// The closest clear via site, preferring directions away from the
/// footprint's centre (under a chip is where its other pins are).
fn best_site(
    pad: &PadInfo,
    net: &str,
    obstacles: &[Obstacle],
    geometry: &BoardGeometry,
    options: &FanoutOptions,
) -> Option<(Point, f64)> {
    let via_r = options.via_size_mm / 2.0;
    let half_w = options.track_width_mm / 2.0;
    let outward = {
        let (dx, dy) = (pad.center.0 - pad.footprint_center.0, pad.center.1 - pad.footprint_center.1);
        let length = dx.hypot(dy);
        if length > 1e-6 { (dx / length, dy / length) } else { (0.0, 0.0) }
    };
    let s = std::f64::consts::FRAC_1_SQRT_2;
    let directions = [(1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0), (s, s), (s, -s), (-s, s), (-s, -s)];
    let mut best: Option<(Point, f64, f64)> = None;
    for (dx, dy) in directions {
        let outwardness = dx * outward.0 + dy * outward.1;
        let mut distance = 0.0;
        while distance <= options.max_stub_mm + 1e-9 {
            distance += 0.05;
            let site = (pad.center.0 + dx * distance, pad.center.1 + dy * distance);
            // Not on its own pad: a via in a small SMD pad wicks the solder away.
            if point_polygon_distance(site, &pad.corners()) < via_r + 0.05 {
                continue;
            }
            if !geometry.inside_board(site, options.edge_clearance_mm + via_r) {
                break;
            }
            if !geometry.reaches_plane_with(net, site, via_r)
                || !geometry.via_allowed(site, via_r)
                || !geometry.track_allowed(pad.center, site, half_w, pad.layer)
                || !site_is_clear(site, pad, net, obstacles, via_r, half_w, options.clearance_mm, true)
            {
                continue;
            }
            let score = distance - 0.3 * outwardness;
            if best.is_none_or(|(_, _, s)| score < s) {
                best = Some((site, distance, score));
            }
            break;
        }
    }
    best.map(|(site, stub, _)| (site, stub))
}

/// `via_site`: the site is a new via, so same-net vias need drill spacing
/// from it; a stub end (a tie into a pad) has no drill.
#[allow(clippy::too_many_arguments)]
fn site_is_clear(
    site: Point,
    pad: &PadInfo,
    net: &str,
    obstacles: &[Obstacle],
    via_r: f64,
    half_w: f64,
    clear: f64,
    via_site: bool,
) -> bool {
    let other = |owner: &Option<String>| owner.as_deref() != Some(net);
    for obstacle in obstacles {
        match obstacle {
            Obstacle::Poly { corners, net: owner, layers, reference, center, large } => {
                // Another net's pad, another part's pad on the same net
                // (touching it would join that part's copper: a daisy chain),
                // or a small pad of the same part (a via in the neighbouring
                // pin). Only the pad itself and its part's exposed pad are free.
                let itself = *reference == pad.reference && (center.0 - pad.center.0).hypot(center.1 - pad.center.1) < 1e-6;
                if other(owner) || *reference != pad.reference || (!itself && !large) {
                    let same_layer = match layers {
                        Layers::All => true,
                        Layers::One(layer) => *layer == pad.layer,
                    };
                    // A via spans every layer; a stub end (a tie) only the pad's.
                    if (via_site || same_layer) && point_polygon_distance(site, corners) < via_r + clear - EPS {
                        return false;
                    }
                    if same_layer && segment_polygon_distance(pad.center, site, corners) < half_w + clear - EPS {
                        return false;
                    }
                }
            }
            Obstacle::Segment { a, b, half_width, net: owner, layer } => {
                if other(owner) {
                    if (via_site || layer == pad.layer) && point_segment_distance(site, *a, *b) < via_r + half_width + clear - EPS {
                        return false;
                    }
                    if layer == pad.layer && segment_segment_distance(pad.center, site, *a, *b) < half_w + half_width + clear - EPS {
                        return false;
                    }
                }
            }
            Obstacle::Circle { center, radius, net: owner } => {
                let gap = (site.0 - center.0).hypot(site.1 - center.1);
                if other(owner) {
                    if gap < via_r + radius + clear - EPS
                        || point_segment_distance(*center, pad.center, site) < radius + half_w + clear - EPS
                    {
                        return false;
                    }
                } else if via_site && gap < via_r + radius + clear.max(0.25) {
                    // Same-net vias still need drill-to-drill spacing.
                    return false;
                }
            }
        }
    }
    true
}

// ─── Geometry helpers ───────────────────────────────────────────────────────

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

fn point_rect_distance((x, y): Point, (x0, y0, x1, y1): Bbox) -> f64 {
    let dx = (x0 - x).max(0.0).max(x - x1);
    let dy = (y0 - y).max(0.0).max(y - y1);
    dx.hypot(dy)
}

fn point_segment_distance(p: Point, a: Point, b: Point) -> f64 {
    let (vx, vy) = (b.0 - a.0, b.1 - a.1);
    let length2 = vx * vx + vy * vy;
    let t = if length2 == 0.0 { 0.0 } else { (((p.0 - a.0) * vx + (p.1 - a.1) * vy) / length2).clamp(0.0, 1.0) };
    (p.0 - (a.0 + t * vx)).hypot(p.1 - (a.1 + t * vy))
}

fn segments_intersect(a: Point, b: Point, c: Point, d: Point) -> bool {
    let cross = |o: Point, p: Point, q: Point| (p.0 - o.0) * (q.1 - o.1) - (p.1 - o.1) * (q.0 - o.0);
    let (d1, d2) = (cross(c, d, a), cross(c, d, b));
    let (d3, d4) = (cross(a, b, c), cross(a, b, d));
    (d1 > 0.0) != (d2 > 0.0) && (d3 > 0.0) != (d4 > 0.0)
}

fn segment_segment_distance(a: Point, b: Point, c: Point, d: Point) -> f64 {
    if segments_intersect(a, b, c, d) {
        return 0.0;
    }
    point_segment_distance(a, c, d)
        .min(point_segment_distance(b, c, d))
        .min(point_segment_distance(c, a, b))
        .min(point_segment_distance(d, a, b))
}


/// Distances within this of a limit count as meeting it (float rounding on
/// exact-pitch layouts: a 0.5 mm-pitch pin's stub sits exactly at clearance).
const EPS: f64 = 1e-6;

fn point_polygon_distance(p: Point, polygon: &[Point]) -> f64 {
    if point_in_polygon(p, polygon) {
        return 0.0;
    }
    polygon_edges(polygon).map(|(a, b)| point_segment_distance(p, a, b)).fold(f64::INFINITY, f64::min)
}

fn segment_polygon_distance(a: Point, b: Point, polygon: &[Point]) -> f64 {
    if point_in_polygon(a, polygon) || point_in_polygon(b, polygon) {
        return 0.0;
    }
    polygon_edges(polygon).map(|(p, q)| segment_segment_distance(a, b, p, q)).fold(f64::INFINITY, f64::min)
}

pub(crate) fn point_in_polygon((x, y): Point, polygon: &[Point]) -> bool {
    let mut inside = false;
    let mut j = polygon.len().wrapping_sub(1);
    for i in 0..polygon.len() {
        let (xi, yi) = polygon[i];
        let (xj, yj) = polygon[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// A polygon's edges without its fracture slits: KiCad writes a fill with
/// holes as one outline whose holes are joined to it by zero-width slits,
/// each an edge walked once in each direction. Copper lies on both sides of
/// a slit, so it is no edge for a via's clearance (before Pass 24 a via
/// within its radius of a slit was refused as "off the plane").
fn solid_edges(polygon: &[Point]) -> impl Iterator<Item = (Point, Point)> + '_ {
    let key = |p: Point| ((p.0 * 1e4).round() as i64, (p.1 * 1e4).round() as i64);
    let seen: BTreeSet<((i64, i64), (i64, i64))> = polygon_edges(polygon).map(|(a, b)| (key(a), key(b))).collect();
    polygon_edges(polygon).filter(move |&(a, b)| key(a) == key(b) || !seen.contains(&(key(b), key(a))))
}

fn polygon_edges(polygon: &[Point]) -> impl Iterator<Item = (Point, Point)> + '_ {
    (0..polygon.len()).map(move |i| (polygon[i], polygon[(i + 1) % polygon.len()]))
}

fn circle_hits_polygon(c: Point, r: f64, polygon: &[Point]) -> bool {
    point_in_polygon(c, polygon) || polygon_edges(polygon).any(|(a, b)| point_segment_distance(c, a, b) < r)
}

fn segment_hits_polygon(a: Point, b: Point, half_width: f64, polygon: &[Point]) -> bool {
    point_in_polygon(a, polygon)
        || point_in_polygon(b, polygon)
        || polygon_edges(polygon).any(|(p, q)| segment_segment_distance(a, b, p, q) < half_width)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_crystal_referenced_x_keeps_the_standard_courtyard_margin() {
        let bbox = (0.0, 0.0, 3.2, 2.5);
        assert_eq!(courtyard_margin("X1", "Crystal:Crystal_SMD_3225-4Pin_3.2x2.5mm", 4, bbox), 0.25);
        assert_eq!(courtyard_margin("X2", "Connector_PinHeader_2.54mm:PinHeader_1x04", 4, bbox), 0.5);
        assert_eq!(courtyard_margin("J1", "Connector_USB:USB_C_Receptacle", 16, bbox), 0.5);
    }

    use super::*;
    use konnect_sexp::parse_sexp;

    fn defaults(_: &str) -> FanoutOptions {
        FanoutOptions::default()
    }

    /// A 20 × 20 mm board, a GND plane on In1.Cu, one 0603 capacitor whose
    /// pad 2 is GND and pad 1 is VCC.
    fn board(extra: &str) -> SexpNode {
        parse_sexp(&format!(
            "(kicad_pcb (version 20260206)
              (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\"))
              (footprint \"C\" (layer \"F.Cu\") (at 10 10)
                (property \"Reference\" \"C1\")
                (pad \"1\" smd roundrect (at -0.775 0) (size 0.9 0.95) (layers \"F.Cu\") (net \"VCC\"))
                (pad \"2\" smd roundrect (at 0.775 0) (size 0.9 0.95) (layers \"F.Cu\") (net \"GND\")))
              (zone (net \"GND\") (layers \"In1.Cu\") (polygon (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20))))
              {extra})"
        ))
        .unwrap()
    }

    fn gnd() -> Vec<String> {
        vec!["GND".to_string()]
    }

    #[test]
    fn plane_nets_are_the_nets_with_inner_zones() {
        assert_eq!(plane_nets(&board("")), gnd());
    }

    #[test]
    fn a_gnd_pad_gets_a_clear_via_outward_from_the_part() {
        let plan = plan(&board(""), &gnd(), &defaults);
        assert_eq!(plan.fanouts.len(), 1, "{plan:?}");
        let fanout = &plan.fanouts[0];
        assert_eq!((fanout.reference.as_str(), fanout.pad.as_str()), ("C1", "2"));
        assert!(fanout.via_x > 10.775 + 0.45 + 0.3, "{fanout:?}");
        assert!((fanout.via_y - 10.0).abs() < 1e-6, "{fanout:?}");
        assert!(!fanout.in_pad);
    }

    #[test]
    fn another_nets_track_or_arc_pushes_the_via_elsewhere() {
        for extra in [
            "(segment (start 12.2 5) (end 12.2 15) (width 0.3) (layer \"F.Cu\") (net \"VCC\"))",
            "(arc (start 12.2 5) (mid 12.6 10) (end 12.2 15) (width 0.3) (layer \"B.Cu\") (net \"VCC\"))",
        ] {
            let plan = plan(&board(extra), &gnd(), &defaults);
            let f = &plan.fanouts[0];
            assert!(!(f.via_x > 11.7 && f.via_x < 13.2 && f.via_y > 5.0 && f.via_y < 15.0), "{extra}: {f:?}");
        }
    }

    #[test]
    fn keepouts_are_never_entered_by_the_via_circle() {
        let ko = "(zone (layers \"F.Cu\" \"In1.Cu\" \"B.Cu\") (keepout (tracks not_allowed) (vias not_allowed)) (polygon (pts (xy 11.9 0) (xy 20 0) (xy 20 20) (xy 11.9 20))))";
        for extra in [ko.to_string(), format!("(footprint \"M\" (layer \"F.Cu\") (at 15 15) (property \"Reference\" \"M1\") {ko})")] {
            let plan = plan(&board(&extra), &gnd(), &defaults);
            for f in &plan.fanouts {
                assert!(f.via_x + 0.3 <= 11.9, "{f:?}");
            }
        }
    }

    #[test]
    fn a_via_never_lands_outside_an_l_shaped_outline() {
        // L-shape: the top-right quadrant (x > 10.9, y < 10.4) is not board.
        let outline = "(gr_line (start 0 0) (end 10.9 0) (layer \"Edge.Cuts\")) (gr_line (start 10.9 0) (end 10.9 10.4) (layer \"Edge.Cuts\")) (gr_line (start 10.9 10.4) (end 20 10.4) (layer \"Edge.Cuts\")) (gr_line (start 20 10.4) (end 20 20) (layer \"Edge.Cuts\")) (gr_line (start 20 20) (end 0 20) (layer \"Edge.Cuts\")) (gr_line (start 0 20) (end 0 0) (layer \"Edge.Cuts\"))";
        let tree = parse_sexp(&format!(
            "(kicad_pcb {outline}
              (footprint \"C\" (layer \"F.Cu\") (at 10 11.5) (property \"Reference\" \"C1\")
                (pad \"2\" smd roundrect (at 0.775 0) (size 0.9 0.95) (layers \"F.Cu\") (net \"GND\")))
              (zone (net \"GND\") (layers \"In1.Cu\") (polygon (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20)))))"
        ))
        .unwrap();
        let plan = plan(&tree, &gnd(), &defaults);
        let geometry = BoardGeometry::read(&tree);
        for f in &plan.fanouts {
            assert!(geometry.inside_board((f.via_x, f.via_y), 0.5 + 0.3), "{f:?}");
        }
        assert!(!plan.fanouts.is_empty());
    }

    #[test]
    fn a_via_must_land_inside_the_plane() {
        // GND plane covers only x < 11: the outward (+x) site is off the plane.
        let tree = parse_sexp(
            "(kicad_pcb (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\"))
              (footprint \"C\" (layer \"F.Cu\") (at 10 10) (property \"Reference\" \"C1\")
                (pad \"2\" smd roundrect (at 0.775 0) (size 0.9 0.95) (layers \"F.Cu\") (net \"GND\")))
              (zone (net \"GND\") (layers \"In1.Cu\") (polygon (pts (xy 0 0) (xy 11 0) (xy 11 20) (xy 0 20)))))",
        )
        .unwrap();
        let plan = plan(&tree, &gnd(), &defaults);
        for f in &plan.fanouts {
            assert!(f.via_x < 11.0, "{f:?}");
        }
    }

    #[test]
    fn a_pad_joined_only_to_another_pad_does_not_count_as_reaching_the_plane() {
        // A GND track from pad 2 to somewhere, no via: still needs a fan-out.
        let joined = board("(segment (start 10.775 10) (end 10.775 14) (width 0.3) (layer \"F.Cu\") (net \"GND\"))");
        let plan = plan(&joined, &gnd(), &defaults);
        assert_eq!(plan.fanouts.len(), 1, "{plan:?}");
        assert!(!plan.all_pads_reach_planes());
    }

    #[test]
    fn a_pad_whose_track_ends_on_a_plane_via_reaches_it() {
        let done = board(
            "(via (at 10.775 13) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (net \"GND\"))
             (segment (start 10.775 10) (end 10.775 12) (width 0.3) (layer \"F.Cu\") (net \"GND\"))
             (segment (start 10.775 12) (end 10.775 13) (width 0.3) (layer \"F.Cu\") (net \"GND\"))",
        );
        let plan = plan(&done, &gnd(), &defaults);
        assert!(plan.fanouts.is_empty(), "{plan:?}");
        assert!(plan.all_pads_reach_planes());
    }

    #[test]
    fn a_large_rotated_pad_gets_a_via_grid_inside_it() {
        // A 3.45 mm exposed pad rotated 45°.
        let tree = parse_sexp(
            "(kicad_pcb (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\"))
              (footprint \"QFN\" (layer \"F.Cu\") (at 10 10 45) (property \"Reference\" \"U1\")
                (pad \"33\" smd rect (at 0 0 45) (size 3.45 3.45) (layers \"F.Cu\") (net \"GND\")))
              (zone (net \"GND\") (layers \"In1.Cu\") (polygon (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20)))))",
        )
        .unwrap();
        let plan = plan(&tree, &gnd(), &defaults);
        assert_eq!(plan.fanouts.len(), 9, "{plan:?}");
        for f in &plan.fanouts {
            assert!(f.in_pad && f.stub_mm == 0.0);
            // Inside the rotated square: |u|,|v| <= half size in the pad frame.
            let (dx, dy) = (f.via_x - 10.0, f.via_y - 10.0);
            let (c, s) = (std::f64::consts::FRAC_1_SQRT_2, std::f64::consts::FRAC_1_SQRT_2);
            let (u, v) = (dx * c - dy * s, dx * s + dy * c);
            assert!(u.abs() <= 1.725 && v.abs() <= 1.725, "{f:?}");
        }
    }

    /// A 3.45 mm exposed GND pad at (10, 10); `extra` adds copper.
    fn exposed_pad(extra: &str) -> SexpNode {
        parse_sexp(&format!(
            "(kicad_pcb (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\"))
              (footprint \"QFN\" (layer \"F.Cu\") (at 10 10) (property \"Reference\" \"U1\")
                (pad \"33\" smd rect (at 0 0) (size 3.45 3.45) (layers \"F.Cu\") (net \"GND\")))
              (zone (net \"GND\") (layers \"In1.Cu\") (polygon (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20)))
                {extra}))"
        ))
        .unwrap()
    }

    #[test]
    fn an_in_pad_via_avoids_other_net_copper_on_the_far_side() {
        // A B.Cu signal track runs under the pad along y = 10; a through via
        // there would short it. The grid row on y = 10 must be dropped.
        let clear = plan(&exposed_pad(""), &gnd(), &defaults);
        assert_eq!(clear.fanouts.len(), 9);
        let tree = parse_sexp(
            "(kicad_pcb (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\"))
              (footprint \"QFN\" (layer \"F.Cu\") (at 10 10) (property \"Reference\" \"U1\")
                (pad \"33\" smd rect (at 0 0) (size 3.45 3.45) (layers \"F.Cu\") (net \"GND\")))
              (zone (net \"GND\") (layers \"In1.Cu\") (polygon (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20))))
              (segment (start 0 10) (end 20 10) (width 0.2) (layer \"B.Cu\") (net \"SIG\")))",
        )
        .unwrap();
        let crossed = plan(&tree, &gnd(), &defaults);
        assert_eq!(crossed.fanouts.len(), 6, "{crossed:?}");
        for f in &crossed.fanouts {
            assert!((f.via_y - 10.0).abs() > 0.3 + 0.1 + 0.2, "{f:?}");
        }
    }

    #[test]
    fn a_void_in_the_plane_fill_is_not_the_plane() {
        // The outline covers the board, but the fill has a void over the pad's
        // left half (x < 10): only the right-hand grid column lands on copper.
        let fill = "(filled_polygon (layer \"In1.Cu\") (pts (xy 10 0) (xy 20 0) (xy 20 20) (xy 10 20)))";
        let plan = plan(&exposed_pad(fill), &gnd(), &defaults);
        assert!(plan.plane_source.starts_with("filled zones"), "{}", plan.plane_source);
        assert!(!plan.fanouts.is_empty());
        for f in &plan.fanouts {
            assert!(f.via_x - 0.3 >= 10.0 - 1e-6, "via circle must be wholly on the fill: {f:?}");
        }
        let unfilled = plan_fn(&exposed_pad(""));
        assert!(unfilled.plane_source.starts_with("zone outlines"));
    }

    fn plan_fn(tree: &SexpNode) -> FanoutPlan {
        plan(tree, &gnd(), &defaults)
    }

    #[test]
    fn per_net_options_set_the_sizes() {
        let sizes = |_: &str| FanoutOptions { via_size_mm: 0.7, via_drill_mm: 0.35, track_width_mm: 0.4, ..FanoutOptions::default() };
        let plan = plan(&board(""), &gnd(), &sizes);
        let f = &plan.fanouts[0];
        assert_eq!((f.via_size_mm, f.via_drill_mm, f.track_width_mm), (0.7, 0.35, 0.4));
    }

    #[test]
    fn arcs_sample_through_their_midpoint() {
        let points = arc_points((0.0, 1.0), (1.0, 0.0), (0.0, -1.0), 8);
        assert!(points.iter().all(|&(x, y)| ((x * x + y * y).sqrt() - 1.0).abs() < 1e-9));
        assert!(points.iter().any(|&(x, _)| x > 0.99), "passes through the mid side");
    }
}

#[cfg(test)]
mod regress2_tests {
    //! The second regression board: see tests/fixtures/fanout_regress2.README.md.
    use super::*;

    const BOARD: &str = include_str!("../../tests/fixtures/fanout_regress2.kicad_pcb");

    fn planned() -> (SexpNode, FanoutPlan) {
        let tree = konnect_sexp::parse_sexp(BOARD).unwrap();
        let nets = plane_nets(&tree);
        assert_eq!(nets, vec!["+3V3".to_string(), "GND".to_string()]);
        let plan = plan(&tree, &nets, &|_| FanoutOptions::default());
        (tree, plan)
    }

    #[test]
    fn every_smd_power_pad_is_planned() {
        let (_, plan) = planned();
        assert!(plan.skipped.is_empty(), "{:#?}", plan.skipped);
        // Vias inside U2's rotated exposed pad: 5 of the 3 × 3 grid. The
        // other 4 would touch C6.2, a GND pad of a back-side capacitor under
        // the QFN (before Pass 24 same-net pads of other parts were not
        // obstacles, and all 9 were placed).
        let ep = plan.fanouts.iter().filter(|f| f.reference == "U2" && f.pad == "33").collect::<Vec<_>>();
        assert_eq!(ep.len(), 5);
        assert!(ep.iter().all(|f| f.in_pad && f.via));
        let tree = konnect_sexp::parse_sexp(BOARD).unwrap();
        let c6 = collect_pads(&tree).into_iter().find(|p| p.reference == "C6" && p.number == "2").unwrap();
        for f in &ep {
            assert!(point_polygon_distance((f.via_x, f.via_y), &c6.corners()) >= 0.3 + 0.2 - 1e-6, "{f:?}");
        }
        // U2's ground pins between the +3V3 pins tie to the exposed pad.
        for pin in ["5", "13"] {
            let tie = plan.fanouts.iter().find(|f| f.reference == "U2" && f.pad == pin).unwrap();
            assert!(!tie.via && !tie.in_pad, "{tie:?}");
        }
        // Back-side capacitors fan out on B.Cu.
        for f in plan.fanouts.iter().filter(|f| f.reference == "C5" || f.reference == "C6") {
            assert_eq!(f.layer, "B.Cu", "{f:?}");
        }
    }

    #[test]
    fn every_via_is_on_board_on_its_plane_out_of_keepouts_and_clear_of_other_nets() {
        let (tree, plan) = planned();
        let geometry = BoardGeometry::read(&tree);
        let pads = collect_pads(&tree);
        let options = FanoutOptions::default();
        let r = options.via_size_mm / 2.0;
        let vias = plan.fanouts.iter().filter(|f| f.via).collect::<Vec<_>>();
        for f in &vias {
            let p = (f.via_x, f.via_y);
            assert!(geometry.inside_board(p, options.edge_clearance_mm + r - 1e-6), "off board: {f:?}");
            // Not in the L's missing corner (board x 135..160, y 100..120).
            assert!(!(p.0 > 135.0 && p.1 < 120.0), "{f:?}");
            assert!(geometry.reaches_plane(&f.net, p), "off its plane: {f:?}");
            assert!(geometry.via_allowed(p, r), "in a keep-out: {f:?}");
            for pad in pads.iter().filter(|pad| pad.net.as_deref() != Some(f.net.as_str())) {
                let d = point_polygon_distance(p, &pad.corners());
                assert!(d >= r + options.clearance_mm - 1e-6, "{f:?} is {d:.3} mm from {} {}", pad.reference, pad.number);
            }
            for g in vias.iter().filter(|g| g.net != f.net) {
                let d = (g.via_x - f.via_x).hypot(g.via_y - f.via_y);
                assert!(d >= 2.0 * r + options.clearance_mm - 1e-6, "{f:?} vs {g:?}");
            }
        }
    }
}

// ─── Shared with design-intent checks ───────────────────────────────────────

/// A copper pad in board coordinates, as its true (rotated) rectangle.
#[derive(Debug, Clone)]
pub(crate) struct CopperPad {
    pub reference: String,
    pub number: String,
    pub net: Option<String>,
    pub corners: [(f64, f64); 4],
    pub layer: &'static str,
    /// Surface-mount (one layer) rather than plated through (every layer).
    pub smd: bool,
    /// The true pad outline (circle, oval, roundrect sampled), convex.
    pub outline: Vec<(f64, f64)>,
    /// Centre in board coordinates, size in the pad's own frame, and the
    /// pad's board-space angle (degrees, as KiCad writes it).
    pub center: (f64, f64),
    pub size: (f64, f64),
    pub angle: f64,
    /// The owning footprint's position and rotation (degrees).
    pub footprint_center: (f64, f64),
    pub footprint_rotation: f64,
    pub uuid: Option<String>,
}

pub(crate) fn copper_pads(tree: &SexpNode) -> Vec<CopperPad> {
    collect_pads(tree)
        .into_iter()
        .map(|pad| CopperPad {
            corners: pad.corners(),
            outline: pad.outline(),
            reference: pad.reference,
            number: pad.number,
            net: pad.net,
            layer: pad.layer,
            smd: pad.smd,
            center: pad.center,
            size: pad.size,
            angle: pad.angle,
            footprint_center: pad.footprint_center,
            footprint_rotation: pad.footprint_rotation,
            uuid: pad.uuid,
        })
        .collect()
}

/// Edge-to-edge distance between two convex polygons (0 when they overlap).
pub(crate) fn polygon_distance(a: &[(f64, f64)], b: &[(f64, f64)]) -> f64 {
    if a.iter().any(|p| point_in_polygon(*p, b)) || b.iter().any(|p| point_in_polygon(*p, a)) {
        return 0.0;
    }
    let mut best = f64::INFINITY;
    for (p, q) in polygon_edges(a) {
        for (r, s) in polygon_edges(b) {
            best = best.min(segment_segment_distance(p, q, r, s));
        }
    }
    best
}

/// Sample an arc through start, mid and end (for lengths and drawing).
pub(crate) fn sample_arc(start: (f64, f64), mid: (f64, f64), end: (f64, f64), steps: usize) -> Vec<(f64, f64)> {
    arc_points(start, mid, end, steps)
}

/// The board outline (real Edge.Cuts geometry) for containment checks.
pub(crate) struct BoardOutline(BoardGeometry);

impl BoardOutline {
    pub(crate) fn read(tree: &SexpNode) -> Self {
        Self(BoardGeometry::read(tree))
    }
    /// `p` is inside the outline and at least `margin` from every edge.
    pub(crate) fn contains(&self, p: (f64, f64), margin: f64) -> bool {
        self.0.inside_board(p, margin)
    }

    /// The outline has readable Edge.Cuts geometry.
    pub(crate) fn has_edges(&self) -> bool {
        !self.0.edges.is_empty()
    }

    /// An axis-aligned rectangle lies wholly inside the outline: its corners
    /// are inside and no outline edge enters it (a notch or concave corner
    /// can cut into a rectangle whose four corners are all on the board).
    pub(crate) fn contains_rect(&self, (x0, y0, x1, y1): Bbox) -> bool {
        let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
        if !corners.iter().all(|c| self.0.inside_board(*c, 0.0)) {
            return false;
        }
        let inside = |p: Point| p.0 > x0 && p.0 < x1 && p.1 > y0 && p.1 < y1;
        !self.0.edges.iter().any(|&(a, b)| {
            inside(a)
                || inside(b)
                || (0..4).any(|i| segments_intersect(a, b, corners[i], corners[(i + 1) % 4]))
        })
    }

    /// The outline's edges as straight segments (arcs and circles sampled).
    pub(crate) fn edges(&self) -> &[(Point, Point)] {
        &self.0.edges
    }

    /// Track keep-outs: polygons with the copper layers they apply to
    /// (`*.Cu` and `F&B.Cu` spelled as KiCad writes them).
    pub(crate) fn track_keepouts(&self) -> Vec<(&[Point], &[String])> {
        self.0.keepouts.iter().filter(|k| k.no_tracks).map(|k| (k.polygon.as_slice(), k.layers.as_slice())).collect()
    }

    /// Distance from `p` to the nearest outline edge.
    pub(crate) fn edge_distance(&self, p: Point) -> f64 {
        self.0.edges.iter().map(|&(a, b)| point_segment_distance(p, a, b)).fold(f64::INFINITY, f64::min)
    }
}

#[cfg(test)]
mod low_inductance_tests {
    //! Pass 24: one rule per test, judged by `plane_connection` where it applies.
    use super::*;
    use crate::tools::design_intent::{plane_verdicts, DesignIntent, PlaneException, PlaneVerdict, Stackup};

    const WIDE: Stackup = Stackup { outer_copper_oz: 1.0, inner_copper_oz: 0.5, outer_dielectric_mm: 0.2104, er: 4.4 };
    const CLOSE: Stackup = Stackup { outer_copper_oz: 1.0, inner_copper_oz: 0.5, outer_dielectric_mm: 0.55, er: 4.4 };

    /// 30 × 30 mm, four layers, GND filled on In1 and +3V3 on In2.
    fn board(extra: &str) -> String {
        let fill = |net: &str, layer: &str| {
            format!(
                "(zone (net \"{net}\") (layers \"{layer}\") (polygon (pts (xy 0 0) (xy 30 0) (xy 30 30) (xy 0 30)))
                   (filled_polygon (layer \"{layer}\") (pts (xy 0 0) (xy 30 0) (xy 30 30) (xy 0 30))))"
            )
        };
        format!(
            "(kicad_pcb (version 20260206) (general (thickness 1.6))
              (layers (0 \"F.Cu\" signal) (4 \"In1.Cu\" signal) (6 \"In2.Cu\" signal) (2 \"B.Cu\" signal))
              (gr_rect (start 0 0) (end 30 30) (layer \"Edge.Cuts\"))
              {} {}
              {extra})",
            fill("GND", "In1.Cu"),
            fill("+3V3", "In2.Cu")
        )
    }

    /// An 0402 capacitor (KiCad C_0402_1005Metric pads) along x.
    fn cap(reference: &str, x: f64, y: f64, n1: &str, n2: &str) -> String {
        format!(
            "(footprint \"C_0402\" (layer \"F.Cu\") (at {x} {y}) (property \"Reference\" \"{reference}\")
               (pad \"1\" smd roundrect (at -0.48 0) (size 0.56 0.62) (layers \"F.Cu\") (roundrect_rratio 0.25) (net \"{n1}\"))
               (pad \"2\" smd roundrect (at 0.48 0) (size 0.56 0.62) (layers \"F.Cu\") (roundrect_rratio 0.25) (net \"{n2}\")))"
        )
    }

    /// A device with square 0.6 mm pads at the given offsets.
    fn device(reference: &str, layer: &str, x: f64, y: f64, pads: &[(&str, f64, f64, &str)]) -> String {
        let pads: String = pads
            .iter()
            .map(|(n, dx, dy, net)| format!("(pad \"{n}\" smd rect (at {dx} {dy}) (size 0.6 0.6) (layers \"{layer}\") (net \"{net}\"))"))
            .collect();
        format!("(footprint \"IC\" (layer \"{layer}\") (at {x} {y}) (property \"Reference\" \"{reference}\") {pads})")
    }

    fn gnd_via(x: f64, y: f64) -> String {
        format!("(via (at {x} {y}) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (net \"GND\"))")
    }

    fn seg(net: &str, layer: &str, a: Point, b: Point) -> String {
        format!("(segment (start {} {}) (end {} {}) (width 0.3) (layer \"{layer}\") (net \"{net}\"))", a.0, a.1, b.0, b.1)
    }

    fn intent(stackup: Option<Stackup>) -> DesignIntent {
        DesignIntent { schema: 1, stackup, ..Default::default() }
    }

    fn nets() -> Vec<String> {
        vec!["+3V3".to_string(), "GND".to_string()]
    }

    struct Run {
        plan: FanoutPlan,
        after: Vec<PlaneVerdict>,
    }

    impl Run {
        fn decision(&self, pad: &str) -> &PadDecision {
            self.plan.pads.iter().find(|d| d.pad == pad).unwrap_or_else(|| panic!("no decision for {pad}: {:#?}", self.plan.pads))
        }
        fn verdict(&self, pad: &str) -> &PlaneVerdict {
            self.after.iter().find(|v| v.pad == pad).unwrap_or_else(|| panic!("no verdict for {pad}"))
        }
        fn fanout(&self, pad: &str) -> &PlannedFanout {
            self.plan.fanouts.iter().find(|f| format!("{}.{}", f.reference, f.pad) == pad).unwrap()
        }
    }

    fn run_with(text: &str, intent: &DesignIntent, li: &LowInductance, options_for: &dyn Fn(&str) -> FanoutOptions) -> Run {
        let tree = konnect_sexp::parse_sexp(text).unwrap();
        let (plan, _) = plan_with_judge(&tree, &nets(), options_for, false, li, intent);
        let after_tree = konnect_sexp::parse_sexp(&board_with_plan(text, &plan)).unwrap();
        Run { after: plane_verdicts(&after_tree, intent), plan }
    }

    fn run(text: &str, intent: &DesignIntent) -> Run {
        run_with(text, intent, &LowInductance::default(), &|_| FanoutOptions::default())
    }

    fn pad_of(text: &str, pad: &str) -> PadInfo {
        let tree = konnect_sexp::parse_sexp(text).unwrap();
        collect_pads(&tree).into_iter().find(|p| format!("{}.{}", p.reference, p.number) == pad).unwrap()
    }

    #[test]
    fn every_plane_pad_gets_its_own_via_beside_the_pads_on_one_side() {
        let text = board(&cap("C1", 15.0, 15.0, "+3V3", "GND"));
        let r = run(&text, &intent(Some(WIDE)));
        let (a, b) = (r.decision("C1.1"), r.decision("C1.2"));
        assert_eq!((a.action, b.action), ("via", "via"), "{:#?}", r.plan.pads);
        assert_eq!((a.placement, b.placement), (Some("side"), Some("side")), "{a:?} {b:?}");
        let (ya, yb) = (a.via.unwrap()[1] - 15.0, b.via.unwrap()[1] - 15.0);
        assert!(ya * yb > 0.0, "both vias on the same side of the part: {a:?} {b:?}");
        for d in [a, b] {
            // Against the pad, not in it: drill edge ≥ 0.25 mm from the pad edge.
            assert!(d.hole_gap_mm.unwrap() >= 0.25 - 1e-6, "{d:?}");
            assert!(d.via_gap_mm.unwrap() < 0.2, "butted against the pad: {d:?}");
            assert!(d.stub_mm.unwrap() < 0.3, "{d:?}");
            assert!(d.reason.contains("eurocircuits-tombstoning") && d.reason.contains("UG483"), "{}", d.reason);
            let v = r.verdict(&d.pad);
            assert_eq!((v.status.as_str(), v.measured["connection"].as_str()), ("pass", Some("via")), "{v:?}");
        }
        let pad = pad_of(&text, "C1.2");
        let f = r.fanout("C1.2");
        assert!(point_polygon_distance((f.via_x, f.via_y), &pad.corners()) >= 0.3 + 0.25 - 0.15 - 1e-6);
    }

    #[test]
    fn the_vias_go_on_the_side_facing_the_ic_the_part_serves() {
        for (ic_y, above) in [(18.0, true), (12.0, false)] {
            let parts = format!("{} {}", cap("C1", 15.0, 15.0, "+3V3", "GND"), device("U1", "F.Cu", 15.0, ic_y, &[("1", 0.0, 0.0, "+3V3"), ("2", 1.0, 0.0, "SIG")]));
            let r = run(&board(&parts), &intent(Some(WIDE)));
            for pad in ["C1.1", "C1.2"] {
                let d = r.decision(pad);
                assert_eq!(d.action, "via", "{d:?}");
                assert_eq!(d.via.unwrap()[1] > 15.0, above, "IC at y {ic_y}: {d:?}");
            }
        }
    }

    #[test]
    fn both_pads_of_a_two_pad_part_get_the_same_treatment() {
        // +3V3 has a wider class width: both stubs take the narrower one.
        let widths = |net: &str| FanoutOptions { track_width_mm: if net == "+3V3" { 0.4 } else { 0.3 }, ..FanoutOptions::default() };
        let text = board(&cap("C1", 15.0, 15.0, "+3V3", "GND"));
        let r = run_with(&text, &intent(Some(WIDE)), &LowInductance::default(), &widths);
        assert_eq!(r.fanout("C1.1").track_width_mm, r.fanout("C1.2").track_width_mm);
        assert_eq!(r.fanout("C1.1").track_width_mm, 0.3);
        assert!(r.decision("C1.1").symmetry.as_deref().unwrap().starts_with("symmetric"), "{:?}", r.decision("C1.1"));
        // Declared filled and capped: both pads take the via in the pad.
        let li = LowInductance { via_in_pad: true, ..LowInductance::default() };
        let r = run_with(&text, &intent(Some(WIDE)), &li, &|_| FanoutOptions::default());
        for pad in ["C1.1", "C1.2"] {
            let (d, f) = (r.decision(pad), r.fanout(pad));
            assert_eq!(d.action, "via_in_pad", "{d:?}");
            assert!(f.in_pad && f.stub_mm == 0.0, "{f:?}");
            assert_eq!(r.verdict(pad).status, "pass");
        }
        // Not declared: never in the pad.
        let r = run(&text, &intent(Some(WIDE)));
        assert!(r.plan.fanouts.iter().all(|f| !f.in_pad));
    }

    /// C6.2 (GND) has other-net copper on three sides; the open side is
    /// C5.2, a GND pad of another part (the Pass 18 module/demo finding:
    /// the via went into the neighbour's same-net pad, a daisy chain).
    fn boxed_in() -> String {
        let walls = [
            seg("SIG", "F.Cu", (13.9, 14.15), (13.9, 15.85)),
            seg("SIG", "F.Cu", (13.9, 14.15), (16.2, 14.15)),
            seg("SIG", "F.Cu", (13.9, 15.85), (16.2, 15.85)),
            seg("SIG", "B.Cu", (13.9, 14.15), (13.9, 15.85)),
            seg("SIG", "B.Cu", (13.9, 14.15), (16.2, 14.15)),
            seg("SIG", "B.Cu", (13.9, 15.85), (16.2, 15.85)),
        ]
        .join(" ");
        format!("{} {} {walls}", cap("C6", 15.0, 15.0, "SIG2", "GND"), cap("C5", 16.75, 15.0, "GND", "SIG3"))
    }

    #[test]
    fn a_via_never_touches_another_parts_pad_even_on_the_same_net() {
        let text = board(&boxed_in());
        let c5 = pad_of(&text, "C5.1");
        let tree = konnect_sexp::parse_sexp(&text).unwrap();
        // Both planners (the fix is in the shared clearance test).
        let legacy = plan(&tree, &nets(), &|_| FanoutOptions::default());
        let r = run(&text, &intent(Some(WIDE)));
        for f in legacy.fanouts.iter().chain(r.plan.fanouts.iter()).filter(|f| f.via) {
            let d = point_polygon_distance((f.via_x, f.via_y), &c5.corners());
            if f.reference != "C5" {
                assert!(d >= 0.3 + 0.2 - 1e-6, "{f:?} is {d:.3} mm from C5.1");
            }
        }
        for v in &r.after {
            assert_ne!(v.measured["connection"], "daisy_chain", "{v:?}");
        }
        // C6.2's open side is C5.1: no legal site, so it is listed with what
        // blocked it; every other plane pad gets its via.
        let d = r.decision("C6.2");
        assert_eq!(d.action, "no_site", "{d:?}");
        assert!(d.reason.contains("C5's same-net pad"), "{}", d.reason);
        assert_eq!(r.decision("C5.1").action, "via", "{:?}", r.decision("C5.1"));
        assert!(!legacy.fanouts.iter().any(|f| f.reference == "C6" && f.via), "legacy must not reach into C5.1 either: {legacy:?}");
    }

    /// U1.5 (GND) owns the via at (15, 10.9); C5.1 (GND) is butted against
    /// it (copper 0.10 mm, drill edge 0.25 mm from the pad).
    fn beside_ic(cap_layer_bottom: bool) -> String {
        beside_ic_at(cap_layer_bottom, 16.16)
    }

    fn beside_ic_at(cap_layer_bottom: bool, cap_x: f64) -> String {
        let u1 = if cap_layer_bottom {
            device("U1", "B.Cu", 15.0, 10.0, &[("5", 0.0, 0.0, "GND"), ("6", 0.0, -3.0, "SIG")])
        } else {
            device("U1", "F.Cu", 15.0, 10.0, &[("5", 0.0, 0.0, "GND"), ("6", 0.0, -3.0, "SIG")])
        };
        let layer = if cap_layer_bottom { "B.Cu" } else { "F.Cu" };
        format!("{u1} {} {} {}", seg("GND", layer, (15.0, 10.0), (15.0, 10.9)), gnd_via(15.0, 10.9), cap("C5", cap_x, 10.9, "GND", "+3V3"))
    }

    #[test]
    fn a_capacitor_shares_an_adjacent_ic_via_only_with_widely_spaced_planes_on_the_same_side() {
        let text = board(&beside_ic(false));
        let r = run(&text, &intent(Some(WIDE)));
        let d = r.decision("C5.1");
        assert_eq!((d.action, d.shares_via_of.as_deref()), ("share", Some("U1.5")), "{:#?}", r.plan.pads);
        assert!(d.reason.contains("learnemc-decoupling-widely-spaced-planes"), "{}", d.reason);
        assert!(!r.fanout("C5.1").via, "a shared via places no via of its own");
        let v = r.verdict("C5.1");
        assert_eq!((v.status.as_str(), v.measured["connection"].as_str()), ("pass", Some("shared_via_ok")), "{v:?}");
        // The other pad still gets its own via.
        assert_eq!(r.decision("C5.2").action, "via");
        // 0.32 mm away there would be a real trace: not shared by default
        // (LearnEMC: no trace between the pads and the via), shared only
        // with the explicit share_max_gap opt-in.
        let apart = board(&beside_ic_at(false, 16.38));
        let r = run(&apart, &intent(Some(WIDE)));
        assert_eq!(r.decision("C5.1").action, "via", "{:?}", r.decision("C5.1"));
        let opt_in = LowInductance { share_max_gap_mm: Some(0.5), ..LowInductance::default() };
        let r = run_with(&apart, &intent(Some(WIDE)), &opt_in, &|_| FanoutOptions::default());
        assert_eq!(r.decision("C5.1").action, "share", "{:?}", r.decision("C5.1"));
        // share_with_ic never: own via.
        let never = LowInductance { share_with_ic: false, ..LowInductance::default() };
        let r = run_with(&text, &intent(Some(WIDE)), &never, &|_| FanoutOptions::default());
        assert_eq!(r.decision("C5.1").action, "via", "{:?}", r.decision("C5.1"));
    }

    #[test]
    fn sharing_is_refused_with_close_planes_unknown_spacing_or_across_sides() {
        let text = board(&beside_ic(false));
        let close = run(&text, &intent(Some(CLOSE)));
        let d = close.decision("C5.1");
        assert_eq!(d.action, "via", "{d:?}");
        assert!(d.reason.contains("learnemc-decoupling-closely-spaced-planes"), "{}", d.reason);
        assert_eq!(close.verdict("C5.1").status, "pass");
        let unknown = run(&text, &intent(None));
        let d = unknown.decision("C5.1");
        assert_eq!(d.action, "via", "{d:?}");
        assert!(d.reason.contains("unknown"), "{}", d.reason);
        let bottom = run(&board(&beside_ic(true)), &intent(Some(WIDE)));
        let d = bottom.decision("C5.1");
        assert_eq!(d.action, "via", "{d:?}");
        assert!(d.reason.contains("other side"), "{}", d.reason);
    }

    #[test]
    fn a_capacitor_never_shares_another_capacitors_via() {
        // C2.2 owns the via at (15, 10.9); C5.1 is butted against it, where
        // it shared U1's via above.
        let parts = format!(
            "{} {} {} {}",
            cap("C2", 14.52, 10.0, "+3V3", "GND"),
            seg("GND", "F.Cu", (15.0, 10.0), (15.0, 10.9)),
            gnd_via(15.0, 10.9),
            cap("C5", 16.16, 10.9, "GND", "+3V3")
        );
        let r = run(&board(&parts), &intent(Some(WIDE)));
        let d = r.decision("C5.1");
        assert_eq!(d.action, "via", "{d:?}");
        assert!(d.shares_via_of.is_none());
        let v = r.verdict("C5.1");
        assert_eq!((v.status.as_str(), v.measured["connection"].as_str()), ("pass", Some("via")), "{v:?}");
        assert_eq!(r.decision("C2.2").action, "kept");
    }

    #[test]
    fn declared_exceptions_are_reported_with_their_source_and_get_no_via() {
        let mut i = intent(Some(WIDE));
        i.plane_exceptions.push(PlaneException::Datasheet { pad: Some("C1.2".into()), net: None, rule: "ground via VSS_PA".into(), source: "nordic-nrf52840-ops-v0-5".into() });
        let r = run(&board(&cap("C1", 15.0, 15.0, "+3V3", "GND")), &i);
        let d = r.decision("C1.2");
        assert_eq!(d.action, "exception", "{d:?}");
        assert_eq!(d.source.as_deref(), Some("nordic-nrf52840-ops-v0-5"));
        assert!(r.plan.fanouts.iter().all(|f| !(f.reference == "C1" && f.pad == "2")));
        assert_eq!(r.decision("C1.1").action, "via");
    }

    #[test]
    fn keepouts_through_hole_pads_and_other_side_pads_are_never_hit() {
        // Above the part: a via keep-out; below: a THT pad and a B.Cu pad of
        // other parts. The via must clear all of them.
        let ko = "(zone (layers \"F.Cu\" \"In1.Cu\" \"In2.Cu\" \"B.Cu\") (keepout (tracks allowed) (vias not_allowed)) (polygon (pts (xy 10 15.35) (xy 20 15.35) (xy 20 20) (xy 10 20))))";
        let tht = "(footprint \"J\" (layer \"F.Cu\") (at 14.5 15.9) (property \"Reference\" \"J1\") (pad \"1\" thru_hole circle (at 0 0) (size 0.9 0.9) (drill 0.5) (layers \"*.Cu\") (net \"SIG\")))";
        let bottom = device("U9", "B.Cu", 15.6, 14.0, &[("1", 0.0, 0.0, "SIG")]);
        let text = board(&format!("{} {ko} {tht} {bottom}", cap("C1", 15.0, 15.0, "+3V3", "GND")));
        let r = run(&text, &intent(Some(WIDE)));
        let tree = konnect_sexp::parse_sexp(&text).unwrap();
        let geometry = BoardGeometry::read(&tree);
        let others: Vec<PadInfo> = collect_pads(&tree).into_iter().filter(|p| p.reference != "C1").collect();
        for f in r.plan.fanouts.iter().filter(|f| f.via) {
            assert!(geometry.via_allowed((f.via_x, f.via_y), 0.3), "in the keep-out: {f:?}");
            for p in &others {
                let d = point_polygon_distance((f.via_x, f.via_y), &p.corners());
                assert!(d >= 0.5 - 1e-6, "{f:?} is {d:.3} mm from {}.{}", p.reference, p.number);
            }
        }
        // Both pads still have a legal site (to the left, x < 14.0): planned.
        for pad in ["C1.1", "C1.2"] {
            let d = r.decision(pad);
            assert_eq!(d.action, "via", "{d:?}");
            assert_eq!(r.verdict(pad).status, "pass", "{:?}", r.verdict(pad));
        }
    }

    #[test]
    fn a_via_stays_out_from_under_another_parts_body_on_the_same_side() {
        // R1's courtyard covers everything above y = 15.32 near C1; its body
        // (courtyard less the 0.25 mm margin) everything above 15.57.
        let yard = "(footprint \"R\" (layer \"F.Cu\") (at 15 17) (property \"Reference\" \"R1\")
            (fp_rect (start -3 -1.68) (end 3 1.5) (layer \"F.CrtYd\"))
            (pad \"1\" smd rect (at 0 1) (size 0.5 0.5) (layers \"F.Cu\") (net \"SIG\")))";
        let ic_above = device("U1", "F.Cu", 15.0, 22.0, &[("1", 0.0, 0.0, "+3V3")]);
        let text = board(&format!("{} {yard} {ic_above}", cap("C1", 15.0, 15.0, "+3V3", "GND")));
        let r = run(&text, &intent(Some(WIDE)));
        for pad in ["C1.1", "C1.2"] {
            let d = r.decision(pad);
            assert_eq!(d.action, "via", "{d:?}");
            assert!(d.via.unwrap()[1] + 0.3 <= 15.57 + 1e-6, "kept out from under R1's body: {d:?}");
        }
        let free = LowInductance { avoid_courtyards: false, ..LowInductance::default() };
        let r = run_with(&text, &intent(Some(WIDE)), &free, &|_| FanoutOptions::default());
        assert!(r.decision("C1.1").via.unwrap()[1] > 15.0, "towards the IC when courtyards are ignored: {:?}", r.decision("C1.1"));
        // A courtyard on the other side does not block.
        let back = yard.replace("(layer \"F.Cu\") (at 15 17)", "(layer \"B.Cu\") (at 15 17)").replace("F.CrtYd", "B.CrtYd").replace("(layers \"F.Cu\") (net \"SIG\")", "(layers \"B.Cu\") (net \"SIG\")");
        let r = run(&board(&format!("{} {back} {ic_above}", cap("C1", 15.0, 15.0, "+3V3", "GND"))), &intent(Some(WIDE)));
        assert!(r.decision("C1.1").via.unwrap()[1] > 15.0, "{:?}", r.decision("C1.1"));
    }

    #[test]
    fn every_pad_on_a_plane_net_gets_a_decision() {
        let parts = format!(
            "{} {} {} {}",
            cap("C1", 15.0, 15.0, "+3V3", "GND"),
            cap("C2", 20.0, 20.0, "SIG", "GND"),
            seg("GND", "F.Cu", (20.48, 20.0), (20.48, 21.0)),
            gnd_via(20.48, 21.0)
        );
        let tht = "(footprint \"J\" (layer \"F.Cu\") (at 5 5) (property \"Reference\" \"J1\") (pad \"1\" thru_hole circle (at 0 0) (size 1.7 1.7) (drill 1) (layers \"*.Cu\") (net \"GND\")))";
        let r = run(&board(&format!("{parts} {tht}")), &intent(Some(WIDE)));
        let tree = konnect_sexp::parse_sexp(&board(&format!("{parts} {tht}"))).unwrap();
        let on_nets = collect_pads(&tree).iter().filter(|p| p.net.as_deref().is_some_and(|n| nets().iter().any(|m| m == n))).count();
        assert_eq!(r.plan.pads.len(), on_nets, "{:#?}", r.plan.pads);
        assert_eq!(r.decision("J1.1").action, "through_hole");
        assert_eq!(r.decision("C2.2").action, "kept");
        assert!(r.decision("C2.2").reason.contains("plane_connection"));
        assert!(r.plan.pads.iter().all(|d| !d.reason.is_empty()), "{:#?}", r.plan.pads);
        // C2.1 is on a signal net: not a plane pad, no decision.
        assert!(r.plan.pads.iter().all(|d| d.pad != "C2.1"));
    }

    #[test]
    fn a_tie_or_shared_stub_writes_no_via_into_the_self_check_board() {
        let text = board(&beside_ic(false));
        let r = run(&text, &intent(Some(WIDE)));
        let written = board_with_plan(&text, &r.plan);
        let vias_before = text.matches("(via ").count();
        let own = r.plan.fanouts.iter().filter(|f| f.via).count();
        assert_eq!(written.matches("(via ").count(), vias_before + own);
        assert!(r.plan.fanouts.iter().any(|f| !f.via));
    }

    /// A SOT-223-like regulator: pin 2 and the tab are both numbered 2 on
    /// GND. The tab has its vias (passes); pin 2 runs 5.7 mm to a via (fails).
    fn sot223(tab_first: bool) -> String {
        let tab = "(pad \"2\" smd rect (at 0 -3.15) (size 3.5 2.0) (layers \"F.Cu\") (net \"GND\"))";
        let pins = "(pad \"1\" smd rect (at -2.3 0) (size 0.95 2.0) (layers \"F.Cu\") (net \"VIN\"))
            (pad \"2\" smd rect (at 0 0) (size 0.95 2.0) (layers \"F.Cu\") (net \"GND\"))
            (pad \"3\" smd rect (at 2.3 0) (size 0.95 2.0) (layers \"F.Cu\") (net \"VOUT\"))";
        let pads = if tab_first { format!("{tab} {pins}") } else { format!("{pins} {tab}") };
        format!(
            "(footprint \"SOT-223\" (layer \"F.Cu\") (at 15 15) (property \"Reference\" \"U9\") {pads}) {} {} {} {}",
            gnd_via(14.0, 11.85),
            gnd_via(16.0, 11.85),
            seg("GND", "F.Cu", (15.0, 15.0), (15.0, 20.7)),
            gnd_via(15.0, 20.7)
        )
    }

    #[test]
    fn same_numbered_pads_get_their_own_verdicts_and_decisions() {
        for tab_first in [true, false] {
            let r = run(&board(&sot223(tab_first)), &intent(Some(WIDE)));
            let by_at = |y: f64| r.plan.pads.iter().find(|d| d.pad == "U9.2" && (d.at[1] - y).abs() < 1e-6).unwrap_or_else(|| panic!("{:#?}", r.plan.pads));
            let (tab, pin) = (by_at(11.85), by_at(15.0));
            assert_eq!(tab.action, "kept", "tab_first {tab_first}: the tab passes and gets no grid: {tab:?}");
            assert!(matches!(pin.action, "via" | "tie"), "tab_first {tab_first}: pin 2 fails (5.7 mm) and gets a via: {pin:?}");
            assert!(!r.plan.fanouts.iter().any(|f| f.in_pad), "no via grid in the passing tab");
            // The self-check tells the two apart too, and both pass.
            let rows: Vec<&PlaneVerdict> = r.after.iter().filter(|v| v.pad == "U9.2").collect();
            assert_eq!(rows.len(), 2);
            assert!(rows.iter().all(|v| v.status == "pass"), "{rows:#?}");
            assert_ne!(rows[0].measured["at"], rows[1].measured["at"]);
        }
    }

    #[test]
    fn a_via_never_lands_in_a_neighbouring_pin_of_the_same_part() {
        // Two GND pins of one IC at 0.65 mm pitch: pin 1's via must not sit
        // in (or touch) pin 2, which the judge would then call pin 2's
        // via_in_pad.
        let ic = "(footprint \"IC\" (layer \"F.Cu\") (at 15 15) (property \"Reference\" \"U1\")
               (pad \"1\" smd rect (at 0 0) (size 0.4 1.5) (layers \"F.Cu\") (net \"GND\"))
               (pad \"2\" smd rect (at 0.65 0) (size 0.4 1.5) (layers \"F.Cu\") (net \"GND\")))";
        let text = board(ic);
        let r = run(&text, &intent(Some(WIDE)));
        let p1 = pad_of(&text, "U1.1");
        let p2 = pad_of(&text, "U1.2");
        for f in r.plan.fanouts.iter().filter(|f| f.via) {
            let other = if f.pad == "1" { &p2 } else { &p1 };
            let d = point_polygon_distance((f.via_x, f.via_y), &other.corners());
            assert!(d >= 0.3 + 0.2 - 1e-6, "{f:?} is {d:.3} mm from the neighbouring pin");
        }
        for v in &r.after {
            assert_ne!(v.measured["connection"], "via_in_pad", "{v:?}");
        }
        assert_eq!(r.plan.fanouts.iter().filter(|f| f.via).count(), 2, "{:#?}", r.plan.pads);
    }

    #[test]
    fn small_chips_have_a_smaller_courtyard_margin() {
        // IPC-7351B nominal: 0.15 mm below 0603, 0.25 mm otherwise; 0.5 mm
        // for connectors (KiCad library), 1 mm for BGAs.
        let tree = konnect_sexp::parse_sexp(&board(
            "(footprint \"C_0402\" (layer \"F.Cu\") (at 5 5) (property \"Reference\" \"C9\")
               (fp_rect (start -0.91 -0.46) (end 0.91 0.46) (layer \"F.CrtYd\"))
               (pad \"1\" smd rect (at -0.48 0) (size 0.56 0.62) (layers \"F.Cu\") (net \"A\"))
               (pad \"2\" smd rect (at 0.48 0) (size 0.56 0.62) (layers \"F.Cu\") (net \"B\")))
             (footprint \"C_0805\" (layer \"F.Cu\") (at 10 5) (property \"Reference\" \"C8\")
               (fp_rect (start -1.7 -0.98) (end 1.7 0.98) (layer \"F.CrtYd\"))
               (pad \"1\" smd rect (at -0.95 0) (size 1 1.45) (layers \"F.Cu\") (net \"A\"))
               (pad \"2\" smd rect (at 0.95 0) (size 1 1.45) (layers \"F.Cu\") (net \"B\")))
             (footprint \"USB\" (layer \"F.Cu\") (at 20 5) (property \"Reference\" \"J1\")
               (fp_rect (start -4 -3) (end 4 3) (layer \"F.CrtYd\")))
             (footprint \"BGA-64\" (layer \"B.Cu\") (at 20 20) (property \"Reference\" \"U5\")
               (fp_rect (start -4 -4) (end 4 4) (layer \"B.CrtYd\")) (fp_rect (start -9 -9) (end 9 9) (layer \"F.CrtYd\")))",
        ))
        .unwrap();
        let yards = courtyards(&tree);
        let margin = |r: &str| yards.iter().find(|y| y.reference == r).unwrap().margin;
        assert_eq!((margin("C9"), margin("C8"), margin("J1"), margin("U5")), (0.15, 0.25, 0.5, 1.0));
        // A back-side footprint uses its B.CrtYd only: the F.CrtYd ring
        // around it would make it look hollow to the even-odd test.
        let u5 = yards.iter().find(|y| y.reference == "U5").unwrap();
        assert!(u5.back && u5.inside((20.0, 20.0)));
        assert!((u5.bbox.2 - u5.bbox.0 - 8.0).abs() < 1e-9, "{:?}", u5.bbox);
    }

    #[test]
    fn legacy_mode_keeps_the_old_placement() {
        // The legacy planner's own result on a lone 0603-ish capacitor, as
        // before Pass 24: the GND via straight out along the part's axis
        // (+x), at the first clear 0.05 mm step. Low-inductance mode puts it
        // beside the pads instead.
        let text = board(&cap("C1", 15.0, 15.0, "SIG", "GND"));
        let tree = konnect_sexp::parse_sexp(&text).unwrap();
        let (legacy, _) = plan_with_judge(&tree, &nets(), &|_| FanoutOptions::default(), true, &LowInductance::default(), &intent(Some(WIDE)));
        assert_eq!(legacy.mode, "legacy");
        assert!(legacy.pads.is_empty());
        let f = &legacy.fanouts[0];
        assert!((f.via_y - 15.0).abs() < 1e-9 && f.via_x > 15.48, "{f:?}");
        assert!(f.placement.is_none() && f.via_gap_mm.is_none());
        // Own pad edge 15.76; legacy keeps the via copper 0.05 mm off it.
        assert!((f.via_x - (15.76 + 0.3 + 0.05)).abs() <= 0.05 + 1e-9, "{f:?}");
        let r = run(&text, &intent(Some(WIDE)));
        assert_eq!(r.decision("C1.2").placement, Some("side"));
    }

    #[test]
    fn a_via_beside_a_fracture_slit_of_the_fill_is_on_the_plane() {
        // A fill with one square hole (2..4, 2..4), joined to the outline by
        // a slit along y = 3 as KiCad writes it.
        let fill = [(0.0, 0.0), (10.0, 0.0), (10.0, 10.0), (0.0, 10.0), (0.0, 3.0), (2.0, 3.0), (2.0, 4.0), (4.0, 4.0), (4.0, 2.0), (2.0, 2.0), (2.0, 3.0), (0.0, 3.0)];
        let index = PolyIndex::new(&fill);
        // 0.1 mm off the slit, far from the hole: copper on both sides.
        assert!(index.contains((1.0, 3.1)) && index.clear_of_edges((1.0, 3.1), 0.3));
        // In the hole, and too close to the hole's edge.
        assert!(!index.contains((3.0, 3.0)));
        assert!(!index.clear_of_edges((4.1, 3.0), 0.3));
        // The outline's own edge still counts.
        assert!(!index.clear_of_edges((0.1, 6.0), 0.3));
    }

    #[test]
    fn a_courtyard_is_its_outline_not_its_bounding_box() {
        // An octagonal QFP-like courtyard with cut corners: a via in the
        // cut corner is outside it, one inside its body is not.
        let yard = "(footprint \"QFP\" (layer \"F.Cu\") (at 10 10) (property \"Reference\" \"U1\")
            (fp_poly (pts (xy -2 -4) (xy 2 -4) (xy 4 -2) (xy 4 2) (xy 2 4) (xy -2 4) (xy -4 2) (xy -4 -2)) (layer \"F.CrtYd\")))";
        let tree = konnect_sexp::parse_sexp(&board(yard)).unwrap();
        let yards = courtyards(&tree);
        assert_eq!(yards.len(), 1);
        let y = &yards[0];
        assert_eq!(y.margin, 0.25, "an IC: IPC-7351B nominal");
        assert!(!y.circle_reaches_body((13.6, 13.6), 0.3, y.margin), "cut corner");
        assert!(y.circle_reaches_body((10.0, 10.0), 0.3, y.margin), "body");
        // In the 0.25 mm margin, the via circle not reaching the body.
        assert!(!y.circle_reaches_body((10.0, 14.3), 0.3, y.margin));
        assert!(y.circle_reaches_body((10.0, 13.9), 0.3, y.margin));
    }

}

#[cfg(test)]
mod board_validation {
    //! Pass 24 validation on board copies, offline (kicad-cli only). Run:
    //! `PASS24_BOARD=/copy/of/board.kicad_pcb cargo test -p konnect-core --lib
    //! board_validation -- --ignored --nocapture`. It writes `<stem>_ripped`,
    //! `<stem>_fanout` and `<stem>_inplace` boards beside the copy; it never
    //! touches anything else. `PASS24_SHARE=never`, `PASS24_COURTYARDS=ignore`
    //! change the settings.
    use super::*;
    use crate::tools::design_intent::{intent_for_board, plane_verdicts, PlaneVerdict};
    use std::path::{Path, PathBuf};

    fn rip(text: &str, nets: &[String]) -> (String, usize) {
        let mut out = String::with_capacity(text.len());
        let mut block: Vec<&str> = Vec::new();
        let mut removed = 0;
        for line in text.split_inclusive('\n') {
            let t = line.trim_end_matches(['\n', '\r']);
            if block.is_empty() && matches!(t, "\t(via" | "\t(segment" | "\t(arc") {
                block.push(line);
                continue;
            }
            if !block.is_empty() {
                block.push(line);
                if t == "\t)" {
                    let joined: String = block.concat();
                    if nets.iter().any(|n| joined.contains(&format!("(net \"{n}\")"))) {
                        removed += 1;
                    } else {
                        out.push_str(&joined);
                    }
                    block.clear();
                }
                continue;
            }
            out.push_str(line);
        }
        (out, removed)
    }

    fn refill(path: &Path) -> String {
        let status = std::process::Command::new("kicad-cli")
            .args(["pcb", "drc", "--refill-zones", "--save-board", "--format", "json", "-o"])
            .arg(path.with_extension("refill.json"))
            .arg(path)
            .output()
            .expect("kicad-cli");
        assert!(status.status.success() || status.status.code() == Some(5), "{:?}", status);
        std::fs::read_to_string(path).unwrap()
    }

    fn summary(v: &[PlaneVerdict], nets: &[String]) -> String {
        let mut by: BTreeMap<&str, usize> = BTreeMap::new();
        let mut n = 0;
        for x in v.iter().filter(|x| nets.contains(&x.net)) {
            *by.entry(x.status.as_str()).or_default() += 1;
            n += 1;
        }
        format!("{} plane pads: {by:?}", n)
    }

    fn failing(v: &[PlaneVerdict]) -> Vec<String> {
        v.iter().filter(|x| x.status == "fail").map(|x| format!("{} [{}] {}", x.pad, x.measured["connection"].as_str().unwrap_or(""), x.detail.chars().take(160).collect::<String>())).collect()
    }

    fn routability(rt: &tokio::runtime::Runtime, board: &Path, text: &str) -> String {
        use crate::tools::routability::*;
        let tree = konnect_sexp::parse_sexp(text).unwrap();
        let project = std::fs::read_to_string(board.with_extension("kicad_pro")).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
        let rules = NetRules::from_project(project.as_ref());
        let model = BoardModel::read(&tree);
        let (report, _) = rt.block_on(crate::tools::pcb_routing::drc_with_project_rules("kicad-cli", board, text, true)).unwrap();
        let airwires = airwires_from_drc(report.unconnected_items.as_deref().unwrap_or(&[]), &model);
        let options = Options { signal_layers: None, cell_mm: 1.0, efficiency: DEFAULT_LAYOUT_EFFICIENCY, efficiency_given: false };
        let r = analyse(&model, &rules, &airwires, None, &options).unwrap();
        let reasons: Vec<String> = r["reasons"].as_array().into_iter().flatten().map(|x| format!("{}:{}", x["metric"].as_str().unwrap_or(""), x["level"].as_str().unwrap_or(""))).collect();
        format!(
            "verdict {} | demand airwires {} | reasons {:?} | crossings {} | RUDY overflow {:.4} max util {:.3} blocked cells w/ demand {} | density util {:.3}",
            r["verdict"],
            r["airwires"]["demand"],
            reasons,
            r["crossings"]["crossings"],
            r["congestion"]["overflow_fraction"].as_f64().unwrap_or(f64::NAN),
            r["congestion"]["max_utilization"].as_f64().unwrap_or(f64::NAN),
            r["congestion"]["blocked_cells_with_demand"],
            r["density"]["utilization"].as_f64().unwrap_or(f64::NAN)
        )
    }

    fn gate(rt: &tokio::runtime::Runtime, board: &Path, before: &str, after: &str) -> String {
        let (b, _) = rt.block_on(crate::tools::pcb_routing::drc_with_project_rules("kicad-cli", board, before, true)).unwrap();
        let (a, _) = rt.block_on(crate::tools::pcb_routing::drc_with_project_rules("kicad-cli", board, after, true)).unwrap();
        let added = crate::tools::pcb_routing::new_drc_errors(&b, &a);
        let errors = |r: &crate::tools::cli::DrcReport| r.violations.iter().filter(|v| v.severity == "error").count();
        let un = |r: &crate::tools::cli::DrcReport| r.unconnected_items.as_ref().map_or(0, Vec::len);
        let examples: Vec<String> = added.iter().take(8).map(|v| format!("{}: {}", v.rule, v.description)).collect();
        format!(
            "DRC errors {} -> {} (new by identity: {}), unconnected {} -> {}{}",
            errors(&b),
            errors(&a),
            added.len(),
            un(&b),
            un(&a),
            if examples.is_empty() { String::new() } else { format!("\n    new: {examples:#?}") }
        )
    }

    fn report_plan(plan: &FanoutPlan) {
        let mut by: BTreeMap<&str, usize> = BTreeMap::new();
        let mut placements: BTreeMap<&str, usize> = BTreeMap::new();
        for d in &plan.pads {
            *by.entry(d.action).or_default() += 1;
            if let Some(p) = d.placement {
                *placements.entry(p).or_default() += 1;
            }
        }
        let vias = plan.fanouts.iter().filter(|f| f.via).count();
        let shared = plan.fanouts.iter().filter(|f| f.shares_via_of.is_some()).count();
        println!("  decisions {by:?}\n  placements {placements:?}\n  vias added {vias}, shared {shared}");
        for d in plan.pads.iter().filter(|d| d.action == "no_site" || d.action == "share" || d.action == "not_planned" || d.action == "via_grid") {
            println!("    {} {} {}: {}", d.pad, d.net, d.action, d.reason.chars().take(260).collect::<String>());
        }
        let gaps: Vec<f64> = plan.pads.iter().filter(|d| d.action == "via").filter_map(|d| d.stub_mm).collect();
        if !gaps.is_empty() {
            let max = gaps.iter().cloned().fold(0.0, f64::max);
            let mean = gaps.iter().sum::<f64>() / gaps.len() as f64;
            println!("  own-via connecting trace: mean {mean:.3} mm, max {max:.3} mm");
        }
    }

    #[test]
    #[ignore]
    fn pass24_board_validation() {
        let Ok(path) = std::env::var("PASS24_BOARD") else { return };
        let path = PathBuf::from(path);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let tree = konnect_sexp::parse_sexp(&text).unwrap();
        let (intent, _) = intent_for_board(&path).unwrap();
        let nets = plane_nets(&tree);
        let li = LowInductance {
            share_with_ic: std::env::var("PASS24_SHARE").map_or(true, |v| v != "never"),
            avoid_courtyards: std::env::var("PASS24_COURTYARDS").map_or(true, |v| v != "ignore"),
            ..LowInductance::default()
        };
        let classes = crate::tools::pcb_routing::netclass_fanout_options_for_tests(&path);
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let sibling = |tag: &str| path.with_file_name(format!("{stem}_{tag}.kicad_pcb"));
        for tag in ["ripped", "fanout", "inplace"] {
            for ext in ["kicad_pro", "kicad_dru"] {
                let _ = std::fs::copy(path.with_extension(ext), sibling(tag).with_extension(ext));
            }
            let _ = std::fs::copy(path.with_file_name(format!("{stem}.intent.json")), path.with_file_name(format!("{stem}_{tag}.intent.json")));
        }
        println!("== {} nets {nets:?}", path.display());
        let before = plane_verdicts(&tree, &intent);
        println!("saved board: {}", summary(&before, &nets));
        for f in failing(&before) {
            println!("    fail {f}");
        }
        println!("  routability (saved): {}", routability(&rt, &path, &text));

        // A. In place: fix what fails on the saved board.
        let t0 = std::time::Instant::now();
        let (plan_a, _) = plan_with_judge(&tree, &nets, &classes, false, &li, &intent);
        println!("-- A. in place ({:.1} s)", t0.elapsed().as_secs_f64());
        report_plan(&plan_a);
        let a_text = board_with_plan(&text, &plan_a);
        std::fs::write(sibling("inplace"), &a_text).unwrap();
        let a_dry = plane_verdicts(&konnect_sexp::parse_sexp(&a_text).unwrap(), &intent);
        println!("  self-check (dry run): {}", summary(&a_dry, &nets));
        println!("  gate: {}", gate(&rt, &path, &text, &a_text));
        let a_filled = refill(&sibling("inplace"));
        let a_after = plane_verdicts(&konnect_sexp::parse_sexp(&a_filled).unwrap(), &intent);
        println!("  after refill: {}", summary(&a_after, &nets));
        for f in failing(&a_after) {
            println!("    fail {f}");
        }

        // B. Rip up the plane nets' tracks and vias, then fan out from scratch.
        let (ripped, removed) = rip(&text, &nets);
        std::fs::write(sibling("ripped"), &ripped).unwrap();
        let ripped = refill(&sibling("ripped"));
        let rtree = konnect_sexp::parse_sexp(&ripped).unwrap();
        let r_before = plane_verdicts(&rtree, &intent);
        println!("-- B. ripped up {removed} tracks/vias on {nets:?}: {}", summary(&r_before, &nets));
        println!("  routability (ripped): {}", routability(&rt, &sibling("ripped"), &ripped));
        let t0 = std::time::Instant::now();
        let (plan_b, _) = plan_with_judge(&rtree, &nets, &classes, false, &li, &intent);
        println!("  planned in {:.1} s", t0.elapsed().as_secs_f64());
        report_plan(&plan_b);
        let b_text = board_with_plan(&ripped, &plan_b);
        std::fs::write(sibling("fanout"), &b_text).unwrap();
        let b_dry = plane_verdicts(&konnect_sexp::parse_sexp(&b_text).unwrap(), &intent);
        println!("  self-check (dry run): {}", summary(&b_dry, &nets));
        println!("  gate: {}", gate(&rt, &sibling("ripped"), &ripped, &b_text));
        let b_filled = refill(&sibling("fanout"));
        let b_after = plane_verdicts(&konnect_sexp::parse_sexp(&b_filled).unwrap(), &intent);
        println!("  after refill: {}", summary(&b_after, &nets));
        for f in failing(&b_after) {
            println!("    fail {f}");
        }
        println!("  routability (fanned out): {}", routability(&rt, &sibling("fanout"), &b_filled));
        // C. Before routing: every track and via ripped (the fan-out's real
        // place in the flow), then fan out; routability with and without.
        let all_nets: Vec<String> = konnect_sexp::net::collect_net_keys(&tree).into_iter().filter(|n| !n.is_empty()).collect();
        let (bare, removed_all) = rip(&text, &all_nets);
        std::fs::write(sibling("bare"), &bare).unwrap();
        for ext in ["kicad_pro", "kicad_dru"] {
            let _ = std::fs::copy(path.with_extension(ext), sibling("bare").with_extension(ext));
        }
        let _ = std::fs::copy(path.with_file_name(format!("{stem}.intent.json")), path.with_file_name(format!("{stem}_bare.intent.json")));
        let bare = refill(&sibling("bare"));
        let btree = konnect_sexp::parse_sexp(&bare).unwrap();
        println!("-- C. all routing ripped ({removed_all} items)");
        println!("  routability (bare): {}", routability(&rt, &sibling("bare"), &bare));
        let (plan_c, _) = plan_with_judge(&btree, &nets, &classes, false, &li, &intent);
        report_plan(&plan_c);
        let c_text = board_with_plan(&bare, &plan_c);
        println!("  gate: {}", gate(&rt, &sibling("bare"), &bare, &c_text));
        let c_after = plane_verdicts(&konnect_sexp::parse_sexp(&c_text).unwrap(), &intent);
        println!("  self-check (dry run): {}", summary(&c_after, &nets));
        println!("  routability (low-inductance fan-out): {}", routability(&rt, &sibling("bare"), &c_text));
        let (plan_cl, _) = plan_with_judge(&btree, &nets, &classes, true, &li, &intent);
        let cl_text = board_with_plan(&bare, &plan_cl);
        let cl_after = plane_verdicts(&konnect_sexp::parse_sexp(&cl_text).unwrap(), &intent);
        println!("  legacy: {} vias, {} skipped; self-check {}", plan_cl.fanouts.iter().filter(|f| f.via).count(), plan_cl.skipped.len(), summary(&cl_after, &nets));
        println!("  legacy gate: {}", gate(&rt, &sibling("bare"), &bare, &cl_text));
        println!("  routability (legacy fan-out): {}", routability(&rt, &sibling("bare"), &cl_text));
        // Legacy planner on the same ripped board, for comparison.
        let (plan_l, _) = plan_with_judge(&rtree, &nets, &classes, true, &li, &intent);
        let l_text = board_with_plan(&ripped, &plan_l);
        let l_dry = plane_verdicts(&konnect_sexp::parse_sexp(&l_text).unwrap(), &intent);
        println!("-- legacy planner on the ripped board: {} vias, {} skipped; self-check {}", plan_l.fanouts.iter().filter(|f| f.via).count(), plan_l.skipped.len(), summary(&l_dry, &nets));
        println!("  legacy gate: {}", gate(&rt, &sibling("ripped"), &ripped, &l_text));
    }
}
