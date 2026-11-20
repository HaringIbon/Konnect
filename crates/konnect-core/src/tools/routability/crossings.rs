//! Airwire crossings and a layer/via estimate from them.
//!
//! Reusable on purpose: the Tier 3 pin-swap optimiser scores a candidate pin
//! order by re-running [`airwire_crossings`] and [`plan_layers`] on moved
//! airwire end points.
//!
//! Why crossings and not a planarity bound: the airwire graph is a forest
//! (a spanning tree per net), and every forest is planar, so graph thickness
//! and crossing-number bounds say nothing about it (`wikipedia-graph-thickness`,
//! `wikipedia-crossing-number`). The useful object is the *conflict graph*:
//! one node per airwire, an edge where two airwires of different nets cross.
//! Two crossing airwires cannot both run straight on one layer; colouring the
//! conflict graph gives a layer estimate. Crossing counts between airwires as
//! a placement-quality signal: `arxiv-2210-14259-ns-place`.

use super::geom::{proper_intersection, Pt};
use serde::Serialize;

/// The copper layers an airwire end sits on: one layer for an SMD pad or a
/// track end, every layer for a through-hole pad or a via.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EndLayers {
    All,
    One(String),
}

impl EndLayers {
    pub(crate) fn on(&self, layer: &str) -> bool {
        match self {
            EndLayers::All => true,
            EndLayers::One(own) => own == layer,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct AirwireEnd {
    pub at: Pt,
    pub layers: EndLayers,
    /// `(reference, pad number)` when the end is a footprint pad.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pad: Option<(String, String)>,
}

/// One unrouted connection (a ratsnest edge) between two copper items of a net.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Airwire {
    pub net: String,
    pub a: AirwireEnd,
    pub b: AirwireEnd,
}

impl Airwire {
    pub(crate) fn manhattan(&self) -> f64 {
        (self.a.at.0 - self.b.at.0).abs() + (self.a.at.1 - self.b.at.1).abs()
    }
    pub(crate) fn length(&self) -> f64 {
        (self.a.at.0 - self.b.at.0).hypot(self.a.at.1 - self.b.at.1)
    }
    /// Both ends are single-layer (SMD pads or track ends).
    pub(crate) fn smd_smd(&self) -> bool {
        matches!(self.a.layers, EndLayers::One(_)) && matches!(self.b.layers, EndLayers::One(_))
    }
    /// Vias this airwire needs if it is routed entirely on `layer`: one per
    /// single-layer end on another layer.
    pub(crate) fn vias_on(&self, layer: &str) -> usize {
        usize::from(!self.a.layers.on(layer)) + usize::from(!self.b.layers.on(layer))
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Crossing {
    pub i: usize,
    pub j: usize,
    pub at: Pt,
}

/// Every proper crossing between airwires of different nets (pairwise; the
/// boards this serves have well under 5 000 airwires).
pub(crate) fn airwire_crossings(airwires: &[Airwire]) -> Vec<Crossing> {
    let boxes: Vec<(f64, f64, f64, f64)> = airwires
        .iter()
        .map(|w| (w.a.at.0.min(w.b.at.0), w.a.at.1.min(w.b.at.1), w.a.at.0.max(w.b.at.0), w.a.at.1.max(w.b.at.1)))
        .collect();
    let mut out = Vec::new();
    for i in 0..airwires.len() {
        for j in i + 1..airwires.len() {
            if airwires[i].net == airwires[j].net {
                continue;
            }
            let (a, b) = (boxes[i], boxes[j]);
            if a.2 < b.0 || b.2 < a.0 || a.3 < b.1 || b.3 < a.1 {
                continue;
            }
            if let Some(at) = proper_intersection(airwires[i].a.at, airwires[i].b.at, airwires[j].a.at, airwires[j].b.at) {
                out.push(Crossing { i, j, at });
            }
        }
    }
    out
}

/// A straight piece of existing copper (a track or a sampled arc).
#[derive(Debug, Clone)]
pub(crate) struct TrackSeg {
    pub a: Pt,
    pub b: Pt,
    pub layer: String,
    pub net: Option<String>,
}

/// For each airwire and each of `layers`: does its straight line cross
/// existing copper of another net on that layer? A crossed layer is not
/// lost, but the route there must detour or hop.
pub(crate) fn track_blocks(airwires: &[Airwire], tracks: &[TrackSeg], layers: &[String]) -> Vec<Vec<usize>> {
    airwires
        .iter()
        .map(|w| {
            layers
                .iter()
                .map(|layer| {
                    tracks
                        .iter()
                        .filter(|t| &t.layer == layer && t.net.as_deref() != Some(w.net.as_str()))
                        .filter(|t| proper_intersection(w.a.at, w.b.at, t.a, t.b).is_some())
                        .count()
                })
                .collect()
        })
        .collect()
}

/// DSatur greedy colouring (Brélaz): repeatedly colour the uncoloured node
/// with the most distinct neighbour colours (ties: highest degree) with the
/// smallest colour free among its neighbours. An upper bound on the
/// chromatic number, usually tight on sparse graphs.
pub(crate) fn dsatur(n: usize, edges: &[(usize, usize)]) -> Vec<usize> {
    let mut adjacency = vec![Vec::new(); n];
    for &(a, b) in edges {
        adjacency[a].push(b);
        adjacency[b].push(a);
    }
    let mut colour: Vec<Option<usize>> = vec![None; n];
    let mut neighbour_colours: Vec<std::collections::BTreeSet<usize>> = vec![Default::default(); n];
    for _ in 0..n {
        let next = (0..n)
            .filter(|&v| colour[v].is_none())
            .max_by_key(|&v| (neighbour_colours[v].len(), adjacency[v].len(), std::cmp::Reverse(v)))
            .expect("an uncoloured node remains");
        let c = (0..).find(|c| !neighbour_colours[next].contains(c)).expect("a free colour");
        colour[next] = Some(c);
        for &u in &adjacency[next] {
            neighbour_colours[u].insert(c);
        }
    }
    colour.into_iter().map(|c| c.unwrap_or(0)).collect()
}

/// Airwires assigned to signal layers from the conflict-graph colouring.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct LayerPlan {
    /// Colours DSatur needed: the straight-line layer estimate `k`.
    pub colours: usize,
    /// Index into the caller's `layers` for every airwire.
    pub layer_of: Vec<usize>,
    /// Vias from ends that sit on another layer than their airwire.
    pub end_vias: usize,
    /// Crossings left on one layer because there were more colours than
    /// layers; each needs one of the two airwires to hop (2 vias).
    pub unresolved_crossings: usize,
    /// `end_vias + 2 · unresolved_crossings` (judgement: 2 vias per hop).
    pub vias_estimate: usize,
}

/// Map colour classes to layers, minimising the via estimate. Exhaustive
/// when `layers^colours` is small, otherwise greedy by class size.
pub(crate) fn plan_layers(airwires: &[Airwire], crossings: &[Crossing], layers: &[String]) -> LayerPlan {
    let n = airwires.len();
    if layers.is_empty() || n == 0 {
        return LayerPlan { colours: 0, layer_of: vec![0; n], end_vias: 0, unresolved_crossings: 0, vias_estimate: 0 };
    }
    let edges: Vec<(usize, usize)> = crossings.iter().map(|c| (c.i, c.j)).collect();
    let colour = dsatur(n, &edges);
    let k = colour.iter().max().map_or(0, |c| c + 1);
    // Cost of putting class c on layer l (end vias), and class pairs that cross.
    let mut end_cost = vec![vec![0usize; layers.len()]; k];
    for (w, &c) in airwires.iter().zip(&colour) {
        for (l, layer) in layers.iter().enumerate() {
            end_cost[c][l] += w.vias_on(layer);
        }
    }
    let mut pair = vec![vec![0usize; k]; k];
    for &(a, b) in &edges {
        let (ca, cb) = (colour[a], colour[b]);
        pair[ca.min(cb)][ca.max(cb)] += 1;
    }
    let cost = |assign: &[usize]| -> (usize, usize) {
        let ends: usize = (0..k).map(|c| end_cost[c][assign[c]]).sum();
        let mut unresolved = 0;
        for a in 0..k {
            for b in a + 1..k {
                if assign[a] == assign[b] {
                    unresolved += pair[a][b];
                }
            }
        }
        (ends, unresolved)
    };
    // Cheapest first; on a tie, fewer crossings left on one layer.
    let total = |(e, u): (usize, usize)| (e + 2 * u, u);
    let combos = (layers.len() as f64).powi(k as i32);
    let mut best: Vec<usize> = vec![0; k];
    if combos <= 200_000.0 {
        let mut assign = vec![0usize; k];
        let mut best_cost = (usize::MAX, usize::MAX);
        loop {
            let c = total(cost(&assign));
            if c < best_cost {
                best_cost = c;
                best = assign.clone();
            }
            // Next combination (odometer).
            let mut i = 0;
            loop {
                if i == k {
                    break;
                }
                assign[i] += 1;
                if assign[i] < layers.len() {
                    break;
                }
                assign[i] = 0;
                i += 1;
            }
            if i == k {
                break;
            }
        }
    } else {
        let mut size: Vec<(usize, usize)> = (0..k).map(|c| (colour.iter().filter(|&&x| x == c).count(), c)).collect();
        size.sort_by(|a, b| b.cmp(a));
        let mut placed: Vec<Option<usize>> = vec![None; k];
        for &(_, c) in &size {
            let l = (0..layers.len())
                .min_by_key(|&l| {
                    let clash: usize = (0..k)
                        .filter(|&o| placed[o] == Some(l))
                        .map(|o| pair[c.min(o)][c.max(o)])
                        .sum();
                    end_cost[c][l] + 2 * clash
                })
                .unwrap_or(0);
            placed[c] = Some(l);
        }
        best = placed.into_iter().map(|l| l.unwrap_or(0)).collect();
    }
    let (end_vias, unresolved_crossings) = cost(&best);
    LayerPlan {
        colours: k,
        layer_of: colour.iter().map(|&c| best[c]).collect(),
        end_vias,
        unresolved_crossings,
        vias_estimate: end_vias + 2 * unresolved_crossings,
    }
}
