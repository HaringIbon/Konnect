//! The board's copper/dielectric stack and the transmission-line model of
//! each copper layer, for impedance targets and checks.
//!
//! The stack comes from the board's `(setup (stackup …))` when it has one,
//! else from the design intent's stackup and the board thickness. A layer's
//! model depends on which neighbouring copper layers are reference planes:
//! an outer layer over a plane is a microstrip; an inner layer between two
//! planes a (possibly asymmetric) stripline. Anything else has no model and
//! callers must report it as unverified, never as a number.

use std::collections::BTreeSet;

use konnect_sexp::SexpNode;
use serde::Serialize;

use super::design_intent::Stackup;
use super::eecalc::{self, LineGeometry};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct Dielectric {
    pub thickness_mm: f64,
    pub er: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct LayerStack {
    /// Copper layers, top to bottom, with their thickness in mm.
    pub copper: Vec<(String, f64)>,
    /// `gaps[i]` lies between `copper[i]` and `copper[i + 1]`.
    pub gaps: Vec<Dielectric>,
    pub source: &'static str,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
#[serde(tag = "model")]
pub(crate) enum LineModel {
    Microstrip { h_mm: f64, t_mm: f64, er: f64 },
    /// Trace between two planes, `h1_mm` above and `h2_mm` below.
    Stripline { h1_mm: f64, h2_mm: f64, t_mm: f64, er: f64 },
}

/// One impedance evaluation.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct LineZ {
    pub z_ohm: f64,
    pub method: &'static str,
    /// False when the geometry is outside the formula's fitted range.
    pub in_range: bool,
    pub warnings: Vec<String>,
}

const OZ_MM: f64 = eecalc::MM_PER_OZ;

/// Copper layer names from the board's layer table, top to bottom.
pub(crate) fn copper_layer_names(tree: &SexpNode) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    if let Some(layers) = tree.find("layers") {
        for layer in layers.children().unwrap_or(&[]) {
            if let Some(name) = layer.get(1).and_then(|n| n.as_str()) {
                if name.ends_with(".Cu") {
                    names.push(name.to_string());
                }
            }
        }
    }
    let rank = |n: &str| match n {
        "F.Cu" => 0,
        "B.Cu" => 10_000,
        other => other.trim_start_matches("In").trim_end_matches(".Cu").parse::<i32>().unwrap_or(5_000),
    };
    names.sort_by_key(|n| rank(n));
    names.dedup();
    names
}

impl LayerStack {
    /// The stack the board itself declares, if it declares one with every
    /// dielectric between copper layers.
    pub(crate) fn from_board(tree: &SexpNode) -> Option<Self> {
        let stackup = tree.find("setup")?.find("stackup")?;
        let mut copper = Vec::new();
        let mut gaps = Vec::new();
        let mut pending: Option<(f64, f64)> = None; // (thickness, Σ t/εr)
        for layer in stackup.find_all("layer") {
            let kind = layer.find_str("type").unwrap_or("");
            let name = layer.get(1).and_then(|n| n.as_str()).unwrap_or("");
            if kind == "copper" {
                if !copper.is_empty() {
                    let (t, t_over_er) = pending.take()?;
                    // Series layers: the thickness-weighted harmonic mean εr.
                    gaps.push(Dielectric { thickness_mm: t, er: t / t_over_er });
                }
                copper.push((name.to_string(), layer.find_f64("thickness").unwrap_or(0.035)));
            } else if matches!(kind, "core" | "prepreg") && !copper.is_empty() {
                // Sub-layers (`addsublayer`) repeat thickness and epsilon_r.
                let thicknesses: Vec<f64> = layer.find_all("thickness").iter().filter_map(|n| n.get_f64(1)).collect();
                let ers: Vec<f64> = layer.find_all("epsilon_r").iter().filter_map(|n| n.get_f64(1)).collect();
                for (i, t) in thicknesses.iter().enumerate() {
                    let er = ers.get(i).or(ers.last()).copied().unwrap_or(4.5);
                    let entry = pending.get_or_insert((0.0, 0.0));
                    entry.0 += t;
                    entry.1 += t / er;
                }
            }
        }
        (copper.len() >= 2 && gaps.len() == copper.len() - 1).then_some(Self { copper, gaps, source: "board stackup" })
    }

