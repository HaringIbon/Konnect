//! Routing supply against demand: section-crossing cuts, cuts through the
//! channels between neighbouring parts, a RUDY congestion grid, and Holden's
//! global density ratio.
//!
//! Sources (doc DB):
//! - `holden-tt42-density-equations` names the "Section Crossing Method"
//!   among the wiring-demand models; here it is applied literally: every
//!   route between two points on opposite sides of a straight line across
//!   the whole board must cross that line on some layer, so the line's free
//!   length bounds how many can.
//! - `kim-iccad11-simplr` (SimPLR): simple estimators miss partial routing
//!   obstacles; capacity is the unblocked length after taking "the union of
//!   all routing-obstacle shapes", divided by minWidth + minSpacing. The
//!   cuts and the grid both subtract pads, keep-outs, existing tracks and
//!   vias, the board-edge clearance and pours on mixed layers.
//! - `lou-ispd2001-probabilistic-congestion-slides`: per-grid routing supply
//!   `H × Σ 1/L` (tracks per pitch). PCB layers are not directional, so a
//!   cell's supply is its free area over the track pitch on each layer.
//! - `circuitnet-routability-features`: RUDY demand
//!   `(w_k + h_k)/(w_k · h_k)` spread over the box, and overflow-based
//!   congestion (overflow / total tracks). Applied per airwire, not per net
//!   (`arxiv-2206-03032-xie-thesis-rudy` discusses RUDY's per-net box; an
//!   airwire is a 2-pin net). A box thinner than one track pitch is clamped
//!   to one pitch, so a straight airwire still has length.
//! - `holden-tt26-density-equation`: wiring demand Wd against substrate
//!   capacity Wc; Wd > Wc cannot finish, and about 20 % spare (Wd ≤ Wc/1.2)
//!   is the target when autorouting. Layout efficiency (the share of the
//!   capacity a router can actually use) has no number in the source; it is
//!   a calibrated parameter here.

use super::crossings::{Airwire, TrackSeg};
use super::escape::round3;
use super::geom::{edge_soup_intervals, merge, subtract, Axis, Pt, Raster, Shape};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// An obstacle on some signal layers (indices into the caller's layer list).
#[derive(Debug, Clone)]
pub(crate) struct Obstacle {
    pub shape: Shape,
    pub layers: Vec<usize>,
    pub net: Option<String>,
    pub pad: bool,
    /// The board-edge clearance band. A free interval ending on it needs no
    /// track clearance at that end: the band already is the clearance.
    pub edge: bool,
}

/// Track geometry for the cuts and the grid.
#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct Rules {
    /// The default class: what a lane is counted in.
    pub w: f64,
    pub s: f64,
    /// Smallest clearance among the demand nets (the capacity bound uses it).
    pub s_min: f64,
    /// The fabricator minimums from the project (`min_track_width`,
    /// `min_clearance`), to say whether necking down would fit.
    pub fab_w: f64,
    pub fab_s: f64,
}

impl Rules {
    pub(crate) fn pitch(&self) -> f64 {
        self.w + self.s
    }
}

pub(crate) struct Board<'a> {
    pub layers: &'a [String],
    pub edges: &'a [(Pt, Pt)],
    pub obstacles: &'a [Obstacle],
    pub tracks: &'a [TrackSeg],
    pub raster: &'a Raster,
    pub rules: Rules,
}

fn lanes(gap: f64, w: f64, s: f64) -> usize {
    let n = ((gap - s) / (w + s) + 1e-9).floor();
    if n > 0.0 { n as usize } else { 0 }
}

/// A free interval and how many of its ends lie on the board edge (or its
/// clearance band) rather than on copper or a keep-out.
#[derive(Debug, Clone, Copy)]
struct Open {
    a: f64,
    b: f64,
    edge_ends: u8,
}

impl Open {
    /// The gap as the lane formula sees it: an end on the edge band needs no
    /// extra clearance, so it gets one `s` back (the formula charges s at both ends).
    fn gap(&self, s: f64) -> f64 {
        self.b - self.a + s * f64::from(self.edge_ends)
    }
}

