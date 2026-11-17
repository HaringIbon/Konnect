//! Design intent: what the design must do, stored per project in
//! `<project>.intent.json`, turned into KiCad rules, and checked on the board.
//!
//! DRC can only say "no rule broken". Intent says what was meant: this pair
//! is 90 Ω and matched within 1 mm, this rail carries 0.5 A and may drop
//! 100 mV, this capacitor sits within 2 mm of that pin. The calculators in
//! `eecalc` turn intent into numbers; `apply_design_intent` writes them into
//! net classes and `.kicad_dru` rules so KiCad's own DRC enforces what it
//! can; `check_design_intent` measures the rest on the board.
//!
//! All five tools are for the AI's own decisions: results are compact JSON
//! with the method, assumptions and limits behind every number.

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::board_source::{self, BoardRead, BoardSource};
use crate::tools::eecalc::{self, LineGeometry};
use crate::tools::fanout::{copper_pads, point_in_polygon, polygon_distance, sample_arc, BoardOutline, CopperPad};
use crate::tools::layer_stack::{width_bounds, LayerStack, LayerWidth};
use crate::tools::{ToolContext, ToolDef};
use anyhow::{bail, Context};
use konnect_sexp::SexpNode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::path::{Path, PathBuf};

pub const INTENT_SCHEMA: u32 = 1;
const RULE_PREFIX: &str = "intent: ";
const ROLES: &[&str] = &["signal", "diff_pair", "power", "clock", "sensitive", "plane"];

// ─── Schema ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DesignIntent {
    #[serde(default = "schema_version")]
    pub schema: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stackup: Option<Stackup>,
    #[serde(default)]
    pub nets: BTreeMap<String, NetIntent>,
    #[serde(default)]
    pub ties: Vec<Tie>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

fn schema_version() -> u32 {
    INTENT_SCHEMA
}

/// The board build the calculators assume. Defaults: JLCPCB JLC04161H-7628
/// (1 oz outer, 0.5 oz inner, 0.2104 mm 7628 prepreg, εr 4.4).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Stackup {
    #[serde(default = "one")]
    pub outer_copper_oz: f64,
    #[serde(default = "half")]
    pub inner_copper_oz: f64,
    /// Outer layer to its reference plane, mm.
    #[serde(default = "prepreg")]
    pub outer_dielectric_mm: f64,
    #[serde(default = "fr4_er")]
    pub er: f64,
}

fn one() -> f64 { 1.0 }
fn half() -> f64 { 0.5 }
fn prepreg() -> f64 { 0.2104 }
fn fr4_er() -> f64 { 4.4 }