    /// The intent's stack for a board with these copper layers: outer
    /// dielectrics from the intent, the core taking the rest of the board
    /// thickness.
    pub(crate) fn from_intent(stack: Stackup, names: &[String], board_thickness_mm: f64) -> Self {
        let n = names.len().max(2);
        let copper: Vec<(String, f64)> = names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let oz = if i == 0 || i == n - 1 { stack.outer_copper_oz } else { stack.inner_copper_oz };
                (name.clone(), oz * OZ_MM)
            })
            .collect();
        let copper_total: f64 = copper.iter().map(|c| c.1).sum();
        let mut gaps = Vec::new();
        if n == 2 {
            gaps.push(Dielectric { thickness_mm: (board_thickness_mm - copper_total).max(0.05), er: stack.er });
        } else {
            // n − 1 gaps: the two outer prepregs from the intent, and the
            // n − 3 inner ones sharing what is left of the board thickness.
            let inner = n - 3;
            let core = (board_thickness_mm - copper_total - 2.0 * stack.outer_dielectric_mm).max(0.05);
            gaps.push(Dielectric { thickness_mm: stack.outer_dielectric_mm, er: stack.er });
            for _ in 0..inner {
                gaps.push(Dielectric { thickness_mm: core / inner as f64, er: stack.er });
            }
            gaps.push(Dielectric { thickness_mm: stack.outer_dielectric_mm, er: stack.er });
        }
        Self { copper, gaps, source: "intent stackup and board thickness" }
    }

    /// The board's stack, else the intent's.
    pub(crate) fn for_board(tree: &SexpNode, stack: Stackup) -> Self {
        Self::from_board(tree).unwrap_or_else(|| {
            let names = copper_layer_names(tree);
            let names = if names.len() >= 2 { names } else { vec!["F.Cu".into(), "B.Cu".into()] };
            let thickness = tree.find("general").and_then(|g| g.find_f64("thickness")).unwrap_or(1.6);
            Self::from_intent(stack, &names, thickness)
        })
    }

    pub(crate) fn index(&self, layer: &str) -> Option<usize> {
        self.copper.iter().position(|(name, _)| name == layer)
    }

    pub(crate) fn is_outer(&self, layer: &str) -> bool {
        self.index(layer).is_some_and(|i| i == 0 || i + 1 == self.copper.len())
    }

    /// The model of `layer` given which copper layers are reference planes.
    pub(crate) fn model(&self, layer: &str, planes: &BTreeSet<String>) -> Result<LineModel, String> {
        let i = self.index(layer).ok_or_else(|| format!("{layer} is not a copper layer of this board"))?;
        let last = self.copper.len() - 1;
        let t = self.copper[i].1;
        let plane = |j: usize| planes.contains(&self.copper[j].0);
        if i == 0 || i == last {
            let (j, gap) = if i == 0 { (1, &self.gaps[0]) } else { (last - 1, &self.gaps[last - 1]) };
            return if plane(j) {
                Ok(LineModel::Microstrip { h_mm: gap.thickness_mm, t_mm: t, er: gap.er })
            } else {
                Err(format!("{layer}: the adjacent layer {} is not a reference plane", self.copper[j].0))
            };
        }
        if plane(i - 1) && plane(i + 1) {
            let (above, below) = (&self.gaps[i - 1], &self.gaps[i]);
            let er = (above.thickness_mm + below.thickness_mm)
                / (above.thickness_mm / above.er + below.thickness_mm / below.er);
            Ok(LineModel::Stripline { h1_mm: above.thickness_mm, h2_mm: below.thickness_mm, t_mm: t, er })
        } else {
            Err(format!("{layer}: an inner layer needs reference planes on both sides (embedded microstrip is not modelled)"))
        }
    }
}