/// One cut: a line (or a segment of it) and what must cross it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Cut {
    pub axis: Axis,
    /// The line's fixed coordinate (y for a horizontal cut).
    pub at: f64,
    /// The span along the line that was examined (the board's extent for a
    /// full-board cut).
    pub from: f64,
    pub to: f64,
    /// Airwires whose straight line crosses the cut (minus those crossing on
    /// their own pad).
    pub demand_airwires: usize,
    /// Nets with unconnected copper on both sides and no copper of their own
    /// across the line: each must cross at least once, whatever the routing.
    pub demand_nets_lower_bound: usize,
    /// Σ over layers and free intervals of ⌊(g − s)/(w + s)⌋, default class.
    pub capacity_lanes: usize,
    pub capacity_lanes_per_layer: Vec<usize>,
    /// The same at the fabricator minimum width and clearance.
    pub capacity_lanes_at_fab_minimum: usize,
    /// Σ (w + s) of the crossing airwires' nets over Σ (g − s_min).
    pub utilization: f64,
    /// The lower-bound nets' Σ (w + s) over the same capacity.
    pub lower_bound_utilization: f64,
    /// Free intervals per layer (largest first, at most 4 each).
    pub free: BTreeMap<String, Vec<(f64, f64)>>,
    /// Nets crossing, most airwires first (at most 8).
    pub nets: Vec<(String, usize)>,
    #[serde(skip)]
    pub lower_bound_nets: Vec<String>,
    /// Σ (g − s_min) over the free intervals of every layer (mm).
    pub capacity_width: f64,
    #[serde(skip)]
    pub lower_bound_lanes_needed: usize,
    #[serde(skip)]
    pub lower_bound_min_pitch_lanes: usize,
}

impl Cut {
    /// A necessary condition fails: the nets that must cross cannot fit.
    pub(crate) fn proves_doom(&self) -> bool {
        self.lower_bound_lanes_needed > self.lower_bound_min_pitch_lanes || self.lower_bound_utilization > 1.0 + 1e-9
    }
}

