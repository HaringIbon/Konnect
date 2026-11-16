//! Plane fan-out planning: for each surface-mount pad on a plane net, a short
//! stub and a through via that drop it to the net's plane.
//!
//! This is the step that lets a board with inner planes be autorouted without
//! routing the plane nets as traces: once every SMD power pad has its own via,
//! the plane connects them, and the autorouter only sees signal nets.
//!
//! Pure geometry over a board snapshot, so it can be planned, inspected and
//! tested without KiCad; the tool applies the plan over IPC.
//!
//! The checks are conservative rather than exact:
//! - pads are their bounding boxes;
//! - a via spans every copper layer, so every pad, track and via of another
//!   net on any layer is an obstacle;
//! - a net-less zone (a keep-out) is never entered;
//! - the via stays inside the outline's bounding box, inset by the edge
//!   clearance.
//!
//! Anything a box test would wave through is left for the DRC that follows.

use konnect_sexp::bounds::{pad_bbox, Bbox};
use konnect_sexp::SexpNode;
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy)]
pub(crate) struct FanoutOptions {
    pub via_size_mm: f64,
    pub via_drill_mm: f64,
    pub track_width_mm: f64,
    pub clearance_mm: f64,
    pub edge_clearance_mm: f64,
    pub max_stub_mm: f64,
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
    pub stub_mm: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct SkippedPad {
    pub reference: String,
    pub pad: String,
    pub net: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct FanoutPlan {
    pub nets: Vec<String>,
    pub fanouts: Vec<PlannedFanout>,
    pub skipped: Vec<SkippedPad>,
    /// Through-hole pads on the nets: they reach every layer already.
    pub through_hole_pads: usize,
}

/// Nets with a zone on an inner copper layer: the planes a pad can drop to.
pub(crate) fn plane_nets(tree: &SexpNode) -> Vec<String> {
    let mut nets = BTreeSet::new();
    for zone in konnect_sexp::board::zones(tree).items {
        if let Some(net) = zone.net {
            if zone.layers.iter().any(|layer| is_inner(layer)) {
                nets.insert(net);
            }
        }
    }
    nets.into_iter().collect()
}

fn is_inner(layer: &str) -> bool {
    layer.starts_with("In") && layer.ends_with(".Cu")
}

struct PadInfo {
    reference: String,
    number: String,
    net: Option<String>,
    center: (f64, f64),
    bbox: Bbox,
    smd: bool,
    layer: &'static str,
    footprint_center: (f64, f64),
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
            let Some(bbox) = pad_bbox(pad, fx, fy, rot) else { continue };
            let kind = pad.get(2).and_then(SexpNode::as_str).unwrap_or("");
            let net = pad
                .find("net")
                .and_then(|node| konnect_sexp::net::net_name(node))
                .filter(|name| !name.is_empty())
                .map(str::to_string);
            pads.push(PadInfo {
                reference: reference.clone(),
                number: pad.get(1).and_then(SexpNode::as_str).unwrap_or("").to_string(),
                net,
                center: ((bbox.0 + bbox.2) / 2.0, (bbox.1 + bbox.3) / 2.0),
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
    Rect { bbox: Bbox, net: Option<String>, layers: Layers },
    Segment { a: (f64, f64), b: (f64, f64), half_width: f64, net: Option<String>, layer: String },
    Circle { center: (f64, f64), radius: f64, net: Option<String> },
}

/// Which copper a pad occupies.
#[derive(Clone)]
enum Layers {
    All,
    One(&'static str),
}

pub(crate) fn plan(tree: &SexpNode, nets: &[String], options: &FanoutOptions) -> FanoutPlan {
    let wanted: BTreeSet<&str> = nets.iter().map(String::as_str).collect();
    let pads = collect_pads(tree);
    let mut obstacles = Vec::new();
    for pad in &pads {
        obstacles.push(Obstacle::Rect {
            bbox: pad.bbox,
            net: pad.net.clone(),
            layers: if pad.smd { Layers::One(pad.layer) } else { Layers::All },
        });
    }
    for track in konnect_sexp::board::tracks(tree).items {
        obstacles.push(Obstacle::Segment {
            a: (track.x1, track.y1),
            b: (track.x2, track.y2),
            half_width: track.width / 2.0,
            net: track.net,
            layer: track.layer,
        });
    }
    for via in konnect_sexp::board::vias(tree).items {
        obstacles.push(Obstacle::Circle {
            center: (via.x, via.y),
            radius: via.size / 2.0,
            net: via.net,
        });
    }
    let keepouts: Vec<Vec<(f64, f64)>> = konnect_sexp::board::zones(tree)
        .items
        .into_iter()
        .filter(|zone| zone.net.is_none())
        .map(|zone| zone.points)
        .collect();
    let area = konnect_sexp::board::board_outline_bbox(tree).map(|(x0, y0, x1, y1)| {
        let inset = options.edge_clearance_mm + options.via_size_mm / 2.0;
        (x0 + inset, y0 + inset, x1 - inset, y1 - inset)
    });
    let existing_vias = konnect_sexp::board::vias(tree).items;
    let existing_tracks = konnect_sexp::board::tracks(tree).items;

    let mut result = FanoutPlan {
        nets: nets.to_vec(),
        ..FanoutPlan::default()
    };
    for pad in &pads {
        let Some(net) = pad.net.as_deref().filter(|net| wanted.contains(net)) else {
            continue;
        };
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
        // Already dropped: a same-net via on the pad, or a same-net track
        // ending on it (left alone; the caller routed it deliberately).
        let on_pad = |(x, y): (f64, f64)| point_rect_distance((x, y), pad.bbox) == 0.0;
        if existing_vias
            .iter()
            .any(|via| via.net.as_deref() == Some(net) && on_pad((via.x, via.y)))
            || existing_tracks.iter().any(|track| {
                track.net.as_deref() == Some(net)
                    && (on_pad((track.x1, track.y1)) || on_pad((track.x2, track.y2)))
            })
        {
            result.skipped.push(skip("already connected by a track or via"));
            continue;
        }
        let Some(area) = area else {
            result.skipped.push(skip("board has no readable outline"));
            continue;
        };
        match best_site(pad, net, &obstacles, &keepouts, area, options) {
            Some((via, stub)) => {
                // Later pads must clear this via and stub too.
                obstacles.push(Obstacle::Circle {
                    center: via,
                    radius: options.via_size_mm / 2.0,
                    net: Some(net.to_string()),
                });
                obstacles.push(Obstacle::Segment {
                    a: pad.center,
                    b: via,
                    half_width: options.track_width_mm / 2.0,
                    net: Some(net.to_string()),
                    layer: pad.layer.to_string(),
                });
                result.fanouts.push(PlannedFanout {
                    reference: pad.reference.clone(),
                    pad: pad.number.clone(),
                    net: net.to_string(),
                    layer: pad.layer.to_string(),
                    pad_x: round4(pad.center.0),
                    pad_y: round4(pad.center.1),
                    via_x: round4(via.0),
                    via_y: round4(via.1),
                    stub_mm: round4(stub),
                });
            }
            None => result.skipped.push(skip(&format!(
                "no via site within {} mm clears every other net",
                options.max_stub_mm
            ))),
        }
    }
    result
}

/// The closest clear via site, preferring directions away from the
/// footprint's centre (under a chip is where its other pins are).
fn best_site(
    pad: &PadInfo,
    net: &str,
    obstacles: &[Obstacle],
    keepouts: &[Vec<(f64, f64)>],
    area: Bbox,
    options: &FanoutOptions,
) -> Option<((f64, f64), f64)> {
    let via_r = options.via_size_mm / 2.0;
    let half_w = options.track_width_mm / 2.0;
    let clear = options.clearance_mm;
    let outward = {
        let (dx, dy) = (pad.center.0 - pad.footprint_center.0, pad.center.1 - pad.footprint_center.1);
        let length = dx.hypot(dy);
        if length > 1e-6 { (dx / length, dy / length) } else { (0.0, 0.0) }
    };
    let s = std::f64::consts::FRAC_1_SQRT_2;
    let directions = [
        (1.0, 0.0), (-1.0, 0.0), (0.0, 1.0), (0.0, -1.0),
        (s, s), (s, -s), (-s, s), (-s, -s),
    ];
    let mut best: Option<((f64, f64), f64, f64)> = None;
    for (dx, dy) in directions {
        let outwardness = dx * outward.0 + dy * outward.1;
        let mut distance = 0.0;
        while distance <= options.max_stub_mm + 1e-9 {
            distance += 0.05;
            let site = (pad.center.0 + dx * distance, pad.center.1 + dy * distance);
            // Not on its own pad: a via in an SMD pad wicks the solder away.
            if point_rect_distance(site, pad.bbox) < via_r + 0.05 {
                continue;
            }
            if !(area.0..=area.2).contains(&site.0) || !(area.1..=area.3).contains(&site.1) {
                break;
            }
            if keepouts.iter().any(|polygon| point_in_polygon(site, polygon)) {
                continue;
            }
            if !site_is_clear(site, pad, net, obstacles, via_r, half_w, clear, options) {
                continue;
            }
            let stub = distance;
            // Shorter wins; outward breaks near-ties (0.3 mm per unit).
            let score = stub - 0.3 * outwardness;
            if best.is_none_or(|(_, _, s)| score < s) {
                best = Some((site, stub, score));
            }
            break;
        }
    }
    best.map(|(site, stub, _)| (site, stub))
}

#[allow(clippy::too_many_arguments)]
fn site_is_clear(
    site: (f64, f64),
    pad: &PadInfo,
    net: &str,
    obstacles: &[Obstacle],
    via_r: f64,
    half_w: f64,
    clear: f64,
    options: &FanoutOptions,
) -> bool {
    let other = |owner: &Option<String>| owner.as_deref() != Some(net);
    for obstacle in obstacles {
        match obstacle {
            Obstacle::Rect { bbox, net: owner, layers } => {
                if other(owner) {
                    if point_rect_distance(site, *bbox) < via_r + clear {
                        return false;
                    }
                    let same_layer = match layers {
                        Layers::All => true,
                        Layers::One(layer) => *layer == pad.layer,
                    };
                    if same_layer && segment_rect_distance(pad.center, site, *bbox) < half_w + clear {
                        return false;
                    }
                }
            }
            Obstacle::Segment { a, b, half_width, net: owner, layer } => {
                if other(owner) {
                    if point_segment_distance(site, *a, *b) < via_r + half_width + clear {
                        return false;
                    }
                    if layer == pad.layer
                        && segment_segment_distance(pad.center, site, *a, *b)
                            < half_w + half_width + clear
                    {
                        return false;
                    }
                }
            }
            Obstacle::Circle { center, radius, net: owner } => {
                let gap = (site.0 - center.0).hypot(site.1 - center.1);
                if other(owner) {
                    if gap < via_r + radius + clear
                        || point_segment_distance(*center, pad.center, site) < radius + half_w + clear
                    {
                        return false;
                    }
                } else if gap < via_r + radius + options.clearance_mm.max(0.25) {
                    // Same-net vias still need drill-to-drill spacing.
                    return false;
                }
            }
        }
    }
    true
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

fn point_rect_distance((x, y): (f64, f64), (x0, y0, x1, y1): Bbox) -> f64 {
    let dx = (x0 - x).max(0.0).max(x - x1);
    let dy = (y0 - y).max(0.0).max(y - y1);
    dx.hypot(dy)
}

fn point_segment_distance(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (vx, vy) = (b.0 - a.0, b.1 - a.1);
    let length2 = vx * vx + vy * vy;
    let t = if length2 == 0.0 {
        0.0
    } else {
        (((p.0 - a.0) * vx + (p.1 - a.1) * vy) / length2).clamp(0.0, 1.0)
    };
    (p.0 - (a.0 + t * vx)).hypot(p.1 - (a.1 + t * vy))
}

fn segments_intersect(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> bool {
    let cross = |o: (f64, f64), p: (f64, f64), q: (f64, f64)| {
        (p.0 - o.0) * (q.1 - o.1) - (p.1 - o.1) * (q.0 - o.0)
    };
    let (d1, d2) = (cross(c, d, a), cross(c, d, b));
    let (d3, d4) = (cross(a, b, c), cross(a, b, d));
    (d1 > 0.0) != (d2 > 0.0) && (d3 > 0.0) != (d4 > 0.0)
}

fn segment_segment_distance(a: (f64, f64), b: (f64, f64), c: (f64, f64), d: (f64, f64)) -> f64 {
    if segments_intersect(a, b, c, d) {
        return 0.0;
    }
    point_segment_distance(a, c, d)
        .min(point_segment_distance(b, c, d))
        .min(point_segment_distance(c, a, b))
        .min(point_segment_distance(d, a, b))
}

fn segment_rect_distance(a: (f64, f64), b: (f64, f64), rect: Bbox) -> f64 {
    let (x0, y0, x1, y1) = rect;
    if point_rect_distance(a, rect) == 0.0 || point_rect_distance(b, rect) == 0.0 {
        return 0.0;
    }
    let corners = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
    (0..4)
        .map(|i| segment_segment_distance(a, b, corners[i], corners[(i + 1) % 4]))
        .fold(f64::INFINITY, f64::min)
}

fn point_in_polygon((x, y): (f64, f64), polygon: &[(f64, f64)]) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use konnect_sexp::parse_sexp;

    /// A 20 × 20 mm board, a GND plane on In1.Cu, one 0603 capacitor whose
    /// pad 2 is GND and pad 1 is VCC, and a VCC track running past pad 2.
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

    #[test]
    fn plane_nets_are_the_nets_with_inner_zones() {
        assert_eq!(plane_nets(&board("")), vec!["GND".to_string()]);
    }

    #[test]
    fn a_gnd_pad_gets_a_clear_via_outward_from_the_part() {
        let plan = plan(&board(""), &["GND".to_string()], &FanoutOptions::default());
        assert_eq!(plan.fanouts.len(), 1, "{plan:?}");
        let fanout = &plan.fanouts[0];
        assert_eq!((fanout.reference.as_str(), fanout.pad.as_str()), ("C1", "2"));
        // Outward (+x) from the capacitor, off the pad, within the stub limit.
        assert!(fanout.via_x > 10.775 + 0.45 + 0.3, "{fanout:?}");
        assert!((fanout.via_y - 10.0).abs() < 1e-6, "{fanout:?}");
        assert!(fanout.stub_mm <= 2.5);
    }

    #[test]
    fn another_nets_track_pushes_the_via_elsewhere() {
        // A VCC track straight through the outward site, top to bottom.
        let blocked = board(
            "(segment (start 12.2 5) (end 12.2 15) (width 0.3) (layer \"F.Cu\") (net \"VCC\"))",
        );
        let plan = plan(&blocked, &["GND".to_string()], &FanoutOptions::default());
        let fanout = &plan.fanouts[0];
        let gap = point_segment_distance((fanout.via_x, fanout.via_y), (12.2, 5.0), (12.2, 15.0));
        assert!(gap >= 0.3 + 0.15 + 0.2 - 1e-9, "{fanout:?} gap {gap}");
    }

    #[test]
    fn a_keepout_is_never_entered() {
        let keepout = board("(zone (layers \"F.Cu\" \"In1.Cu\" \"B.Cu\") (keepout (vias not_allowed)) (polygon (pts (xy 11 0) (xy 20 0) (xy 20 20) (xy 11 20))))");
        let plan = plan(&keepout, &["GND".to_string()], &FanoutOptions::default());
        for fanout in &plan.fanouts {
            assert!(fanout.via_x < 11.0, "{fanout:?}");
        }
    }

    #[test]
    fn an_already_connected_pad_is_left_alone() {
        let done = board("(via (at 11.5 10) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (net \"GND\"))
                          (segment (start 10.775 10) (end 11.5 10) (width 0.3) (layer \"F.Cu\") (net \"GND\"))");
        let plan = plan(&done, &["GND".to_string()], &FanoutOptions::default());
        assert!(plan.fanouts.is_empty());
        assert_eq!(plan.skipped.len(), 1);
    }
}