impl LineModel {
    /// Z0 (or Zdiff with `gap_mm`) of a trace `width_mm` wide.
    pub(crate) fn impedance(&self, width_mm: f64, gap_mm: Option<f64>) -> LineZ {
        let pick = |c: &eecalc::Calc<eecalc::Impedance>| {
            if gap_mm.is_some() { c.result.zdiff_ohm.unwrap_or(c.result.z0_ohm) } else { c.result.z0_ohm }
        };
        match *self {
            LineModel::Microstrip { h_mm, t_mm, er } => {
                let c = eecalc::microstrip(LineGeometry { width_mm, height_mm: h_mm, thickness_mm: t_mm, er, gap_mm });
                LineZ { z_ohm: pick(&c), method: c.method, in_range: c.warnings.is_empty(), warnings: c.warnings }
            }
            LineModel::Stripline { h1_mm, h2_mm, t_mm, er } => {
                // IPC-2141A asymmetric stripline: the parallel combination of
                // the symmetric lines with the trace centred at each spacing.
                let line = |h: f64| eecalc::stripline(LineGeometry { width_mm, height_mm: 2.0 * h + t_mm, thickness_mm: t_mm, er, gap_mm });
                let (a, b) = (line(h1_mm), line(h2_mm));
                let (za, zb) = (pick(&a), pick(&b));
                let z = if (h1_mm - h2_mm).abs() < 1e-9 { za } else { 2.0 * za * zb / (za + zb) };
                let mut warnings = a.warnings;
                for w in b.warnings {
                    if !warnings.contains(&w) {
                        warnings.push(w);
                    }
                }
                LineZ {
                    z_ohm: (z * 100.0).round() / 100.0,
                    method: if (h1_mm - h2_mm).abs() < 1e-9 {
                        "IPC-2141A symmetric stripline"
                    } else {
                        "IPC-2141A asymmetric stripline: 2·Za·Zb/(Za+Zb) of the symmetric lines at 2h1+t and 2h2+t"
                    },
                    in_range: warnings.is_empty(),
                    warnings,
                }
            }
        }
    }

    /// The width giving `target_ohm`, by bisection (impedance falls with width).
    pub(crate) fn width_for(&self, target_ohm: f64, gap_mm: Option<f64>) -> Option<f64> {
        let (mut lo, mut hi) = (0.01_f64, 5.0_f64);
        let z = |w: f64| self.impedance(w, gap_mm).z_ohm;
        if z(lo) < target_ohm || z(hi) > target_ohm {
            return None;
        }
        for _ in 0..60 {
            let mid = 0.5 * (lo + hi);
            if z(mid) > target_ohm { lo = mid } else { hi = mid }
        }
        Some((0.5 * (lo + hi) * 10_000.0).round() / 10_000.0)
    }
}

/// Width bounds on one layer for an impedance target within `tol` (0.1 = ±10 %).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct LayerWidth {
    pub layer: String,
    pub min_mm: f64,
    pub opt_mm: f64,
    pub max_mm: f64,
    pub model: LineModel,
    pub method: &'static str,
    pub in_range: bool,
}

pub(crate) fn width_bounds(model: LineModel, layer: &str, target: f64, tol: f64, gap: Option<f64>) -> Option<LayerWidth> {
    // Higher impedance is narrower: the minimum width is the +tol bound.
    let opt = model.width_for(target, gap)?;
    let min = model.width_for(target * (1.0 + tol), gap)?;
    let max = model.width_for(target * (1.0 - tol), gap)?;
    let z = model.impedance(opt, gap);
    Some(LayerWidth { layer: layer.to_string(), min_mm: min, opt_mm: opt, max_mm: max, model, method: z.method, in_range: z.in_range })
}

#[cfg(test)]
mod tests {
    use super::*;

