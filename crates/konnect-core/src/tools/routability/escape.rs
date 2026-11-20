//! Pin-escape capacity of one IC: how many traces leave each side, how many
//! the side can carry, and how many signal layers the escape needs.
//!
//! Reusable on purpose: the Tier 3 pin-swap optimiser re-scores a candidate
//! pin order with [`analyse_package`].
//!
//! Sources (doc DB):
//! - `wang-iccad06-escape-layer-minimization`: grid-channel capacity
//!   `C = ⌊(p − d − s)/(w + s)⌋` and diagonal-channel capacity
//!   `D = ⌊(√2·p − d − s)/(w + s)⌋` (the text extraction drops the radical;
//!   the PDF in `docs/raw` and both worked examples — 225/100/25/25 gives
//!   C = 2, D = 3; 350/200/50/50 gives C = 1, D = 2 — confirm √2·p). For a
//!   full n×n array the first-layer bottleneck is the 4(n − 1) boundary
//!   channels.
//! - `intel-an114-bga-board-design`: the same per-channel trace count. Its
//!   3-trace table row is inconsistent with its own rule, so the general
//!   formula is used.
//! - `xilinx-ug1099-bga-design-rules`, Eq. 2-1 (layers ≈ signals / (routing
//!   channels × routes per channel), channels = 4(n − 1)) and the row rule:
//!   "If one signal escapes between adjacent pads, then one signal row can be
//!   routed on a single metal layer. The exception to this is the outermost
//!   row, which allows two routes per layer." So with C routes per channel a
//!   first layer escapes C + 1 rows and each further layer C rows:
//!   `layers = 1 + ⌈max(0, R − (C + 1))/C⌉` — an upper bound, which suits
//!   Freerouting (it does not optimise escape order).

use super::geom::Pt;
use serde::Serialize;

/// Wang's grid-channel capacity: traces between two adjacent pads whose
/// edges are `gap` apart (`gap = p − d`).
pub(crate) fn channel_capacity(gap: f64, w: f64, s: f64) -> usize {
    let c = ((gap - s) / (w + s) + 1e-9).floor();
    if c > 0.0 { c as usize } else { 0 }
}

/// Wang's diagonal-channel capacity for pitch `p` and pad size `d`.
pub(crate) fn diagonal_capacity(p: f64, d: f64, w: f64, s: f64) -> usize {
    channel_capacity(std::f64::consts::SQRT_2 * p - d, w, s)
}

/// Signal layers to escape `rows` rows of an array with `c` traces per
/// channel (UG1099 row rule). `None` when inner rows cannot escape at all
/// (`c = 0` and more than one row).
pub(crate) fn array_layers_needed(rows: usize, c: usize) -> Option<usize> {
    if rows == 0 {
        return Some(0);
    }
    if rows <= c + 1 {
        return Some(1);
    }
    if c == 0 {
        return None;
    }
    Some(1 + (rows - (c + 1)).div_ceil(c))
}

/// UG1099 Eq. 2-1: signals / (4(n − 1) channels × routes per channel).
pub(crate) fn ug1099_layers(signals: usize, n: usize, routes_per_channel: usize) -> Option<usize> {
    let channels = 4 * n.saturating_sub(1) * routes_per_channel;
    (channels > 0).then(|| signals.div_ceil(channels))
}

/// One pad of the package in the footprint's own frame.
#[derive(Debug, Clone)]
pub(crate) struct PackagePad {
    pub number: String,
    pub net: Option<String>,
    /// Centre in the footprint frame (rotation removed).
    pub local: Pt,
    /// Half extents along the footprint's x and y.
    pub half: (f64, f64),
    /// The pad's net needs routing (not a plane net, not single-pad, in the group).
    pub demand: bool,
    pub round: bool,
    /// The pad's net track width and clearance, when known (else the
    /// package's `w`, `s`).
    pub rule: Option<(f64, f64)>,
}