impl Default for Stackup {
    fn default() -> Self {
        Self { outer_copper_oz: 1.0, inner_copper_oz: 0.5, outer_dielectric_mm: 0.2104, er: 4.4 }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct NetIntent {
    pub role: String,
    /// 1 is routed (and locked) first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,
    /// The other net of a differential pair.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pair: Option<String>,
    /// Differential impedance for a pair, single-ended otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impedance_ohm: Option<f64>,
    /// Edge-to-edge gap for a differential pair (default 0.15 mm).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap_mm: Option<f64>,
    /// Impedance tolerance, ± percent (default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub impedance_tolerance_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_a: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_temp_rise_c: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_drop_mv: Option<f64>,
    /// The pad that feeds the net, `REF.PAD` (e.g. `U10.3`), for voltage drop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_skew_mm: Option<f64>,
    /// Keep other nets at least this far away.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clearance_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// A part that must sit close to another part's pin: a decoupling capacitor,
/// a pull-up, a crystal, an ESD array.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Tie {
    pub part: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    pub max_mm: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

impl DesignIntent {
    fn validate(&self) -> Result<(), String> {
        let positive = |name: &str, value: Option<f64>| match value {
            Some(v) if !(v.is_finite() && v > 0.0) => Err(format!("{name} must be > 0")),
            _ => Ok(()),
        };
        for (net, intent) in &self.nets {
            if !ROLES.contains(&intent.role.as_str()) {
                return Err(format!("net '{net}': role must be one of {ROLES:?}"));
            }
            for (name, value) in [
                ("impedance_ohm", intent.impedance_ohm),
                ("gap_mm", intent.gap_mm),
                ("impedance_tolerance_pct", intent.impedance_tolerance_pct),
                ("current_a", intent.current_a),
                ("max_temp_rise_c", intent.max_temp_rise_c),
                ("max_drop_mv", intent.max_drop_mv),
                ("max_length_mm", intent.max_length_mm),
                ("max_skew_mm", intent.max_skew_mm),
                ("clearance_mm", intent.clearance_mm),
            ] {
                positive(&format!("net '{net}': {name}"), value)?;
            }
            if intent.role == "diff_pair" {
                let Some(partner) = &intent.pair else {
                    return Err(format!("net '{net}': a diff_pair needs `pair`"));
                };
                match self.nets.get(partner) {
                    Some(other) if other.role == "diff_pair" && other.pair.as_deref() == Some(net) => {}
                    _ => return Err(format!("net '{net}': its pair '{partner}' must also be a diff_pair naming '{net}'")),
                }
            }
            if let Some(source) = &intent.source {
                if source.split_once('.').is_none_or(|(r, p)| r.is_empty() || p.is_empty()) {
                    return Err(format!("net '{net}': source must be REF.PAD, e.g. U10.3"));
                }
            }
        }
        for tie in &self.ties {
            if tie.part.is_empty() || tie.to.is_empty() || !(tie.max_mm.is_finite() && tie.max_mm > 0.0) {
                return Err(format!("tie {} → {}: part, to and max_mm > 0 are required", tie.part, tie.to));
            }
        }
        if let Some(s) = self.stackup {
            for (name, v) in [("outer_copper_oz", s.outer_copper_oz), ("inner_copper_oz", s.inner_copper_oz), ("outer_dielectric_mm", s.outer_dielectric_mm), ("er", s.er)] {
                positive(&format!("stackup {name}"), Some(v))?;
            }
        }
        Ok(())
    }

    fn stackup(&self) -> Stackup {
        self.stackup.unwrap_or_default()
    }
}

// ─── Files ──────────────────────────────────────────────────────────────────

/// `<stem>` from a `.kicad_pro`, `.kicad_pcb` or `.kicad_sch` path, or a
/// directory holding exactly one `.kicad_pro`.
fn project_stem(path: &Path) -> anyhow::Result<PathBuf> {
    if path.is_dir() {
        let projects = std::fs::read_dir(path)?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "kicad_pro"))
            .collect::<Vec<_>>();
        match projects.as_slice() {
            [one] => return Ok(one.with_extension("")),
            [] => bail!("{} holds no .kicad_pro", path.display()),
            _ => bail!("{} holds several .kicad_pro files; name one", path.display()),
        }
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some("kicad_pro" | "kicad_pcb" | "kicad_sch") => Ok(path.with_extension("")),
        _ => bail!("project must be a .kicad_pro, .kicad_pcb, .kicad_sch or a project directory"),
    }
}

fn intent_path(stem: &Path) -> PathBuf {
    PathBuf::from(format!("{}.intent.json", stem.display()))
}

fn load_intent(stem: &Path) -> anyhow::Result<(DesignIntent, Option<String>)> {
    let path = intent_path(stem);
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let intent: DesignIntent = serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
            if intent.schema != INTENT_SCHEMA {
                bail!("{} is intent schema {}, this server reads {INTENT_SCHEMA}", path.display(), intent.schema);
            }
            Ok((intent, Some(text)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((DesignIntent { schema: INTENT_SCHEMA, ..Default::default() }, None)),
        Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
    }
}

fn conflict(path: &Path, what: &str) -> CallToolResult {
    CallToolResult::error_kind(
        ToolErrorKind::Conflict { paths: vec![path.display().to_string()] },
        format!("{} changed after Konnect read it, so {what} wrote nothing; retry.", path.display()),
    )
}

/// Write `content` only if the file still holds `expected` (or is still
/// absent when `expected` is `None`).
fn write_checked(path: &Path, expected: Option<&str>, content: &str, what: &str) -> anyhow::Result<Option<CallToolResult>> {
    let result = match expected {
        Some(expected) => konnect_sexp::write_atomic_if_unchanged(path, expected, content),
        None => konnect_sexp::write_new_atomic(path, content),
    };
    match result {
        Ok(()) => Ok(None),
        Err(konnect_sexp::SexpError::Conflict { .. }) => Ok(Some(conflict(path, what))),
        Err(e) if expected.is_none() && path.exists() => {
            let _ = e;
            Ok(Some(conflict(path, what)))
        }
        Err(e) => Err(e.into()),
    }
}

// ─── Derived requirements ───────────────────────────────────────────────────

/// What the intent works out to, net by net.
#[derive(Debug, Clone, Serialize)]
struct NetRequirement {
    net: String,
    role: String,
    priority: Option<u8>,
    /// The net-class width: the autorouter routes every layer with it.
    track_width_mm: Option<f64>,
    /// IPC-2221 minimum widths for the current, outer and inner layers.
    current_width_mm: Option<f64>,
    inner_current_width_mm: Option<f64>,
    /// Impedance width bounds per layer that has a reference model.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    impedance_widths: Vec<LayerWidth>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    impedance_unmodelled: Vec<String>,
    diff_pair_gap_mm: Option<f64>,
    clearance_mm: Option<f64>,
    max_length_mm: Option<f64>,
    max_skew_mm: Option<f64>,
    basis: Vec<Value>,
    warnings: Vec<String>,
}

fn requirements(intent: &DesignIntent) -> Vec<NetRequirement> {
    requirements_on(intent, None)
}

/// Copper layers that act as reference planes: those holding a zone of a
/// `plane` net, else (no plane nets in the intent) any inner layer with a
/// net zone.
fn plane_layers(tree: &SexpNode, intent: &DesignIntent) -> BTreeSet<String> {
    let zones = konnect_sexp::board::zones(tree).items;
    let plane_nets: BTreeSet<&str> = intent.nets.iter().filter(|(_, n)| n.role == "plane").map(|(k, _)| k.as_str()).collect();
    let mut layers: BTreeSet<String> = zones
        .iter()
        .filter(|z| z.net.as_deref().is_some_and(|n| plane_nets.contains(n)))
        .flat_map(|z| z.layers.iter().filter(|l| l.ends_with(".Cu")).cloned())
        .collect();
    if layers.is_empty() {
        layers = zones
            .iter()
            .filter(|z| z.net.is_some())
            .flat_map(|z| z.layers.iter().filter(|l| l.starts_with("In")).cloned())
            .collect();
    }
    layers
}

/// The stack and reference planes the requirements use, and where they came from.
fn stack_and_planes(intent: &DesignIntent, board: Option<&SexpNode>) -> (LayerStack, BTreeSet<String>, String) {
    match board {
        Some(tree) => {
            let stack = LayerStack::for_board(tree, intent.stackup());
            let planes = plane_layers(tree, intent);
            let note = format!("{}; reference planes from the board's zones: {:?}", stack.source, planes);
            (stack, planes, note)
        }
        None => {
            let names: Vec<String> = ["F.Cu", "In1.Cu", "In2.Cu", "B.Cu"].iter().map(|s| s.to_string()).collect();
            let stack = LayerStack::from_intent(intent.stackup(), &names, 1.6);
            let planes: BTreeSet<String> = ["In1.Cu".to_string(), "In2.Cu".to_string()].into();
            (stack, planes, "no board read: assumed 4 layers, 1.6 mm, planes on In1/In2".to_string())
        }
    }
}

/// The fab's minimum track width on a layer (JLCPCB standard process).
fn fab_min_width(stack: &LayerStack, layer: &str) -> f64 {
    if stack.is_outer(layer) { 0.127 } else { 0.09 }
}

fn requirements_on(intent: &DesignIntent, board: Option<&SexpNode>) -> Vec<NetRequirement> {
    let (layers, planes, stack_note) = stack_and_planes(intent, board);
    let outer_oz = layers.copper.first().map_or(intent.stackup().outer_copper_oz, |c| c.1 / eecalc::MM_PER_OZ);
    let inner_oz = if layers.copper.len() > 2 { layers.copper[1].1 / eecalc::MM_PER_OZ } else { intent.stackup().inner_copper_oz };
    let has_inner = layers.copper.len() > 2;
    let mut out = Vec::new();
    for (net, n) in &intent.nets {
        let mut r = NetRequirement {
            net: net.clone(),
            role: n.role.clone(),
            priority: n.priority,
            track_width_mm: None,
            current_width_mm: None,
            inner_current_width_mm: None,
            impedance_widths: vec![],
            impedance_unmodelled: vec![],
            diff_pair_gap_mm: None,
            clearance_mm: n.clearance_mm,
            max_length_mm: n.max_length_mm,
            max_skew_mm: n.max_skew_mm,
            basis: vec![],
            warnings: vec![],
        };
        if let Some(current) = n.current_a {
            let rise = n.max_temp_rise_c.unwrap_or(10.0);
            let outer = eecalc::width_for_current(current, rise, outer_oz, false);
            let inner = eecalc::width_for_current(current, rise, inner_oz, true);
            let (wo, wi) = (outer.result.width_mm.max(0.15), inner.result.width_mm.max(0.15));
            r.current_width_mm = Some(wo);
            r.inner_current_width_mm = has_inner.then_some(wi);
            // One net-class width serves every layer the autorouter uses, so
            // it is the wider (inner) width when the board has inner layers.
            r.track_width_mm = Some(if has_inner { wo.max(wi) } else { wo });
            if has_inner && n.role != "plane" {
                r.warnings.push(format!("net-class width {} mm is the inner-layer width ({inner_oz:.2} oz); outer runs need only {} mm", r3(wo.max(wi)), r3(wo)));
            }
            r.warnings.extend(outer.warnings.clone());
            r.basis.push(serde_json::to_value(&outer).unwrap_or(Value::Null));
            if has_inner {
                r.warnings.extend(inner.warnings.clone());
                r.basis.push(serde_json::to_value(&inner).unwrap_or(Value::Null));
            }
        }
        if let Some(target) = n.impedance_ohm {
            let gap = (n.role == "diff_pair").then(|| n.gap_mm.unwrap_or(0.15));
            let tol = n.impedance_tolerance_pct.unwrap_or(10.0) / 100.0;
            r.diff_pair_gap_mm = gap;
            for (layer, _) in &layers.copper {
                if planes.contains(layer) {
                    continue;
                }
                match layers.model(layer, &planes) {
                    Ok(model) => match width_bounds(model, layer, target, tol, gap) {
                        Some(bounds) => {
                            let fab = fab_min_width(&layers, layer);
                            if bounds.min_mm < fab {
                                r.warnings.push(format!("conflict: on {layer} the {target} Ω ±{:.0} % minimum width {} mm is below the fab minimum {fab} mm", tol * 100.0, r3(bounds.min_mm)));
                            }
                            if !bounds.in_range {
                                r.warnings.push(format!("{layer}: the geometry is outside the {} fit; treat the width as an estimate", bounds.method));
                            }
                            r.impedance_widths.push(bounds);
                        }
                        None => r.warnings.push(format!("{layer}: {target} Ω is not reachable with widths 0.01–5 mm")),
                    },
                    Err(reason) => r.impedance_unmodelled.push(reason),
                }
            }
            if r.impedance_widths.is_empty() {
                r.warnings.push(format!("no signal layer has a reference model for {target} Ω: {}", r.impedance_unmodelled.join("; ")));
            }
            let preferred = r.impedance_widths.iter().find(|w| layers.is_outer(&w.layer)).or(r.impedance_widths.first());
            if let Some(p) = preferred {
                if let Some(cw) = r.current_width_mm {
                    if cw > p.max_mm {
                        r.warnings.push(format!(
                            "conflict: the current needs {} mm but {target} Ω allows at most {} mm on {}; the impedance governs and the current rule is not emitted",
                            r3(cw), r3(p.max_mm), p.layer
                        ));
                    }
                }
                r.track_width_mm = Some(p.opt_mm);
                if r.impedance_widths.iter().any(|w| (w.opt_mm - p.opt_mm).abs() > 0.01) {
                    r.warnings.push(format!("the net class carries the {} width ({} mm); per-layer width rules hold the other layers, so keep this net on {} or route it by hand", p.layer, r3(p.opt_mm), p.layer));
                }
            }
            r.basis.push(json!({ "stackup": stack_note, "tolerance_pct": tol * 100.0 }));
        }
        if n.role == "diff_pair" {
            let recognised = |name: &str| name.ends_with('P') || name.ends_with('N') || name.ends_with('+') || name.ends_with('-');
            if !recognised(net) {
                r.warnings.push(format!(
                    "KiCad pairs nets by a P/N or +/- suffix; '{net}' has neither, so its diff-pair gap and skew rules are not applied by DRC (rename, e.g. USB_D_P/USB_D_N)"
                ));
            }
        }
        out.push(r);
    }
    out.sort_by_key(|r| (r.priority.unwrap_or(u8::MAX), r.net.clone()));
    out
}

fn mm(value: f64) -> String {
    format!("{}mm", (value * 10_000.0).round() / 10_000.0)
}

fn quoted(name: &str) -> String {
    name.replace('\\', "\\\\").replace('\'', "\\'")
}

/// The `.kicad_dru` rules the intent implies, by rule name.
fn dru_rules(intent: &DesignIntent, reqs: &[NetRequirement]) -> BTreeMap<String, String> {
    let mut rules = BTreeMap::new();
    let mut paired = BTreeSet::new();
    for r in reqs {
        let net = &r.net;
        let n = &intent.nets[net];
        if n.role == "diff_pair" {
            let partner = n.pair.clone().unwrap_or_default();
            let key = if *net < partner { (net.clone(), partner.clone()) } else { (partner.clone(), net.clone()) };
            if paired.insert(key.clone()) {
                let mut body = Vec::new();
                if let Some(g) = r.diff_pair_gap_mm {
                    body.push(format!("  (constraint diff_pair_gap (min {}) (opt {}) (max {}))", mm(g * 0.9), mm(g), mm(g * 1.2)));
                }
                for w in &r.impedance_widths {
                    let name = format!("{RULE_PREFIX}impedance {}/{} {}", key.0, key.1, w.layer);
                    rules.insert(name.clone(), format!(
                        "(rule \"{name}\"\n  (layer \"{}\")\n  (condition \"A.NetName == '{}' || A.NetName == '{}'\")\n  (constraint track_width (min {}) (opt {})))",
                        w.layer, quoted(&key.0), quoted(&key.1), mm(w.min_mm), mm(w.opt_mm)
                    ));
                }
                if let Some(skew) = r.max_skew_mm {
                    body.push(format!("  (constraint skew (max {}))", mm(skew)));
                }
                if !body.is_empty() {
                    rules.insert(
                        format!("{RULE_PREFIX}pair {}/{}", key.0, key.1),
                        format!(
                            "(rule \"{RULE_PREFIX}pair {}/{}\"\n  (condition \"A.NetName == '{}' || A.NetName == '{}'\")\n{})",
                            key.0, key.1, quoted(&key.0), quoted(&key.1), body.join("\n")
                        ),
                    );
                }
            }
        } else if !r.impedance_widths.is_empty() {
            for w in &r.impedance_widths {
                let name = format!("{RULE_PREFIX}impedance {net} {}", w.layer);
                rules.insert(name.clone(), format!(
                    "(rule \"{name}\"\n  (layer \"{}\")\n  (condition \"A.NetName == '{}'\")\n  (constraint track_width (min {}) (opt {})))",
                    w.layer, quoted(net), mm(w.min_mm), mm(w.opt_mm)
                ));
            }
        } else if let (Some(w), true) = (r.current_width_mm, n.role != "plane") {
            let layer = if r.inner_current_width_mm.is_some() { "\n  (layer outer)" } else { "" };
            rules.insert(
                format!("{RULE_PREFIX}width {net}"),
                format!(
                    "(rule \"{RULE_PREFIX}width {net}\"{layer}\n  (condition \"A.NetName == '{}'\")\n  (constraint track_width (min {})))",
                    quoted(net), mm(w)
                ),
            );
            if let Some(wi) = r.inner_current_width_mm {
                rules.insert(
                    format!("{RULE_PREFIX}width {net} inner"),
                    format!(
                        "(rule \"{RULE_PREFIX}width {net} inner\"\n  (layer inner)\n  (condition \"A.NetName == '{}'\")\n  (constraint track_width (min {})))",
                        quoted(net), mm(wi)
                    ),
                );
            }
        }
        if let Some(length) = r.max_length_mm {
            rules.insert(
                format!("{RULE_PREFIX}length {net}"),
                format!(
                    "(rule \"{RULE_PREFIX}length {net}\"\n  (condition \"A.NetName == '{}'\")\n  (constraint length (max {})))",
                    quoted(net), mm(length)
                ),
            );
        }
        if let Some(clearance) = r.clearance_mm {
            rules.insert(
                format!("{RULE_PREFIX}clearance {net}"),
                format!(
                    "(rule \"{RULE_PREFIX}clearance {net}\"\n  (condition \"A.NetName == '{}' && B.NetName != '{}'\")\n  (constraint clearance (min {})))",
                    quoted(net), quoted(net), mm(clearance)
                ),
            );
        }
    }
    rules
}

/// Rewrite a `.kicad_dru` so its `intent: …` rules are exactly `rules`,
/// leaving every other rule and comment as it was.
fn merge_dru(existing: Option<&str>, rules: &BTreeMap<String, String>) -> String {
    merge_managed_rules(existing, rules, RULE_PREFIX)
}

/// Fab limits are floors: KiCad lets a later rule override an earlier one
/// for the same constraint, so they go first in the file, where any user or
/// intent rule (an 0.8 mm power width, say) takes precedence over them.
fn merge_fab_rules(existing: Option<&str>, rules: &[(String, String)]) -> String {
    let none = BTreeMap::new();
    let content = merge_managed_rules(existing, &none, FAB_PREFIX);
    // In table order: within the fab set too, a later rule overrides an
    // earlier one, so general limits come before specific ones.
    let block: String = rules.iter().map(|(_, rule)| format!("{rule}\n")).collect();
    let insert_at = match content.find("(version") {
        Some(at) => content[at..].find('\n').map_or(content.len(), |end| at + end + 1),
        None => 0,
    };
    let mut out = content;
    out.insert_str(insert_at, &block);
    out
}

/// Rewrite a `.kicad_dru` so the rules whose names start with `prefix` are
/// exactly `rules`, leaving every other rule and comment as it was.
fn merge_managed_rules(existing: Option<&str>, rules: &BTreeMap<String, String>, prefix: &str) -> String {
    let mut content = existing.map(str::to_string).unwrap_or_else(|| "(version 1)\n".to_string());
    // Remove stale managed rules.
    loop {
        let names = managed_rule_names(&content, prefix);
        let Some(stale) = names.into_iter().find(|name| !rules.contains_key(name)) else { break };
        let Some((start, end)) = crate::tools::verification::named_rule_range(&content, &stale) else { break };
        let end = if content[end..].starts_with('\n') { end + 1 } else { end };
        content.replace_range(start..end, "");
    }
    for (name, rule) in rules {
        content = crate::tools::verification::upsert_named_rule(&content, name, rule);
    }
    if !content.ends_with('\n') {
        content.push('\n');
    }
    content
}

fn managed_rule_names(content: &str, prefix: &str) -> Vec<String> {
    let needle = format!("(rule \"{prefix}");
    let mut names = Vec::new();
    let mut rest = content;
    while let Some(at) = rest.find(&needle) {
        let after = &rest[at + "(rule \"".len()..];
        if let Some(end) = after.find('"') {
            names.push(after[..end].to_string());
        }
        rest = &rest[at + needle.len()..];
    }
    names
}

/// Net classes in the `.kicad_pro`: one `intent_<net>` class per net with a
/// width or pair geometry, cloned from `Default` so every field KiCad expects
/// is present, plus an exact-name `netclass_patterns` entry. Stale
/// `intent_*` classes and patterns are removed.
fn merge_netclasses(project: &mut Value, intent: &DesignIntent, reqs: &[NetRequirement]) -> anyhow::Result<Vec<String>> {
    let settings = project
        .get_mut("net_settings")
        .and_then(Value::as_object_mut)
        .context("project has no net_settings object")?;
    let default = settings
        .get("classes")
        .and_then(Value::as_array)
        .and_then(|classes| classes.iter().find(|c| c["name"] == "Default"))
        .cloned()
        .unwrap_or_else(|| json!({ "name": "Default", "clearance": 0.2, "track_width": 0.2, "via_diameter": 0.6, "via_drill": 0.3 }));
    let mut wanted: Vec<(String, String, Value)> = Vec::new();
    for r in reqs {
        if r.track_width_mm.is_none() && r.clearance_mm.is_none() {
            continue;
        }
        let class_name = format!("intent_{}", r.net.replace(['/', ' '], "_"));
        let mut class = default.clone();
        class["name"] = json!(class_name);
        class["priority"] = json!(0);
        if let Some(w) = r.track_width_mm {
            class["track_width"] = json!(w);
            if intent.nets[&r.net].role == "diff_pair" {
                class["diff_pair_width"] = json!(w);
            }
        }
        if let Some(g) = r.diff_pair_gap_mm {
            class["diff_pair_gap"] = json!(g);
        }
        if let Some(c) = r.clearance_mm {
            class["clearance"] = json!(c.max(class["clearance"].as_f64().unwrap_or(0.0)));
        }
        wanted.push((r.net.clone(), class_name, class));
    }
    let classes = settings.entry("classes").or_insert_with(|| json!([])).as_array_mut().context("net_settings.classes is not a list")?;
    classes.retain(|c| !c["name"].as_str().is_some_and(|n| n.starts_with("intent_")));
    classes.extend(wanted.iter().map(|(_, _, c)| c.clone()));
    let patterns = settings.entry("netclass_patterns").or_insert_with(|| json!([])).as_array_mut().context("netclass_patterns is not a list")?;
    let intent_nets: BTreeSet<&str> = wanted.iter().map(|(n, _, _)| n.as_str()).collect();
    patterns.retain(|p| {
        !(p["netclass"].as_str().is_some_and(|n| n.starts_with("intent_"))
            || p["pattern"].as_str().is_some_and(|n| intent_nets.contains(n)))
    });
    // Intent patterns go first so they win over broader user patterns.
    let mut new_patterns: Vec<Value> = wanted.iter().map(|(net, class, _)| json!({ "netclass": class, "pattern": net })).collect();
    new_patterns.append(patterns);
    *patterns = new_patterns;
    Ok(wanted.into_iter().map(|(_, class, _)| class).collect())
}

// ─── Checking the board ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
struct Check {
    kind: &'static str,
    subject: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Value::is_null")]
    measured: Value,
    #[serde(skip_serializing_if = "Value::is_null")]
    required: Value,
    detail: String,
}

/// A routed piece of copper: (start, end, width_mm, layer).
type Segment = ((f64, f64), (f64, f64), f64, String);
/// A footprint's courtyard: (reference, bbox, back side, anchor, rotation).
type Courtyard = (String, Rect, bool, (f64, f64), f64);
/// A pad's net and outline.
type PadOutline = (Option<String>, Vec<(f64, f64)>);

struct NetCopper {
    segments: Vec<Segment>,
    has_zone: bool,
}

fn board_copper(tree: &SexpNode) -> BTreeMap<String, NetCopper> {
    let mut nets: BTreeMap<String, NetCopper> = BTreeMap::new();
    fn entry<'a>(nets: &'a mut BTreeMap<String, NetCopper>, net: &str) -> &'a mut NetCopper {
        nets.entry(net.to_string()).or_insert_with(|| NetCopper { segments: vec![], has_zone: false })
    }
    for t in konnect_sexp::board::tracks(tree).items {
        if let Some(net) = &t.net {
            entry(&mut nets, net).segments.push(((t.x1, t.y1), (t.x2, t.y2), t.width, t.layer.clone()));
        }
    }
    for arc in tree.find_all("arc") {
        let point = |tag: &str| arc.find(tag).and_then(|n| Some((n.get_f64(1)?, n.get_f64(2)?)));
        let (Some(s), Some(m), Some(e)) = (point("start"), point("mid"), point("end")) else { continue };
        let Some(net) = arc.find("net").and_then(konnect_sexp::net::net_name).filter(|n| !n.is_empty()) else { continue };
        let width = arc.find_f64("width").unwrap_or(0.2);
        let layer = arc.find_str("layer").unwrap_or("").to_string();
        let points = sample_arc(s, m, e, 16);
        let target = entry(&mut nets, net);
        for w in points.windows(2) {
            target.segments.push((w[0], w[1], width, layer.clone()));
        }
    }
    for zone in konnect_sexp::board::zones(tree).items {
        if let Some(net) = &zone.net {
            entry(&mut nets, net).has_zone = true;
        }
    }
    nets
}

fn length(segments: &[Segment]) -> f64 {
    segments.iter().map(|(a, b, _, _)| (b.0 - a.0).hypot(b.1 - a.1)).sum()
}

fn r3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Plating thickness assumed for via barrels (mm): JLCPCB's typical hole wall.
const VIA_PLATING_MM: f64 = 0.020;

/// The voltage-drop result over routed copper.
#[derive(Debug)]
struct DropResult {
    drop_mv: f64,
    worst_pad: String,
    /// Load pads no routed path reaches (fed through a zone, or not routed).
    unreachable: Vec<String>,
    temp_c: f64,
}