    const VIDEO_STACKUP: &str = r#"(kicad_pcb (general (thickness 1.6)) (layers (0 "F.Cu" signal) (1 "In1.Cu" signal) (2 "In2.Cu" signal) (31 "B.Cu" signal))
      (setup (stackup
      (layer "F.Mask" (type "Top Solder Mask") (thickness 0.01))
      (layer "F.Cu" (type "copper") (thickness 0.035))
      (layer "dielectric 1" (type "core") (thickness 0.480066) (material "FR4") (epsilon_r 4.5) (loss_tangent 0.02))
      (layer "In1.Cu" (type "copper") (thickness 0.035))
      (layer "dielectric 2" (type "prepreg") (thickness 0.2) (material "FR4") (epsilon_r 4.0) (loss_tangent 0.02) addsublayer (thickness 0.2) (material "FR4") (epsilon_r 5.0) (loss_tangent 0.02))
      (layer "In2.Cu" (type "copper") (thickness 0.035))
      (layer "dielectric 3" (type "core") (thickness 0.480066) (material "FR4") (epsilon_r 4.5) (loss_tangent 0.02))
      (layer "B.Cu" (type "copper") (thickness 0.035))
      (layer "B.Mask" (type "Bottom Solder Mask") (thickness 0.01)))))"#;

    #[test]
    fn board_stackup_is_read_with_sublayers() {
        let tree = konnect_sexp::parse_sexp(VIDEO_STACKUP).unwrap();
        let s = LayerStack::from_board(&tree).unwrap();
        assert_eq!(s.copper.iter().map(|c| c.0.as_str()).collect::<Vec<_>>(), ["F.Cu", "In1.Cu", "In2.Cu", "B.Cu"]);
        assert!((s.gaps[0].thickness_mm - 0.480066).abs() < 1e-9);
        // 0.2 mm of εr 4 in series with 0.2 mm of εr 5: 0.4 / (0.05 + 0.04).
        assert!((s.gaps[1].thickness_mm - 0.4).abs() < 1e-9);
        assert!((s.gaps[1].er - 0.4 / 0.09).abs() < 1e-9);
    }

    #[test]
    fn models_follow_the_reference_planes() {
        let tree = konnect_sexp::parse_sexp(VIDEO_STACKUP).unwrap();
        let s = LayerStack::from_board(&tree).unwrap();
        let planes: BTreeSet<String> = ["In1.Cu".to_string(), "In2.Cu".to_string()].into();
        assert!(matches!(s.model("F.Cu", &planes), Ok(LineModel::Microstrip { .. })));
        assert!(s.model("In1.Cu", &planes).is_err(), "In1 is itself a plane, between F.Cu and In2");
        let none = BTreeSet::new();
        assert!(s.model("F.Cu", &none).is_err(), "no plane: no number");
        let six: BTreeSet<String> = ["F.Cu".to_string(), "In2.Cu".to_string()].into();
        assert!(matches!(s.model("In1.Cu", &six), Ok(LineModel::Stripline { .. })));
    }

    #[test]
    fn intent_fallback_fills_the_board_thickness() {
        let names: Vec<String> = ["F.Cu", "In1.Cu", "In2.Cu", "B.Cu"].iter().map(|s| s.to_string()).collect();
        let s = LayerStack::from_intent(Stackup::default(), &names, 1.6);
        assert_eq!(s.gaps.len(), 3);
        assert!((s.gaps[0].thickness_mm - 0.2104).abs() < 1e-9);
        let total: f64 = s.gaps.iter().map(|g| g.thickness_mm).sum::<f64>() + s.copper.iter().map(|c| c.1).sum::<f64>();
        assert!((total - 1.6).abs() < 1e-6, "{total}");
        let two = LayerStack::from_intent(Stackup::default(), &names[..1].iter().chain(&names[3..]).cloned().collect::<Vec<_>>(), 1.6);
        assert_eq!(two.gaps.len(), 1);
    }

    #[test]
    fn asymmetric_stripline_reduces_to_symmetric_and_lies_between() {
        let sym = LineModel::Stripline { h1_mm: 0.2, h2_mm: 0.2, t_mm: 0.0175, er: 4.4 };
        let direct = eecalc::stripline(LineGeometry { width_mm: 0.1, height_mm: 0.4175, thickness_mm: 0.0175, er: 4.4, gap_mm: None });
        assert!((sym.impedance(0.1, None).z_ohm - direct.result.z0_ohm).abs() < 0.01);
        let asym = LineModel::Stripline { h1_mm: 0.15, h2_mm: 0.3, t_mm: 0.0175, er: 4.4 };
        let near = LineModel::Stripline { h1_mm: 0.15, h2_mm: 0.15, t_mm: 0.0175, er: 4.4 }.impedance(0.1, None).z_ohm;
        let far = LineModel::Stripline { h1_mm: 0.3, h2_mm: 0.3, t_mm: 0.0175, er: 4.4 }.impedance(0.1, None).z_ohm;
        let z = asym.impedance(0.1, None).z_ohm;
        assert!(near < z && z < far, "{near} < {z} < {far}");
    }

    #[test]
    fn width_bounds_bracket_the_target_in_impedance() {
        let m = LineModel::Microstrip { h_mm: 0.2104, t_mm: 0.035, er: 4.4 };
        let b = width_bounds(m, "F.Cu", 50.0, 0.1, None).unwrap();
        assert!(b.min_mm < b.opt_mm && b.opt_mm < b.max_mm);
        assert!((m.impedance(b.min_mm, None).z_ohm - 55.0).abs() < 0.1);
        assert!((m.impedance(b.max_mm, None).z_ohm - 45.0).abs() < 0.1);
        assert!((m.impedance(b.opt_mm, None).z_ohm - 50.0).abs() < 0.1);
    }
}
