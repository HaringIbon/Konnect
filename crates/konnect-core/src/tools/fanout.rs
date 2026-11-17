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
    pub fanouts: Vec<PlannedFanout>,
    pub skipped: Vec<SkippedPad>,
    /// Through-hole pads on the nets: they reach every layer already.
    pub through_hole_pads: usize,
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
    keepouts: Vec<Keepout>,
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
            }
        }
        Self { edges, planes, keepouts }
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
        self.planes.get(net).is_some_and(|zones| zones.iter().any(|zone| point_in_polygon(p, zone)))
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
}

impl PadInfo {
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
            });
        }
    }
    pads
}

/// An obstacle of some net (`None`: no net, an obstacle for every net).
enum Obstacle {
    /// A pad as its true (rotated) rectangle.
    Poly { corners: [Point; 4], net: Option<String>, layers: Layers },
    Segment { a: Point, b: Point, half_width: f64, net: Option<String>, layer: String },
    Circle { center: Point, radius: f64, net: Option<String> },
}

#[derive(Clone)]
enum Layers {
    All,
    One(&'static str),
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
        obstacles.push(Obstacle::Poly {
            corners: pad.corners(),
            net: pad.net.clone(),
            layers: if pad.smd { Layers::One(pad.layer) } else { Layers::All },
        });
    }

    let mut result = FanoutPlan { nets: nets.to_vec(), ..FanoutPlan::default() };
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
        };
        if pad.size.0.min(pad.size.1) >= options.large_pad_mm {
            let sites = in_pad_sites(pad, net, &geometry, &options);
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
fn in_pad_sites(pad: &PadInfo, net: &str, geometry: &BoardGeometry, options: &FanoutOptions) -> Vec<Point> {
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
            if geometry.inside_board(site, options.edge_clearance_mm + via_r)
                && geometry.reaches_plane(net, site)
                && geometry.via_allowed(site, via_r)
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
            if !geometry.reaches_plane(net, site)
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
            Obstacle::Poly { corners, net: owner, layers } => {
                if other(owner) {
                    if point_polygon_distance(site, corners) < via_r + clear - EPS {
                        return false;
                    }
                    let same_layer = match layers {
                        Layers::All => true,
                        Layers::One(layer) => *layer == pad.layer,
                    };
                    if same_layer && segment_polygon_distance(pad.center, site, corners) < half_w + clear - EPS {
                        return false;
                    }
                }
            }
            Obstacle::Segment { a, b, half_width, net: owner, layer } => {
                if other(owner) {
                    if point_segment_distance(site, *a, *b) < via_r + half_width + clear - EPS {
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

fn point_in_polygon((x, y): Point, polygon: &[Point]) -> bool {
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
        // 9 vias inside U2's rotated exposed pad.
        let ep = plan.fanouts.iter().filter(|f| f.reference == "U2" && f.pad == "33").collect::<Vec<_>>();
        assert_eq!(ep.len(), 9);
        assert!(ep.iter().all(|f| f.in_pad && f.via));
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