/// Worst-case voltage drop from `source` to each pad of the net through the
/// routed tracks and vias (Dijkstra on resistance), assuming the full current
/// flows along each path: conservative for a net with several loads. Nodes
/// are per layer; a via joins the layers it spans through its barrel; a
/// track end, via or pad centre lying on another segment splits it (a T);
/// a surface pad joins its own layer, a through pad every layer. Copper is
/// taken at 25 °C plus the allowed rise. Zones are not modelled.
fn voltage_drop(
    net: &str,
    n: &NetIntent,
    copper: &NetCopper,
    pads: &[CopperPad],
    vias: &[konnect_sexp::board::Via],
    layers: &LayerStack,
) -> Result<DropResult, String> {
    let (src_ref, src_pad) = n.source.as_deref().and_then(|s| s.split_once('.')).ok_or("no source")?;
    let current = n.current_a.ok_or("no current_a")?;
    let temp_c = 25.0 + n.max_temp_rise_c.unwrap_or(10.0);
    let oz = |layer: &str| layers.index(layer).map_or(1.0, |i| layers.copper[i].1 / eecalc::MM_PER_OZ);
    let snap = |p: (f64, f64)| ((p.0 * 100.0).round() as i64, (p.1 * 100.0).round() as i64);
    type NodeKey = (String, (i64, i64));
    let mut ids: HashMap<NodeKey, usize> = HashMap::new();
    let node = |layer: &str, p: (f64, f64), ids: &mut HashMap<NodeKey, usize>| {
        let next = ids.len();
        *ids.entry((layer.to_string(), snap(p))).or_insert(next)
    };
    let net_pads: Vec<&CopperPad> = pads.iter().filter(|p| p.net.as_deref() == Some(net)).collect();
    let net_vias: Vec<&konnect_sexp::board::Via> = vias.iter().filter(|v| v.net.as_deref() == Some(net)).collect();
    // The layers a via spans, in stack order.
    let span = |v: &konnect_sexp::board::Via| -> Vec<String> {
        let idx: Vec<usize> = v.layers.iter().filter_map(|l| layers.index(l)).collect();
        match (idx.iter().min(), idx.iter().max()) {
            (Some(&a), Some(&b)) => (a..=b).map(|i| layers.copper[i].0.clone()).collect(),
            _ => Vec::new(),
        }
    };
    // Points that split a segment on a layer: other track ends, vias, pad centres.
    let mut splits: HashMap<&str, Vec<(f64, f64)>> = HashMap::new();
    for (a, b, _, layer) in &copper.segments {
        splits.entry(layer.as_str()).or_default().extend([*a, *b]);
    }
    for pad in &net_pads {
        let centre = (
            pad.corners.iter().map(|c| c.0).sum::<f64>() / 4.0,
            pad.corners.iter().map(|c| c.1).sum::<f64>() / 4.0,
        );
        for (layer, list) in splits.iter_mut() {
            if !pad.smd || *layer == pad.layer {
                list.push(centre);
            }
        }
    }
    let via_spans: Vec<Vec<String>> = net_vias.iter().map(|v| span(v)).collect();
    for (v, layers_of) in net_vias.iter().zip(&via_spans) {
        for l in layers_of {
            if let Some(list) = splits.get_mut(l.as_str()) {
                list.push((v.x, v.y));
            }
        }
    }
    let mut edges: Vec<(usize, usize, f64)> = Vec::new();
    for (a, b, w, layer) in &copper.segments {
        let len = (b.0 - a.0).hypot(b.1 - a.1);
        if len < 1e-9 {
            continue;
        }
        let (ux, uy) = ((b.0 - a.0) / len, (b.1 - a.1) / len);
        let mut cuts: Vec<f64> = vec![0.0, len];
        for p in splits.get(layer.as_str()).into_iter().flatten() {
            let t = (p.0 - a.0) * ux + (p.1 - a.1) * uy;
            let off = ((p.0 - a.0) * uy - (p.1 - a.1) * ux).abs();
            if t > 1e-3 && t < len - 1e-3 && off <= w / 2.0 {
                cuts.push(t);
            }
        }
        cuts.sort_by(f64::total_cmp);
        cuts.dedup_by(|x, y| (*x - *y).abs() < 1e-3);
        for pair in cuts.windows(2) {
            let (t0, t1) = (pair[0], pair[1]);
            let p0 = (a.0 + ux * t0, a.1 + uy * t0);
            let p1 = (a.0 + ux * t1, a.1 + uy * t1);
            let r = eecalc::trace_resistance(t1 - t0, *w, oz(layer), temp_c, None).result.resistance_ohm;
            let (i0, i1) = (node(layer, p0, &mut ids), node(layer, p1, &mut ids));
            edges.push((i0, i1, r));
        }
    }
    // Via barrels between consecutive spanned layers.
    for (v, layers_of) in net_vias.iter().zip(&via_spans) {
        for pair in layers_of.windows(2) {
            let (i, j) = (layers.index(&pair[0]).unwrap_or(0), layers.index(&pair[1]).unwrap_or(0));
            let height = layers.gaps.get(i.min(j)).map_or(0.2, |g| g.thickness_mm);
            let circumference = std::f64::consts::PI * (v.drill + VIA_PLATING_MM);
            let r = eecalc::trace_resistance(height, circumference, VIA_PLATING_MM / eecalc::MM_PER_OZ, temp_c, None).result.resistance_ohm;
            let (a, b) = (node(&pair[0], (v.x, v.y), &mut ids), node(&pair[1], (v.x, v.y), &mut ids));
            edges.push((a, b, r));
        }
    }
    // Pads join every node on their layers that lies on them.
    let endpoint_list: Vec<(NodeKey, usize)> = ids.iter().map(|(k, v)| (k.clone(), *v)).collect();
    let mut pad_nodes: Vec<(String, usize)> = Vec::new();
    for pad in &net_pads {
        let pad_node = ids.len() + pad_nodes.len();
        pad_nodes.push((format!("{}.{}", pad.reference, pad.number), pad_node));
        for ((layer, key), endpoint) in &endpoint_list {
            if pad.smd && layer != pad.layer {
                continue;
            }
            let p = (key.0 as f64 / 100.0, key.1 as f64 / 100.0);
            if polygon_distance(&[p, p, p], &pad.corners) <= 0.02 {
                edges.push((pad_node, *endpoint, 0.0));
            }
        }
    }
    let count = ids.len() + pad_nodes.len();
    let mut adj = vec![Vec::new(); count];
    for (a, b, r) in edges {
        adj[a].push((b, r));
        adj[b].push((a, r));
    }
    let start = pad_nodes
        .iter()
        .find(|(name, _)| name == &format!("{src_ref}.{src_pad}"))
        .map(|(_, node)| *node)
        .ok_or_else(|| format!("source pad {src_ref}.{src_pad} is not on net {net}"))?;
    let mut dist = vec![f64::INFINITY; count];
    dist[start] = 0.0;
    #[derive(PartialEq)]
    struct State(f64, usize);
    impl Eq for State {}
    impl PartialOrd for State {
        fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(o)) }
    }
    impl Ord for State {
        fn cmp(&self, o: &Self) -> std::cmp::Ordering { o.0.total_cmp(&self.0) }
    }
    let mut heap = BinaryHeap::from([State(0.0, start)]);
    while let Some(State(d, u)) = heap.pop() {
        if d > dist[u] {
            continue;
        }
        for &(v, r) in &adj[u] {
            if d + r < dist[v] {
                dist[v] = d + r;
                heap.push(State(d + r, v));
            }
        }
    }
    let loads: Vec<&(String, usize)> = pad_nodes.iter().filter(|(_, node)| *node != start).collect();
    let unreachable: Vec<String> = loads.iter().filter(|(_, node)| !dist[*node].is_finite()).map(|(name, _)| name.clone()).collect();
    if loads.is_empty() {
        return Err("the net has no load pad besides the source".to_string());
    }
    // No reachable load still reports which loads are cut off.
    let (worst, r) = loads
        .iter()
        .filter(|(_, node)| dist[*node].is_finite())
        .map(|(name, node)| (name.clone(), dist[*node]))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap_or_default();
    Ok(DropResult { drop_mv: current * r * 1000.0, worst_pad: worst, unreachable, temp_c })
}

/// How much of a net's routed length meets its impedance target.
#[derive(Debug, Default, Serialize)]
struct ImpedanceProfile {
    total_mm: f64,
    in_tolerance_mm: f64,
    out_of_tolerance_mm: f64,
    /// No reference model for the layer, or no plane under the segment.
    unmodelled_mm: f64,
    /// Pair segments with no parallel partner segment beside them.
    uncoupled_mm: f64,
    /// Modelled, but outside the formula's fitted range.
    outside_fit_mm: f64,
    z_min_ohm: Option<f64>,
    z_max_ohm: Option<f64>,
    gap_min_mm: Option<f64>,
    gap_max_mm: Option<f64>,
    reasons: Vec<String>,
}

impl ImpedanceProfile {
    fn status(&self) -> &'static str {
        let modelled = self.in_tolerance_mm + self.out_of_tolerance_mm;
        if modelled < 0.5 * self.total_mm || modelled <= 0.0 {
            "unverified"
        } else if self.out_of_tolerance_mm > 0.1 * modelled {
            "fail"
        } else {
            "pass"
        }
    }
}

/// The partner segment running beside `(a, b)` on the same layer: parallel
/// within 5° and overlapping in projection. Returns (edge-to-edge gap,
/// overlap length).
fn coupled_gap(a: (f64, f64), b: (f64, f64), w: f64, layer: &str, partner: &NetCopper) -> Option<(f64, f64)> {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = dx.hypot(dy);
    if len < 1e-6 {
        return None;
    }
    let (ux, uy) = (dx / len, dy / len);
    let mut best: Option<(f64, f64)> = None;
    for (p, q, pw, pl) in &partner.segments {
        if pl != layer {
            continue;
        }
        let (ex, ey) = (q.0 - p.0, q.1 - p.1);
        let plen = ex.hypot(ey);
        if plen < 1e-6 || (ux * ey - uy * ex).abs() / plen > (5.0_f64).to_radians().sin() {
            continue;
        }
        let along = |pt: (f64, f64)| (pt.0 - a.0) * ux + (pt.1 - a.1) * uy;
        let (s0, s1) = { let (x, y) = (along(*p), along(*q)); (x.min(y), x.max(y)) };
        let overlap = s1.min(len) - s0.max(0.0);
        if overlap <= 1e-6 {
            continue;
        }
        let mid = ((p.0 + q.0) / 2.0, (p.1 + q.1) / 2.0);
        let distance = ((mid.0 - a.0) * uy - (mid.1 - a.1) * ux).abs();
        let gap = distance - (w + pw) / 2.0;
        if gap > 0.0 && best.is_none_or(|(g, _)| gap < g) {
            best = Some((gap, overlap));
        }
    }
    best
}

#[allow(clippy::too_many_arguments)]
fn impedance_profile(
    layers: &LayerStack,
    planes: &BTreeSet<String>,
    zones: &[konnect_sexp::board::ZoneOutline],
    copper: &NetCopper,
    partner: Option<&NetCopper>,
    target: f64,
    tol: f64,
) -> ImpedanceProfile {
    let mut p = ImpedanceProfile::default();
    let note = |p: &mut ImpedanceProfile, reason: String| {
        if !p.reasons.contains(&reason) && p.reasons.len() < 6 {
            p.reasons.push(reason);
        }
    };
    let covered = |layer: &str, point: (f64, f64)| {
        zones.iter().any(|z| z.net.is_some() && z.layers.iter().any(|l| l == layer) && point_in_polygon(point, &z.points))
    };
    for (a, b, w, layer) in &copper.segments {
        let len = (b.0 - a.0).hypot(b.1 - a.1);
        p.total_mm += len;
        let model = match layers.model(layer, planes) {
            Ok(m) => m,
            Err(reason) => {
                p.unmodelled_mm += len;
                note(&mut p, reason);
                continue;
            }
        };
        // The plane must actually lie under (and over) the segment.
        let mid = ((a.0 + b.0) / 2.0, (a.1 + b.1) / 2.0);
        let idx = layers.index(layer).unwrap_or(0);
        let refs: Vec<&str> = [idx.checked_sub(1), Some(idx + 1)]
            .into_iter()
            .flatten()
            .filter_map(|j| layers.copper.get(j))
            .map(|c| c.0.as_str())
            .filter(|l| planes.contains(*l))
            .collect();
        if let Some(missing) = refs.iter().find(|l| !covered(l, mid)) {
            p.unmodelled_mm += len;
            note(&mut p, format!("no plane zone on {missing} under the segment at ({:.2}, {:.2})", mid.0, mid.1));
            continue;
        }
        let (gap, modelled_len) = match partner {
            Some(other) => match coupled_gap(*a, *b, *w, layer, other) {
                Some((g, overlap)) => {
                    p.uncoupled_mm += (len - overlap).max(0.0);
                    p.gap_min_mm = Some(p.gap_min_mm.map_or(g, |m: f64| m.min(g)));
                    p.gap_max_mm = Some(p.gap_max_mm.map_or(g, |m: f64| m.max(g)));
                    (Some(g), overlap.min(len))
                }
                None => {
                    p.uncoupled_mm += len;
                    continue;
                }
            },
            None => (None, len),
        };
        let z = model.impedance(*w, gap);
        if !z.in_range {
            p.outside_fit_mm += modelled_len;
        }
        p.z_min_ohm = Some(p.z_min_ohm.map_or(z.z_ohm, |m: f64| m.min(z.z_ohm)));
        p.z_max_ohm = Some(p.z_max_ohm.map_or(z.z_ohm, |m: f64| m.max(z.z_ohm)));
        if (z.z_ohm - target).abs() <= target * tol {
            p.in_tolerance_mm += modelled_len;
        } else {
            p.out_of_tolerance_mm += modelled_len;
        }
    }
    for v in [&mut p.total_mm, &mut p.in_tolerance_mm, &mut p.out_of_tolerance_mm, &mut p.unmodelled_mm, &mut p.uncoupled_mm, &mut p.outside_fit_mm] {
        *v = r3(*v);
    }
    p
}