#[derive(Debug, Clone)]
pub(crate) struct Package {
    pub reference: String,
    pub pads: Vec<PackagePad>,
    /// Footprint rotation (degrees), used only to name sides on the board.
    pub rotation: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum LocalSide {
    West,
    East,
    North,
    South,
}

impl LocalSide {
    fn normal(self) -> Pt {
        match self {
            LocalSide::West => (-1.0, 0.0),
            LocalSide::East => (1.0, 0.0),
            LocalSide::North => (0.0, -1.0),
            LocalSide::South => (0.0, 1.0),
        }
    }
}

/// The board compass direction (KiCad y grows down) of a local outward normal.
fn board_direction(normal: Pt, rotation: f64) -> &'static str {
    let (x, y) = konnect_sexp::geometry::transform_pad(normal.0, normal.1, 0.0, 0.0, rotation);
    if x.abs() >= y.abs() {
        if x > 0.0 { "east" } else { "west" }
    } else if y > 0.0 {
        "south"
    } else {
        "north"
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct NetPad {
    pub net: String,
    pub pad: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SideReport {
    /// Direction the side faces on the board.
    pub faces: &'static str,
    pub pads: usize,
    pub demand_pads: usize,
    pub pitch_mm: f64,
    pub pad_width_mm: f64,
    /// Wang's C for the regular gap of this side.
    pub channel_capacity: usize,
    /// Σ C over every gap of the side (wider gaps where pins are missing).
    pub channel_total: usize,
    /// Interior nets that escape through this side's channels.
    pub interior_assigned: usize,
    /// `interior_assigned / channel_total` (0 when nothing is assigned).
    pub channel_utilization: f64,
    /// Every two neighbouring pads that both need a trace are far enough
    /// apart for both traces at their class widths; `false` means they must
    /// neck down at the pads (allowed in the breakout area, UG1099).
    pub fanout_pitch_ok: bool,
    /// The neighbouring pads that both need a trace with the least room:
    /// their centre distance, and what their two traces need.
    pub tightest_demand_pitch_mm: Option<f64>,
    pub needed_pitch_mm: Option<f64>,
    /// Demand pads whose other end lies behind the package (past its far side).
    pub behind: Vec<NetPad>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct EscapeReport {
    pub reference: String,
    /// `perimeter` (QFN/QFP/SOIC: pads on the outline, optional centre pad)
    /// or `grid` (BGA-like array).
    pub kind: &'static str,
    pub pads: usize,
    pub demand_pads: usize,
    pub sides: Vec<SideReport>,
    /// Interior (centre / inner-row) pads with a net to route.
    pub interior_demand: Vec<NetPad>,
    /// Interior nets that found no channel: they need a via inside the ring.
    pub interior_needs_via: Vec<NetPad>,
    /// Ring pads whose other end is inside the ring (a part underneath, the
    /// centre pad) while the ring has no channel (C = 0): the route needs a
    /// via outside the ring and another layer under it.
    pub via_outside_ring: Vec<NetPad>,
    /// Interior nets and ring pads that need a via only at the class widths
    /// (with fab-minimum traces they fit a channel).
    pub interior_needs_via_at_class_rules_only: Vec<NetPad>,
    pub via_outside_ring_at_class_rules_only: Vec<NetPad>,
    /// Grid arrays: rows holding signal pads, C and D (Wang), and UG1099 Eq. 2-1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grid: Option<GridReport>,
    /// Signal layers the escape needs with traces necked to the fab minimum
    /// in the breakout; `None` when it cannot escape even then.
    pub layers_needed: Option<usize>,
    /// The same at the net classes' widths and clearances.
    pub layers_needed_at_class_rules: Option<usize>,
    /// The class widths need more layers than necked-down traces do.
    pub needs_neck_down: bool,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct GridReport {
    pub columns: usize,
    pub rows: usize,
    pub pitch_mm: f64,
    pub pad_mm: f64,
    pub signal_rows: usize,
    pub c: usize,
    pub d: usize,
    /// C with fab-minimum traces in the breakout.
    pub c_at_fab_minimum: usize,
    pub boundary_channels: usize,
    pub ug1099_layers: Option<usize>,
}

fn median(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn cluster(values: &[f64], tol: f64) -> Vec<f64> {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mut out: Vec<f64> = Vec::new();
    for v in sorted {
        if out.last().is_none_or(|last| v - last > tol) {
            out.push(v);
        }
    }
    out
}

fn net_pad(pad: &PackagePad) -> NetPad {
    NetPad { net: pad.net.clone().unwrap_or_default(), pad: pad.number.clone() }
}

/// Analyse one package. `destinations[i]` are the far ends (footprint frame)
/// of pad `i`'s demand airwires; `w`/`s` are the track width and clearance.
///
/// Doom is judged at the fabricator minimum `fab` (w, s): necking down in
/// the breakout area is allowed (`xilinx-ug1099-bga-design-rules`: "necking
/// down the trace width in the critical space between the BGA pads/vias (the
/// breakout area) is allowable"). The rest of the report is at the class
/// rules; `layers_needed_at_class_rules` and `needs_neck_down` say whether
/// the class widths alone would need more.
pub(crate) fn analyse_package(package: &Package, destinations: &[Vec<Pt>], w: f64, s: f64, fab: (f64, f64)) -> EscapeReport {
    let at = |w: f64, s: f64| analyse_grid(package, w, s).unwrap_or_else(|| analyse_perimeter(package, destinations, w, s));
    let mut report = at(w, s);
    let necked = at(fab.0.min(w), fab.1.min(s));
    report.layers_needed_at_class_rules = report.layers_needed;
    report.layers_needed = necked.layers_needed;
    // The via lists follow `layers_needed` (fab minimum); what only the
    // class widths would add is listed apart, under the neck-down note.
    let extra = |class: &[NetPad], necked: &[NetPad]| -> Vec<NetPad> { class.iter().filter(|c| !necked.contains(c)).cloned().collect() };
    report.interior_needs_via_at_class_rules_only = extra(&report.interior_needs_via, &necked.interior_needs_via);
    report.via_outside_ring_at_class_rules_only = extra(&report.via_outside_ring, &necked.via_outside_ring);
    report.interior_needs_via = necked.interior_needs_via.clone();
    report.via_outside_ring = necked.via_outside_ring.clone();
    report.needs_neck_down = report.layers_needed_at_class_rules != report.layers_needed;
    if let (Some(grid), Some(necked)) = (report.grid.as_mut(), necked.grid.as_ref()) {
        grid.c_at_fab_minimum = necked.c;
    }
    report
}

fn analyse_grid(package: &Package, w: f64, s: f64) -> Option<EscapeReport> {
    let pads = &package.pads;
    if pads.len() < 9 || pads.iter().filter(|p| p.round).count() * 5 < pads.len() * 4 {
        return None;
    }
    let xs = cluster(&pads.iter().map(|p| p.local.0).collect::<Vec<_>>(), 0.02);
    let ys = cluster(&pads.iter().map(|p| p.local.1).collect::<Vec<_>>(), 0.02);
    if xs.len() < 3 || ys.len() < 3 || pads.len() * 2 < xs.len() * ys.len() {
        return None;
    }
    let pitch = median(xs.windows(2).map(|w| w[1] - w[0]).chain(ys.windows(2).map(|w| w[1] - w[0])).collect());
    let index = |grid: &[f64], v: f64| grid.iter().position(|g| (g - v).abs() <= 0.02);
    let mut signal_rows = 0;
    for pad in pads.iter().filter(|p| p.demand) {
        let (Some(i), Some(j)) = (index(&xs, pad.local.0), index(&ys, pad.local.1)) else { continue };
        let ring = i.min(j).min(xs.len() - 1 - i).min(ys.len() - 1 - j);
        signal_rows = signal_rows.max(ring + 1);
    }
    let pad_mm = median(pads.iter().map(|p| 2.0 * p.half.0.max(p.half.1)).collect());
    let c = channel_capacity(pitch - pad_mm, w, s);
    let d = diagonal_capacity(pitch, pad_mm, w, s);
    let n = xs.len().max(ys.len());
    let demand = pads.iter().filter(|p| p.demand).count();
    let grid = GridReport {
        columns: xs.len(),
        rows: ys.len(),
        pitch_mm: round3(pitch),
        pad_mm: round3(pad_mm),
        signal_rows,
        c,
        d,
        c_at_fab_minimum: c,
        boundary_channels: 4 * (n - 1),
        ug1099_layers: ug1099_layers(demand, n, c.max(1)),
    };
    Some(EscapeReport {
        reference: package.reference.clone(),
        kind: "grid",
        pads: pads.len(),
        demand_pads: demand,
        sides: Vec::new(),
        interior_demand: Vec::new(),
        interior_needs_via: Vec::new(),
        via_outside_ring: Vec::new(),
        layers_needed: array_layers_needed(signal_rows, c),
        layers_needed_at_class_rules: None,
        needs_neck_down: false,
        interior_needs_via_at_class_rules_only: Vec::new(),
        via_outside_ring_at_class_rules_only: Vec::new(),
        grid: Some(grid),
    })
}

fn analyse_perimeter(package: &Package, destinations: &[Vec<Pt>], w: f64, s: f64) -> EscapeReport {
    let pads = &package.pads;
    let x0 = pads.iter().map(|p| p.local.0).fold(f64::INFINITY, f64::min);
    let x1 = pads.iter().map(|p| p.local.0).fold(f64::NEG_INFINITY, f64::max);
    let y0 = pads.iter().map(|p| p.local.1).fold(f64::INFINITY, f64::min);
    let y1 = pads.iter().map(|p| p.local.1).fold(f64::NEG_INFINITY, f64::max);
    let tol = 0.05;
    let side_of = |p: &PackagePad| -> Option<LocalSide> {
        let vertical = if (p.local.0 - x0).abs() <= tol {
            Some(LocalSide::West)
        } else if (p.local.0 - x1).abs() <= tol {
            Some(LocalSide::East)
        } else {
            None
        };
        let horizontal = if (p.local.1 - y0).abs() <= tol {
            Some(LocalSide::North)
        } else if (p.local.1 - y1).abs() <= tol {
            Some(LocalSide::South)
        } else {
            None
        };
        match (vertical, horizontal) {
            (Some(v), Some(h)) => Some(if p.half.0 >= p.half.1 { v } else { h }),
            (v, h) => v.or(h),
        }
    };
    let sides_of: Vec<Option<LocalSide>> = pads.iter().map(side_of).collect();
    // Outer extents of the pad ring, for "behind" and "inside the ring".
    let ring_x = (x0 - pads.iter().map(|p| p.half.0).fold(0.0, f64::max), x1 + pads.iter().map(|p| p.half.0).fold(0.0, f64::max));
    let ring_y = (y0 - pads.iter().map(|p| p.half.1).fold(0.0, f64::max), y1 + pads.iter().map(|p| p.half.1).fold(0.0, f64::max));
    let centre = ((ring_x.0 + ring_x.1) / 2.0, (ring_y.0 + ring_y.1) / 2.0);
    let half_extent = |side: LocalSide| match side {
        LocalSide::West | LocalSide::East => (ring_x.1 - ring_x.0) / 2.0,
        LocalSide::North | LocalSide::South => (ring_y.1 - ring_y.0) / 2.0,
    };
    // Inside the ring: strictly within the inner edges of the side pads.
    let inner = |p: Pt| {
        let ix0 = pads.iter().filter(|q| side_of(q) == Some(LocalSide::West)).map(|q| q.local.0 + q.half.0).fold(f64::NEG_INFINITY, f64::max);
        let ix1 = pads.iter().filter(|q| side_of(q) == Some(LocalSide::East)).map(|q| q.local.0 - q.half.0).fold(f64::INFINITY, f64::min);
        let iy0 = pads.iter().filter(|q| side_of(q) == Some(LocalSide::North)).map(|q| q.local.1 + q.half.1).fold(f64::NEG_INFINITY, f64::max);
        let iy1 = pads.iter().filter(|q| side_of(q) == Some(LocalSide::South)).map(|q| q.local.1 - q.half.1).fold(f64::INFINITY, f64::min);
        let (ix0, ix1) = (if ix0.is_finite() { ix0 } else { ring_x.0 }, if ix1.is_finite() { ix1 } else { ring_x.1 });
        let (iy0, iy1) = (if iy0.is_finite() { iy0 } else { ring_y.0 }, if iy1.is_finite() { iy1 } else { ring_y.1 });
        p.0 > ix0 && p.0 < ix1 && p.1 > iy0 && p.1 < iy1
    };

    struct SideWork {
        side: LocalSide,
        members: Vec<usize>,
        channel_capacity: usize,
        channel_total: usize,
        pitch: f64,
        width: f64,
        assigned: usize,
        demand_pitch: f64,
        needed_pitch: f64,
    }
    let mut work: Vec<SideWork> = Vec::new();
    for side in [LocalSide::North, LocalSide::East, LocalSide::South, LocalSide::West] {
        let mut members: Vec<usize> = (0..pads.len()).filter(|&i| sides_of[i] == Some(side)).collect();
        if members.is_empty() {
            continue;
        }
        let along = |i: usize| match side {
            LocalSide::West | LocalSide::East => pads[i].local.1,
            LocalSide::North | LocalSide::South => pads[i].local.0,
        };
        let half_along = |i: usize| match side {
            LocalSide::West | LocalSide::East => pads[i].half.1,
            LocalSide::North | LocalSide::South => pads[i].half.0,
        };
        members.sort_by(|&a, &b| along(a).total_cmp(&along(b)));
        let pitch = median(members.windows(2).map(|m| along(m[1]) - along(m[0])).collect());
        let width = median(members.iter().map(|&i| 2.0 * half_along(i)).collect());
        let gaps: Vec<usize> = members
            .windows(2)
            .map(|m| channel_capacity((along(m[1]) - half_along(m[1])) - (along(m[0]) + half_along(m[0])), w, s))
            .collect();
        // Neighbouring pads that both need a trace: their centre distance
        // against the pitch the two traces need (half of each width plus the
        // larger clearance). Reported as the smallest margin.
        let rule = |i: usize| pads[i].rule.unwrap_or((w, s));
        let (demand_pitch, needed_pitch) = members
            .windows(2)
            .filter(|m| pads[m[0]].demand && pads[m[1]].demand)
            .map(|m| {
                let ((w1, s1), (w2, s2)) = (rule(m[0]), rule(m[1]));
                (along(m[1]) - along(m[0]), (w1 + w2) / 2.0 + s1.max(s2))
            })
            .min_by(|a, b| (a.0 - a.1).total_cmp(&(b.0 - b.1)))
            .unwrap_or((f64::INFINITY, 0.0));
        work.push(SideWork {
            demand_pitch,
            needed_pitch,
            side,
            channel_capacity: channel_capacity(pitch - width, w, s),
            channel_total: gaps.iter().sum(),
            members,
            pitch,
            width,
            assigned: 0,
        });
    }

    // Interior demand: assign each net to the side facing its destination,
    // spilling to the side with most spare channels; leftovers need a via.
    let mut interior_demand = Vec::new();
    let mut interior_needs_via = Vec::new();
    for (i, pad) in pads.iter().enumerate() {
        if sides_of[i].is_some() || !pad.demand {
            continue;
        }
        interior_demand.push(net_pad(pad));
        let target = destinations.get(i).and_then(|d| d.first()).copied().unwrap_or(centre);
        let direction = (target.0 - pad.local.0, target.1 - pad.local.1);
        let mut order: Vec<usize> = (0..work.len()).collect();
        order.sort_by(|&a, &b| {
            let (na, nb) = (work[a].side.normal(), work[b].side.normal());
            (nb.0 * direction.0 + nb.1 * direction.1).total_cmp(&(na.0 * direction.0 + na.1 * direction.1))
        });
        match order.into_iter().find(|&k| work[k].assigned < work[k].channel_total) {
            Some(k) => work[k].assigned += 1,
            None => interior_needs_via.push(net_pad(pad)),
        }
    }

    let mut via_outside_ring = Vec::new();
    let mut sides = Vec::new();
    for side in &work {
        let normal = side.side.normal();
        let mut behind = Vec::new();
        for &i in &side.members {
            if !pads[i].demand {
                continue;
            }
            let far = destinations.get(i).map(Vec::as_slice).unwrap_or(&[]);
            for &dest in far {
                let projection = normal.0 * (dest.0 - centre.0) + normal.1 * (dest.1 - centre.1);
                if inner(dest) {
                    if side.channel_capacity == 0 {
                        via_outside_ring.push(net_pad(&pads[i]));
                    }
                } else if projection < -half_extent(side.side) {
                    behind.push(net_pad(&pads[i]));
                }
            }
        }
        sides.push(SideReport {
            faces: board_direction(normal, package.rotation),
            pads: side.members.len(),
            demand_pads: side.members.iter().filter(|&&i| pads[i].demand).count(),
            pitch_mm: round3(side.pitch),
            pad_width_mm: round3(side.width),
            channel_capacity: side.channel_capacity,
            channel_total: side.channel_total,
            interior_assigned: side.assigned,
            channel_utilization: if side.assigned == 0 {
                0.0
            } else if side.channel_total == 0 {
                f64::INFINITY
            } else {
                round3(side.assigned as f64 / side.channel_total as f64)
            },
            fanout_pitch_ok: side.demand_pitch + 1e-9 >= side.needed_pitch,
            tightest_demand_pitch_mm: side.demand_pitch.is_finite().then(|| round3(side.demand_pitch)),
            needed_pitch_mm: side.demand_pitch.is_finite().then(|| round3(side.needed_pitch)),
            behind,
        });
    }
    let needs_second_layer = !interior_needs_via.is_empty() || !via_outside_ring.is_empty();
    EscapeReport {
        reference: package.reference.clone(),
        kind: "perimeter",
        pads: pads.len(),
        demand_pads: pads.iter().filter(|p| p.demand).count(),
        sides,
        interior_demand,
        interior_needs_via,
        via_outside_ring,
        grid: None,
        layers_needed: Some(if pads.iter().any(|p| p.demand) { 1 + usize::from(needs_second_layer) } else { 0 }),
        layers_needed_at_class_rules: None,
        needs_neck_down: false,
        interior_needs_via_at_class_rules_only: Vec::new(),
        via_outside_ring_at_class_rules_only: Vec::new(),
    }
}

pub(crate) fn round3(v: f64) -> f64 {
    // `+ 0.0` turns a rounded -0.0 into 0.0.
    if v.is_finite() { (v * 1000.0).round() / 1000.0 + 0.0 } else { v }
}