/// Evaluate the cut `across = at` along `axis`, optionally limited to the
/// span `range` along the line. `pitch[i]` is airwire i's net w + s.
pub(crate) fn cut(board: &Board, airwires: &[Airwire], pitch: &[f64], axis: Axis, at: f64, range: Option<(f64, f64)>) -> Cut {
    let rules = board.rules;
    let mut base = edge_soup_intervals(board.edges.iter().copied(), axis, at);
    if let Some((lo, hi)) = range {
        base = base.into_iter().map(|(a, b)| (a.max(lo), b.min(hi))).filter(|(a, b)| a < b).collect();
    }
    let (from, to) = (base.first().map_or(at, |b| b.0), base.last().map_or(at, |b| b.1));
    let mut free = BTreeMap::new();
    let mut per_layer = Vec::new();
    let mut lanes_fab = 0;
    let mut capacity_width = 0.0;
    let mut free_by_layer: Vec<Vec<Open>> = Vec::new();
    for (l, name) in board.layers.iter().enumerate() {
        let mut blocked = Vec::new();
        let mut edge_band = Vec::new();
        for obstacle in board.obstacles.iter().filter(|o| o.layers.contains(&l)) {
            let (x0, y0, x1, y1) = obstacle.shape.bbox();
            let (lo, hi) = match axis {
                Axis::Horizontal => (y0, y1),
                Axis::Vertical => (x0, x1),
            };
            if at < lo || at > hi {
                continue;
            }
            let intervals = obstacle.shape.line_intervals(axis, at);
            if obstacle.edge {
                edge_band.extend(intervals.iter().copied());
            }
            blocked.extend(intervals);
        }
        let edge_band = merge(edge_band);
        let free_now = subtract(&base, &merge(blocked));
        // An end is on the edge when it is where the edge band stops, or the
        // board outline itself (a full-board cut's base interval ends).
        let on_edge = |x: f64| {
            edge_band.iter().any(|&(lo, hi)| (x - lo).abs() < 1e-6 || (x - hi).abs() < 1e-6)
                || (range.is_none() && base.iter().any(|&(lo, hi)| (x - lo).abs() < 1e-6 || (x - hi).abs() < 1e-6))
        };
        let open: Vec<Open> = free_now.iter().map(|&(a, b)| Open { a, b, edge_ends: u8::from(on_edge(a)) + u8::from(on_edge(b)) }).collect();
        per_layer.push(open.iter().map(|o| lanes(o.gap(rules.s), rules.w, rules.s)).sum());
        lanes_fab += open.iter().map(|o| lanes(o.gap(rules.fab_s), rules.fab_w, rules.fab_s)).sum::<usize>();
        capacity_width += open.iter().map(|o| (o.gap(rules.s_min) - rules.s_min).max(0.0)).sum::<f64>();
        let mut shown = free_now.clone();
        shown.sort_by(|a, b| (b.1 - b.0).total_cmp(&(a.1 - a.0)));
        free.insert(name.clone(), shown.into_iter().take(4).map(|(a, b)| (round3(a), round3(b))).collect());
        free_by_layer.push(open);
    }
    // Nets with their own copper across the line need no new crossing.
    let mut straddling: BTreeSet<&str> = BTreeSet::new();
    for track in board.tracks {
        let (ta, tb) = (axis.split(track.a).1 - at, axis.split(track.b).1 - at);
        if ta * tb <= 0.0 {
            if let Some(net) = &track.net {
                straddling.insert(net);
            }
        }
    }
    let point = |along: f64| match axis {
        Axis::Horizontal => (along, at),
        Axis::Vertical => (at, along),
    };
    let mut own_pads: BTreeMap<&str, Vec<&Shape>> = BTreeMap::new();
    for obstacle in board.obstacles.iter().filter(|o| o.pad) {
        if let Some(net) = &obstacle.net {
            own_pads.entry(net).or_default().push(&obstacle.shape);
            let (x0, y0, x1, y1) = obstacle.shape.bbox();
            let (lo, hi) = match axis {
                Axis::Horizontal => (y0, y1),
                Axis::Vertical => (x0, x1),
            };
            if at >= lo && at <= hi && !obstacle.shape.line_intervals(axis, at).is_empty() {
                straddling.insert(net);
            }
        }
    }
    let mut demand = 0usize;
    let mut demand_width = 0.0;
    let mut nets: BTreeMap<String, usize> = BTreeMap::new();
    let mut lb: BTreeMap<String, f64> = BTreeMap::new();
    for (i, wire) in airwires.iter().enumerate() {
        let (a, b) = (axis.split(wire.a.at), axis.split(wire.b.at));
        if (a.1 - at) * (b.1 - at) >= 0.0 {
            continue;
        }
        let along = a.0 + (at - a.1) * (b.0 - a.0) / (b.1 - a.1);
        if let Some((lo, hi)) = range {
            if along < lo || along > hi {
                continue;
            }
        }
        let p = point(along);
        if own_pads.get(wire.net.as_str()).is_some_and(|shapes| shapes.iter().any(|s| s.contains(p))) {
            continue;
        }
        demand += 1;
        demand_width += pitch[i];
        *nets.entry(wire.net.clone()).or_default() += 1;
        if range.is_none() && !straddling.contains(wire.net.as_str()) {
            let e = lb.entry(wire.net.clone()).or_default();
            *e = e.max(pitch[i]);
        }
    }
    let lb_width: f64 = lb.values().sum();
    let min_pitch = lb.values().copied().fold(f64::INFINITY, f64::min);
    let lb_min_pitch_lanes = if lb.is_empty() {
        usize::MAX
    } else {
        // Lanes at the narrowest crossing net's pitch: no more tracks than this fit.
        free_by_layer
            .iter()
            .flatten()
            .map(|o| lanes(o.gap(rules.s_min), min_pitch - rules.s_min, rules.s_min))
            .sum()
    };
    let mut net_list: Vec<(String, usize)> = nets.into_iter().collect();
    net_list.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    net_list.truncate(8);
    let ratio = |x: f64| if capacity_width > 0.0 { round3(x / capacity_width) } else if x > 0.0 { f64::INFINITY } else { 0.0 };
    Cut {
        axis,
        at: round3(at),
        from: round3(from),
        to: round3(to),
        demand_airwires: demand,
        demand_nets_lower_bound: lb.len(),
        capacity_lanes: per_layer.iter().sum(),
        capacity_lanes_per_layer: per_layer,
        capacity_lanes_at_fab_minimum: lanes_fab,
        utilization: ratio(demand_width),
        lower_bound_utilization: ratio(lb_width),
        free,
        nets: net_list,
        lower_bound_nets: lb.keys().cloned().collect(),
        capacity_width: round3(capacity_width),
        lower_bound_lanes_needed: lb.len(),
        lower_bound_min_pitch_lanes: lb_min_pitch_lanes,
    }
}