fn check_board(tree: &SexpNode, intent: &DesignIntent) -> Vec<Check> {
    let mut checks = Vec::new();
    let pads = copper_pads(tree);
    let copper = board_copper(tree);
    let reqs = requirements_on(intent, Some(tree));
    let (layers, planes, stack_note) = stack_and_planes(intent, Some(tree));
    let zones = konnect_sexp::board::zones(tree).items;
    let vias = konnect_sexp::board::vias(tree).items;
    let empty = NetCopper { segments: vec![], has_zone: false };

    for tie in &intent.ties {
        let subject = format!("{} → {}{}", tie.part, tie.to, tie.pin.as_deref().map(|p| format!(".{p}")).unwrap_or_default());
        let part: Vec<&CopperPad> = pads.iter().filter(|p| p.reference == tie.part).collect();
        let target: Vec<&CopperPad> = pads
            .iter()
            .filter(|p| p.reference == tie.to && tie.pin.as_deref().is_none_or(|pin| p.number == pin))
            .collect();
        if part.is_empty() || target.is_empty() {
            checks.push(Check { kind: "tie", subject, status: "unverified", measured: Value::Null, required: json!({"max_mm": tie.max_mm}), detail: "part or pin not on the board".to_string() });
            continue;
        }
        // Measure from the part's pads on the pin's net (the pad that does
        // the job), falling back to any pad when the pin has no net.
        let nets: BTreeSet<&str> = target.iter().filter_map(|p| p.net.as_deref()).collect();
        let sharing: Vec<&&CopperPad> = part.iter().filter(|p| p.net.as_deref().is_some_and(|n| nets.contains(n))).collect();
        let (from, note) = if sharing.is_empty() {
            (part.iter().collect::<Vec<_>>(), "part shares no net with the pin; measured from its nearest pad")
        } else {
            (sharing, "measured from the part's pad on the pin's net, pad edge to pad edge")
        };
        let d = from
            .iter()
            .flat_map(|a| target.iter().map(move |b| polygon_distance(&a.corners, &b.corners)))
            .fold(f64::INFINITY, f64::min);
        let same_side = from.iter().any(|a| target.iter().any(|b| a.layer == b.layer));
        checks.push(Check {
            kind: "tie",
            subject,
            status: if d <= tie.max_mm { "pass" } else { "fail" },
            measured: json!({"distance_mm": r3(d), "same_side": same_side}),
            required: json!({"max_mm": tie.max_mm}),
            detail: tie.why.clone().map_or(note.to_string(), |w| format!("{w}; {note}")),
        });
    }

    for r in &reqs {
        let n = &intent.nets[&r.net];
        let net_copper = copper.get(&r.net).unwrap_or(&empty);
        let routed = !net_copper.segments.is_empty();
        if n.role == "plane" {
            checks.push(Check {
                kind: "plane",
                subject: r.net.clone(),
                status: if net_copper.has_zone { "pass" } else { "fail" },
                measured: json!({"has_zone": net_copper.has_zone}),
                required: json!({"has_zone": true}),
                detail: "a plane net needs a zone".to_string(),
            });
            continue;
        }
        if let (Some(outer_min), true) = (r.current_width_mm, r.impedance_widths.is_empty()) {
            if routed {
                // Each segment against its own layer's minimum, rounded as the
                // DRC rule is, so this check and KiCad's DRC agree.
                let inner_min = r.inner_current_width_mm.unwrap_or(outer_min);
                let rounded = |w: f64| (w * 10_000.0).round() / 10_000.0;
                let (mut under, mut total, mut narrowest) = (0.0, 0.0, f64::INFINITY);
                for (a, b, w, layer) in &net_copper.segments {
                    let len = (b.0 - a.0).hypot(b.1 - a.1);
                    let need = rounded(if layers.is_outer(layer) { outer_min } else { inner_min });
                    total += len;
                    narrowest = narrowest.min(*w);
                    if *w + 1e-6 < need {
                        under += len;
                    }
                }
                checks.push(Check {
                    kind: "track_width",
                    subject: r.net.clone(),
                    status: if under <= 1.0 { "pass" } else { "fail" },
                    measured: json!({"undersized_length_mm": r3(under), "routed_length_mm": r3(total), "narrowest_mm": r3(narrowest)}),
                    required: json!({"outer_min_mm": r3(outer_min), "inner_min_mm": r.inner_current_width_mm.map(r3)}),
                    detail: "IPC-2221 width for the current on each segment's layer (inner layers from the inner copper weight); up to 1 mm of narrower copper (pad necks) passes here, DRC still lists it".to_string(),
                });
            } else if !net_copper.has_zone {
                checks.push(Check { kind: "track_width", subject: r.net.clone(), status: "unverified", measured: Value::Null, required: json!({"outer_min_mm": r3(outer_min)}), detail: "net not routed yet".to_string() });
            }
        }
        if let Some(max) = r.max_length_mm {
            let l = length(&net_copper.segments);
            checks.push(Check {
                kind: "length",
                subject: r.net.clone(),
                status: if !routed { "unverified" } else if l <= max { "pass" } else { "fail" },
                measured: json!({"routed_length_mm": r3(l)}),
                required: json!({"max_mm": max}),
                detail: "sum of routed segment and arc lengths (vias excluded)".to_string(),
            });
        }
        if let (Some(max_skew), Some(partner)) = (r.max_skew_mm, n.pair.as_ref()) {
            if r.net < *partner {
                let other = copper.get(partner).unwrap_or(&empty);
                let skew = (length(&net_copper.segments) - length(&other.segments)).abs();
                let both = routed && !other.segments.is_empty();
                checks.push(Check {
                    kind: "skew",
                    subject: format!("{}/{}", r.net, partner),
                    status: if !both { "unverified" } else if skew <= max_skew { "pass" } else { "fail" },
                    measured: json!({"skew_mm": r3(skew)}),
                    required: json!({"max_mm": max_skew}),
                    detail: "difference of routed lengths".to_string(),
                });
            }
        }
        if let Some(max_mv) = n.max_drop_mv {
            let check = if !routed && net_copper.has_zone {
                Check { kind: "voltage_drop", subject: r.net.clone(), status: "unverified", measured: Value::Null, required: json!({"max_mv": max_mv}), detail: "carried by a zone; zone resistance is not computed".to_string() }
            } else {
                match voltage_drop(&r.net, n, net_copper, &pads, &vias, &layers) {
                    Ok(d) => {
                        let zone_note = if net_copper.has_zone { "; a zone on this net may carry part of the current, so this track-only drop is an upper bound" } else { "" };
                        let status = if !d.unreachable.is_empty() {
                            "unverified"
                        } else if d.drop_mv <= max_mv {
                            "pass"
                        } else if net_copper.has_zone {
                            "unverified"
                        } else {
                            "fail"
                        };
                        Check {
                            kind: "voltage_drop",
                            subject: r.net.clone(),
                            status,
                            measured: json!({"drop_mv": r3(d.drop_mv), "worst_pad": d.worst_pad, "unreachable_pads": d.unreachable, "copper_temp_c": d.temp_c, "tracks_only": true}),
                            required: json!({"max_mv": max_mv}),
                            detail: format!(
                                "I × resistance of routed tracks and via barrels ({VIA_PLATING_MM} mm plating) from the source to the farthest reachable pad, full current on every path{}{zone_note}",
                                if d.unreachable.is_empty() { String::new() } else { format!("; {} load pad(s) are not reached by routed copper, so the net is not verified", d.unreachable.len()) }
                            ),
                        }
                    }
                    Err(reason) => Check { kind: "voltage_drop", subject: r.net.clone(), status: "unverified", measured: Value::Null, required: json!({"max_mv": max_mv}), detail: reason },
                }
            };
            checks.push(check);
        }
        if let (Some(target), true) = (n.impedance_ohm, routed) {
            let partner = n.pair.as_ref().map(|p| copper.get(p).unwrap_or(&empty));
            let z = impedance_profile(&layers, &planes, &zones, net_copper, partner, target, n.impedance_tolerance_pct.unwrap_or(10.0) / 100.0);
            checks.push(Check {
                kind: "impedance",
                subject: r.net.clone(),
                status: z.status(),
                measured: serde_json::to_value(&z).unwrap_or(Value::Null),
                required: json!({"target_ohm": target, "tolerance_pct": n.impedance_tolerance_pct.unwrap_or(10.0), "pair_with": n.pair}),
                detail: format!(
                    "per segment on its own layer's model ({stack_note}); a pair is measured edge to edge against the partner's parallel overlapping segments; fails when over 10 % of the modelled length is out of tolerance, unverified when under half the length can be modelled"
                ),
            });
        }
    }
    checks
}

// ─── Fab rules ──────────────────────────────────────────────────────────────

const FAB_PREFIX: &str = "fab: ";

/// JLCPCB standard-process limits as KiCad DRC rules. Values from the
/// community rule set annotated against jlcpcb.com/capabilities (document
/// database: `labtroll-jlcpcb-kicad-dru`; JLCPCB's page itself was not
/// reachable to cross-check). Outer layers use the 1–2-layer 0.127 mm
/// track/space; inner layers JLCPCB's multilayer 0.09 mm.
fn jlcpcb_rules() -> Vec<(&'static str, &'static str)> {
    vec![
        // The board-wide hole range first: the NPTH rule below must override it.
        ("hole diameter", "(constraint hole_size (min 0.2mm) (max 6.3mm))"),
        ("track width, outer", "(layer outer)\n  (condition \"A.Type == 'track'\")\n  (constraint track_width (min 0.127mm))"),
        ("track spacing, outer", "(layer outer)\n  (condition \"A.Type == 'track' && B.Type == A.Type\")\n  (constraint clearance (min 0.127mm))"),
        ("track width, inner", "(layer inner)\n  (condition \"A.Type == 'track'\")\n  (constraint track_width (min 0.09mm))"),
        ("track spacing, inner", "(layer inner)\n  (condition \"A.Type == 'track' && B.Type == A.Type\")\n  (constraint clearance (min 0.09mm))"),
        ("silkscreen text", "(layer \"?.Silkscreen\")\n  (condition \"A.Type == 'Text' || A.Type == 'Text Box'\")\n  (constraint text_thickness (min 0.15mm))\n  (constraint text_height (min 1mm))"),
        ("pad to silkscreen", "(layer outer)\n  (condition \"A.Type == 'pad' && B.Layer == '?.Silkscreen'\")\n  (constraint silk_clearance (min 0.15mm))"),
        ("routed edge to track", "(condition \"A.Type == 'track'\")\n  (constraint edge_clearance (min 0.3mm))"),
        // `!A.isPlated()` never matches an NPTH pad in KiCad 10 (found by the
        // DRC oracle test); the pad type does.
        ("NPTH diameter", "(condition \"A.Pad_Type == 'NPTH, mechanical'\")\n  (constraint hole_size (min 0.5mm))"),
        ("annular ring, via and PTH", "(layer outer)\n  (condition \"A.isPlated()\")\n  (constraint annular_width (min 0.075mm))"),
        ("hole to hole, different nets", "(layer outer)\n  (condition \"A.Net != B.Net\")\n  (constraint hole_to_hole (min 0.5mm))"),
        ("hole to hole, same net", "(layer outer)\n  (condition \"A.Net == B.Net\")\n  (constraint hole_to_hole (min 0.254mm))"),
        ("track to NPTH", "(condition \"A.Pad_Type == 'NPTH, mechanical' && B.Type == 'track'\")\n  (constraint hole_clearance (min 0.254mm))"),
        // Bites where the hole has no copper on the track's layer (an inner
        // layer with unused via pads removed); elsewhere copper clearance governs.
        ("track to PTH hole", "(condition \"A.isPlated() && B.Type == 'track' && A.Net != B.Net\")\n  (constraint hole_clearance (min 0.33mm))"),
        ("track to pad", "(condition \"A.Type == 'pad' && B.Type == 'track' && A.Net != B.Net\")\n  (constraint clearance (min 0.2mm))"),
        ("pad/via to pad/via", "(layer outer)\n  (condition \"A.isPlated() && B.isPlated() && A.Net != B.Net\")\n  (constraint clearance (min 0.127mm))"),
    ]
}

/// The fab's rules in the order they must be written, general first.
fn fab_rules(fab: &str) -> Option<Vec<(String, String)>> {
    let table = match fab {
        "jlcpcb" => jlcpcb_rules(),
        _ => return None,
    };
    Some(
        table
            .into_iter()
            .map(|(name, body)| {
                let full = format!("{FAB_PREFIX}{fab} {name}");
                (full.clone(), format!("(rule \"{full}\"\n  {body})"))
            })
            .collect(),
    )
}

fn handle_apply_fab_rules(args: &Value) -> anyhow::Result<CallToolResult> {
    let stem = match project_arg(args) {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };
    let fab = args["fab"].as_str().unwrap_or("jlcpcb");
    let Some(rules) = fab_rules(fab) else {
        return Ok(invalid("fab", "supported: jlcpcb"));
    };
    let dry_run = args["dry_run"].as_bool().unwrap_or(true);
    let dru_path = stem.with_extension("kicad_dru");
    let existing = std::fs::read_to_string(&dru_path).ok();
    let merged = merge_fab_rules(existing.as_deref(), &rules);
    let plan = json!({
        "fab": fab,
        "dry_run": dry_run,
        "rules": rules.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        "source": "document database: labtroll-jlcpcb-kicad-dru (community rules annotated against jlcpcb.com/capabilities); standard process, 1 oz outer",
        "path": dru_path,
        "dru_text": if dry_run { Value::String(merged.clone()) } else { Value::Null },
    });
    if dry_run {
        return Ok(CallToolResult::json(&plan));
    }
    if let Some(refusal) = crate::tools::pcb_board::refuse_if_project_locked_by_kicad(&stem.with_extension("kicad_pro"), "fab rules") {
        return Ok(refusal);
    }
    if let Some(refusal) = write_checked(&dru_path, existing.as_deref(), &merged, "apply_fab_rules")? {
        return Ok(refusal);
    }
    let mut done = plan;
    done["written"] = json!(dru_path);
    Ok(CallToolResult::json(&done))
}

// ─── Placement for ties ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TiePlacement {
    pub reference: String,
    pub x: f64,
    pub y: f64,
    pub rotation: f64,
    pub tie: String,
    pub distance_mm: f64,
    pub max_mm: f64,
    pub meets: bool,
}

type Rect = (f64, f64, f64, f64);

fn rects_overlap(a: Rect, b: Rect) -> bool {
    a.0 < b.2 && b.0 < a.2 && a.1 < b.3 && b.1 < a.3
}

fn hull(points: &[(f64, f64)]) -> Rect {
    points.iter().fold((f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY), |acc, p| {
        (acc.0.min(p.0), acc.1.min(p.1), acc.2.max(p.0), acc.3.max(p.1))
    })
}

/// Board point → footprint-local point for a footprint at `at`, `rot`.
fn to_local(p: (f64, f64), at: (f64, f64), rot: f64) -> (f64, f64) {
    konnect_sexp::geometry::transform_pad(p.0 - at.0, p.1 - at.1, 0.0, 0.0, -rot)
}

fn to_board(p: (f64, f64), at: (f64, f64), rot: f64) -> (f64, f64) {
    konnect_sexp::geometry::transform_pad(p.0, p.1, at.0, at.1, rot)
}

/// For each tie that fails, the position and rotation (same side) that put
/// the part's pad on the pin's net closest to the pin, with its courtyard
/// clear of every other courtyard on that side and inside the outline.
/// A plan only: apply with `set_component_placements`.
pub(crate) fn plan_ties(tree: &SexpNode, intent: &DesignIntent, clearance: f64) -> (Vec<TiePlacement>, Vec<Value>) {
    let pads = copper_pads(tree);
    let outline = BoardOutline::read(tree);
    let mut courtyards: Vec<Courtyard> = konnect_sexp::board::footprint_courtyards(tree)
        .items
        .into_iter()
        .filter_map(|c| Some((c.reference?, c.bbox, matches!(c.layer_side, konnect_sexp::board::Side::Back), c.at, c.rotation_deg)))
        .collect();
    let mut plans = Vec::new();
    let mut notes = Vec::new();
    for tie in &intent.ties {
        let label = format!("{} → {}{}", tie.part, tie.to, tie.pin.as_deref().map(|p| format!(".{p}")).unwrap_or_default());
        let target: Vec<&CopperPad> = pads.iter().filter(|p| p.reference == tie.to && tie.pin.as_deref().is_none_or(|pin| p.number == pin)).collect();
        let part: Vec<&CopperPad> = pads.iter().filter(|p| p.reference == tie.part).collect();
        let (Some(me), Some(ic)) = (
            courtyards.iter().position(|c| c.0 == tie.part),
            courtyards.iter().find(|c| c.0 == tie.to).cloned(),
        ) else {
            notes.push(json!({"tie": label, "status": "skipped", "reason": "part or target has no readable courtyard"}));
            continue;
        };
        if target.is_empty() || part.is_empty() {
            notes.push(json!({"tie": label, "status": "skipped", "reason": "part or pin not on the board"}));
            continue;
        }
        let nets: BTreeSet<&str> = target.iter().filter_map(|p| p.net.as_deref()).collect();
        let net_pads: Vec<&&CopperPad> = part.iter().filter(|p| p.net.as_deref().is_some_and(|n| nets.contains(n))).collect();
        let job: Vec<&&CopperPad> = if net_pads.is_empty() { part.iter().collect() } else { net_pads };
        let distance_now = job.iter().flat_map(|a| target.iter().map(move |b| polygon_distance(&a.corners, &b.corners))).fold(f64::INFINITY, f64::min);
        if distance_now <= tie.max_mm {
            notes.push(json!({"tie": label, "status": "already_met", "distance_mm": r3(distance_now)}));
            continue;
        }
        let (_, my_rect, back, at, rot) = courtyards[me].clone();
        // The part in its own frame: courtyard corners and the working pad.
        let local_courtyard: Vec<(f64, f64)> = [(my_rect.0, my_rect.1), (my_rect.2, my_rect.1), (my_rect.2, my_rect.3), (my_rect.0, my_rect.3)]
            .iter().map(|p| to_local(*p, at, rot)).collect();
        let local_pads: Vec<PadOutline> = part.iter()
            .map(|p| (p.net.clone(), p.corners.iter().map(|c| to_local(*c, at, rot)).collect()))
            .collect();
        let work_net = job[0].net.clone();
        let work_local: Vec<(f64, f64)> = local_pads.iter().find(|(n, _)| *n == work_net).map(|(_, c)| c.clone()).unwrap_or_default();
        let work_center = (work_local.iter().map(|p| p.0).sum::<f64>() / 4.0, work_local.iter().map(|p| p.1).sum::<f64>() / 4.0);
        let pin = target[0];
        let pin_center = (pin.corners.iter().map(|p| p.0).sum::<f64>() / 4.0, pin.corners.iter().map(|p| p.1).sum::<f64>() / 4.0);
        let (dx, dy) = (pin_center.0 - ic.3 .0, pin_center.1 - ic.3 .1);
        // Outward along the pin's dominant axis; lateral along the IC edge.
        let outward = if dx.abs() >= dy.abs() { (dx.signum(), 0.0) } else { (0.0, dy.signum()) };
        let lateral = (-outward.1, outward.0);
        let others: Vec<Rect> = courtyards.iter().enumerate().filter(|(i, c)| *i != me && c.2 == back).map(|(_, c)| c.1).collect();
        let other_pads: Vec<&CopperPad> = pads.iter().filter(|p| p.reference != tie.part).collect();
        let mut best: Option<(f64, (f64, f64), f64)> = None;
        for rotation in [0.0, 90.0, 180.0, 270.0] {
            let offset = to_board(work_center, (0.0, 0.0), rotation);
            let mut s = 0.0;
            while s <= tie.max_mm + 4.0 {
                s += 0.1;
                let mut t = -3.0;
                while t <= 3.0 + 1e-9 {
                    let want = (pin_center.0 + outward.0 * s + lateral.0 * t, pin_center.1 + outward.1 * s + lateral.1 * t);
                    let origin = (want.0 - offset.0, want.1 - offset.1);
                    t += 0.25;
                    let court: Vec<(f64, f64)> = local_courtyard.iter().map(|p| to_board(*p, origin, rotation)).collect();
                    let rect = hull(&court);
                    if others.iter().any(|o| rects_overlap(rect, *o)) || !court.iter().all(|p| outline.contains(*p, 0.0)) {
                        continue;
                    }
                    let placed: Vec<PadOutline> = local_pads.iter()
                        .map(|(n, c)| (n.clone(), c.iter().map(|p| to_board(*p, origin, rotation)).collect()))
                        .collect();
                    let clash = placed.iter().any(|(n, c)| other_pads.iter().any(|o| o.layer == pin.layer && o.net != *n && polygon_distance(c, &o.corners) < clearance));
                    if clash {
                        continue;
                    }
                    let work: Vec<(f64, f64)> = work_local.iter().map(|p| to_board(*p, origin, rotation)).collect();
                    let d = target.iter().map(|b| polygon_distance(&work, &b.corners)).fold(f64::INFINITY, f64::min);
                    let score = d + 0.02 * (t - 0.25).abs();
                    if best.is_none_or(|(b, _, _)| score < b) {
                        best = Some((score, origin, rotation));
                    }
                }
            }
        }
        match best {
            Some((_, origin, rotation)) => {
                let work: Vec<(f64, f64)> = work_local.iter().map(|p| to_board(*p, origin, rotation)).collect();
                let d = target.iter().map(|b| polygon_distance(&work, &b.corners)).fold(f64::INFINITY, f64::min);
                // The part's new courtyard is an obstacle for the next tie.
                let court: Vec<(f64, f64)> = local_courtyard.iter().map(|p| to_board(*p, origin, rotation)).collect();
                courtyards[me].1 = hull(&court);
                courtyards[me].3 = origin;
                courtyards[me].4 = rotation;
                plans.push(TiePlacement {
                    reference: tie.part.clone(),
                    x: r3(origin.0),
                    y: r3(origin.1),
                    rotation,
                    tie: label,
                    distance_mm: r3(d),
                    max_mm: tie.max_mm,
                    meets: d <= tie.max_mm,
                });
            }
            None => notes.push(json!({"tie": label, "status": "no_site", "reason": "no position clears the other courtyards inside the outline"})),
        }
    }
    (plans, notes)
}

// ─── Tools ──────────────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "engineering_calc",
            "Engineering numbers for design decisions, as JSON with method, assumptions, valid \
             range and warnings. calculation: width_for_current (IPC-2221), trace_resistance \
             (with voltage drop), impedance (microstrip/stripline, differential with gap_mm), \
             width_for_impedance, standard_value (E12/E24/E96), divider. Lengths mm, current A, \
             temperature °C.",
            json!({
                "type": "object",
                "properties": {
                    "calculation": { "type": "string", "enum": ["width_for_current", "trace_resistance", "impedance", "width_for_impedance", "standard_value", "divider"] },
                    "current_a": { "type": "number", "description": "A" },
                    "temp_rise_c": { "type": "number", "default": 10, "description": "Allowed rise, °C" },
                    "copper_oz": { "type": "number", "default": 1 },
                    "layer": { "type": "string", "enum": ["outer", "inner"], "default": "outer" },
                    "length_mm": { "type": "number" },
                    "width_mm": { "type": "number" },
                    "temp_c": { "type": "number", "default": 25 },
                    "geometry": { "type": "string", "enum": ["microstrip", "stripline"], "default": "microstrip" },
                    "height_mm": { "type": "number", "description": "Microstrip: dielectric to the plane. Stripline: plane-to-plane spacing." },
                    "thickness_mm": { "type": "number", "default": 0.035 },
                    "er": { "type": "number", "description": "Relative permittivity" },
                    "gap_mm": { "type": "number", "description": "Differential pair edge-to-edge gap; omit for single-ended" },
                    "target_ohm": { "type": "number" },
                    "value": { "type": "number", "description": "standard_value: the wanted value" },
                    "series": { "type": "string", "enum": ["E12", "E24", "E96"], "default": "E24" },
                    "ratio": { "type": "number", "description": "divider: Vout/Vin (or give vin_v and vout_v)" },
                    "vin_v": { "type": "number" },
                    "vout_v": { "type": "number" },
                    "total_ohm": { "type": "number", "default": 100000, "description": "divider: target Rtop + Rbottom" }
                },
                "required": ["calculation"]
            }),
            |args, _ctx| async move { handle_engineering_calc(args) }
        ),
        tool!(
            "get_design_intent",
            "Read the project's design intent (<project>.intent.json): net roles, priorities, pair \
             and current targets, part ties, stackup. Also returns what it works out to per net \
             (track width, pair gap, with the calculation behind each), sorted by routing priority.",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string", "description": ".kicad_pro, .kicad_pcb, .kicad_sch or project directory" }
                },
                "required": ["project"]
            }),
            |args, _ctx| async move { handle_get_design_intent(args) }
        ),
        tool!(
            "set_design_intent",
            "Merge into the project's design intent. nets: {NET: {role: signal|diff_pair|power|clock|sensitive|plane, \
             priority, pair, impedance_ohm, gap_mm, current_a, max_temp_rise_c, max_drop_mv, source: 'REF.PAD', \
             max_length_mm, max_skew_mm, clearance_mm, why}}; a null value removes the net. ties: \
             [{part, to, pin, max_mm, why}] replace ties with the same part/to/pin. stackup: \
             {outer_copper_oz, inner_copper_oz, outer_dielectric_mm, er}. Validated before writing; \
             revision-checked write.",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "nets": {
                        "type": "object",
                        "description": "Keyed by net name; null removes the net",
                        "additionalProperties": {
                            "type": ["object", "null"],
                            "properties": {
                                "role": { "type": "string", "enum": ["signal", "diff_pair", "power", "clock", "sensitive", "plane"] },
                                "priority": { "type": ["integer", "null"], "minimum": 1, "maximum": 255 },
                                "pair": { "type": ["string", "null"] },
                                "impedance_ohm": { "type": ["number", "null"] },
                                "gap_mm": { "type": ["number", "null"] },
                                "current_a": { "type": ["number", "null"] },
                                "max_temp_rise_c": { "type": ["number", "null"] },
                                "max_drop_mv": { "type": ["number", "null"] },
                                "source": { "type": ["string", "null"], "description": "REF.PAD feeding the net" },
                                "max_length_mm": { "type": ["number", "null"] },
                                "max_skew_mm": { "type": ["number", "null"] },
                                "clearance_mm": { "type": ["number", "null"] },
                                "why": { "type": ["string", "null"] }
                            },
                            "additionalProperties": false
                        }
                    },
                    "ties": { "type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "part": { "type": "string" },
                            "to": { "type": "string" },
                            "pin": { "type": "string" },
                            "max_mm": { "type": "number" },
                            "why": { "type": "string" }
                        },
                        "required": ["part", "to", "max_mm"],
                        "additionalProperties": false
                    } },
                    "remove_ties": { "type": "array", "description": "[{part, to, pin}] to delete", "items": {
                        "type": "object",
                        "properties": { "part": { "type": "string" }, "to": { "type": "string" }, "pin": { "type": "string" } },
                        "required": ["part", "to"],
                        "additionalProperties": false
                    } },
                    "stackup": {
                        "type": "object",
                        "properties": {
                            "outer_copper_oz": { "type": "number" },
                            "inner_copper_oz": { "type": "number" },
                            "outer_dielectric_mm": { "type": "number" },
                            "er": { "type": "number" }
                        },
                        "additionalProperties": false
                    },
                    "notes": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["project"]
            }),
            |args, _ctx| async move { handle_set_design_intent(args) }
        ),
        tool!(
            "apply_design_intent",
            "Write the intent's numbers into KiCad: an intent_<net> net class per net with a width \
             or pair geometry (track_width, diff_pair_width/gap, clearance) in the .kicad_pro, and \
             'intent: …' rules in the .kicad_dru (track_width min, diff_pair_gap, skew, length, \
             clearance) so KiCad's DRC enforces them. Other classes and rules are kept. dry_run \
             (default true) returns the plan. Refused while KiCad holds the project.",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "dry_run": { "type": "boolean", "default": true }
                },
                "required": ["project"]
            }),
            |args, _ctx| async move { handle_apply_design_intent(args) }
        ),
        tool!(
            "apply_fab_rules",
            "Write a fab house's manufacturing limits into the project's .kicad_dru as named \
             'fab: <fab> …' rules (track width/space outer and inner, edge clearance, hole sizes, \
             annular ring, hole-to-hole, hole and pad clearances, silkscreen) so KiCad's DRC \
             checks them. Other rules are kept. Dry run by default; refused while KiCad holds \
             the project.",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "fab": { "type": "string", "enum": ["jlcpcb"], "default": "jlcpcb" },
                    "dry_run": { "type": "boolean", "default": true }
                },
                "required": ["project"]
            }),
            |args, _ctx| async move { handle_apply_fab_rules(args) }
        ),
        tool!(
            "plan_tie_placement",
            "For each design-intent tie that the board fails, propose a position and rotation (same \
             side) that puts the part's pad on the pin's net as close to the pin as possible, with \
             its courtyard clear of other courtyards and inside the real board outline, and pads \
             clear of other nets. A plan only: apply with set_component_placements, then re-run \
             check_design_intent. Later ties see earlier planned positions.",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "clearance": { "type": "number", "default": 0.2, "description": "Pad-to-pad clearance to other nets, mm" },
                    "board_source": board_source::board_source_schema()
                },
                "required": ["project"]
            }),
            |args, ctx| async move { handle_plan_tie_placement(args, ctx).await }
        ),
        tool!(
            "check_design_intent",
            "Measure the board against the design intent: part ties (pad-edge distance), routed \
             length, pair skew, track width against the current/impedance requirement, voltage \
             drop from the source pad, impedance estimate, plane zones. Each check is pass, fail \
             or unverified, with measured and required values. Reads the live board when KiCad \
             holds it.",
            json!({
                "type": "object",
                "properties": {
                    "project": { "type": "string" },
                    "board_source": board_source::board_source_schema()
                },
                "required": ["project"]
            }),
            |args, ctx| async move { handle_check_design_intent(args, ctx).await }
        ),
    ]
}

fn invalid(field: &str, reason: &str) -> CallToolResult {
    CallToolResult::error_kind(
        ToolErrorKind::InvalidArgument { field: field.to_string(), reason: reason.to_string() },
        format!("Invalid '{field}': {reason}"),
    )
}