/// Full-board cuts every `step` mm in both directions.
pub(crate) fn full_board_cuts(board: &Board, airwires: &[Airwire], pitch: &[f64], step: f64) -> Vec<Cut> {
    let (x0, y0, x1, y1) = board.edges.iter().fold(
        (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY),
        |(a, b, c, d), &(p, q)| (a.min(p.0).min(q.0), b.min(p.1).min(q.1), c.max(p.0).max(q.0), d.max(p.1).max(q.1)),
    );
    let mut out = Vec::new();
    for (axis, lo, hi) in [(Axis::Horizontal, y0, y1), (Axis::Vertical, x0, x1)] {
        let mut at = lo + step / 2.0;
        while at < hi {
            out.push(cut(board, airwires, pitch, axis, at, None));
            at += step;
        }
    }
    out
}

/// A footprint's courtyard box, for the channel cuts.
#[derive(Debug, Clone)]
pub(crate) struct PartBox {
    pub reference: String,
    pub bbox: (f64, f64, f64, f64),
    /// Copper pads of the part: channels are examined only next to a part
    /// with at least [`CHANNEL_MIN_PADS`] (an IC or connector), where a
    /// detour is long; small passives are easy to route round.
    pub pads: usize,
}

pub(crate) const CHANNEL_MIN_PADS: usize = 8;
/// The parts must face each other along at least this much (mm).
pub(crate) const CHANNEL_MIN_SPAN: f64 = 2.0;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ChannelCut {
    pub between: [String; 2],
    pub gap_mm: f64,
    #[serde(flatten)]
    pub cut: Cut,
}