fn handle_engineering_calc(args: &Value) -> anyhow::Result<CallToolResult> {
    let num = |key: &str| args[key].as_f64();
    let need = |key: &str| num(key).ok_or_else(|| invalid(key, "required for this calculation"));
    let calc = args["calculation"].as_str().unwrap_or("");
    // Physical inputs must be finite and in range: the formulas otherwise
    // return infinities or NaN that read like answers.
    for (key, lower, inclusive) in [
        ("current_a", 0.0, false), ("temp_rise_c", 0.0, false), ("copper_oz", 0.0, false),
        ("length_mm", 0.0, false), ("width_mm", 0.0, false), ("height_mm", 0.0, false),
        ("thickness_mm", 0.0, true), ("er", 1.0, true), ("gap_mm", 0.0, false),
        ("target_ohm", 0.0, false), ("total_ohm", 0.0, false), ("temp_c", -273.15, false),
    ] {
        if let Some(v) = args.get(key).filter(|v| !v.is_null()) {
            let ok = v.as_f64().is_some_and(|x| x.is_finite() && if inclusive { x >= lower } else { x > lower });
            if !ok {
                let bound = if inclusive { "≥" } else { ">" };
                return Ok(invalid(key, &format!("must be a finite number {bound} {lower}")));
            }
        }
    }
    let value = (|| -> Result<Value, CallToolResult> {
        Ok(match calc {
            "width_for_current" => serde_json::to_value(eecalc::width_for_current(
                need("current_a")?,
                num("temp_rise_c").unwrap_or(10.0),
                num("copper_oz").unwrap_or(1.0),
                args["layer"].as_str() == Some("inner"),
            )),
            "trace_resistance" => serde_json::to_value(eecalc::trace_resistance(
                need("length_mm")?,
                need("width_mm")?,
                num("copper_oz").unwrap_or(1.0),
                num("temp_c").unwrap_or(25.0),
                num("current_a"),
            )),
            "impedance" | "width_for_impedance" => {
                let g = LineGeometry {
                    width_mm: if calc == "impedance" { need("width_mm")? } else { 0.0 },
                    height_mm: need("height_mm")?,
                    thickness_mm: num("thickness_mm").unwrap_or(0.035),
                    er: need("er")?,
                    gap_mm: num("gap_mm"),
                };
                let strip = args["geometry"].as_str() == Some("stripline");
                if calc == "impedance" {
                    serde_json::to_value(if strip { eecalc::stripline(g) } else { eecalc::microstrip(g) })
                } else {
                    let target = need("target_ohm")?;
                    match eecalc::width_for_impedance(target, g, strip) {
                        Some(c) => serde_json::to_value(c),
                        None => return Err(invalid("target_ohm", "not reachable with widths between 0.01 mm and 20 × height")),
                    }
                }
            }
            "standard_value" => {
                let name = match args["series"].as_str().unwrap_or("E24") { "E12" => "E12", "E96" => "E96", _ => "E24" };
                let target = need("value")?;
                if target <= 0.0 {
                    return Err(invalid("value", "must be > 0"));
                }
                serde_json::to_value(eecalc::nearest_standard(target, name, eecalc::series(name).unwrap()))
            }
            "divider" => {
                let name = match args["series"].as_str().unwrap_or("E24") { "E12" => "E12", "E96" => "E96", _ => "E24" };
                let ratio = match (num("ratio"), num("vin_v"), num("vout_v")) {
                    (Some(r), _, _) => r,
                    (None, Some(vin), Some(vout)) if vin > 0.0 => vout / vin,
                    _ => return Err(invalid("ratio", "give ratio, or vin_v and vout_v")),
                };
                if !(0.0..1.0).contains(&ratio) || ratio == 0.0 {
                    return Err(invalid("ratio", "must be between 0 and 1"));
                }
                serde_json::to_value(eecalc::divider(ratio, num("total_ohm").unwrap_or(100_000.0), eecalc::series(name).unwrap(), name, num("vin_v")))
            }
            other => return Err(invalid("calculation", &format!("unknown calculation '{other}'"))),
        }
        .unwrap_or(Value::Null))
    })();
    Ok(match value {
        Ok(v) => CallToolResult::json(&json!({ "calculation": calc, "output": v })),
        Err(e) => e,
    })
}

fn project_arg(args: &Value) -> Result<PathBuf, CallToolResult> {
    let raw = args["project"].as_str().ok_or_else(|| invalid("project", "required"))?;
    project_stem(Path::new(raw)).map_err(|e| invalid("project", &e.to_string()))
}

fn handle_get_design_intent(args: &Value) -> anyhow::Result<CallToolResult> {
    let stem = match project_arg(args) {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };
    let (intent, text) = load_intent(&stem)?;
    Ok(CallToolResult::json(&json!({
        "path": intent_path(&stem),
        "exists": text.is_some(),
        "intent": intent,
        "stackup_used": intent.stackup(),
        "requirements": requirements(&intent),
    })))
}

fn handle_set_design_intent(args: &Value) -> anyhow::Result<CallToolResult> {
    let stem = match project_arg(args) {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };
    let (mut intent, text) = load_intent(&stem)?;
    if let Some(nets) = args["nets"].as_object() {
        for (name, value) in nets {
            if value.is_null() {
                intent.nets.remove(name);
                continue;
            }
            let mut merged = intent.nets.get(name).map(|n| serde_json::to_value(n).unwrap_or(json!({}))).unwrap_or(json!({}));
            if let (Some(base), Some(update)) = (merged.as_object_mut(), value.as_object()) {
                for (k, v) in update {
                    if v.is_null() { base.remove(k); } else { base.insert(k.clone(), v.clone()); }
                }
            }
            match serde_json::from_value::<NetIntent>(merged) {
                Ok(n) => {
                    intent.nets.insert(name.clone(), n);
                }
                Err(e) => return Ok(invalid("nets", &format!("net '{name}': {e}"))),
            }
        }
    }
    let key = |t: &Tie| (t.part.clone(), t.to.clone(), t.pin.clone());
    if let Some(remove) = args["remove_ties"].as_array() {
        for r in remove {
            let part = r["part"].as_str().unwrap_or_default();
            let to = r["to"].as_str().unwrap_or_default();
            let pin = r["pin"].as_str().map(str::to_string);
            intent.ties.retain(|t| key(t) != (part.to_string(), to.to_string(), pin.clone()));
        }
    }
    if let Some(ties) = args["ties"].as_array() {
        for t in ties {
            match serde_json::from_value::<Tie>(t.clone()) {
                Ok(tie) => {
                    intent.ties.retain(|existing| key(existing) != key(&tie));
                    intent.ties.push(tie);
                }
                Err(e) => return Ok(invalid("ties", &e.to_string())),
            }
        }
    }
    if !args["stackup"].is_null() {
        match serde_json::from_value::<Stackup>(args["stackup"].clone()) {
            Ok(s) => intent.stackup = Some(s),
            Err(e) => return Ok(invalid("stackup", &e.to_string())),
        }
    }
    if let Some(notes) = args["notes"].as_array() {
        intent.notes = notes.iter().filter_map(|n| n.as_str().map(str::to_string)).collect();
    }
    if let Err(reason) = intent.validate() {
        return Ok(invalid("nets", &reason));
    }
    let path = intent_path(&stem);
    let content = serde_json::to_string_pretty(&intent)? + "\n";
    if let Some(refusal) = write_checked(&path, text.as_deref(), &content, "set_design_intent")? {
        return Ok(refusal);
    }
    Ok(CallToolResult::json(&json!({
        "path": path,
        "nets": intent.nets.len(),
        "ties": intent.ties.len(),
        "requirements": requirements(&intent),
    })))
}

fn handle_apply_design_intent(args: &Value) -> anyhow::Result<CallToolResult> {
    let stem = match project_arg(args) {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };
    let dry_run = args["dry_run"].as_bool().unwrap_or(true);
    let (intent, text) = load_intent(&stem)?;
    if text.is_none() {
        return Ok(invalid("project", "no design intent yet; call set_design_intent first"));
    }
    let board_tree = std::fs::read_to_string(stem.with_extension("kicad_pcb"))
        .ok()
        .and_then(|text| konnect_sexp::parse_sexp(&text).ok());
    let reqs = requirements_on(&intent, board_tree.as_ref());
    let rules = dru_rules(&intent, &reqs);
    let pro_path = stem.with_extension("kicad_pro");
    let dru_path = stem.with_extension("kicad_dru");
    let pro_text = std::fs::read_to_string(&pro_path).with_context(|| format!("read {}", pro_path.display()))?;
    let mut project: Value = serde_json::from_str(&pro_text).with_context(|| format!("parse {}", pro_path.display()))?;
    let classes = merge_netclasses(&mut project, &intent, &reqs)?;
    let dru_text = std::fs::read_to_string(&dru_path).ok();
    let new_dru = merge_dru(dru_text.as_deref(), &rules);
    let warnings: Vec<String> = reqs.iter().flat_map(|r| r.warnings.iter().map(move |w| format!("{}: {w}", r.net))).collect();
    let plan = json!({
        "dry_run": dry_run,
        "netclasses": classes,
        "dru_rules": rules.keys().collect::<Vec<_>>(),
        "dru_text": if dry_run { Value::String(new_dru.clone()) } else { Value::Null },
        "requirements": reqs,
        "warnings": warnings,
    });
    if dry_run {
        return Ok(CallToolResult::json(&plan));
    }
    if let Some(refusal) = crate::tools::pcb_board::refuse_if_project_locked_by_kicad(&pro_path, "design-intent rules") {
        return Ok(refusal);
    }
    let pro_new = serde_json::to_string_pretty(&project)? + "\n";
    if let Some(refusal) = crate::tools::pcb_board::write_project_if_unchanged(&pro_path, "apply_design_intent", pro_text, pro_new)? {
        return Ok(refusal);
    }
    if let Some(refusal) = write_checked(&dru_path, dru_text.as_deref(), &new_dru, "apply_design_intent")? {
        return Ok(refusal);
    }
    let mut done = plan;
    done["written"] = json!([pro_path, dru_path]);
    Ok(CallToolResult::json(&done))
}

async fn read_intent_board(args: &Value, ctx: &ToolContext, what: &str) -> anyhow::Result<Result<(DesignIntent, PathBuf, SexpNode, String), CallToolResult>> {
    let stem = match project_arg(args) {
        Ok(s) => s,
        Err(e) => return Ok(Err(e)),
    };
    let source = match BoardSource::from_args(args) {
        Ok(s) => s,
        Err(e) => return Ok(Err(e)),
    };
    let (intent, text) = load_intent(&stem)?;
    if text.is_none() {
        return Ok(Err(invalid("project", "no design intent yet; call set_design_intent first")));
    }
    let board = stem.with_extension("kicad_pcb");
    let read = board_source::read_board(ctx, &board, source, what, |client, document| client.save_document_to_string_in(document)).await?;
    let (snapshot, from) = match read {
        BoardRead::Live(text) => (text, "live".to_string()),
        BoardRead::Saved(saved) => (
            std::fs::read_to_string(&board).with_context(|| format!("read {}", board.display()))?,
            format!("saved file ({})", saved.detail()),
        ),
        BoardRead::Refused(result) => return Ok(Err(result)),
    };
    let tree = konnect_sexp::parse_sexp(&snapshot).context("parse board")?;
    Ok(Ok((intent, board, tree, from)))
}