/// Cuts across the gap between neighbouring parts: for two parts facing
/// each other across a gap of at most `max_gap` mm with no third part in it
/// (one of them an IC or connector),
/// segments across the gap (courtyard to courtyard) at a quarter, half and
/// three quarters of their shared span. A route can go round, so these only
/// ever say "tight".
pub(crate) fn channel_cuts(board: &Board, airwires: &[Airwire], pitch: &[f64], parts: &[PartBox], max_gap: f64) -> Vec<ChannelCut> {
    let mut out = Vec::new();
    for (i, a) in parts.iter().enumerate() {
        for (j, b) in parts.iter().enumerate() {
            if i == j || a.pads.max(b.pads) < CHANNEL_MIN_PADS {
                continue;
            }
            let (ax0, ay0, ax1, ay1) = a.bbox;
            let (bx0, by0, bx1, by1) = b.bbox;
            // b to the right of a (gap in x), or b below a (gap in y).
            for horizontal_gap in [true, false] {
                let (gap, span_lo, span_hi) = if horizontal_gap {
                    (bx0 - ax1, ay0.max(by0), ay1.min(by1))
                } else {
                    (by0 - ay1, ax0.max(bx0), ax1.min(bx1))
                };
                if gap <= 0.0 || gap > max_gap || span_hi - span_lo < CHANNEL_MIN_SPAN {
                    continue;
                }
                let gap_box = if horizontal_gap { (ax1, span_lo, bx0, span_hi) } else { (span_lo, ay1, span_hi, by0) };
                let blocked = parts.iter().enumerate().any(|(k, c)| {
                    k != i && k != j && c.bbox.0 < gap_box.2 && c.bbox.2 > gap_box.0 && c.bbox.1 < gap_box.3 && c.bbox.3 > gap_box.1
                });
                if blocked {
                    continue;
                }
                let worst = [0.25, 0.5, 0.75]
                    .iter()
                    .map(|t| {
                        let at = span_lo + t * (span_hi - span_lo);
                        if horizontal_gap {
                            cut(board, airwires, pitch, Axis::Horizontal, at, Some((ax1, bx0)))
                        } else {
                            cut(board, airwires, pitch, Axis::Vertical, at, Some((ay1, by0)))
                        }
                    })
                    .max_by(|x, y| x.utilization.total_cmp(&y.utilization));
                if let Some(cut) = worst {
                    if cut.demand_airwires > 0 {
                        out.push(ChannelCut { between: [a.reference.clone(), b.reference.clone()], gap_mm: round3(gap), cut });
                    }
                }
            }
        }
    }
    out.sort_by(|x, y| y.cut.utilization.total_cmp(&x.cut.utilization));
    out
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Cell {
    pub x: f64,
    pub y: f64,
    /// Demand and supply in mm of default-class track.
    pub demand_mm: f64,
    pub supply_mm: f64,
    pub utilization: f64,
    pub nets: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Congestion {
    pub cell_mm: f64,
    pub cells: usize,
    pub overflow_cells: usize,
    pub overflow_fraction: f64,
    /// CircuitNet's overflow-based congestion over the whole board:
    /// Σ max(0, demand − supply) / Σ supply.
    pub total_overflow: f64,
    pub max_utilization: f64,
    /// Cells with demand but no free track (pin fields, keep-outs): not
    /// counted as overflow.
    pub blocked_cells_with_demand: usize,
    pub worst_cells: Vec<Cell>,
}

/// RUDY box of an airwire, clamped to at least one track pitch each way.
fn rudy_box(wire: &Airwire, min: f64) -> (f64, f64, f64, f64) {
    let (mut x0, mut x1) = (wire.a.at.0.min(wire.b.at.0), wire.a.at.0.max(wire.b.at.0));
    let (mut y0, mut y1) = (wire.a.at.1.min(wire.b.at.1), wire.a.at.1.max(wire.b.at.1));
    if x1 - x0 < min {
        let c = (x0 + x1) / 2.0;
        (x0, x1) = (c - min / 2.0, c + min / 2.0);
    }
    if y1 - y0 < min {
        let c = (y0 + y1) / 2.0;
        (y0, y1) = (c - min / 2.0, c + min / 2.0);
    }
    (x0, y0, x1, y1)
}

pub(crate) fn congestion(board: &Board, airwires: &[Airwire], pitch: &[f64], cell: f64) -> Congestion {
    let raster = board.raster;
    let (bx0, by0) = (raster.x0, raster.y0);
    let nx = ((raster.nx as f64 * raster.res) / cell).ceil() as usize;
    let ny = ((raster.ny as f64 * raster.res) / cell).ceil() as usize;
    let p0 = board.rules.pitch();
    let mut demand = vec![0.0; nx * ny];
    let overlap = |(a0, a1): (f64, f64), (b0, b1): (f64, f64)| (a1.min(b1) - a0.max(b0)).max(0.0);
    let contributions = |wire: &Airwire, weight: f64, mut f: Box<dyn FnMut(usize, f64) + '_>| {
        let (x0, y0, x1, y1) = rudy_box(wire, p0);
        let density = (x1 - x0 + y1 - y0) / ((x1 - x0) * (y1 - y0)) * weight;
        let i0 = ((x0 - bx0) / cell).floor().max(0.0) as usize;
        let i1 = (((x1 - bx0) / cell).floor().max(0.0) as usize).min(nx.saturating_sub(1));
        let j0 = ((y0 - by0) / cell).floor().max(0.0) as usize;
        let j1 = (((y1 - by0) / cell).floor().max(0.0) as usize).min(ny.saturating_sub(1));
        for j in j0..=j1 {
            for i in i0..=i1 {
                let (cx0, cy0) = (bx0 + i as f64 * cell, by0 + j as f64 * cell);
                let area = overlap((x0, x1), (cx0, cx0 + cell)) * overlap((y0, y1), (cy0, cy0 + cell));
                if area > 0.0 {
                    f(j * nx + i, density * area);
                }
            }
        }
    };
    for (k, wire) in airwires.iter().enumerate() {
        contributions(wire, pitch[k] / p0, Box::new(|idx, v| demand[idx] += v));
    }
    let mut cells = 0;
    let mut overflow_cells = 0;
    let mut blocked_with_demand = 0;
    let mut total_supply = 0.0;
    let mut total_overflow = 0.0;
    let mut max_util: f64 = 0.0;
    let mut ranked: Vec<(f64, usize, f64)> = Vec::new();
    let mut supply = vec![0.0; nx * ny];
    for j in 0..ny {
        for i in 0..nx {
            let bbox = (bx0 + i as f64 * cell, by0 + j as f64 * cell, bx0 + (i + 1) as f64 * cell, by0 + (j + 1) as f64 * cell);
            if raster.inside_area_in(bbox) < 0.5 * cell * cell {
                continue;
            }
            cells += 1;
            let s: f64 = (0..board.layers.len()).map(|l| raster.free_area_in(l, bbox)).sum::<f64>() / p0;
            let idx = j * nx + i;
            supply[idx] = s;
            let d = demand[idx];
            // A cell with no free track at all is a pin field or a keep-out:
            // airwires there end on their own pads. Counted apart, not as
            // overflow (the pads' own copper carries the connection).
            if s <= 1e-9 {
                if d > 0.0 {
                    blocked_with_demand += 1;
                }
                continue;
            }
            let util = d / s;
            total_supply += s;
            total_overflow += (d - s).max(0.0);
            if d > s + 1e-9 {
                overflow_cells += 1;
            }
            max_util = max_util.max(util);
            if d > 0.0 {
                ranked.push((util, idx, d));
            }
        }
    }
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.2.total_cmp(&a.2)));
    ranked.truncate(10);
    let mut worst = Vec::new();
    for &(util, idx, d) in &ranked {
        let mut by_net: BTreeMap<String, f64> = BTreeMap::new();
        for (k, wire) in airwires.iter().enumerate() {
            let net = wire.net.clone();
            let weight = pitch[k] / p0;
            let mut add = 0.0;
            contributions(wire, weight, Box::new(|cell_idx, v| {
                if cell_idx == idx {
                    add += v;
                }
            }));
            if add > 0.0 {
                *by_net.entry(net).or_default() += add;
            }
        }
        let mut nets: Vec<(String, f64)> = by_net.into_iter().collect();
        nets.sort_by(|a, b| b.1.total_cmp(&a.1));
        let (i, j) = (idx % nx, idx / nx);
        worst.push(Cell {
            x: round3(bx0 + (i as f64 + 0.5) * cell),
            y: round3(by0 + (j as f64 + 0.5) * cell),
            demand_mm: round3(d),
            supply_mm: round3(supply[idx]),
            utilization: round3(util),
            nets: nets.into_iter().take(3).map(|n| n.0).collect(),
        });
    }
    Congestion {
        cell_mm: cell,
        cells,
        overflow_cells,
        overflow_fraction: if cells > 0 { round3(overflow_cells as f64 / cells as f64) } else { 0.0 },
        total_overflow: if total_supply > 0.0 { round3(total_overflow / total_supply) } else { 0.0 },
        max_utilization: round3(max_util),
        blocked_cells_with_demand: blocked_with_demand,
        worst_cells: worst,
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Density {
    /// Wd: Σ Manhattan airwire length, weighted by each net's pitch over the
    /// default pitch (mm).
    pub demand_mm: f64,
    /// Σ over signal layers of free area / pitch (mm of track), before E.
    pub raw_capacity_mm: f64,
    pub per_layer_capacity_mm: BTreeMap<String, f64>,
    pub layout_efficiency: f64,
    /// Wc = raw capacity × E.
    pub capacity_mm: f64,
    /// U = Wd / Wc.
    pub utilization: f64,
    /// Wd / raw capacity: U at E = 1, the number E is calibrated against.
    pub raw_utilization: f64,
    /// Existing track on the signal layers (all nets), for calibration.
    pub existing_track_mm: f64,
}

pub(crate) fn density(board: &Board, airwires: &[Airwire], pitch: &[f64], efficiency: f64) -> Density {
    let p0 = board.rules.pitch();
    let demand: f64 = airwires.iter().zip(pitch).map(|(w, p)| w.manhattan() * p / p0).sum();
    let mut per_layer = BTreeMap::new();
    let mut raw = 0.0;
    for (l, name) in board.layers.iter().enumerate() {
        let c = board.raster.total_free_area(l) / p0;
        raw += c;
        per_layer.insert(name.clone(), round3(c));
    }
    let existing: f64 = board
        .tracks
        .iter()
        .filter(|t| board.layers.contains(&t.layer))
        .map(|t| (t.a.0 - t.b.0).hypot(t.a.1 - t.b.1))
        .sum();
    let capacity = raw * efficiency;
    Density {
        demand_mm: round3(demand),
        raw_capacity_mm: round3(raw),
        per_layer_capacity_mm: per_layer,
        layout_efficiency: efficiency,
        capacity_mm: round3(capacity),
        utilization: if capacity > 0.0 { round3(demand / capacity) } else { f64::INFINITY },
        raw_utilization: if raw > 0.0 { round3(demand / raw) } else { f64::INFINITY },
        existing_track_mm: round3(existing),
    }
}