async fn handle_plan_tie_placement(args: &Value, ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let clearance = args["clearance"].as_f64().unwrap_or(0.2);
    let (intent, board, tree, from) = match read_intent_board(args, ctx, "tie placement plan").await? {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let (plans, notes) = plan_ties(&tree, &intent, clearance);
    Ok(CallToolResult::json(&json!({
        "board": board,
        "source": from,
        "placements": plans.iter().map(|p| json!({"reference": p.reference, "x": p.x, "y": p.y, "rotation": p.rotation})).collect::<Vec<_>>(),
        "detail": plans,
        "notes": notes,
        "next_step": "set_component_placements with `placements`, then check_design_intent",
    })))
}

async fn handle_check_design_intent(args: &Value, ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let stem = match project_arg(args) {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };
    let source = match BoardSource::from_args(args) {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };
    let (intent, text) = load_intent(&stem)?;
    if text.is_none() {
        return Ok(invalid("project", "no design intent yet; call set_design_intent first"));
    }
    let board = stem.with_extension("kicad_pcb");
    let read = board_source::read_board(ctx, &board, source, "design intent check", |client, document| {
        client.save_document_to_string_in(document)
    })
    .await?;
    let (snapshot, from) = match read {
        BoardRead::Live(text) => (text, "live".to_string()),
        BoardRead::Saved(saved) => (
            std::fs::read_to_string(&board).with_context(|| format!("read {}", board.display()))?,
            format!("saved file ({})", saved.detail()),
        ),
        BoardRead::Refused(result) => return Ok(result),
    };
    let tree = konnect_sexp::parse_sexp(&snapshot).context("parse board")?;
    let checks = check_board(&tree, &intent);
    let count = |s: &str| checks.iter().filter(|c| c.status == s).count();
    Ok(CallToolResult::json(&json!({
        "board": board,
        "source": from,
        "summary": { "pass": count("pass"), "fail": count("fail"), "unverified": count("unverified") },
        "checks": checks,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usb_intent() -> DesignIntent {
        let mut intent = DesignIntent { schema: 1, ..Default::default() };
        intent.nets.insert("USB_D_P".into(), NetIntent { role: "diff_pair".into(), pair: Some("USB_D_N".into()), impedance_ohm: Some(90.0), max_skew_mm: Some(1.0), priority: Some(1), ..Default::default() });
        intent.nets.insert("USB_D_N".into(), NetIntent { role: "diff_pair".into(), pair: Some("USB_D_P".into()), impedance_ohm: Some(90.0), max_skew_mm: Some(1.0), priority: Some(1), ..Default::default() });
        intent.nets.insert("+5V".into(), NetIntent { role: "power".into(), current_a: Some(1.0), max_drop_mv: Some(100.0), source: Some("J1.A4".into()), priority: Some(2), ..Default::default() });
        intent.nets.insert("GND".into(), NetIntent { role: "plane".into(), ..Default::default() });
        intent.ties.push(Tie { part: "C1".into(), to: "U1".into(), pin: Some("1".into()), max_mm: 2.0, why: None });
        intent
    }

    #[test]
    fn validation_catches_unpaired_pairs_and_bad_roles() {
        assert!(usb_intent().validate().is_ok());
        let mut bad = usb_intent();
        bad.nets.get_mut("USB_D_N").unwrap().pair = Some("OTHER".into());
        assert!(bad.validate().is_err());
        let mut bad = usb_intent();
        bad.nets.get_mut("+5V").unwrap().role = "rail".into();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn requirements_follow_the_calculators_and_priority() {
        let reqs = requirements(&usb_intent());
        assert_eq!(reqs[0].priority, Some(1));
        let pair = reqs.iter().find(|r| r.net == "USB_D_P").unwrap();
        let w = pair.track_width_mm.unwrap();
        // 90 Ω differential on 0.2104 mm 7628 with a 0.15 mm gap.
        let z = eecalc::microstrip(LineGeometry { width_mm: w, height_mm: 0.2104, thickness_mm: eecalc::MM_PER_OZ, er: 4.4, gap_mm: Some(0.15) });
        assert!((z.result.zdiff_ohm.unwrap() - 90.0).abs() < 0.5, "{w} → {:?}", z.result);
        let power = reqs.iter().find(|r| r.net == "+5V").unwrap();
        assert!(power.track_width_mm.unwrap() >= 0.15);
    }

    #[test]
    fn dru_rules_are_named_and_merged_without_touching_user_rules() {
        let intent = usb_intent();
        let rules = dru_rules(&intent, &requirements(&intent));
        assert!(rules.keys().any(|k| k == "intent: pair USB_D_N/USB_D_P"));
        assert!(rules.keys().any(|k| k == "intent: width +5V"));
        let user = "(version 1)\n(rule \"my rule\"\n  (constraint clearance (min 0.3mm)))\n(rule \"intent: width OLD\"\n  (constraint track_width (min 1mm)))\n";
        let merged = merge_dru(Some(user), &rules);
        assert!(merged.contains("(rule \"my rule\""));
        assert!(!merged.contains("intent: width OLD"), "stale intent rule removed: {merged}");
        assert!(merged.contains("diff_pair_gap"));
        // Re-applying is idempotent.
        assert_eq!(merge_dru(Some(&merged), &rules), merged);
    }

    #[test]
    fn fab_rules_coexist_with_intent_and_user_rules() {
        let user = "(version 1)\n(rule \"mine\"\n  (constraint clearance (min 0.3mm)))\n";
        let intent = usb_intent();
        let with_intent = merge_dru(Some(user), &dru_rules(&intent, &requirements(&intent)));
        let fab = fab_rules("jlcpcb").unwrap();
        let both = merge_fab_rules(Some(&with_intent), &fab);
        assert!(both.contains("(rule \"mine\""));
        assert!(both.contains("(rule \"intent: width +5V\""), "intent rules untouched by the fab merge");
        assert_eq!(managed_rule_names(&both, FAB_PREFIX).len(), fab.len());
        // Fab floors come before every other rule, whichever was applied first,
        // so KiCad's later-rule-wins order lets intent and user rules override them.
        let fab_first = |text: &str| {
            let last_fab = text.rfind("(rule \"fab: ").unwrap();
            let first_other = ["(rule \"mine\"", "(rule \"intent: "].iter().filter_map(|n| text.find(n)).min().unwrap();
            last_fab < first_other
        };
        assert!(fab_first(&both));
        // Re-applying is idempotent, and re-merging intent keeps the fab rules first.
        assert_eq!(merge_fab_rules(Some(&both), &fab), both);
        let again = merge_dru(Some(&both), &dru_rules(&intent, &requirements(&intent)));
        assert_eq!(managed_rule_names(&again, FAB_PREFIX).len(), fab.len());
        assert!(fab_first(&again));
        let reversed = merge_dru(Some(&merge_fab_rules(Some(user), &fab)), &dru_rules(&intent, &requirements(&intent)));
        assert!(fab_first(&reversed));
    }

    #[test]
    fn netclasses_are_cloned_from_default_and_patterns_go_first() {
        let mut project = json!({ "net_settings": {
            "classes": [{ "name": "Default", "clearance": 0.2, "track_width": 0.2, "via_diameter": 0.6, "via_drill": 0.3, "wire_width": 6 }],
            "netclass_patterns": [{ "netclass": "Power", "pattern": "+5V" }, { "netclass": "Power", "pattern": "+3V3" }]
        }});
        let intent = usb_intent();
        let classes = merge_netclasses(&mut project, &intent, &requirements(&intent)).unwrap();
        assert!(classes.contains(&"intent_+5V".to_string()));
        let pats = project["net_settings"]["netclass_patterns"].as_array().unwrap();
        assert!(pats[0]["netclass"].as_str().unwrap().starts_with("intent_"));
        assert!(!pats.iter().any(|p| p["netclass"] == "Power" && p["pattern"] == "+5V"), "the intent class replaces the old +5V pattern");
        assert!(pats.iter().any(|p| p["pattern"] == "+3V3"));
        let class = project["net_settings"]["classes"].as_array().unwrap().iter().find(|c| c["name"] == "intent_USB_D_P").unwrap();
        assert_eq!(class["wire_width"], 6, "cloned from Default");
        assert!(class["diff_pair_gap"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn a_far_capacitor_is_planned_beside_its_pin_without_overlapping_the_ic() {
        let board = konnect_sexp::parse_sexp(
            "(kicad_pcb
              (gr_rect (start 0 0) (end 60 40) (layer \"Edge.Cuts\"))
              (footprint \"QFP\" (layer \"F.Cu\") (at 20 20) (property \"Reference\" \"U1\")
                (fp_rect (start -4 -4) (end 4 4) (layer \"F.CrtYd\"))
                (pad \"1\" smd rect (at -3.5 0) (size 1.0 0.3) (layers \"F.Cu\") (net \"VDD\"))
                (pad \"2\" smd rect (at -3.5 0.5) (size 1.0 0.3) (layers \"F.Cu\") (net \"SIG\")))
              (footprint \"C\" (layer \"F.Cu\") (at 50 30) (property \"Reference\" \"C1\")
                (fp_rect (start -1.5 -0.75) (end 1.5 0.75) (layer \"F.CrtYd\"))
                (pad \"1\" smd rect (at -0.8 0) (size 0.9 0.95) (layers \"F.Cu\") (net \"VDD\"))
                (pad \"2\" smd rect (at 0.8 0) (size 0.9 0.95) (layers \"F.Cu\") (net \"GND\"))))",
        )
        .unwrap();
        let mut intent = DesignIntent { schema: 1, ..Default::default() };
        intent.ties.push(Tie { part: "C1".into(), to: "U1".into(), pin: Some("1".into()), max_mm: 2.0, why: None });
        let (plans, notes) = plan_ties(&board, &intent, 0.2);
        assert_eq!(plans.len(), 1, "{notes:?}");
        let p = &plans[0];
        assert!(p.meets && p.distance_mm <= 2.0, "{p:?}");
        // Left of the pin row (outward is −x), so its courtyard clears U1's.
        assert!(p.x < 16.0, "{p:?}");
        // Re-checking the moved board agrees with the planner.
        let moved = konnect_sexp::parse_sexp(&format!(
            "(kicad_pcb
              (footprint \"QFP\" (layer \"F.Cu\") (at 20 20) (property \"Reference\" \"U1\")
                (pad \"1\" smd rect (at -3.5 0) (size 1.0 0.3) (layers \"F.Cu\") (net \"VDD\")))
              (footprint \"C\" (layer \"F.Cu\") (at {} {} {}) (property \"Reference\" \"C1\")
                (pad \"1\" smd rect (at -0.8 0 {}) (size 0.9 0.95) (layers \"F.Cu\") (net \"VDD\"))))",
            p.x, p.y, p.rotation, p.rotation
        ))
        .unwrap();
        let tie = check_board(&moved, &intent).into_iter().find(|c| c.kind == "tie").unwrap();
        assert_eq!(tie.status, "pass", "{tie:?}");
    }

    #[test]
    fn checks_measure_ties_lengths_widths_and_drop() {
        let board = konnect_sexp::parse_sexp(
            "(kicad_pcb
              (footprint \"U\" (layer \"F.Cu\") (at 10 10) (property \"Reference\" \"U1\")
                (pad \"1\" smd rect (at 0 0) (size 0.5 0.5) (layers \"F.Cu\") (net \"+5V\")))
              (footprint \"C\" (layer \"F.Cu\") (at 11.5 10) (property \"Reference\" \"C1\")
                (pad \"1\" smd rect (at 0 0) (size 0.5 0.5) (layers \"F.Cu\") (net \"+5V\")))
              (footprint \"J\" (layer \"F.Cu\") (at 40 10) (property \"Reference\" \"J1\")
                (pad \"A4\" smd rect (at 0 0) (size 0.5 0.5) (layers \"F.Cu\") (net \"+5V\")))
              (segment (start 40 10) (end 10 10) (width 0.2) (layer \"F.Cu\") (net \"+5V\"))
              (segment (start 20 20) (end 30 20) (width 0.2) (layer \"F.Cu\") (net \"USB_D_P\"))
              (segment (start 20 21) (end 32 21) (width 0.2) (layer \"F.Cu\") (net \"USB_D_N\"))
              (zone (net \"GND\") (layers \"In1.Cu\") (polygon (pts (xy 0 0) (xy 50 0) (xy 50 50)))))",
        )
        .unwrap();
        let checks = check_board(&board, &usb_intent());
        let get = |kind: &str| checks.iter().find(|c| c.kind == kind).unwrap();
        let tie = get("tie");
        assert_eq!(tie.status, "pass", "{tie:?}");
        assert!((tie.measured["distance_mm"].as_f64().unwrap() - 1.0).abs() < 1e-6);
        let skew = get("skew");
        assert_eq!(skew.status, "fail");
        assert!((skew.measured["skew_mm"].as_f64().unwrap() - 2.0).abs() < 1e-6);
        let drop = get("voltage_drop");
        // 30 mm of 0.2 mm 1 oz copper ≈ 75.6 mΩ at 25 °C, ≈ 78.6 mΩ at
        // 35 °C (25 °C + the default 10 °C rise); × 1 A. C1 sits on the
        // trace mid-segment (a T), so it is reached, not silently dropped.
        assert_eq!(drop.status, "pass", "{drop:?}");
        assert_eq!(drop.measured["unreachable_pads"], json!([]));
        let mv = drop.measured["drop_mv"].as_f64().unwrap();
        assert!((mv - 78.6).abs() < 1.0, "{mv}");
        let width = checks.iter().find(|c| c.kind == "track_width" && c.subject == "+5V").unwrap();
        assert_eq!(width.status, "fail", "0.2 mm is under the IPC-2221 width for 1 A (≈0.3 mm): {width:?}");
        assert_eq!(get("plane").status, "pass");
    }
}

#[cfg(test)]
mod per_layer_tests {
    use super::*;

    fn intent() -> DesignIntent {
        let mut intent = DesignIntent { schema: 1, ..Default::default() };
        for (net, pair) in [("USB_D_P", "USB_D_N"), ("USB_D_N", "USB_D_P")] {
            intent.nets.insert(net.into(), NetIntent { role: "diff_pair".into(), pair: Some(pair.into()), impedance_ohm: Some(90.0), ..Default::default() });
        }
        intent.nets.insert("+5V".into(), NetIntent { role: "power".into(), current_a: Some(2.0), ..Default::default() });
        intent.nets.insert("GND".into(), NetIntent { role: "plane".into(), ..Default::default() });
        intent.nets.insert("+3V3".into(), NetIntent { role: "plane".into(), ..Default::default() });
        intent
    }

    /// GND on In1 covers x 0–30 only; +3V3 on In2 covers the board.
    fn board(segments: &str) -> SexpNode {
        konnect_sexp::parse_sexp(&format!(
            r#"(kicad_pcb (general (thickness 1.6))
              (layers (0 "F.Cu" signal) (4 "In1.Cu" signal) (6 "In2.Cu" signal) (2 "B.Cu" signal))
              (zone (net "GND") (layer "In1.Cu") (polygon (pts (xy 0 0) (xy 30 0) (xy 30 50) (xy 0 50))))
              (zone (net "+3V3") (layer "In2.Cu") (polygon (pts (xy 0 0) (xy 60 0) (xy 60 50) (xy 0 50))))
              {segments})"#
        ))
        .unwrap()
    }

    fn seg(net: &str, layer: &str, w: f64, (x1, y1): (f64, f64), (x2, y2): (f64, f64)) -> String {
        format!(r#"(segment (start {x1} {y1}) (end {x2} {y2}) (width {w}) (layer "{layer}") (net "{net}"))"#)
    }

    #[test]
    fn rules_are_per_layer_and_power_has_an_inner_minimum() {
        let tree = board("");
        let reqs = requirements_on(&intent(), Some(&tree));
        let pair = reqs.iter().find(|r| r.net == "USB_D_P").unwrap();
        let layers: Vec<&str> = pair.impedance_widths.iter().map(|w| w.layer.as_str()).collect();
        assert_eq!(layers, ["F.Cu", "B.Cu"], "inner layers are planes here");
        let rules = dru_rules(&intent(), &reqs);
        assert!(rules.contains_key("intent: impedance USB_D_N/USB_D_P F.Cu"));
        assert!(rules.contains_key("intent: impedance USB_D_N/USB_D_P B.Cu"));
        assert!(rules["intent: impedance USB_D_N/USB_D_P F.Cu"].contains("(layer \"F.Cu\")"));
        let power = reqs.iter().find(|r| r.net == "+5V").unwrap();
        let (outer, inner) = (power.current_width_mm.unwrap(), power.inner_current_width_mm.unwrap());
        // Inner: half the k and half the copper (0.5 oz) — several times wider.
        assert!(inner > 3.0 * outer, "{outer} vs {inner}");
        assert_eq!(power.track_width_mm, Some(inner), "net class carries the width valid on every layer");
        assert!(rules["intent: width +5V"].contains("(layer outer)"));
        assert!(rules["intent: width +5V inner"].contains("(layer inner)"));
    }

    #[test]
    fn impedance_is_measured_per_segment_with_the_real_gap_and_plane_coverage() {
        let tree = board("");
        let reqs = requirements_on(&intent(), Some(&tree));
        let w = reqs.iter().find(|r| r.net == "USB_D_P").unwrap().impedance_widths[0].opt_mm;
        let pitch = w + 0.15;
        let routed = |gap_pitch: f64| {
            board(&[
                seg("USB_D_P", "F.Cu", w, (1.0, 10.0), (20.0, 10.0)),
                seg("USB_D_N", "F.Cu", w, (1.0, 10.0 + gap_pitch), (20.0, 10.0 + gap_pitch)),
                // Beyond x = 30 there is no GND under F.Cu.
                seg("USB_D_P", "F.Cu", w, (31.0, 10.0), (40.0, 10.0)),
                seg("USB_D_N", "F.Cu", w, (31.0, 10.0 + gap_pitch), (40.0, 10.0 + gap_pitch)),
            ]
            .join("\n"))
        };
        let good = check_board(&routed(pitch), &intent());
        let z = good.iter().find(|c| c.kind == "impedance" && c.subject == "USB_D_P").unwrap();
        assert_eq!(z.status, "pass", "{:?}", z.measured);
        assert!((z.measured["in_tolerance_mm"].as_f64().unwrap() - 19.0).abs() < 1e-6);
        assert!((z.measured["unmodelled_mm"].as_f64().unwrap() - 9.0).abs() < 1e-6);
        assert!((z.measured["gap_min_mm"].as_f64().unwrap() - 0.15).abs() < 1e-6);
        // The same widths 0.6 mm apart: the rule's gap no longer stands in for
        // the routed one, so Zdiff is out of tolerance.
        let wide = check_board(&routed(w + 0.6), &intent());
        let z = wide.iter().find(|c| c.kind == "impedance" && c.subject == "USB_D_P").unwrap();
        assert_eq!(z.status, "fail", "{:?}", z.measured);
    }

    #[test]
    fn an_inner_run_at_the_outer_width_fails() {
        let tree = board("");
        let reqs = requirements_on(&intent(), Some(&tree));
        let outer = reqs.iter().find(|r| r.net == "+5V").unwrap().current_width_mm.unwrap();
        let ok = check_board(&board(&seg("+5V", "F.Cu", outer, (0.0, 0.0), (5.0, 0.0))), &intent());
        assert_eq!(ok.iter().find(|c| c.kind == "track_width").unwrap().status, "pass");
        let bad = check_board(&board(&seg("+5V", "In1.Cu", outer, (0.0, 0.0), (5.0, 0.0))), &intent());
        let c = bad.iter().find(|c| c.kind == "track_width").unwrap();
        assert_eq!(c.status, "fail");
        assert!((c.measured["undersized_length_mm"].as_f64().unwrap() - 5.0).abs() < 1e-6);
    }
}

#[cfg(test)]
mod calc_input_tests {
    use super::handle_engineering_calc;
    use serde_json::json;

    #[test]
    fn non_physical_inputs_are_refused() {
        for args in [
            json!({"calculation": "impedance", "width_mm": 0.2, "height_mm": 0.0, "er": 4.4}),
            json!({"calculation": "impedance", "width_mm": 0.2, "height_mm": 0.2, "er": 0.5}),
            json!({"calculation": "width_for_current", "current_a": 1.0, "temp_rise_c": -5.0}),
            json!({"calculation": "trace_resistance", "length_mm": 10.0, "width_mm": -1.0}),
            json!({"calculation": "width_for_current", "current_a": "lots"}),
        ] {
            assert!(handle_engineering_calc(&args).unwrap().is_error, "{args}");
        }
        assert!(!handle_engineering_calc(&json!({"calculation": "impedance", "width_mm": 0.2, "height_mm": 0.2, "er": 4.4})).unwrap().is_error);
    }
}

#[cfg(test)]
mod voltage_drop_tests {
    use super::*;

    fn intent() -> DesignIntent {
        let mut intent = DesignIntent { schema: 1, ..Default::default() };
        intent.nets.insert("+5V".into(), NetIntent { role: "power".into(), current_a: Some(1.0), max_drop_mv: Some(500.0), source: Some("J1.1".into()), ..Default::default() });
        intent
    }

    const PADS: &str = r#"
      (layers (0 "F.Cu" signal) (2 "B.Cu" signal))
      (footprint "J" (layer "F.Cu") (at 0 0) (property "Reference" "J1")
        (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net "+5V")))
      (footprint "U" (layer "F.Cu") (at 20 0) (property "Reference" "U1")
        (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net "+5V")))"#;

    fn drop(extra: &str) -> Check {
        let tree = konnect_sexp::parse_sexp(&format!("(kicad_pcb (general (thickness 1.6)) {PADS} {extra})")).unwrap();
        check_board(&tree, &intent()).into_iter().find(|c| c.kind == "voltage_drop").unwrap()
    }

    #[test]
    fn a_layer_change_needs_a_via() {
        let tracks = r#"(segment (start 0 0) (end 10 0) (width 0.5) (layer "F.Cu") (net "+5V"))
                        (segment (start 10 0) (end 20 0) (width 0.5) (layer "B.Cu") (net "+5V"))"#;
        // Without a via the B.Cu half does not join: U1 is unreachable.
        let no_via = drop(tracks);
        assert_eq!(no_via.status, "unverified", "{no_via:?}");
        assert_eq!(no_via.measured["unreachable_pads"], json!(["U1.1"]));
        // An SMD pad on F.Cu does not touch B.Cu either: a via is needed at U1 too.
        let with_vias = drop(&format!(
            r#"{tracks}
               (via (at 10 0) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu") (net "+5V"))
               (via (at 20 0) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu") (net "+5V"))"#
        ));
        assert_eq!(with_vias.status, "pass", "{with_vias:?}");
        // Two barrels add a little over the 20 mm of 0.5 mm track.
        let tracks_only = eecalc::trace_resistance(20.0, 0.5, 1.0, 35.0, None).result.resistance_ohm * 1000.0;
        let mv = with_vias.measured["drop_mv"].as_f64().unwrap();
        assert!(mv > tracks_only && mv < tracks_only * 1.2, "{mv} vs {tracks_only}");
    }

    #[test]
    fn a_tee_into_the_middle_of_a_segment_conducts() {
        let tracks = r#"(segment (start 0 0) (end 20 0) (width 0.5) (layer "F.Cu") (net "+5V"))
                        (footprint "C" (layer "F.Cu") (at 10 5) (property "Reference" "C1")
                          (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net "+5V")))
                        (segment (start 10 5) (end 10 0) (width 0.3) (layer "F.Cu") (net "+5V"))"#;
        let d = drop(tracks);
        assert_eq!(d.status, "pass", "{d:?}");
        assert_eq!(d.measured["unreachable_pads"], json!([]));
    }
}

/// KiCad's own DRC as the oracle for the generated rules: each fab rule fires
/// on a seeded violation and stays silent on a near-miss at least 10 µm clear
/// of its limit; an intent rule still wins over the fab floor. Needs
/// kicad-cli (KICAD_CLI or PATH): `cargo test -p konnect-core --lib -- --ignored drc_oracle`.
#[cfg(test)]
mod drc_oracle {
    use super::*;

    fn kicad_cli() -> String {
        std::env::var("KICAD_CLI").unwrap_or_else(|_| "kicad-cli".to_string())
    }

    /// The nets used: A, B and +5V.
    const HEADER: &str = r#"(kicad_pcb (version 20241229) (generator "konnect-test") (generator_version "9.0")
  (general (thickness 1.6) (legacy_teardrops no))
  (paper "A4")
  (layers (0 "F.Cu" signal) (1 "In1.Cu" signal) (2 "In2.Cu" signal) (31 "B.Cu" signal)
    (37 "F.SilkS" user "F.Silkscreen") (39 "F.Mask" user) (44 "Edge.Cuts" user))
  (setup (pad_to_mask_clearance 0))
  (net 0 "") (net 1 "A") (net 2 "B") (net 3 "+5V")
  (gr_rect (start 0 0) (end 100 100) (stroke (width 0.1) (type default)) (fill no) (layer "Edge.Cuts"))
"#;

    fn seg(x1: f64, y1: f64, x2: f64, y2: f64, w: f64, layer: &str, net: u32) -> String {
        format!("  (segment (start {x1} {y1}) (end {x2} {y2}) (width {w}) (layer \"{layer}\") (net {net}))\n")
    }

    fn via(x: f64, y: f64, size: f64, drill: f64, net: u32) -> String {
        format!("  (via (at {x} {y}) (size {size}) (drill {drill}) (layers \"F.Cu\" \"B.Cu\") (net {net}))\n")
    }

    fn footprint(r: &str, x: f64, y: f64, pads: &str) -> String {
        format!("  (footprint \"T:{r}\" (layer \"F.Cu\") (at {x} {y}) (property \"Reference\" \"{r}\" (at 0 -3) (layer \"F.Fab\")) {pads})\n")
    }

    /// One board seeding every fab rule; `e` is how far each item sits from
    /// the limit: negative violates it, positive clears it.
    fn seeded(e: f64) -> String {
        let mut b = HEADER.to_string();
        // Track width, outer (0.127) and inner (0.09).
        b += &seg(10.0, 10.0, 20.0, 10.0, 0.127 + e, "F.Cu", 1);
        b += &seg(10.0, 20.0, 20.0, 20.0, 0.09 + e, "In1.Cu", 1);
        // Track spacing, outer (0.127) and inner (0.09): 0.2 / 0.1 wide tracks.
        b += &seg(30.0, 10.0, 40.0, 10.0, 0.2, "F.Cu", 1);
        b += &seg(30.0, 10.0 + 0.2 + 0.127 + e, 40.0, 10.0 + 0.2 + 0.127 + e, 0.2, "F.Cu", 2);
        b += &seg(30.0, 20.0, 40.0, 20.0, 0.1, "In1.Cu", 1);
        b += &seg(30.0, 20.0 + 0.1 + 0.09 + e, 40.0, 20.0 + 0.1 + 0.09 + e, 0.1, "In1.Cu", 2);
        // Routed edge to track (0.3): a 0.2 mm track along the top edge.
        b += &seg(50.0, 0.1 + 0.3 + e, 60.0, 0.1 + 0.3 + e, 0.2, "F.Cu", 1);
        // Hole diameter (0.2 min) and annular ring (0.075).
        b += &via(10.0, 40.0, 0.2 + e + 0.3, 0.2 + e, 1);
        b += &via(15.0, 40.0, 0.3 + 2.0 * (0.075 + e), 0.3, 1);
        // Hole to hole: different nets 0.5, same net 0.254 (0.3 drills).
        b += &via(30.0, 40.0, 0.6, 0.3, 1);
        b += &via(30.0 + 0.3 + 0.5 + e, 40.0, 0.6, 0.3, 2);
        b += &via(40.0, 40.0, 0.6, 0.3, 1);
        b += &via(40.0 + 0.3 + 0.254 + e, 40.0, 0.6, 0.3, 1);
        // Track to PTH hole (0.33): hole edge to track edge, on an inner layer
        // where the via's unused pad is removed so the hole is bare.
        b += "  (via (at 50 40) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (remove_unused_layers yes) (keep_end_layers yes) (net 2))\n";
        b += &seg(45.0, 40.0 + 0.15 + 0.33 + e + 0.1, 55.0, 40.0 + 0.15 + 0.33 + e + 0.1, 0.2, "In1.Cu", 1);
        // NPTH diameter (0.5) and track to NPTH (0.254).
        b += &footprint("H1", 10.0, 60.0, &format!("(pad \"\" np_thru_hole circle (at 0 0) (size {d} {d}) (drill {d}) (layers \"*.Cu\" \"*.Mask\"))", d = 0.5 + e));
        b += &footprint("H2", 20.0, 60.0, "(pad \"\" np_thru_hole circle (at 0 0) (size 1 1) (drill 1) (layers \"*.Cu\" \"*.Mask\"))");
        b += &seg(15.0, 60.0 + 0.5 + 0.254 + e + 0.1, 25.0, 60.0 + 0.5 + 0.254 + e + 0.1, 0.2, "F.Cu", 1);
        // Track to pad (0.2): a 1 × 1 mm SMD pad on B.
        b += &footprint("P1", 30.0, 60.0, "(pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\" \"F.Mask\") (net 2 \"B\"))");
        b += &seg(25.0, 60.0 + 0.5 + 0.2 + e + 0.1, 35.0, 60.0 + 0.5 + 0.2 + e + 0.1, 0.2, "F.Cu", 1);
        // Pad to pad, plated (0.127): two 1 mm through pads with 0.3 drills.
        let th = |n: u32, name: &str| format!("(pad \"1\" thru_hole circle (at 0 0) (size 1 1) (drill 0.3) (layers \"*.Cu\" \"*.Mask\") (net {n} \"{name}\"))");
        b += &footprint("P2", 50.0, 60.0, &th(1, "A"));
        b += &footprint("P3", 50.0 + 1.0 + 0.127 + e, 60.0, &th(2, "B"));
        // Silkscreen text (1 mm high, 0.15 thick) and pad to silkscreen (0.15).
        b += &format!("  (gr_text \"T\" (at 10 80) (layer \"F.SilkS\") (effects (font (size {h} {h}) (thickness {t}))))\n", h = 1.0 + e, t = 0.15 + e);
        b += &footprint("P4", 30.0, 80.0, "(pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\" \"F.Mask\") (net 1 \"A\"))");
        let y = 80.0 + 0.5 + 0.15 + e + 0.06;
        b += &format!("  (gr_line (start 28 {y}) (end 32 {y}) (stroke (width 0.12) (type default)) (layer \"F.SilkS\"))\n");
        b += ")\n";
        b
    }

    /// kicad-cli instances share a lock file; run them one at a time.
    static KICAD_CLI: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn drc(board: &str, dru: &str) -> Vec<String> {
        let _one_at_a_time = KICAD_CLI.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let pcb = dir.path().join("o.kicad_pcb");
        std::fs::write(&pcb, board).unwrap();
        std::fs::write(dir.path().join("o.kicad_dru"), dru).unwrap();
        let out = dir.path().join("o.json");
        let run = std::process::Command::new(kicad_cli())
            .args(["pcb", "drc", "--format", "json", "-o"])
            .arg(&out)
            .arg(&pcb)
            .output()
            .expect("kicad-cli");
        let stderr = String::from_utf8_lossy(&run.stderr);
        let errors: Vec<&str> = stderr.lines().filter(|l| l.to_lowercase().contains("error") && !l.contains("lock file")).collect();
        assert!(errors.is_empty(), "kicad-cli reported: {errors:?}");
        let report: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        report["violations"].as_array().unwrap().iter().map(|v| v["description"].as_str().unwrap_or("").to_string()).collect()
    }

    fn fired<'a>(descriptions: &'a [String], rule: &str) -> Vec<&'a String> {
        let needle = format!("rule '{rule}'");
        descriptions.iter().filter(|d| d.contains(&needle)).collect()
    }

    #[test]
    #[ignore = "needs kicad-cli"]
    fn every_fab_rule_fires_on_its_violation_and_not_on_a_near_miss() {
        let rules = fab_rules("jlcpcb").unwrap();
        let dru = merge_fab_rules(None, &rules);
        let bad = drc(&seeded(-0.02), &dru);
        let near = drc(&seeded(0.01), &dru);
        // Silkscreen thresholds clear by 50 µm: KiCad compares text sizes in
        // integer nm after its own rounding.
        let mut missing = Vec::new();
        for (name, _) in &rules {
            if fired(&bad, name).is_empty() {
                missing.push(name.clone());
            }
            let false_alarms = fired(&near, name);
            assert!(false_alarms.is_empty(), "'{name}' fired on a near-miss: {false_alarms:?}");
        }
        assert!(missing.is_empty(), "rules that did not fire on their seeded violation: {missing:?}\nall: {bad:#?}");
    }

    #[test]
    #[ignore = "needs kicad-cli"]
    fn an_intent_rule_wins_over_the_fab_floor_only_when_written_after_it() {
        let mut intent = DesignIntent { schema: 1, ..Default::default() };
        intent.nets.insert("+5V".into(), NetIntent { role: "power".into(), current_a: Some(3.0), ..Default::default() });
        let reqs = requirements_on(&intent, None);
        let intent_rules = dru_rules(&intent, &reqs);
        let fab = fab_rules("jlcpcb").unwrap();
        let board = format!("{HEADER}{}\n)\n", seg(10.0, 10.0, 20.0, 10.0, 0.5, "F.Cu", 3));
        // As written by the tools: fab floors first, intent after.
        let good = merge_dru(Some(&merge_fab_rules(None, &fab)), &intent_rules);
        assert!(!fired(&drc(&board, &good), "intent: width +5V").is_empty(), "the 3 A width rule must flag a 0.5 mm track");
        // The order the review warned about: fab last masks the intent rule.
        let masked = merge_managed_rules(Some(&merge_dru(None, &intent_rules)), &fab.into_iter().collect(), FAB_PREFIX);
        assert!(fired(&drc(&board, &masked), "intent: width +5V").is_empty(), "KiCad's later-rule-wins order changed");
    }

    #[test]
    #[ignore = "needs kicad-cli"]
    fn per_layer_intent_rules_fire_on_their_own_layer_only() {
        let mut intent = DesignIntent { schema: 1, ..Default::default() };
        intent.nets.insert("A".into(), NetIntent { role: "signal".into(), impedance_ohm: Some(50.0), ..Default::default() });
        intent.nets.insert("+5V".into(), NetIntent { role: "power".into(), current_a: Some(1.0), ..Default::default() });
        let reqs = requirements_on(&intent, None);
        let a = reqs.iter().find(|r| r.net == "A").unwrap();
        let f = a.impedance_widths.iter().find(|w| w.layer == "F.Cu").unwrap();
        let p = reqs.iter().find(|r| r.net == "+5V").unwrap();
        let (outer, inner) = (p.current_width_mm.unwrap(), p.inner_current_width_mm.unwrap());
        let dru = merge_dru(Some(&merge_fab_rules(None, &fab_rules("jlcpcb").unwrap())), &dru_rules(&intent, &reqs));
        // A at 20 µm under its F.Cu minimum; +5V at the outer width on F.Cu
        // (fine) and on In2.Cu (too narrow for an inner layer).
        let board = format!(
            "{HEADER}{}{}{})\n",
            seg(10.0, 10.0, 20.0, 10.0, f.min_mm - 0.02, "F.Cu", 1),
            seg(10.0, 30.0, 20.0, 30.0, outer + 0.01, "F.Cu", 3),
            seg(10.0, 50.0, 20.0, 50.0, outer + 0.01, "In2.Cu", 3),
        );
        let found = drc(&board, &dru);
        assert_eq!(fired(&found, "intent: impedance A F.Cu").len(), 1, "{found:#?}");
        assert!(fired(&found, "intent: width +5V").is_empty(), "outer width is enough on F.Cu: {found:#?}");
        assert_eq!(fired(&found, "intent: width +5V inner").len(), 1, "inner {inner} mm: {found:#?}");
    }
}
