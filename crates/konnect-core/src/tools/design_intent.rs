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
const STANDARDS: &[&str] = &["usb2_fs", "usb2_hs", "ethernet_100base_tx", "ethernet_1000base_t", "rmii"];
const TIE_KINDS: &[&str] = &["crystal", "core_regulator_cap", "decoupling"];
/// Layout skew budget as a fraction of the 10–90 % rise time. Johnson (EDN
/// 2004-01-22, "Common Mode Analysis of Skew"): a skew of 20 % of tr makes a
/// common-mode component of 10 %; Johnson (EDN 2002-04-18, "Your Layout is
/// Skewed"): an unspecified driver already has ≥ 10 % — so the layout gets the
/// other half.
pub(crate) const EDGE_SKEW_FRACTION: f64 = 0.1;
/// Default allowed temperature rise for current widths, °C (KiCad's calculator default).
const DEFAULT_RISE_C: f64 = 10.0;

/// Published defaults for an interface: (value, source) per field.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StandardDefaults {
    pub pair: bool,
    pub impedance_ohm: Option<(f64, &'static str)>,
    pub tolerance_pct: Option<(f64, &'static str)>,
    pub max_skew_mm: Option<(f64, &'static str)>,
    pub max_length_mm: Option<(f64, &'static str)>,
    pub rise_time_ns: Option<(f64, &'static str)>,
}

const TI_USB2: &str = "TI SPRAAR7J (2023) Table A-1: USB2.0 pair 81–99 Ω, skew within any USB2.0 pair ≤ 50 mil [docdb ti-spraar7j]";
const TI_PHY: &str = "TI SNLA387 (2021) Ethernet PHY PCB Design Layout Checklist: MDI < 2 in, 100 Ω ±10 %, matched within 50 mil (10/100M) or 20 mil (1G) [docdb ti-snla387-ethernet-phy-layout]";

pub(crate) fn standard_defaults(name: &str) -> Option<StandardDefaults> {
    // TI's 50 mil is a high-speed (480 Mb/s) layout number. A full-speed
    // pair (4 ns edges) gets its skew from the edge-rate rule instead, which
    // is tens of millimetres: applying 1.27 mm there fails good layouts.
    let usb = |rise: (f64, &'static str), skew: Option<(f64, &'static str)>| StandardDefaults {
        pair: true,
        impedance_ohm: Some((90.0, TI_USB2)),
        tolerance_pct: Some((10.0, TI_USB2)),
        max_skew_mm: skew,
        max_length_mm: None,
        rise_time_ns: Some(rise),
    };
    Some(match name {
        "usb2_hs" => usb((0.5, "USB 2.0 spec §7.1.2.2: high-speed rise time ≥ 500 ps (not re-read: usb.org blocked from this host)"), Some((1.27, TI_USB2))),
        "usb2_fs" => usb((4.0, "USB 2.0 full-speed rise time 4–20 ns, as tabulated in STM32H563 DS14258 USB FS characteristics [docdb st-ds14258-stm32h563]"), None),
        "ethernet_100base_tx" => StandardDefaults {
            pair: true,
            impedance_ohm: Some((100.0, TI_PHY)),
            tolerance_pct: Some((10.0, TI_PHY)),
            max_skew_mm: Some((1.27, TI_PHY)),
            max_length_mm: Some((50.8, TI_PHY)),
            rise_time_ns: Some((3.0, "LAN8742A datasheet 100BASE-TX TRF 3.0–5.0 ns [docdb microchip-lan8742a-ds]")),
        },
        "ethernet_1000base_t" => StandardDefaults {
            pair: true,
            impedance_ohm: Some((100.0, TI_PHY)),
            tolerance_pct: Some((10.0, TI_PHY)),
            max_skew_mm: Some((0.508, TI_PHY)),
            max_length_mm: Some((50.8, TI_PHY)),
            rise_time_ns: None,
        },
        "rmii" => StandardDefaults {
            pair: false,
            impedance_ohm: None,
            tolerance_pct: None,
            max_skew_mm: None,
            max_length_mm: Some((152.4, "RMII/MII traces < 6 in: TI SNLA079D §5.2 and SNLA387; LAN8742 routing checklist (REF_CLK < 6.0 in, matched 0.10 in) [docdb ti-snla079d-phyter-layout, microchip-lan8742-routing-checklist]")),
            rise_time_ns: None,
        },
        _ => return None,
    })
}

// ─── Schema ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DesignIntent {
    #[serde(default = "schema_version")]
    pub schema: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stackup: Option<Stackup>,
    /// Copper layers planned as reference planes (e.g. `In1.Cu`), so the
    /// impedance rules can be made before the plane pours are drawn. Layers
    /// holding a pour of a `plane` net count as well.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plane_layers: Vec<String>,
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
    /// The interface the net belongs to (`usb2_fs`, `usb2_hs`,
    /// `ethernet_100base_tx`, `ethernet_1000base_t`, `rmii`): fills the
    /// impedance, tolerance, skew and length the user leaves unset with
    /// published values. See [`standard_defaults`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standard: Option<String>,
    /// 10–90 % edge time of the signal, ns: a pair without a published skew
    /// limit gets one from it (see [`EDGE_SKEW_FRACTION`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rise_time_ns: Option<f64>,
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
    /// Pad edge to pad edge, mm. Optional when `kind` supplies a default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_mm: Option<f64>,
    /// `crystal`, `core_regulator_cap` or `decoupling`: defaults for the
    /// distance, routed path and via count. See [`tie_limits`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Routed track length from the part's pad to the pin, mm, along the
    /// shortest track path on their shared net (via barrels not counted;
    /// vias are limited by `max_vias`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_path_mm: Option<f64>,
    /// Vias on that routed path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_vias: Option<u32>,
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
                ("rise_time_ns", intent.rise_time_ns),
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
            if let Some(other) = intent.pair.as_ref().and_then(|p| self.nets.get(p)) {
                for (name, mine, theirs) in [
                    ("rise_time_ns", intent.rise_time_ns, other.rise_time_ns),
                    ("impedance_ohm", intent.impedance_ohm, other.impedance_ohm),
                    ("impedance_tolerance_pct", intent.impedance_tolerance_pct, other.impedance_tolerance_pct),
                    ("gap_mm", intent.gap_mm, other.gap_mm),
                    ("max_skew_mm", intent.max_skew_mm, other.max_skew_mm),
                ] {
                    if let (Some(a), Some(b)) = (mine, theirs) {
                        if (a - b).abs() > 1e-9 {
                            return Err(format!("net '{net}': {name} {a} differs from its pair's {b}; a pair shares one value (set it on either net)"));
                        }
                    }
                }
            }
            if let Some(standard) = &intent.standard {
                let Some(d) = standard_defaults(standard) else {
                    return Err(format!("net '{net}': standard must be one of {STANDARDS:?}"));
                };
                if d.pair != (intent.role == "diff_pair") {
                    return Err(format!(
                        "net '{net}': standard '{standard}' is for {}",
                        if d.pair { "a diff_pair" } else { "single-ended nets, not a diff_pair" }
                    ));
                }
                if let Some(other) = intent.pair.as_ref().and_then(|p| self.nets.get(p)) {
                    if other.standard.is_some() && other.standard != intent.standard {
                        return Err(format!("net '{net}': its pair has a different standard"));
                    }
                }
            }
            if let Some(source) = &intent.source {
                if source.split_once('.').is_none_or(|(r, p)| r.is_empty() || p.is_empty()) {
                    return Err(format!("net '{net}': source must be REF.PAD, e.g. U10.3"));
                }
            }
        }
        for tie in &self.ties {
            if tie.part.is_empty() || tie.to.is_empty() {
                return Err(format!("tie {} → {}: part and to are required", tie.part, tie.to));
            }
            if let Some(kind) = &tie.kind {
                if !TIE_KINDS.contains(&kind.as_str()) {
                    return Err(format!("tie {} → {}: kind must be one of {TIE_KINDS:?}", tie.part, tie.to));
                }
            } else if tie.max_mm.is_none() && tie.max_path_mm.is_none() {
                return Err(format!("tie {} → {}: give max_mm (or max_path_mm), or a kind that supplies them", tie.part, tie.to));
            }
            for (name, value) in [("max_mm", tie.max_mm), ("max_path_mm", tie.max_path_mm)] {
                positive(&format!("tie {} → {}: {name}", tie.part, tie.to), value)?;
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
            // A file edited by hand (or written by an older build) gets the
            // same checks as `set_design_intent`: an unknown kind or standard,
            // or a tie without a limit, must not resolve to "no limit".
            if let Err(reason) = intent.validate() {
                bail!("{} is not a valid design intent: {reason}; fix the file or rewrite it with set_design_intent", path.display());
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
    /// IPC-2152 minimum widths for the current, outer and inner layers.
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
    /// Where each number came from: "user" or the default's citation.
    sources: BTreeMap<String, String>,
    /// The net's intent with defaults filled in; checks use this.
    #[serde(skip)]
    effective: NetIntent,
    warnings: Vec<String>,
}

/// A diff pair's net with the pair-level fields (standard, rise time,
/// impedance, tolerance, gap, skew, length) its partner sets and it leaves unset, so
/// both halves resolve to the same limits whichever net carries them. Also
/// returns the fields taken from the partner.
fn with_pair_fields(intent: &DesignIntent, n: &NetIntent) -> (NetIntent, Vec<&'static str>) {
    let mut m = n.clone();
    let mut taken = Vec::new();
    let Some(other) = (n.role == "diff_pair").then(|| n.pair.as_ref().and_then(|p| intent.nets.get(p))).flatten() else {
        return (m, taken);
    };
    if m.standard.is_none() && other.standard.is_some() {
        m.standard = other.standard.clone();
        taken.push("standard");
    }
    for (name, slot, theirs) in [
        ("rise_time_ns", &mut m.rise_time_ns, other.rise_time_ns),
        ("impedance_ohm", &mut m.impedance_ohm, other.impedance_ohm),
        ("impedance_tolerance_pct", &mut m.impedance_tolerance_pct, other.impedance_tolerance_pct),
        ("gap_mm", &mut m.gap_mm, other.gap_mm),
        ("max_skew_mm", &mut m.max_skew_mm, other.max_skew_mm),
        ("max_length_mm", &mut m.max_length_mm, other.max_length_mm),
    ] {
        if slot.is_none() && theirs.is_some() {
            *slot = theirs;
            taken.push(name);
        }
    }
    (m, taken)
}

/// The net's intent with defaults filled in for every field the user left
/// unset, and the source of each value. User values always win.
fn resolve_net(n: &NetIntent, layers: &LayerStack, planes: &BTreeSet<String>) -> (NetIntent, BTreeMap<String, String>, Vec<String>) {
    let mut e = n.clone();
    let mut src = BTreeMap::new();
    let mut warnings = Vec::new();
    for (field, set) in [
        ("impedance_ohm", n.impedance_ohm.is_some()),
        ("impedance_tolerance_pct", n.impedance_tolerance_pct.is_some()),
        ("max_skew_mm", n.max_skew_mm.is_some()),
        ("max_length_mm", n.max_length_mm.is_some()),
        ("current_a", n.current_a.is_some()),
        ("max_temp_rise_c", n.max_temp_rise_c.is_some()),
        ("rise_time_ns", n.rise_time_ns.is_some()),
        ("gap_mm", n.gap_mm.is_some()),
    ] {
        if set {
            src.insert(field.to_string(), "user".to_string());
        }
    }
    let d = n.standard.as_deref().and_then(standard_defaults);
    let mut fill = |slot: &mut Option<f64>, field: &str, value: Option<(f64, &str)>| {
        if let (None, Some((v, why))) = (*slot, value) {
            *slot = Some(v);
            src.insert(field.to_string(), format!("default {}: {why}", n.standard.as_deref().unwrap_or("")));
        }
    };
    if let Some(d) = d {
        fill(&mut e.impedance_ohm, "impedance_ohm", d.impedance_ohm);
        fill(&mut e.impedance_tolerance_pct, "impedance_tolerance_pct", d.tolerance_pct);
        fill(&mut e.max_length_mm, "max_length_mm", d.max_length_mm);
        fill(&mut e.rise_time_ns, "rise_time_ns", d.rise_time_ns);
    }
    if n.role == "diff_pair" && n.max_skew_mm.is_none() {
        // Edge-rate bound: EDGE_SKEW_FRACTION of the rise time, as length at
        // the slowest routable signal layer's propagation velocity (largest
        // delay per mm, so the fewest mm per ps: always the conservative
        // choice; a stripline is slower than the outer microstrip). With no
        // modelled signal layer, c/√εr of the slowest dielectric, which no
        // real line is slower than.
        let edge = e.rise_time_ns.map(|tr| {
            let slowest = layers
                .copper
                .iter()
                .filter(|(l, _)| !planes.contains(l))
                .filter_map(|(l, _)| layers.model(l, planes).ok().map(|m| (m.per_mm(0.2).delay_ps_per_mm, l.clone())))
                .max_by(|a, b| a.0.total_cmp(&b.0));
            let (delay, how) = match slowest {
                Some((d, l)) => (d, format!("{l}, the slowest routable signal layer (εeff from the stack)")),
                None => {
                    let er = layers.gaps.iter().map(|g| g.er).fold(f64::NAN, f64::max);
                    let er = if er.is_finite() { er } else { 4.4 };
                    (er.sqrt() / crate::tools::layer_stack::C0_MM_PER_PS, format!("c/√εr, εr {er} (no modelled signal layer)"))
                }
            };
            let mm = EDGE_SKEW_FRACTION * tr * 1000.0 / delay;
            (mm, format!("{:.0} % of tr {tr} ns = {:.0} ps at {:.2} ps/mm ({how}); Johnson, EDN 2004 'Common Mode Analysis of Skew' (20 % of tr → 10 % common mode) and EDN 2002 'Your Layout is Skewed' (driver skew ≥ 10 % of tr) [docdb johnson-edn-2004-cm-analysis-of-skew, johnson-edn-2002-your-layout-is-skewed]", EDGE_SKEW_FRACTION * 100.0, EDGE_SKEW_FRACTION * tr * 1000.0, delay))
        });
        let published = d.and_then(|d| d.max_skew_mm);
        let chosen = match (published, edge) {
            (Some((p, why)), Some((m, how))) if p <= m => Some((p, format!("default {}: {why}; tighter than the edge-rate bound {} mm ({how})", n.standard.as_deref().unwrap_or(""), r3(m)))),
            (Some((p, why)), Some((m, how))) => Some((m, format!("edge-rate bound ({how}); tighter than the published {p} mm ({why})"))),
            (Some((p, why)), None) => Some((p, format!("default {}: {why}", n.standard.as_deref().unwrap_or("")))),
            (None, Some((m, how))) => Some((m, format!("edge-rate bound: {how}"))),
            (None, None) => None,
        };
        match chosen {
            Some((v, why)) => {
                e.max_skew_mm = Some(r3(v));
                src.insert("max_skew_mm".to_string(), why);
            }
            None => warnings.push("no skew limit: give max_skew_mm, a standard, or rise_time_ns".to_string()),
        }
    }
    if e.current_a.is_some() && e.max_temp_rise_c.is_none() {
        e.max_temp_rise_c = Some(DEFAULT_RISE_C);
        src.insert("max_temp_rise_c".to_string(), "default 10 °C rise (KiCad pcb_calculator default; IPC-2152 conservative no-plane case)".to_string());
    }
    if n.role == "power" && e.current_a.is_none() {
        warnings.push("power net without current_a: no width is derived; give current_a (e.g. the fuse or PTC trip current)".to_string());
    }
    if n.role == "diff_pair" && e.impedance_ohm.is_some() && e.gap_mm.is_none() {
        src.insert("gap_mm".to_string(), "default 0.15 mm (Konnect): the impedance width is solved for this gap".to_string());
    }
    (e, src, warnings)
}

#[cfg(test)]
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
    layers.extend(intent.plane_layers.iter().cloned());
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
            let note = format!("{} (saved board file); reference planes from the board's zones: {:?}", stack.source, planes);
            (stack, planes, note)
        }
        None => {
            let names: Vec<String> = ["F.Cu", "In1.Cu", "In2.Cu", "B.Cu"].iter().map(|s| s.to_string()).collect();
            let stack = LayerStack::from_intent(intent.stackup(), &names, 1.6);
            let planes: BTreeSet<String> = if intent.plane_layers.is_empty() {
                ["In1.Cu".to_string(), "In2.Cu".to_string()].into()
            } else {
                intent.plane_layers.iter().cloned().collect()
            };
            let note = format!("assumed default stack-up (no board file read): 4 layers, 1.6 mm, planes on {planes:?}; limits change once the board exists");
            (stack, planes, note)
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
    // Only inner layers that can carry tracks need the inner-layer width:
    // a board whose inner layers are all planes routes power on the outside.
    let has_inner = layers.copper.iter().skip(1).take(layers.copper.len().saturating_sub(2)).any(|(l, _)| !planes.contains(l));
    let mut out = Vec::new();
    for (net, user) in &intent.nets {
        let (merged, taken) = with_pair_fields(intent, user);
        let (resolved, mut sources, resolve_warnings) = resolve_net(&merged, &layers, &planes);
        for field in taken {
            // A default filled from an inherited standard keeps its citation.
            if field == "standard" || sources.get(field).is_some_and(|s| s == "user") {
                sources.insert(field.to_string(), format!("user, set on pair partner {}", user.pair.as_deref().unwrap_or("")));
            }
        }
        let n = &resolved;
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
            sources,
            effective: resolved.clone(),
            warnings: resolve_warnings,
        };
        if let Some(current) = n.current_a {
            let rise = n.max_temp_rise_c.unwrap_or(DEFAULT_RISE_C);
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
/// `net_settings` as KiCad 10.0 writes it for a new project.
fn kicad_default_net_settings() -> Value {
    json!({
        "classes": [{
            "bus_width": 12, "clearance": 0.2, "diff_pair_gap": 0.25, "diff_pair_via_gap": 0.25,
            "diff_pair_width": 0.2, "line_style": 0, "microvia_diameter": 0.3, "microvia_drill": 0.1,
            "name": "Default", "pcb_color": "rgba(0, 0, 0, 0.000)", "priority": 2147483647,
            "schematic_color": "rgba(0, 0, 0, 0.000)", "track_width": 0.2, "tuning_profile": "",
            "via_diameter": 0.6, "via_drill": 0.3, "wire_width": 6
        }],
        "meta": { "version": 5 },
        "net_colors": {},
        "netclass_assignments": {},
        "netclass_patterns": []
    })
}

fn merge_netclasses(project: &mut Value, intent: &DesignIntent, reqs: &[NetRequirement]) -> anyhow::Result<Vec<String>> {
    // A project made by `create_project` (or never opened in KiCad) has no
    // net settings yet: start from KiCad 10's own defaults.
    let root = project.as_object_mut().context("project file is not a JSON object")?;
    if !root.get("net_settings").is_some_and(Value::is_object) {
        root.insert("net_settings".to_string(), kicad_default_net_settings());
    }
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

// ─── Tie limits ─────────────────────────────────────────────────────────────

/// The electrical budget a tie's routed connection is compared with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum TieBudget {
    /// Per node (each shared net on its own): routed capacitance, pF.
    CapacitancePf(f64),
    /// Over the whole loop (supply leg + return leg): inductance, nH.
    LoopInductanceNh(f64),
}

/// A tie's limits with defaults from its kind, and where each came from.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct TieLimits {
    /// Pad edge to pad edge. `None` only when nothing resolves (an intent
    /// that never passed validation): the checks then report `unverified`,
    /// never a pass against "no limit".
    pub max_mm: Option<f64>,
    /// Routed track length per leg: the user's value, or the crystal guide.
    pub max_path_mm: Option<f64>,
    pub max_vias: Option<u32>,
    #[serde(skip)]
    pub budget: Option<TieBudget>,
    /// Length guide for a leg whose tracks have no line model (no reference
    /// plane), where the budget cannot be evaluated.
    pub unmodelled_path_mm: Option<f64>,
    pub sources: BTreeMap<String, String>,
}

impl TieLimits {
    fn budget_pf(&self) -> Option<f64> {
        match self.budget {
            Some(TieBudget::CapacitancePf(v)) => Some(v),
            _ => None,
        }
    }

    fn budget_nh(&self) -> Option<f64> {
        match self.budget {
            Some(TieBudget::LoopInductanceNh(v)) => Some(v),
            _ => None,
        }
    }

    /// The limits as the tools report them.
    fn to_json(&self) -> Value {
        json!({
            "max_mm": self.max_mm.map(r3),
            "max_path_mm": self.max_path_mm,
            "max_vias": self.max_vias,
            "budget_pf": self.budget_pf(),
            "budget_nh": self.budget_nh(),
            "unmodelled_path_mm": self.unmodelled_path_mm,
            "sources": self.sources,
        })
    }
}

/// Crystal: routed-trace capacitance budget per node, pF. AVR186 gives typical
/// PCB parasitics of 1 pF XTALIN–GND (2 pF XTALOUT); about half goes to the
/// crystal, load-capacitor and pin pads, leaving 0.5 pF for the trace.
const CRYSTAL_TRACE_PF: f64 = 0.5;
/// Crystal: routed length guide per node, mm, whatever the capacitance works
/// out to (Konnect judgement, WORKFLOW.md §5 item 6; the app notes only say
/// "as short as possible"). It also caps the budget on a board whose plane is
/// far away: there C′ is small and the 0.5 pF alone would allow 12 mm.
const CRYSTAL_PATH_MM: f64 = 6.0;
/// Decoupling capacitor: budget for the inductance of the whole connection
/// loop (supply leg + return leg: tracks, via barrels and the plane between
/// the vias), nH. Xilinx UG483 ch. 2 "Capacitor Mounting Inductance": lands,
/// traces and vias of a two-terminal capacitor contribute 0.3–4 nH
/// "depending on the specific geometry" [docdb xilinx-ug483-7series-pcb].
/// Konnect judgement: a mounting worse than the top of that range fails.
const DECOUPLING_LOOP_NH: f64 = 4.0;
/// Core-regulator capacitor (VCAP, VDDCR): half the decoupling loop budget
/// (Konnect judgement: the capacitor is inside the regulator's loop; ST and
/// Microchip only say "as close as possible", "heavy, wide trace").
const CORE_REG_LOOP_NH: f64 = 2.0;
/// Placement distance defaults, pad edge to pad edge, mm (Konnect judgement:
/// `place_decoupling` puts a 100 nF bypass capacitor within 2.5 mm; no cited
/// source gives a number).
const DECOUPLING_PLACE_MM: f64 = 2.5;
const CORE_REG_PLACE_MM: f64 = 2.5;

/// Defaults for a tie's kind on this stack; explicit tie values win.
pub(crate) fn tie_limits(tie: &Tie, layers: &LayerStack, planes: &BTreeSet<String>) -> TieLimits {
    let mut sources = BTreeMap::new();
    let outer = layers.copper.first().map(|c| c.0.clone()).unwrap_or_default();
    let model = layers.model(&outer, planes).ok();
    type Cited<T> = Option<(T, String)>;
    let mut budget = None;
    let mut unmodelled_path_mm = None;
    let (path, vias, place): (Cited<f64>, Cited<u32>, Cited<f64>) = match tie.kind.as_deref() {
        Some("crystal") => {
            budget = Some(TieBudget::CapacitancePf(CRYSTAL_TRACE_PF));
            let equivalent = match model {
                Some(m) => format!("≈ {:.1} mm of 0.2 mm track on {outer} at {:.3} pF/mm", CRYSTAL_TRACE_PF / m.per_mm(0.2).c_pf_per_mm, m.per_mm(0.2).c_pf_per_mm),
                None => format!("no line model for {outer}: only the length guide applies"),
            };
            sources.insert(
                "budget_pf".to_string(),
                format!("crystal: {CRYSTAL_TRACE_PF} pF of routed track per node, compared with Σ C′·len of the routed path ({equivalent}); Microchip AVR186 typical parasitics (XTALIN 1 pF) less the pads [docdb microchip-avr186-oscillator-layout]"),
            );
            (
                Some((CRYSTAL_PATH_MM, format!("crystal: {CRYSTAL_PATH_MM} mm length guide per node (Konnect judgement, WORKFLOW.md §5 item 6); ST AN5711 §3.1, TI AM67 §8.3.1, LAN8742 routing checklist say only: as short as possible, outer layer"))),
                Some((0, "crystal: route on the outer layer over the ground plane, no vias (TI AM67 §8.3.1; LAN8742 routing checklist)".to_string())),
                Some((CRYSTAL_PATH_MM, "default: the crystal length guide (placement distance ≤ routed path)".to_string())),
            )
        }
        Some(kind @ ("core_regulator_cap" | "decoupling")) => {
            let (nh, place_mm, guide) = if kind == "decoupling" { (DECOUPLING_LOOP_NH, DECOUPLING_PLACE_MM, 6.0) } else { (CORE_REG_LOOP_NH, CORE_REG_PLACE_MM, 3.0) };
            budget = Some(TieBudget::LoopInductanceNh(nh));
            unmodelled_path_mm = Some(guide);
            sources.insert(
                "budget_nh".to_string(),
                format!("{kind}: {nh} nH for the whole loop, supply leg + return leg, tracks + via barrels + plane (Konnect judgement; Xilinx UG483 gives 0.3–4 nH for a capacitor's lands, traces and vias [docdb xilinx-ug483-7series-pcb]; ST AN5711 §7.4/§9.3 and the LAN8742 checklist give no number)"),
            );
            sources.insert("unmodelled_path_mm".to_string(), format!("{kind}: length guide for a leg routed where no reference plane gives a line model (WORKFLOW.md §5 item 6); such a leg is never a pass"));
            (
                None,
                Some((1, format!("{kind}: at most one via on a leg routed as tracks (the cap on the other side at the pin's via); ST AN5711 Fig. 20: fewer vias. Not applied to a leg carried by a plane, whose two vias are in the inductance"))),
                Some((place_mm, format!("{kind}: default {place_mm} mm pad edge to pad edge (Konnect judgement, as place_decoupling's 100 nF default; give max_mm for a bulk capacitor)"))),
            )
        }
        _ => (None, None, None),
    };
    let max_path_mm = match (tie.max_path_mm, path) {
        (Some(v), _) => {
            sources.insert("max_path_mm".into(), "user".into());
            Some(v)
        }
        (None, Some((v, why))) => {
            sources.insert("max_path_mm".into(), why);
            Some(v)
        }
        _ => None,
    };
    let max_vias = match (tie.max_vias, vias) {
        (Some(v), _) => {
            sources.insert("max_vias".into(), "user".into());
            Some(v)
        }
        (None, Some((v, why))) => {
            sources.insert("max_vias".into(), why);
            Some(v)
        }
        _ => None,
    };
    let max_mm = match (tie.max_mm, place, max_path_mm) {
        (Some(v), _, _) => {
            sources.insert("max_mm".into(), "user".into());
            Some(v)
        }
        (None, Some((v, why)), _) => {
            sources.insert("max_mm".into(), why);
            Some(v)
        }
        (None, None, Some(v)) => {
            // The pad-edge distance can be no more than the routed path.
            sources.insert("max_mm".into(), "default: the path limit (placement distance ≤ routed path)".into());
            Some(v)
        }
        (None, None, None) => {
            sources.insert("max_mm".into(), "none: the tie gives no max_mm, max_path_mm or known kind".into());
            None
        }
    };
    TieLimits { max_mm, max_path_mm, max_vias, budget, unmodelled_path_mm, sources }
}

/// Every tie's resolved limits.
fn tie_requirements(intent: &DesignIntent, board: Option<&SexpNode>) -> Vec<Value> {
    let (layers, planes, _) = stack_and_planes(intent, board);
    intent
        .ties
        .iter()
        .map(|t| {
            let mut row = tie_limits(t, &layers, &planes).to_json();
            row["part"] = json!(t.part);
            row["to"] = json!(t.to);
            row["pin"] = json!(t.pin);
            row["kind"] = json!(t.kind);
            row
        })
        .collect()
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

/// Filled pieces of `net`'s zones per layer, as the board file holds them
/// (`filled_polygon`). Empty when no zone of the net is filled: a file saved
/// before the fill, or changed since. KiCad removes or keeps unconnected
/// islands by the zone's own setting; every piece left counts.
fn pour_pieces(tree: &SexpNode, net: &str) -> BTreeMap<String, usize> {
    let mut pieces = BTreeMap::new();
    for zone in tree.find_all("zone") {
        let name = zone.find_str("net_name").or_else(|| zone.find("net").and_then(konnect_sexp::net::net_name));
        if name != Some(net) {
            continue;
        }
        for fill in zone.find_all("filled_polygon") {
            if let Some(layer) = fill.find_str("layer") {
                *pieces.entry(layer.to_string()).or_insert(0) += 1;
            }
        }
    }
    pieces
}

fn length(segments: &[Segment]) -> f64 {
    segments.iter().map(|(a, b, _, _)| (b.0 - a.0).hypot(b.1 - a.1)).sum()
}

fn r3(v: f64) -> f64 {
    // `+ 0.0` turns −0 (an empty sum) into 0.
    (v * 1000.0).round() / 1000.0 + 0.0
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

/// One conducting piece of a net's routed copper, between two graph nodes.
#[derive(Debug, Clone)]
enum EdgeKind {
    /// A straight run of track on one layer (arcs arrive pre-sampled).
    Track { len: f64, width: f64, layer: String },
    /// A via barrel between two consecutive copper layers; `via` indexes the
    /// net's vias so a path can count distinct vias.
    Barrel { via: usize, height: f64, drill: f64 },
    /// A pad joining a node that lies on it (no length).
    Pad,
}

/// A net's routed copper as a graph. Nodes are per layer; a via joins the
/// layers it spans through its barrel; a surface pad joins its own layer, a
/// through pad every layer. Each pad is a node of its own, named `REF.PAD`.
/// Zones are not modelled.
///
/// Copper that overlaps is joined the way KiCad's connectivity joins it, not
/// only where coordinates coincide:
/// - a track end, or a via centre, lying within another track's copper (its
///   half width) splits that track there and is joined to the split point by
///   a short `Track` edge as long as its offset from the centreline;
/// - a track end landing on a via's annular ring is joined to the via centre
///   by a `Track` edge as long as its distance from it;
/// - a track crossing a pad (its centreline enters the pad's rectangle, or
///   the pad's centre lies in the track's copper) is split inside the pad and
///   joined to it, whether or not the track ends there.
struct NetGraph {
    edges: Vec<(usize, usize, EdgeKind)>,
    /// Per node: (neighbour, edge index).
    adj: Vec<Vec<(usize, usize)>>,
    pads: Vec<(String, usize)>,
    /// Per node: its copper layer (`None` for a pad node).
    layer: Vec<Option<String>>,
    /// The net's vias by the index `EdgeKind::Barrel::via` uses: (x, y).
    vias: Vec<(f64, f64)>,
}

impl NetGraph {
    fn pad(&self, name: &str) -> Option<usize> {
        self.pads.iter().find(|(n, _)| n == name).map(|(_, node)| *node)
    }

    /// Multi-source Dijkstra with `weight` per edge: each node's distance and
    /// the edge it was reached by.
    fn shortest(&self, starts: &[usize], weight: impl Fn(&EdgeKind) -> f64) -> (Vec<f64>, Vec<Option<usize>>) {
        let count = self.adj.len();
        let mut dist = vec![f64::INFINITY; count];
        let mut via: Vec<Option<usize>> = vec![None; count];
        #[derive(PartialEq)]
        struct State(f64, usize);
        impl Eq for State {}
        impl PartialOrd for State {
            fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(o)) }
        }
        impl Ord for State {
            fn cmp(&self, o: &Self) -> std::cmp::Ordering { o.0.total_cmp(&self.0) }
        }
        let mut heap = BinaryHeap::new();
        for &s in starts {
            dist[s] = 0.0;
            heap.push(State(0.0, s));
        }
        while let Some(State(d, u)) = heap.pop() {
            if d > dist[u] {
                continue;
            }
            for &(v, e) in &self.adj[u] {
                let nd = d + weight(&self.edges[e].2);
                if nd < dist[v] {
                    dist[v] = nd;
                    via[v] = Some(e);
                    heap.push(State(nd, v));
                }
            }
        }
        (dist, via)
    }

    /// The path from a start to `node`, start first, following `via` back:
    /// the start node and each (edge, node reached).
    fn path(&self, via: &[Option<usize>], node: usize) -> (usize, Vec<(&EdgeKind, usize)>) {
        let mut out = Vec::new();
        let mut at = node;
        while let Some(e) = via[at] {
            let (a, b, kind) = &self.edges[e];
            out.push((kind, at));
            at = if *a == at { *b } else { *a };
            if out.len() > self.edges.len() {
                break;
            }
        }
        out.reverse();
        (at, out)
    }
}

/// Slack on every "lies within the copper" test, mm (1 µm: coordinates in a
/// board file carry six decimals, sampled arcs a little less).
const JOIN_TOLERANCE_MM: f64 = 0.001;

/// The part of the segment `a → b` inside a convex polygon, as fractions
/// (0 at `a`, 1 at `b`) of its length; `None` when it stays outside.
fn clip_to_convex(a: (f64, f64), b: (f64, f64), polygon: &[(f64, f64)]) -> Option<(f64, f64)> {
    let n = polygon.len();
    if n < 3 {
        return None;
    }
    let area: f64 = (0..n).map(|i| polygon[i].0 * polygon[(i + 1) % n].1 - polygon[(i + 1) % n].0 * polygon[i].1).sum();
    let sign = if area >= 0.0 { 1.0 } else { -1.0 };
    let d = (b.0 - a.0, b.1 - a.1);
    let (mut t0, mut t1) = (0.0_f64, 1.0_f64);
    for i in 0..n {
        let (p, q) = (polygon[i], polygon[(i + 1) % n]);
        let e = (q.0 - p.0, q.1 - p.1);
        // Inside the edge's half-plane when num + t·den ≥ 0.
        let num = sign * (e.0 * (a.1 - p.1) - e.1 * (a.0 - p.0));
        let den = sign * (e.0 * d.1 - e.1 * d.0);
        if den.abs() < 1e-12 {
            if num < 0.0 {
                return None;
            }
        } else if den > 0.0 {
            t0 = t0.max(-num / den);
        } else {
            t1 = t1.min(-num / den);
        }
    }
    (t1 > t0).then_some((t0, t1))
}

fn net_graph(net: &str, copper: &NetCopper, pads: &[CopperPad], vias: &[konnect_sexp::board::Via], layers: &LayerStack) -> NetGraph {
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
    let pad_centre = |pad: &CopperPad| (pad.corners.iter().map(|c| c.0).sum::<f64>() / 4.0, pad.corners.iter().map(|c| c.1).sum::<f64>() / 4.0);
    // Anchors per layer: the points other copper connects at, each with the
    // radius of copper around it that a track *end* may land in (a via's
    // annular ring; nothing for a bare track end).
    let mut anchors: HashMap<&str, Vec<((f64, f64), f64)>> = HashMap::new();
    for (a, b, _, layer) in &copper.segments {
        anchors.entry(layer.as_str()).or_default().extend([(*a, 0.0), (*b, 0.0)]);
    }
    let via_spans: Vec<Vec<String>> = net_vias.iter().map(|v| span(v)).collect();
    for (v, layers_of) in net_vias.iter().zip(&via_spans) {
        for l in layers_of {
            if let Some(list) = anchors.get_mut(l.as_str()) {
                list.push(((v.x, v.y), v.size / 2.0));
            }
        }
    }
    let dist = |p: (f64, f64), q: (f64, f64)| (p.0 - q.0).hypot(p.1 - q.1);
    let mut edges: Vec<(usize, usize, EdgeKind)> = Vec::new();
    // (pad index, node) for every node made where a track crosses a pad.
    let mut pad_hits: BTreeSet<(usize, usize)> = BTreeSet::new();
    for (a, b, w, layer) in &copper.segments {
        let len = dist(*a, *b);
        if len < 1e-9 {
            continue;
        }
        let (ux, uy) = ((b.0 - a.0) / len, (b.1 - a.1) / len);
        let at = |t: f64| (a.0 + ux * t, a.1 + uy * t);
        let interior = |t: f64| t > 1e-3 && t < len - 1e-3;
        let mut cuts: Vec<f64> = vec![0.0, len];
        // (position along the segment, the anchor it joins, their distance).
        let mut joins: Vec<(f64, (f64, f64), f64)> = Vec::new();
        for &(p, reach) in anchors.get(layer.as_str()).into_iter().flatten() {
            let t = ((p.0 - a.0) * ux + (p.1 - a.1) * uy).clamp(0.0, len);
            let off = dist(p, at(t));
            if off <= w / 2.0 + JOIN_TOLERANCE_MM {
                // The anchor lies in this track's copper.
                if interior(t) {
                    cuts.push(t);
                }
                joins.push((t, p, off));
            } else {
                // This track's end lands in the copper around the anchor.
                let (da, db) = (dist(p, *a), dist(p, *b));
                let (t, d) = if da <= db { (0.0, da) } else { (len, db) };
                if d <= reach + JOIN_TOLERANCE_MM {
                    joins.push((t, p, d));
                }
            }
        }
        // Pads the track crosses: split inside the pad so a node lies on it.
        let mut on_pads: Vec<(f64, usize)> = Vec::new();
        for (k, pad) in net_pads.iter().enumerate() {
            if pad.smd && *layer != pad.layer {
                continue;
            }
            let centre = pad_centre(pad);
            let t_centre = ((centre.0 - a.0) * ux + (centre.1 - a.1) * uy).clamp(0.0, len);
            let t = match clip_to_convex(*a, *b, &pad.corners) {
                // Where the track runs inside the pad: nearest the centre.
                Some((t0, t1)) => t_centre.clamp(t0 * len, t1 * len),
                None if dist(centre, at(t_centre)) <= w / 2.0 + JOIN_TOLERANCE_MM => t_centre,
                None => continue,
            };
            if interior(t) {
                cuts.push(t);
            }
            on_pads.push((t, k));
        }
        cuts.sort_by(f64::total_cmp);
        cuts.dedup_by(|x, y| (*x - *y).abs() < 1e-3);
        let nearest_cut = |t: f64| cuts.iter().copied().min_by(|x, y| (x - t).abs().total_cmp(&(y - t).abs())).unwrap_or(0.0);
        for pair in cuts.windows(2) {
            let (t0, t1) = (pair[0], pair[1]);
            let (i0, i1) = (node(layer, at(t0), &mut ids), node(layer, at(t1), &mut ids));
            edges.push((i0, i1, EdgeKind::Track { len: t1 - t0, width: *w, layer: layer.clone() }));
        }
        for (t, p, d) in joins {
            let (i0, i1) = (node(layer, at(nearest_cut(t)), &mut ids), node(layer, p, &mut ids));
            if i0 != i1 {
                edges.push((i0, i1, EdgeKind::Track { len: d, width: *w, layer: layer.clone() }));
            }
        }
        for (t, k) in on_pads {
            pad_hits.insert((k, node(layer, at(nearest_cut(t)), &mut ids)));
        }
    }
    // Via barrels between consecutive spanned layers.
    for (k, (v, layers_of)) in net_vias.iter().zip(&via_spans).enumerate() {
        for pair in layers_of.windows(2) {
            let (i, j) = (layers.index(&pair[0]).unwrap_or(0), layers.index(&pair[1]).unwrap_or(0));
            let height = layers.gaps.get(i.min(j)).map_or(0.2, |g| g.thickness_mm);
            let (a, b) = (node(&pair[0], (v.x, v.y), &mut ids), node(&pair[1], (v.x, v.y), &mut ids));
            edges.push((a, b, EdgeKind::Barrel { via: k, height, drill: v.drill }));
        }
    }
    // Pads join every node on their layers that lies on them.
    let endpoint_list: Vec<(NodeKey, usize)> = ids.iter().map(|(k, v)| (k.clone(), *v)).collect();
    let mut pad_nodes: Vec<(String, usize)> = Vec::new();
    let first_pad_node = ids.len();
    for (k, pad) in net_pads.iter().enumerate() {
        let pad_node = first_pad_node + k;
        pad_nodes.push((format!("{}.{}", pad.reference, pad.number), pad_node));
        for ((layer, key), endpoint) in &endpoint_list {
            if pad.smd && layer != pad.layer {
                continue;
            }
            let p = (key.0 as f64 / 100.0, key.1 as f64 / 100.0);
            if pad_hits.contains(&(k, *endpoint)) || polygon_distance(&[p, p, p], &pad.corners) <= 0.02 {
                edges.push((pad_node, *endpoint, EdgeKind::Pad));
            }
        }
    }
    let mut adj = vec![Vec::new(); ids.len() + pad_nodes.len()];
    for (e, (a, b, _)) in edges.iter().enumerate() {
        adj[*a].push((*b, e));
        adj[*b].push((*a, e));
    }
    let mut layer = vec![None; adj.len()];
    for ((l, _), id) in ids {
        layer[id] = Some(l);
    }
    NetGraph { edges, adj, pads: pad_nodes, layer, vias: net_vias.iter().map(|v| (v.x, v.y)).collect() }
}

/// Worst-case voltage drop from `source` to each pad of the net through the
/// routed tracks and vias (Dijkstra on resistance over [`net_graph`]),
/// assuming the full current flows along each path: conservative for a net
/// with several loads. Copper is taken at 25 °C plus the allowed rise. Zones
/// are not modelled.
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
    let graph = net_graph(net, copper, pads, vias, layers);
    let resistance = |e: &EdgeKind| match e {
        EdgeKind::Track { len, width, layer } => eecalc::trace_resistance(*len, *width, oz(layer), temp_c, None).result.resistance_ohm,
        EdgeKind::Barrel { height, drill, .. } => {
            let circumference = std::f64::consts::PI * (drill + VIA_PLATING_MM);
            eecalc::trace_resistance(*height, circumference, VIA_PLATING_MM / eecalc::MM_PER_OZ, temp_c, None).result.resistance_ohm
        }
        EdgeKind::Pad => 0.0,
    };
    let start = graph.pad(&format!("{src_ref}.{src_pad}")).ok_or_else(|| format!("source pad {src_ref}.{src_pad} is not on net {net}"))?;
    let (dist, _) = graph.shortest(&[start], resistance);
    let loads: Vec<&(String, usize)> = graph.pads.iter().filter(|(_, node)| *node != start).collect();
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

// ─── Routed tie legs ────────────────────────────────────────────────────────

/// μ/2π for a non-magnetic board, nH per mm (2·10⁻⁷ H/m).
const MU_OVER_2PI_NH_PER_MM: f64 = 0.2;
/// Capacitance one via adds to a node, pF: Johnson measured 0.37–0.40 pF for
/// a 10 mil via with 24 mil pads through FR-4 planes [docdb
/// johnson-sigcon-via-inductance-2]. One geometry, used as a round figure.
const VIA_PF: f64 = 0.4;

/// Inductance of `h` mm of via barrel of this drill, nH.
///
/// Johnson, "Via Inductance" [docdb johnson-sigcon-via-inductance]: a via
/// and one return via `s` apart make a loop of (μ/2π)·h·2·ln(s/r), each via
/// contributing half of the flux, so one barrel is (μ/2π)·h·ln(s/r). With no
/// known return via the return is taken at s = 2e·h, which is formula [7.9]
/// of *High-Speed Digital Design*, L = 5.08·h·[ln(4h/d) + 1] nH (inches) —
/// "a gross approximation" in Johnson's words, and the larger of the two, so
/// it is also the cap. "Via Inductance II" checks the first form against
/// measurements [docdb johnson-sigcon-via-inductance-2].
fn barrel_nh(h: f64, drill: f64, return_mm: Option<f64>) -> f64 {
    let r = (drill / 2.0).max(0.01);
    let far = 2.0 * std::f64::consts::E * h;
    let s = return_mm.map_or(far, |s| s.min(far)).max(2.0 * r);
    MU_OVER_2PI_NH_PER_MM * h * (s / r).ln()
}

/// Inductance of the plane between two vias `span` mm apart, for one leg of
/// a loop whose other conductor is `h` mm away, nH: the same flux integral,
/// (μ/2π)·h·ln(span/r) per leg. It agrees with the spreading inductance of a
/// plane pair, μ0·h per square (Xilinx UG483 Table 2-10: 130 pH/square at
/// 102 µm [docdb xilinx-ug483-7series-pcb]).
fn plane_nh(h: f64, span: f64, drill: f64) -> f64 {
    let r = (drill / 2.0).max(0.01);
    if span <= r {
        0.0
    } else {
        MU_OVER_2PI_NH_PER_MM * h * (span / r).ln()
    }
}

/// A via used by a walk: its index in the net's vias, position, the barrel
/// length traversed and its drill.
#[derive(Debug, Clone, Copy)]
struct BarrelUse {
    via: usize,
    at: (f64, f64),
    height: f64,
    drill: f64,
}

/// What a path over a net's graph adds up to.
#[derive(Debug, Clone, Default)]
struct Walk {
    /// Track length (via barrels excluded).
    path_mm: f64,
    /// Track length on layers without a line model (no reference plane).
    unmodelled_mm: f64,
    /// Σ L′·len and Σ C′·len over the modelled tracks, each at its own width.
    track_nh: f64,
    track_pf: f64,
    /// Copper layers in path order.
    layers: Vec<String>,
    barrels: Vec<BarrelUse>,
}

impl Walk {
    /// Follow `pred` back from `end`: the start node and what the path holds.
    fn of(graph: &NetGraph, pred: &[Option<usize>], end: usize, layers: &LayerStack, planes: &BTreeSet<String>) -> (usize, Walk) {
        let (start, steps) = graph.path(pred, end);
        let mut models: HashMap<&str, Option<crate::tools::layer_stack::LineModel>> = HashMap::new();
        let mut w = Walk::default();
        let barrel = |k: Option<&(&EdgeKind, usize)>| matches!(k, Some((EdgeKind::Barrel { .. }, _)));
        for (i, &(e, reached)) in steps.iter().enumerate() {
            // Copper layers in path order, including a layer a via lands on
            // straight into a pad, but not the inner layers a via passes through.
            let passing = barrel(Some(&steps[i])) && barrel(steps.get(i + 1));
            if let (Some(l), false) = (&graph.layer[reached], passing) {
                if w.layers.last() != Some(l) {
                    w.layers.push(l.clone());
                }
            }
            match e {
                EdgeKind::Track { len, width, layer } => {
                    w.path_mm += len;
                    match models.entry(layer.as_str()).or_insert_with(|| layers.model(layer, planes).ok()) {
                        Some(m) => {
                            let pm = m.per_mm(*width);
                            w.track_nh += pm.l_nh_per_mm * len;
                            w.track_pf += pm.c_pf_per_mm * len;
                        }
                        None => w.unmodelled_mm += len,
                    }
                }
                EdgeKind::Barrel { via, height, drill } => match w.barrels.iter_mut().find(|b| b.via == *via) {
                    Some(b) => b.height += height,
                    None => w.barrels.push(BarrelUse { via: *via, at: graph.vias.get(*via).copied().unwrap_or_default(), height: *height, drill: *drill }),
                },
                EdgeKind::Pad => {}
            }
        }
        (start, w)
    }
}

/// One conductor of a tie between the tied part and its target on one net:
/// routed tracks and vias all the way, or a stub and via at each end with
/// the net's plane between them.
#[derive(Debug, Clone, Serialize)]
struct TiePath {
    net: String,
    /// `tracks` or `plane`.
    carried_by: &'static str,
    from_pad: String,
    to_pad: String,
    /// Track length along the leg (via barrels and plane excluded).
    path_mm: f64,
    vias: u32,
    /// Via barrel length along the leg.
    via_barrel_mm: f64,
    /// Copper layers in path order, from the part to the pin.
    layers: Vec<String>,
    /// Plane leg: distance between the two vias that drop to the plane.
    #[serde(skip_serializing_if = "Option::is_none")]
    plane_span_mm: Option<f64>,
    /// Σ L′·len over the tracks whose layer has a line model.
    track_inductance_nh: f64,
    /// Σ over the via barrels, see [`barrel_nh`].
    via_inductance_nh: f64,
    /// Plane leg: see [`plane_nh`]. `None` on a plane leg when no second
    /// plane fixes the loop height, and then the estimate is a lower bound.
    #[serde(skip_serializing_if = "Option::is_none")]
    plane_inductance_nh: Option<f64>,
    /// Tracks + via barrels + plane. A lower bound when `unmodelled_mm` > 0.
    est_inductance_nh: f64,
    /// Σ C′·len over the modelled tracks plus [`VIA_PF`] per via.
    est_capacitance_pf: f64,
    /// Track length on layers without a line model (no reference plane),
    /// left out of both estimates.
    unmodelled_mm: f64,
    #[serde(skip)]
    barrels: Vec<BarrelUse>,
    /// Plane leg: the plane's layer.
    #[serde(skip)]
    plane_layer: Option<String>,
}

impl TiePath {
    fn new(net: &str, carried_by: &'static str, from_pad: String, to_pad: String, walk: Walk) -> Self {
        let mut vias: Vec<usize> = walk.barrels.iter().map(|b| b.via).collect();
        vias.sort_unstable();
        vias.dedup();
        let mut leg = TiePath {
            net: net.to_string(),
            carried_by,
            from_pad,
            to_pad,
            path_mm: r3(walk.path_mm),
            vias: vias.len() as u32,
            via_barrel_mm: r3(walk.barrels.iter().map(|b| b.height).sum()),
            layers: walk.layers,
            plane_span_mm: None,
            track_inductance_nh: walk.track_nh,
            via_inductance_nh: 0.0,
            plane_inductance_nh: None,
            est_inductance_nh: 0.0,
            est_capacitance_pf: r3(walk.track_pf + VIA_PF * vias.len() as f64),
            unmodelled_mm: r3(walk.unmodelled_mm),
            barrels: walk.barrels,
            plane_layer: None,
        };
        leg.estimate(&[], None);
        leg
    }

    /// Work out the inductance, given the vias of the tie's other legs (the
    /// nearest one is each barrel's return) and, for a plane leg, the
    /// distance to the conductor that closes the loop.
    fn estimate(&mut self, return_vias: &[(f64, f64)], plane_height: Option<f64>) {
        self.via_inductance_nh = self
            .barrels
            .iter()
            .map(|b| {
                let nearest = return_vias.iter().map(|v| (v.0 - b.at.0).hypot(v.1 - b.at.1)).filter(|d| *d > 1e-6).fold(f64::INFINITY, f64::min);
                barrel_nh(b.height, b.drill, nearest.is_finite().then_some(nearest))
            })
            .sum();
        let drill = self.barrels.iter().map(|b| b.drill).fold(f64::INFINITY, f64::min);
        self.plane_inductance_nh = match (self.plane_span_mm, plane_height) {
            (Some(span), Some(h)) => Some(plane_nh(h, span, if drill.is_finite() { drill } else { 0.3 })),
            _ => None,
        };
        self.est_inductance_nh = self.track_inductance_nh + self.via_inductance_nh + self.plane_inductance_nh.unwrap_or(0.0);
    }

    /// The inductance estimate leaves something out, so it is a lower bound.
    fn lower_bound_only(&self) -> bool {
        self.unmodelled_mm > 0.0 || (self.carried_by == "plane" && self.plane_inductance_nh.is_none())
    }

    fn rounded(mut self) -> Self {
        for v in [&mut self.track_inductance_nh, &mut self.via_inductance_nh, &mut self.est_inductance_nh] {
            *v = r3(*v);
        }
        self.plane_inductance_nh = self.plane_inductance_nh.map(r3);
        self
    }
}

/// Shortest routed path (by length, barrels counted at their height) from any
/// of `from` to any of `to` over `net`'s [`net_graph`]. `None` when routed copper does not join them.
fn tie_path(graph: &NetGraph, net: &str, from: &[&CopperPad], to: &[&CopperPad], layers: &LayerStack, planes: &BTreeSet<String>) -> Option<TiePath> {
    let name = |p: &CopperPad| format!("{}.{}", p.reference, p.number);
    let starts: Vec<(String, usize)> = from.iter().filter_map(|p| graph.pad(&name(p)).map(|n| (name(p), n))).collect();
    let start_nodes: Vec<usize> = starts.iter().map(|s| s.1).collect();
    let (dist, pred) = graph.shortest(&start_nodes, length_weight);
    let (to_pad, end) = to
        .iter()
        .filter_map(|p| graph.pad(&name(p)).map(|n| (name(p), n)))
        .filter(|(_, n)| dist[*n].is_finite())
        .min_by(|a, b| dist[a.1].total_cmp(&dist[b.1]))?;
    let (at, walk) = Walk::of(graph, &pred, end, layers, planes);
    let from_pad = starts.iter().find(|s| s.1 == at).map(|s| s.0.clone()).unwrap_or_default();
    Some(TiePath::new(net, "tracks", from_pad, to_pad, walk))
}

fn length_weight(e: &EdgeKind) -> f64 {
    match e {
        EdgeKind::Track { len, .. } => *len,
        EdgeKind::Barrel { height, .. } => *height,
        EdgeKind::Pad => 0.0,
    }
}

/// The leg of a tie that a plane carries: from the part's pads to the
/// nearest via that drops into the net's pour on an inner layer, across the
/// plane, and from a via in the same plane to the target's pads. `None` when
/// either end has no routed way to such a via. Zones are read by their
/// outline, so a via must lie inside it to count.
#[allow(clippy::too_many_arguments)]
fn plane_leg(
    graph: &NetGraph,
    net: &str,
    from: &[&CopperPad],
    to: &[&CopperPad],
    zones: &[konnect_sexp::board::ZoneOutline],
    layers: &LayerStack,
    planes: &BTreeSet<String>,
) -> Option<TiePath> {
    // Graph nodes where a via of the net sits in the net's pour on an inner layer.
    let mut drops: Vec<(usize, usize, &str)> = Vec::new();
    for (a, b, kind) in &graph.edges {
        let EdgeKind::Barrel { via, .. } = kind else { continue };
        let at = graph.vias.get(*via).copied()?;
        for n in [*a, *b] {
            let Some(layer) = graph.layer[n].as_deref() else { continue };
            let poured = zones.iter().any(|z| z.net.as_deref() == Some(net) && z.layers.iter().any(|l| l == layer) && point_in_polygon(at, &z.points));
            if poured && !layers.is_outer(layer) && !drops.iter().any(|d| d.0 == n) {
                drops.push((n, *via, layer));
            }
        }
    }
    let name = |p: &CopperPad| format!("{}.{}", p.reference, p.number);
    // The nearest drop from a set of pads, optionally on one layer only.
    let stub = |pads: &[&CopperPad], on: Option<&str>| -> Option<(String, Walk, usize, String)> {
        let starts: Vec<(String, usize)> = pads.iter().filter_map(|p| graph.pad(&name(p)).map(|n| (name(p), n))).collect();
        let (dist, pred) = graph.shortest(&starts.iter().map(|s| s.1).collect::<Vec<_>>(), length_weight);
        let (node, via, layer) = drops
            .iter()
            .filter(|d| dist[d.0].is_finite() && on.is_none_or(|l| l == d.2))
            .min_by(|x, y| dist[x.0].total_cmp(&dist[y.0]))?;
        let (at, walk) = Walk::of(graph, &pred, *node, layers, planes);
        let pad = starts.iter().find(|s| s.1 == at).map(|s| s.0.clone()).unwrap_or_default();
        Some((pad, walk, *via, layer.to_string()))
    };
    let (from_pad, near, via_a, plane_layer) = stub(from, None)?;
    let (to_pad, far, via_b, _) = stub(to, Some(&plane_layer))?;
    let (a, b) = (graph.vias[via_a], graph.vias[via_b]);
    let mut walk = near;
    walk.path_mm += far.path_mm;
    walk.unmodelled_mm += far.unmodelled_mm;
    walk.track_nh += far.track_nh;
    walk.track_pf += far.track_pf;
    for l in far.layers.iter().rev() {
        if walk.layers.last() != Some(l) {
            walk.layers.push(l.clone());
        }
    }
    // The same via from both ends is one barrel: its two runs, one from
    // each side down to the plane's layer, do not overlap and add up.
    for barrel in far.barrels {
        match walk.barrels.iter_mut().find(|x| x.via == barrel.via) {
            Some(x) => x.height += barrel.height,
            None => walk.barrels.push(barrel),
        }
    }
    let mut leg = TiePath::new(net, "plane", from_pad, to_pad, walk);
    leg.plane_span_mm = Some(r3((a.0 - b.0).hypot(a.1 - b.1)));
    leg.plane_layer = Some(plane_layer);
    Some(leg)
}

/// Dielectric between two copper layers of the stack, mm.
fn layer_distance(layers: &LayerStack, a: &str, b: &str) -> Option<f64> {
    let (i, j) = (layers.index(a)?, layers.index(b)?);
    Some(layers.gaps.get(i.min(j)..i.max(j))?.iter().map(|g| g.thickness_mm).sum())
}

/// One leg of a tie as judged: the measured path (when there is one) and the verdict.
struct LegResult {
    net: String,
    /// `pass`, `fail`, `unverified`, or `not_judged` (a plane net of a tie
    /// whose limit is not about it).
    status: &'static str,
    detail: String,
    leg: Option<TiePath>,
}

impl LegResult {
    fn to_json(&self) -> Value {
        let mut v = self.leg.clone().map(|l| serde_json::to_value(l.rounded()).unwrap_or(Value::Null)).filter(Value::is_object).unwrap_or_else(|| json!({ "net": self.net }));
        v["status"] = json!(self.status);
        v["detail"] = json!(self.detail);
        v
    }
}

/// The routed-path check of one tie: every leg that matters, measured, and
/// one verdict that no unmeasured leg can overturn.
///
/// - `decoupling`, `core_regulator_cap`: the loop. Every net of the part is a
///   leg to the target (the pin's net to the pin, the others to any pad of the
///   target). A leg is the lower-inductance of its routed track path and its
///   path through the net's plane. The legs' inductances add up and are
///   compared with the budget. A leg that cannot be measured leaves the check
///   `unverified`, unless what was measured already exceeds the budget.
/// - `crystal` and ties with only `max_path_mm` / `max_vias`: every shared
///   net is a leg and each must meet the limits on its own (a crystal's two
///   nodes each have their budget). Plane nets are not judged, unless the tie
///   names a pin on one.
#[allow(clippy::too_many_arguments)]
fn tie_path_check(
    tie: &Tie,
    limits: &TieLimits,
    subject: &str,
    part: &[&CopperPad],
    target: &[&CopperPad],
    all_pads: &[CopperPad],
    intent: &DesignIntent,
    copper: &BTreeMap<String, NetCopper>,
    vias: &[konnect_sexp::board::Via],
    zones: &[konnect_sexp::board::ZoneOutline],
    layers: &LayerStack,
    planes: &BTreeSet<String>,
) -> Check {
    let empty = NetCopper { segments: vec![], has_zone: false };
    let mut required = limits.to_json();
    if let Some(o) = required.as_object_mut() {
        o.remove("max_mm");
    }
    let done = |status: &'static str, measured: Value, detail: String| Check { kind: "tie_path", subject: subject.to_string(), status, measured, required: required.clone(), detail };
    let pin_nets: BTreeSet<&str> = target.iter().filter_map(|p| p.net.as_deref()).collect();
    let part_nets: BTreeSet<&str> = part.iter().filter_map(|p| p.net.as_deref()).collect();
    if !part_nets.iter().any(|n| pin_nets.contains(n)) {
        return done("unverified", Value::Null, "the part shares no net with the pin, so there is no path to measure".to_string());
    }
    let inductive = limits.budget_nh().is_some();
    let inner_pour = |net: &str| zones.iter().any(|z| z.net.as_deref() == Some(net) && z.layers.iter().any(|l| layers.index(l).is_some() && !layers.is_outer(l)));
    let is_plane_net = |net: &str| intent.nets.get(net).is_some_and(|n| n.role == "plane") || inner_pour(net);
    fn on_net<'a>(pads: &[&'a CopperPad], net: &str) -> Vec<&'a CopperPad> {
        pads.iter().copied().filter(|p| p.net.as_deref() == Some(net)).collect()
    }
    let mut results: Vec<LegResult> = Vec::new();
    // Nets whose zone might carry a leg that was measured over tracks only.
    let mut unmeasured_zone: Vec<String> = Vec::new();
    for net in &part_nets {
        let shared = pin_nets.contains(net);
        if !inductive && !shared {
            continue;
        }
        let from = on_net(part, net);
        // The pin's own net goes to the pin; a return net to any pad of the target.
        let to: Vec<&CopperPad> = if shared { on_net(target, net) } else { all_pads.iter().filter(|p| p.reference == tie.to && p.net.as_deref() == Some(*net)).collect() };
        let c = copper.get(*net).unwrap_or(&empty);
        if !inductive && tie.pin.is_none() && is_plane_net(net) {
            results.push(LegResult { net: net.to_string(), status: "not_judged", detail: "a plane net: its connection is each pad's via to the plane, not a routed path between the two parts".to_string(), leg: None });
            continue;
        }
        if to.is_empty() {
            results.push(LegResult { net: net.to_string(), status: "unverified", detail: format!("{} has no pad on {net}, so this leg of the loop cannot be measured", tie.to), leg: None });
            continue;
        }
        let graph = net_graph(net, c, all_pads, vias, layers);
        let tracks = tie_path(&graph, net, &from, &to, layers, planes);
        let plane = if inductive { plane_leg(&graph, net, &from, &to, zones, layers, planes) } else { None };
        // Copper that could carry the leg without being measured: any zone
        // for a tie measured over tracks only; for a loop, a pour on an outer
        // layer, or an inner pour a through-hole pad sits in directly (an
        // inner pour is otherwise reached only through vias, which
        // `plane_leg` follows).
        let outer_pour = zones.iter().any(|z| z.net.as_deref() == Some(*net) && z.layers.iter().any(|l| layers.is_outer(l)));
        let through_hole = from.iter().chain(&to).any(|p| !p.smd);
        if c.has_zone && plane.is_none() && (!inductive || outer_pour || through_hole) {
            unmeasured_zone.push(net.to_string());
        }
        let leg = match (tracks, plane) {
            (Some(t), Some(p)) => Some(if p.est_inductance_nh < t.est_inductance_nh { p } else { t }),
            (t, p) => t.or(p),
        };
        match leg {
            Some(leg) => results.push(LegResult { net: net.to_string(), status: "pass", detail: String::new(), leg: Some(leg) }),
            None if c.has_zone => results.push(LegResult {
                net: net.to_string(),
                status: "unverified",
                detail: if inductive {
                    format!("no routed track path, and no via of {net} inside its inner-layer pour is reached by tracks from both pads (an outer-layer pour or a through-hole pad in the plane is not measured)")
                } else {
                    format!("no routed track path joins the part and the pin; net {net} has a zone that may carry the connection (zone paths are not measured)")
                },
                leg: None,
            }),
            None => results.push(LegResult { net: net.to_string(), status: "unverified", detail: "no path found in the track graph between the part and the target (not routed, or joined in a way the graph does not model)".to_string(), leg: None }),
        }
    }

    // Inductance of each leg, now that every leg's vias are known.
    let via_sets: Vec<Vec<(f64, f64)>> = results.iter().map(|r| r.leg.as_ref().map(|l| l.barrels.iter().map(|b| b.at).collect()).unwrap_or_default()).collect();
    let plane_layers: Vec<Option<String>> = results.iter().map(|r| r.leg.as_ref().and_then(|l| l.plane_layer.clone())).collect();
    for (i, result) in results.iter_mut().enumerate() {
        let Some(leg) = result.leg.as_mut() else { continue };
        let others: Vec<(f64, f64)> = via_sets.iter().enumerate().filter(|(j, _)| *j != i).flat_map(|(_, v)| v.iter().copied()).collect();
        // The loop height of a plane leg: to the nearest other plane, be it
        // another leg's plane or a reference plane of the stack.
        let height = leg.plane_layer.as_ref().and_then(|mine| {
            plane_layers
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .filter_map(|(_, l)| l.as_deref())
                .chain(planes.iter().map(String::as_str))
                .filter(|l| *l != mine.as_str())
                .filter_map(|l| layer_distance(layers, mine, l))
                .min_by(f64::total_cmp)
        });
        leg.estimate(&others, height);
    }

    let mut over: Vec<String> = Vec::new();
    let mut open: Vec<String> = Vec::new();
    let mut measured = json!({});
    if inductive {
        let budget = limits.budget_nh().unwrap_or(f64::INFINITY);
        let legs: Vec<&TiePath> = results.iter().filter_map(|r| r.leg.as_ref()).collect();
        let total: f64 = legs.iter().map(|l| l.est_inductance_nh).sum();
        if total > budget {
            over.push(format!("loop inductance {} nH > {budget} nH", r3(total)));
        }
        for leg in &legs {
            if let Some(max) = limits.max_path_mm.filter(|m| leg.path_mm > *m) {
                over.push(format!("{}: path {} mm > {max} mm", leg.net, leg.path_mm));
            }
            if let Some(max) = limits.max_vias.filter(|m| leg.carried_by == "tracks" && leg.vias > *m) {
                over.push(format!("{}: {} vias > {max}", leg.net, leg.vias));
            }
            if leg.unmodelled_mm > 0.0 {
                match limits.unmodelled_path_mm.filter(|m| leg.path_mm > *m) {
                    Some(guide) => over.push(format!("{}: {} mm of track has no reference plane and the path {} mm exceeds the {guide} mm guide", leg.net, leg.unmodelled_mm, leg.path_mm)),
                    None => open.push(format!("{}: {} mm of track has no reference plane, so its inductance is not in the estimate", leg.net, leg.unmodelled_mm)),
                }
            } else if leg.lower_bound_only() {
                open.push(format!("{}: no second plane fixes the loop height, so the plane between the vias is not in the estimate", leg.net));
            }
        }
        for r in results.iter().filter(|r| r.leg.is_none()) {
            open.push(format!("{}: {}", r.net, r.detail));
        }
        measured["loop_inductance_nh"] = json!(r3(total));
    } else {
        for r in results.iter_mut() {
            let Some(leg) = r.leg.as_ref() else {
                if r.status == "unverified" {
                    open.push(format!("{}: {}", r.net, r.detail));
                }
                continue;
            };
            let mut leg_over = Vec::new();
            if let Some(max) = limits.budget_pf().filter(|m| leg.est_capacitance_pf > *m) {
                leg_over.push(format!("{} pF > {max} pF", leg.est_capacitance_pf));
            }
            if let Some(max) = limits.max_path_mm.filter(|m| leg.path_mm > *m) {
                leg_over.push(format!("path {} mm > {max} mm", leg.path_mm));
            }
            if let Some(max) = limits.max_vias.filter(|m| leg.vias > *m) {
                leg_over.push(format!("{} vias > {max}", leg.vias));
            }
            if !leg_over.is_empty() {
                r.status = "fail";
                r.detail = leg_over.join(", ");
                over.push(format!("{}: {}", r.net, r.detail));
            }
        }
        if !results.iter().any(|r| r.leg.is_some() || r.status == "unverified") {
            open.push("every net the part shares with the target is a plane net; name a pin to check a routed path".to_string());
        }
    }
    // The leg the top-level fields describe: the worst judged one, else the first measured.
    let governing = results
        .iter()
        .filter(|r| r.leg.is_some())
        .max_by(|a, b| (a.status == "fail").cmp(&(b.status == "fail")).then(a.leg.as_ref().map_or(0.0, |l| l.path_mm).total_cmp(&b.leg.as_ref().map_or(0.0, |l| l.path_mm))));
    if let Some(leg) = governing.and_then(|r| r.leg.clone()) {
        if let (Some(m), Value::Object(fields)) = (measured.as_object_mut(), serde_json::to_value(leg.rounded()).unwrap_or(Value::Null)) {
            m.extend(fields);
        }
    }
    // A zone could only shorten a leg, so a pass stands; a failure over a
    // net whose zone was not measured is not proven.
    let status = if !over.is_empty() {
        if unmeasured_zone.is_empty() { "fail" } else { "unverified" }
    } else if !open.is_empty() {
        "unverified"
    } else {
        "pass"
    };
    if inductive {
        for r in results.iter_mut().filter(|r| r.leg.is_some()) {
            r.status = "measured";
        }
    }
    measured["legs"] = Value::Array(results.iter().map(LegResult::to_json).collect());
    let mut detail = match (inductive, status) {
        (true, _) => "loop = every net of the part to the target; each leg is the lower-inductance of its routed tracks and its path through the net's plane; L = Σ L′·len of the tracks at their own width and layer + via barrels (Johnson: (μ/2π)·h·ln(s/r), return at the nearest via of another leg, at most 2e·h) + the plane between the vias".to_string(),
        (false, _) => "every shared net is a leg and each must meet the limits; C = Σ C′·len of the tracks at their own width and layer + 0.4 pF per via".to_string(),
    };
    if !over.is_empty() {
        detail.push_str(&format!("; exceeds the limit: {}", over.join("; ")));
    }
    if !open.is_empty() {
        detail.push_str(&format!("; not verified: {}", open.join("; ")));
    }
    if !unmeasured_zone.is_empty() {
        detail.push_str(&format!("; net {} also has a zone that is not measured and may join the pads more directly", unmeasured_zone.join(", ")));
    }
    done(status, measured, detail)
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

/// How the partner net runs beside `(a, b)` on the same layer: the
/// edge-to-edge gap (length-weighted) and the length of `(a, b)` that has a
/// partner alongside. Partner segments must be parallel within 5°, overlap in
/// projection, and lie within `max_gap` — beyond about three dielectric
/// heights the lines barely couple and the coupled-line fit no longer holds.
/// Overlaps from several partner segments (either side of a length-matching
/// bump) add up; overlapping intervals count once.
fn coupled_gap(a: (f64, f64), b: (f64, f64), w: f64, layer: &str, partner: &NetCopper, max_gap: f64) -> Option<(f64, f64)> {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = dx.hypot(dy);
    if len < 1e-6 {
        return None;
    }
    let (ux, uy) = (dx / len, dy / len);
    let mut spans: Vec<(f64, f64, f64)> = Vec::new(); // (from, to, gap) along (a, b)
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
        let (s0, s1) = { let (x, y) = (along(*p), along(*q)); (x.min(y).max(0.0), x.max(y).min(len)) };
        if s1 - s0 <= 1e-6 {
            continue;
        }
        let mid = ((p.0 + q.0) / 2.0, (p.1 + q.1) / 2.0);
        let distance = ((mid.0 - a.0) * uy - (mid.1 - a.1) * ux).abs();
        let gap = distance - (w + pw) / 2.0;
        if gap > 0.0 && gap <= max_gap {
            spans.push((s0, s1, gap));
        }
    }
    if spans.is_empty() {
        return None;
    }
    // Union of the intervals, keeping the nearest partner where they overlap.
    spans.sort_by(|x, y| x.0.total_cmp(&y.0));
    let mut covered = 0.0;
    let mut weighted = 0.0;
    let mut reach = 0.0_f64;
    for (s0, s1, gap) in &spans {
        let start = s0.max(reach);
        if *s1 > start {
            covered += s1 - start;
            weighted += (s1 - start) * gap;
            reach = *s1;
        }
    }
    (covered > 1e-6).then(|| (weighted / covered, covered))
}

/// The dielectric height that scales coupling: to the plane for a
/// microstrip, to the nearer plane for a stripline.
fn model_height(model: &crate::tools::layer_stack::LineModel) -> f64 {
    match *model {
        crate::tools::layer_stack::LineModel::Microstrip { h_mm, .. } => h_mm,
        crate::tools::layer_stack::LineModel::Stripline { h1_mm, h2_mm, .. } => h1_mm.min(h2_mm),
    }
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
            Some(other) => match coupled_gap(*a, *b, *w, layer, other, 3.0 * model_height(&model)) {
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
        let limits = tie_limits(tie, &layers, &planes);
        let subject = format!("{} → {}{}", tie.part, tie.to, tie.pin.as_deref().map(|p| format!(".{p}")).unwrap_or_default());
        let part: Vec<&CopperPad> = pads.iter().filter(|p| p.reference == tie.part).collect();
        let target: Vec<&CopperPad> = pads
            .iter()
            .filter(|p| p.reference == tie.to && tie.pin.as_deref().is_none_or(|pin| p.number == pin))
            .collect();
        if part.is_empty() || target.is_empty() {
            checks.push(Check { kind: "tie", subject, status: "unverified", measured: Value::Null, required: json!({"max_mm": limits.max_mm.map(r3)}), detail: "part or pin not on the board".to_string() });
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
            subject: subject.clone(),
            // No limit is never a pass.
            status: match limits.max_mm {
                Some(max) if d <= max => "pass",
                Some(_) => "fail",
                None => "unverified",
            },
            measured: json!({"distance_mm": r3(d), "same_side": same_side}),
            required: json!({"max_mm": limits.max_mm.map(r3), "sources": limits.sources}),
            detail: tie.why.clone().map_or(note.to_string(), |w| format!("{w}; {note}")),
        });
        if limits.max_path_mm.is_some() || limits.max_vias.is_some() || limits.budget.is_some() {
            checks.push(tie_path_check(tie, &limits, &subject, &part, &target, &pads, intent, &copper, &vias, &zones, &layers, &planes));
        }
    }

    // An intent net the board does not have is a typo or a missing sheet
    // prefix: KiCad names local nets `/NAME`. Its rules would never match.
    let board_nets: BTreeSet<String> = konnect_sexp::net::collect_net_keys(tree).into_iter().collect();
    for net in intent.nets.keys() {
        if !board_nets.is_empty() && !board_nets.contains(net) {
            let hint = [format!("/{net}"), net.trim_start_matches('/').to_string()]
                .into_iter()
                .find(|candidate| candidate != net && board_nets.contains(candidate));
            checks.push(Check {
                kind: "net_on_board",
                subject: net.clone(),
                status: "fail",
                measured: Value::Null,
                required: json!({"exists": true}),
                detail: match hint {
                    Some(h) => format!("no net '{net}' on the board; did you mean '{h}'? (KiCad names local nets with the sheet path)"),
                    None => format!("no net '{net}' on the board; its rules and checks match nothing"),
                },
            });
        }
    }
    for r in &reqs {
        let n = &r.effective;
        let net_copper = copper.get(&r.net).unwrap_or(&empty);
        let routed = !net_copper.segments.is_empty();
        if n.role == "plane" {
            // The pour as the board file holds it filled: pieces per layer.
            let pieces = pour_pieces(tree, &r.net);
            let cut: Vec<String> = pieces.iter().filter(|(l, n)| **n > 1 && layers.index(l).is_some() && !layers.is_outer(l)).map(|(l, n)| format!("{l}: {n} pieces")).collect();
            let narrowest = net_copper.segments.iter().map(|s| s.2).fold(f64::INFINITY, f64::min);
            let track_mm = length(&net_copper.segments);
            let (status, detail) = if !net_copper.has_zone {
                ("fail", "a plane net needs a zone".to_string())
            } else if pieces.is_empty() {
                ("unverified", "the zone is not filled in this board file, so it joins nothing yet: refill the zones, save, and check again".to_string())
            } else if !cut.is_empty() {
                (
                    "unverified",
                    format!(
                        "the pour is cut into pieces on an inner layer ({}); the pieces are joined only through vias to another layer or by the net's tracks{}; whether they are joined at all is DRC's unconnected-items check, and the current those links carry is not computed",
                        cut.join(", "),
                        if routed { format!(" ({} mm of track, narrowest {} mm)", r3(track_mm), r3(narrowest)) } else { String::new() }
                    ),
                )
            } else {
                ("pass", "a zone of the net, filled in one piece on every inner layer it is on".to_string())
            };
            checks.push(Check {
                kind: "plane",
                subject: r.net.clone(),
                status,
                measured: json!({
                    "has_zone": net_copper.has_zone,
                    "filled_pieces": pieces,
                    "track_length_mm": r3(track_mm),
                    "narrowest_track_mm": routed.then(|| r3(narrowest)),
                }),
                required: json!({"has_zone": true, "inner_layer_pieces": 1}),
                detail,
            });
            // A plane net's tracks still carry its current where they are the
            // only link: with `current_a` the width check below applies to them.
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
                    detail: "IPC-2152 width for the current on each segment's layer (inner layers from the inner copper weight); up to 1 mm of narrower copper (pad necks) passes here, DRC still lists it".to_string(),
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
    let (layers, planes, _) = stack_and_planes(intent, Some(tree));
    for tie in &intent.ties {
        let label = format!("{} → {}{}", tie.part, tie.to, tie.pin.as_deref().map(|p| format!(".{p}")).unwrap_or_default());
        let Some(max_mm) = tie_limits(tie, &layers, &planes).max_mm else {
            notes.push(json!({"tie": label, "status": "skipped", "reason": "the tie has no distance limit"}));
            continue;
        };
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
        if distance_now <= max_mm {
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
            while s <= max_mm + 4.0 {
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
                    max_mm,
                    meets: d <= max_mm,
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
             range and warnings. calculation: width_for_current (IPC-2152; method: ipc2221 for the old chart), trace_resistance \
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
                    "method": { "type": "string", "enum": ["ipc2152", "ipc2221"], "default": "ipc2152", "description": "width_for_current only" },
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
             max_length_mm, max_skew_mm, clearance_mm, standard, rise_time_ns, why}}; a null value removes the net; \
             unset values are filled from `standard` and physics, and `requirements[].sources` names each \
             number's origin. ties: [{part, to, pin, max_mm, kind: crystal|core_regulator_cap|decoupling, \
             max_path_mm, max_vias, why}] replace ties with the same part/to/pin. stackup: \
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
                                "standard": { "type": ["string", "null"], "enum": ["usb2_fs", "usb2_hs", "ethernet_100base_tx", "ethernet_1000base_t", "rmii", null], "description": "Fills unset impedance, tolerance, skew, length and rise time with published values" },
                                "rise_time_ns": { "type": ["number", "null"], "description": "10–90 % edge; a pair's skew default is 10 % of it as length at the slowest signal layer's velocity, or the standard's published skew when that is tighter" },
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
                            "max_mm": { "type": "number", "description": "Pad edge to pad edge; optional with a kind" },
                            "kind": { "type": "string", "enum": ["crystal", "core_regulator_cap", "decoupling"], "description": "Defaults for max_mm, max_path_mm and max_vias from the stack-up" },
                            "max_path_mm": { "type": "number", "description": "Routed track length, part pad to pin on their shared net, checked by check_design_intent" },
                            "max_vias": { "type": "integer", "minimum": 0, "description": "Vias on that routed path" },
                            "why": { "type": "string" }
                        },
                        "required": ["part", "to"],
                        "additionalProperties": false
                    } },
                    "remove_nets": { "type": "array", "items": { "type": "string" }, "description": "Net names to delete from the intent" },
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
                    "plane_layers": { "type": "array", "items": { "type": "string" }, "description": "Copper layers planned as reference planes, e.g. [\"In1.Cu\", \"In2.Cu\"], before their pours exist" },
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
            "Measure the board against the design intent: part ties (pad-edge distance, and for \
             ties with max_path_mm/max_vias the shortest routed path with its vias and estimated \
             nH/pF), routed length, pair skew, track width against the current/impedance requirement, voltage \
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
            "width_for_current" => serde_json::to_value((if args["method"].as_str() == Some("ipc2221") { eecalc::width_for_current_ipc2221 } else { eecalc::width_for_current })(
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

/// The project's saved board file, parsed, when it exists: its stack-up and
/// plane pours make the get/set outputs match check and apply.
fn saved_board(stem: &Path) -> Option<SexpNode> {
    std::fs::read_to_string(stem.with_extension("kicad_pcb")).ok().and_then(|text| konnect_sexp::parse_sexp(&text).ok())
}

fn handle_get_design_intent(args: &Value) -> anyhow::Result<CallToolResult> {
    let stem = match project_arg(args) {
        Ok(s) => s,
        Err(e) => return Ok(e),
    };
    let (intent, text) = load_intent(&stem)?;
    let board = saved_board(&stem);
    Ok(CallToolResult::json(&json!({
        "path": intent_path(&stem),
        "exists": text.is_some(),
        "intent": intent,
        "stackup_used": intent.stackup(),
        "stackup_basis": stack_and_planes(&intent, board.as_ref()).2,
        "requirements": requirements_on(&intent, board.as_ref()),
        "tie_requirements": tie_requirements(&intent, board.as_ref()),
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
    if let Some(remove) = args["remove_nets"].as_array() {
        for net in remove.iter().filter_map(Value::as_str) {
            intent.nets.remove(net);
        }
    }
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
    if let Some(layers) = args["plane_layers"].as_array() {
        intent.plane_layers = layers.iter().filter_map(|l| l.as_str().map(str::to_string)).collect();
        if let Some(bad) = intent.plane_layers.iter().find(|l| !l.ends_with(".Cu")) {
            return Ok(invalid("plane_layers", &format!("'{bad}' is not a copper layer")));
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
    let board = saved_board(&stem);
    Ok(CallToolResult::json(&json!({
        "path": path,
        "nets": intent.nets.len(),
        "ties": intent.ties.len(),
        "stackup_basis": stack_and_planes(&intent, board.as_ref()).2,
        "requirements": requirements_on(&intent, board.as_ref()),
        "tie_requirements": tie_requirements(&intent, board.as_ref()),
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
    let board_tree = saved_board(&stem);
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
        "tie_requirements": tie_requirements(&intent, board_tree.as_ref()),
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
        intent.ties.push(Tie { part: "C1".into(), to: "U1".into(), pin: Some("1".into()), max_mm: Some(2.0), ..Default::default() });
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
    fn a_new_project_without_net_settings_gets_kicads_defaults() {
        let intent = usb_intent();
        let mut project = json!({ "meta": { "filename": "x.kicad_pro", "version": 1 }, "board": { "design_settings": {} } });
        let classes = merge_netclasses(&mut project, &intent, &requirements(&intent)).unwrap();
        assert!(!classes.is_empty());
        let names: Vec<&str> = project["net_settings"]["classes"].as_array().unwrap().iter().filter_map(|c| c["name"].as_str()).collect();
        assert_eq!(names[0], "Default");
        assert!(names.contains(&"intent_USB_D_P"));
        assert_eq!(project["net_settings"]["meta"]["version"], 5);
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
        intent.ties.push(Tie { part: "C1".into(), to: "U1".into(), pin: Some("1".into()), max_mm: Some(2.0), ..Default::default() });
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
        // The zone has no fill in this file: it joins nothing yet.
        assert_eq!(get("plane").status, "unverified");
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
        // With In2 a signal layer (its +3V3 pour not a plane net), power can
        // run inside and needs the inner width.
        let mut signal_in2 = intent();
        signal_in2.nets.remove("+3V3");
        let reqs = requirements_on(&signal_in2, Some(&tree));
        let rules = dru_rules(&signal_in2, &reqs);
        let power = reqs.iter().find(|r| r.net == "+5V").unwrap();
        let (outer, inner) = (power.current_width_mm.unwrap(), power.inner_current_width_mm.unwrap());
        // IPC-2152 does not derate inner layers; the 0.5 oz inner copper alone
        // makes the inner width ≈ 1.5× the 1 oz outer one.
        assert!(inner > 1.3 * outer && inner < 2.0 * outer, "{outer} vs {inner}");
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
    fn declared_plane_layers_stand_in_for_pours_and_plane_only_inners_need_no_inner_width() {
        let mut i = intent();
        i.plane_layers = vec!["In1.Cu".into(), "In2.Cu".into()];
        // No zones at all: the declared planes give F.Cu and B.Cu a model.
        let bare = konnect_sexp::parse_sexp(r#"(kicad_pcb (general (thickness 1.6)) (layers (0 "F.Cu" signal) (4 "In1.Cu" signal) (6 "In2.Cu" signal) (2 "B.Cu" signal)))"#).unwrap();
        let reqs = requirements_on(&i, Some(&bare));
        let pair = reqs.iter().find(|r| r.net == "USB_D_P").unwrap();
        assert_eq!(pair.impedance_widths.len(), 2, "{:?}", pair.warnings);
        // Both inner layers are planes: power keeps the outer width.
        let power = reqs.iter().find(|r| r.net == "+5V").unwrap();
        assert_eq!(power.inner_current_width_mm, None);
        assert_eq!(power.track_width_mm, power.current_width_mm);
    }

    #[test]
    fn a_length_matching_bump_is_uncoupled_not_out_of_tolerance() {
        let tree = board("");
        let reqs = requirements_on(&intent(), Some(&tree));
        let w = reqs.iter().find(|r| r.net == "USB_D_P").unwrap().impedance_widths[0].opt_mm;
        let pitch = w + 0.15;
        // N: one straight run. P: alongside, with a 1.2 mm bump away from N
        // in the middle (x 10–11.2).
        let routed = board(&[
            seg("USB_D_N", "F.Cu", w, (1.0, 10.0), (20.0, 10.0)),
            seg("USB_D_P", "F.Cu", w, (1.0, 10.0 + pitch), (10.0, 10.0 + pitch)),
            seg("USB_D_P", "F.Cu", w, (10.0, 10.0 + pitch), (10.0, 11.2 + pitch)),
            seg("USB_D_P", "F.Cu", w, (10.0, 11.2 + pitch), (11.2, 11.2 + pitch)),
            seg("USB_D_P", "F.Cu", w, (11.2, 11.2 + pitch), (11.2, 10.0 + pitch)),
            seg("USB_D_P", "F.Cu", w, (11.2, 10.0 + pitch), (20.0, 10.0 + pitch)),
        ]
        .join("\n"));
        let checks = check_board(&routed, &intent());
        let n = checks.iter().find(|c| c.kind == "impedance" && c.subject == "USB_D_N").unwrap();
        // N's single segment has P beside it on both sides of the bump: 17.8 of 19 mm coupled.
        assert_eq!(n.status, "pass", "{:?}", n.measured);
        assert!((n.measured["uncoupled_mm"].as_f64().unwrap() - 1.2).abs() < 1e-6, "{:?}", n.measured);
        let p = checks.iter().find(|c| c.kind == "impedance" && c.subject == "USB_D_P").unwrap();
        // The bump's far side (1.2 mm away) is uncoupled, not a 1.2 mm-gap pair out of tolerance.
        assert_eq!(p.status, "pass", "{:?}", p.measured);
        assert_eq!(p.measured["out_of_tolerance_mm"].as_f64().unwrap(), 0.0, "{:?}", p.measured);
    }

    #[test]
    fn an_inner_run_at_the_outer_width_fails() {
        let mut i = intent();
        i.nets.remove("+3V3"); // In2 carries signals
        let tree = board("");
        let reqs = requirements_on(&i, Some(&tree));
        let outer = reqs.iter().find(|r| r.net == "+5V").unwrap().current_width_mm.unwrap();
        let ok = check_board(&board(&seg("+5V", "F.Cu", outer, (0.0, 0.0), (5.0, 0.0))), &i);
        assert_eq!(ok.iter().find(|c| c.kind == "track_width").unwrap().status, "pass");
        let bad = check_board(&board(&seg("+5V", "In2.Cu", outer, (0.0, 0.0), (5.0, 0.0))), &i);
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
    fn an_intent_net_missing_its_sheet_prefix_is_flagged_with_a_hint() {
        let mut i = intent();
        i.nets.insert("USB_D_P".into(), NetIntent { role: "signal".into(), ..Default::default() });
        let tree = konnect_sexp::parse_sexp(&format!(
            "(kicad_pcb (general (thickness 1.6)) {PADS} (segment (start 0 5) (end 5 5) (width 0.2) (layer \"F.Cu\") (net \"/USB_D_P\")))"
        ))
        .unwrap();
        let checks = check_board(&tree, &i);
        let missing = checks.iter().find(|c| c.kind == "net_on_board").unwrap();
        assert_eq!(missing.subject, "USB_D_P");
        assert!(missing.detail.contains("did you mean '/USB_D_P'"), "{}", missing.detail);
        assert!(!checks.iter().any(|c| c.kind == "net_on_board" && c.subject == "+5V"));
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
        // In1 is the plane; In2 carries signals, so power needs an inner width.
        intent.plane_layers = vec!["In1.Cu".into()];
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

#[cfg(test)]
mod default_tests {
    use super::*;

    fn pair(standard: Option<&str>, skew: Option<f64>, rise: Option<f64>) -> DesignIntent {
        let mut intent = DesignIntent { schema: 1, ..Default::default() };
        for (net, other) in [("D_P", "D_N"), ("D_N", "D_P")] {
            intent.nets.insert(
                net.into(),
                NetIntent { role: "diff_pair".into(), pair: Some(other.into()), standard: standard.map(str::to_string), max_skew_mm: skew, rise_time_ns: rise, ..Default::default() },
            );
        }
        intent
    }

    fn req(intent: &DesignIntent, net: &str) -> NetRequirement {
        requirements(intent).into_iter().find(|r| r.net == net).unwrap()
    }

    #[test]
    fn a_standard_fills_impedance_skew_and_length_with_cited_sources() {
        let i = pair(Some("ethernet_100base_tx"), None, None);
        assert!(i.validate().is_ok());
        let r = req(&i, "D_P");
        assert_eq!(r.effective.impedance_ohm, Some(100.0));
        assert_eq!(r.max_skew_mm, Some(1.27), "TI SNLA387: 50 mil");
        assert_eq!(r.max_length_mm, Some(50.8), "TI SNLA387: 2 in");
        assert!(r.sources["max_skew_mm"].contains("SNLA387"), "{:?}", r.sources);
        assert!(r.sources["impedance_ohm"].contains("SNLA387"));
        let usb = req(&pair(Some("usb2_hs"), None, None), "D_P");
        assert_eq!((usb.effective.impedance_ohm, usb.max_skew_mm), (Some(90.0), Some(1.27)));
        assert!(usb.sources["max_skew_mm"].contains("SPRAAR7J"));
        // The published 50 mil is tighter than 10 % of 500 ps (≈ 8 mm).
        assert!(usb.sources["max_skew_mm"].contains("tighter than the edge-rate bound"), "{}", usb.sources["max_skew_mm"]);
        assert_eq!(req(&pair(Some("ethernet_1000base_t"), None, None), "D_P").max_skew_mm, Some(0.508));
    }

    #[test]
    fn explicit_user_values_override_every_default() {
        let mut i = pair(Some("ethernet_100base_tx"), Some(12.0), None);
        for n in i.nets.values_mut() {
            n.impedance_ohm = Some(95.0);
            n.max_length_mm = Some(30.0);
        }
        let r = req(&i, "D_N");
        assert_eq!((r.max_skew_mm, r.max_length_mm, r.effective.impedance_ohm), (Some(12.0), Some(30.0), Some(95.0)));
        for f in ["max_skew_mm", "max_length_mm", "impedance_ohm"] {
            assert_eq!(r.sources[f], "user", "{f}");
        }
        // The unset tolerance still comes from the standard.
        assert_eq!(r.effective.impedance_tolerance_pct, Some(10.0));
    }

    #[test]
    fn a_pair_without_a_standard_gets_skew_from_its_edge_rate_and_the_stackup_velocity() {
        // 1 ns edge, 10 % → 100 ps; default 4-layer stack: F.Cu microstrip
        // over In1. Length = 100 ps / delay(ps/mm) from the line model.
        let i = pair(None, None, Some(1.0));
        let (layers, planes, _) = stack_and_planes(&i, None);
        let delay = layers.model("F.Cu", &planes).unwrap().per_mm(0.2).delay_ps_per_mm;
        // FR-4 (εr 4.4) microstrip, w/h ≈ 1: εeff ≈ 2.9–3.5 → 5.7–6.2 ps/mm
        // (≈ 145–158 ps/in, the usual "≈150 ps/in outer layer" figure).
        assert!((5.6..6.3).contains(&delay), "{delay}");
        let r = req(&i, "D_P");
        assert!((r.max_skew_mm.unwrap() - r3(100.0 / delay)).abs() < 1e-9, "{:?}", r.max_skew_mm);
        assert!(r.sources["max_skew_mm"].contains("edge-rate bound"));
        // Nothing to go on: no limit, and a warning says so.
        let none = req(&pair(None, None, None), "D_P");
        assert_eq!(none.max_skew_mm, None);
        assert!(none.warnings.iter().any(|w| w.contains("no skew limit")));
    }

    #[test]
    fn standards_are_validated_against_the_role() {
        let mut i = pair(Some("rmii"), None, None);
        assert!(i.validate().is_err(), "rmii is single-ended");
        i = DesignIntent { schema: 1, ..Default::default() };
        i.nets.insert("RXD0".into(), NetIntent { role: "signal".into(), standard: Some("rmii".into()), ..Default::default() });
        assert!(i.validate().is_ok());
        assert_eq!(req(&i, "RXD0").max_length_mm, Some(152.4));
        i.nets.get_mut("RXD0").unwrap().standard = Some("usb2_hs".into());
        assert!(i.validate().is_err());
        i.nets.get_mut("RXD0").unwrap().standard = Some("hdmi".into());
        assert!(i.validate().is_err());
    }

    #[test]
    fn power_rise_defaults_to_10c_and_a_power_net_without_current_is_flagged() {
        let mut i = DesignIntent { schema: 1, ..Default::default() };
        i.nets.insert("+5V".into(), NetIntent { role: "power".into(), current_a: Some(1.5), ..Default::default() });
        i.nets.insert("+3V3".into(), NetIntent { role: "power".into(), ..Default::default() });
        let r = req(&i, "+5V");
        assert_eq!(r.effective.max_temp_rise_c, Some(10.0));
        assert!(r.sources["max_temp_rise_c"].contains("10 °C"));
        // IPC-2152 external 1 oz, 1.5 A, 10 °C: ≈ 0.56 mm (Brooks & Adam).
        assert!((0.53..0.60).contains(&r.current_width_mm.unwrap()), "{:?}", r.current_width_mm);
        assert!(req(&i, "+3V3").warnings.iter().any(|w| w.contains("without current_a")));
    }

    #[test]
    fn tie_kinds_give_budgets_and_placement_defaults_and_user_values_win() {
        let intent = DesignIntent { schema: 1, ..Default::default() };
        let (layers, planes, _) = stack_and_planes(&intent, None);
        let tie = |kind: &str| Tie { part: "Y1".into(), to: "U1".into(), kind: Some(kind.into()), ..Default::default() };
        let x = tie_limits(&tie("crystal"), &layers, &planes);
        assert_eq!(x.budget, Some(TieBudget::CapacitancePf(CRYSTAL_TRACE_PF)));
        // The length guide is flat, not the budget turned into millimetres.
        assert_eq!((x.max_path_mm, x.max_vias, x.max_mm), (Some(6.0), Some(0), Some(6.0)));
        assert!(x.sources["budget_pf"].contains("AVR186"));
        assert!(x.sources["max_path_mm"].contains("Konnect judgement"));
        let v = tie_limits(&tie("core_regulator_cap"), &layers, &planes);
        assert_eq!(v.budget, Some(TieBudget::LoopInductanceNh(CORE_REG_LOOP_NH)));
        // No length proxy: the loop inductance is compared directly.
        assert_eq!((v.max_path_mm, v.max_vias, v.max_mm, v.unmodelled_path_mm), (None, Some(1), Some(2.5), Some(3.0)));
        let d = tie_limits(&tie("decoupling"), &layers, &planes);
        assert_eq!(d.budget, Some(TieBudget::LoopInductanceNh(DECOUPLING_LOOP_NH)));
        assert_eq!((d.max_mm, d.unmodelled_path_mm), (Some(2.5), Some(6.0)));
        assert!(d.sources["max_mm"].contains("Konnect judgement"));
        assert!(d.sources["budget_nh"].contains("xilinx-ug483-7series-pcb"));
        // Explicit values override the kind.
        let user = Tie { max_mm: Some(2.5), max_path_mm: Some(9.0), max_vias: Some(3), ..tie("crystal") };
        let u = tie_limits(&user, &layers, &planes);
        assert_eq!((u.max_mm, u.max_path_mm, u.max_vias), (Some(2.5), Some(9.0), Some(3)));
        assert!(["max_mm", "max_path_mm", "max_vias"].iter().all(|k| u.sources[*k] == "user"));
        // A tie with only a path limit is placed within it.
        let plain = Tie { part: "R1".into(), to: "U1".into(), max_path_mm: Some(4.0), ..Default::default() };
        assert_eq!(tie_limits(&plain, &layers, &planes).max_mm, Some(4.0));
        // Review D2: a plane 1.5 mm away gives a small C′, and the 0.5 pF
        // alone would allow about 12 mm; the guide still says 6.
        let two = LayerStack::from_intent(Stackup::default(), &["F.Cu".to_string(), "B.Cu".to_string()], 1.6);
        let far: BTreeSet<String> = ["B.Cu".to_string()].into();
        assert!(CRYSTAL_TRACE_PF / two.model("F.Cu", &far).unwrap().per_mm(0.2).c_pf_per_mm > 10.0);
        assert_eq!(tie_limits(&tie("crystal"), &two, &far).max_path_mm, Some(6.0));
        // Review B2: nothing resolves → no limit at all, not an infinite one.
        let none = Tie { part: "C1".into(), to: "U1".into(), kind: Some("ferrite".into()), ..Default::default() };
        let n = tie_limits(&none, &layers, &planes);
        assert_eq!((n.max_mm, n.max_path_mm, n.budget), (None, None, None));
        // A plain tie needs a limit; a kind supplies one.
        let mut i = DesignIntent { schema: 1, ..Default::default() };
        i.ties.push(Tie { part: "C1".into(), to: "U1".into(), ..Default::default() });
        assert!(i.validate().is_err());
        i.ties[0].kind = Some("decoupling".into());
        assert!(i.validate().is_ok());
        i.ties[0].kind = Some("ferrite".into());
        assert!(i.validate().is_err());
    }

    #[test]
    fn a_crystal_tie_fails_placement_beyond_its_derived_limit() {
        let board = konnect_sexp::parse_sexp(
            "(kicad_pcb (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (4 \"In1.Cu\" signal) (6 \"In2.Cu\" signal) (2 \"B.Cu\" signal))
              (zone (net \"GND\") (layers \"In1.Cu\") (polygon (pts (xy 0 0) (xy 50 0) (xy 50 50))))
              (footprint \"U\" (layer \"F.Cu\") (at 10 10) (property \"Reference\" \"U1\")
                (pad \"5\" smd rect (at 0 0) (size 0.5 0.5) (layers \"F.Cu\") (net \"XI\")))
              (footprint \"Y\" (layer \"F.Cu\") (at 19 10) (property \"Reference\" \"Y1\")
                (pad \"1\" smd rect (at 0 0) (size 0.5 0.5) (layers \"F.Cu\") (net \"XI\"))))",
        )
        .unwrap();
        let mut i = DesignIntent { schema: 1, ..Default::default() };
        i.ties.push(Tie { part: "Y1".into(), to: "U1".into(), pin: Some("5".into()), kind: Some("crystal".into()), ..Default::default() });
        let checks = check_board(&board, &i);
        let tie = checks.iter().find(|c| c.kind == "tie").unwrap();
        assert_eq!(tie.status, "fail", "8.5 mm apart: {tie:?}");
        assert!(tie.required["sources"]["max_mm"].as_str().unwrap().contains("crystal length guide"));
        assert_eq!(checks.iter().find(|c| c.kind == "tie_path").unwrap().status, "unverified");
    }
}

/// The routed path of a tie: shortest track + via path from the tied part's
/// pad to the pin on their shared net, with its vias and L/C estimate.
#[cfg(test)]
mod tie_path_tests {
    use super::*;

    /// Four layers, In1/In2 ground planes; U1.5 on F.Cu at (10, 10) on XI.
    fn checks(kind: &str, extra: &str) -> Vec<Check> {
        let board = konnect_sexp::parse_sexp(&format!(
            "(kicad_pcb (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (4 \"In1.Cu\" signal) (6 \"In2.Cu\" signal) (2 \"B.Cu\" signal))
              (zone (net \"GND\") (layers \"In1.Cu\" \"In2.Cu\") (polygon (pts (xy 0 0) (xy 50 0) (xy 50 50))))
              (footprint \"U\" (layer \"F.Cu\") (at 10 10) (property \"Reference\" \"U1\")
                (pad \"5\" smd rect (at 0 0) (size 0.5 0.5) (layers \"F.Cu\") (net \"XI\")))
              {extra})"
        ))
        .unwrap();
        let mut i = DesignIntent { schema: 1, ..Default::default() };
        i.ties.push(Tie { part: "Y1".into(), to: "U1".into(), pin: Some("5".into()), kind: Some(kind.into()), ..Default::default() });
        check_board(&board, &i)
    }

    fn path(kind: &str, extra: &str) -> Check {
        checks(kind, extra).into_iter().find(|c| c.kind == "tie_path").unwrap()
    }

    const Y1_TOP: &str = "(footprint \"Y\" (layer \"F.Cu\") (at 14 10) (property \"Reference\" \"Y1\")
        (pad \"1\" smd rect (at 0 0) (size 0.5 0.5) (layers \"F.Cu\") (net \"XI\")) (pad \"2\" smd rect (at 2 0) (size 0.5 0.5) (layers \"F.Cu\") (net \"XO\")))";
    const Y1_BOTTOM: &str = "(footprint \"Y\" (layer \"B.Cu\") (at 13 10) (property \"Reference\" \"Y1\")
        (pad \"1\" smd rect (at 0 0) (size 0.5 0.5) (layers \"B.Cu\") (net \"XI\")))";

    fn seg(layer: &str, w: f64, (x1, y1): (f64, f64), (x2, y2): (f64, f64)) -> String {
        format!("(segment (start {x1} {y1}) (end {x2} {y2}) (width {w}) (layer \"{layer}\") (net \"XI\"))")
    }

    #[test]
    fn a_direct_trace_passes_with_its_length_and_capacitance() {
        let c = path("crystal", &format!("{Y1_TOP} {}", seg("F.Cu", 0.2, (10.0, 10.0), (14.0, 10.0))));
        assert_eq!(c.status, "pass", "{c:?}");
        assert_eq!(c.measured["path_mm"], json!(4.0));
        assert_eq!(c.measured["vias"], json!(0));
        assert_eq!((c.measured["from_pad"].as_str(), c.measured["to_pad"].as_str()), (Some("Y1.1"), Some("U1.5")));
        // Σ C′·len on the outer microstrip at 0.2 mm.
        let intent = DesignIntent { schema: 1, ..Default::default() };
        let (layers, _, _) = stack_and_planes(&intent, None);
        let planes: BTreeSet<String> = ["In1.Cu".to_string(), "In2.Cu".to_string()].into();
        let pm = layers.model("F.Cu", &planes).unwrap().per_mm(0.2);
        let pf = c.measured["est_capacitance_pf"].as_f64().unwrap();
        let nh = c.measured["est_inductance_nh"].as_f64().unwrap();
        assert!((pf - 4.0 * pm.c_pf_per_mm).abs() < 0.01, "{pf} vs {}", 4.0 * pm.c_pf_per_mm);
        assert!((nh - 4.0 * pm.l_nh_per_mm).abs() < 0.01, "{nh}");
        assert!(c.required["sources"]["budget_pf"].as_str().unwrap().contains("AVR186"));
        assert_eq!(c.required["budget_pf"], json!(0.5));
    }

    #[test]
    fn a_detour_fails_on_length_and_the_shortest_branch_counts() {
        let detour = [seg("F.Cu", 0.2, (10.0, 10.0), (10.0, 20.0)), seg("F.Cu", 0.2, (10.0, 20.0), (14.0, 20.0)), seg("F.Cu", 0.2, (14.0, 20.0), (14.0, 10.0))].join(" ");
        let c = path("crystal", &format!("{Y1_TOP} {detour}"));
        assert_eq!(c.status, "fail", "{c:?}");
        assert_eq!(c.measured["path_mm"], json!(24.0));
        assert!(c.detail.contains("exceeds"), "{}", c.detail);
        // A tee straight across makes the short path win.
        let c = path("crystal", &format!("{Y1_TOP} {detour} {}", seg("F.Cu", 0.2, (10.0, 10.0), (14.0, 10.0))));
        assert_eq!((c.status, c.measured["path_mm"].clone()), ("pass", json!(4.0)), "{c:?}");
    }

    #[test]
    fn a_path_through_a_via_to_the_bottom_counts_the_via() {
        let routed = format!(
            "{Y1_BOTTOM} {} {} (via (at 12 10) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (net \"XI\"))",
            seg("F.Cu", 0.3, (10.0, 10.0), (12.0, 10.0)),
            seg("B.Cu", 0.3, (12.0, 10.0), (13.0, 10.0))
        );
        // A crystal allows no via.
        let c = path("crystal", &routed);
        assert_eq!(c.status, "fail", "{c:?}");
        assert_eq!((c.measured["path_mm"].clone(), c.measured["vias"].clone()), (json!(3.0), json!(1)));
        // Layers in path order, from the tied part to the pin.
        assert_eq!(c.measured["layers"], json!(["B.Cu", "F.Cu"]));
        assert!(c.detail.contains("1 vias > 0"), "{}", c.detail);
        // A decoupling capacitor allows one.
        assert_eq!(path("decoupling", &routed).status, "pass");
        // A bottom pad sitting on the via: no bottom track, but B.Cu is on the path.
        let on_via = format!(
            "{} {} (via (at 12 10) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (net \"XI\"))",
            Y1_BOTTOM.replace("(at 13 10)", "(at 12 10)"),
            seg("F.Cu", 0.3, (10.0, 10.0), (12.0, 10.0))
        );
        let c = path("decoupling", &on_via);
        assert_eq!((c.status, c.measured["path_mm"].clone(), c.measured["vias"].clone()), ("pass", json!(2.0), json!(1)), "{c:?}");
        assert_eq!(c.measured["layers"], json!(["B.Cu", "F.Cu"]));
        // Without the via the bottom track does not connect.
        let unjoined = format!("{Y1_BOTTOM} {} {}", seg("F.Cu", 0.3, (10.0, 10.0), (12.0, 10.0)), seg("B.Cu", 0.3, (12.0, 10.0), (13.0, 10.0)));
        assert_eq!(path("decoupling", &unjoined).status, "unverified");
    }

    #[test]
    fn an_unrouted_or_zone_carried_tie_is_unverified_with_the_reason() {
        let c = path("crystal", Y1_TOP);
        assert_eq!(c.status, "unverified");
        assert!(c.detail.contains("not routed"), "{}", c.detail);
        let zone = "(zone (net \"XI\") (layers \"F.Cu\") (polygon (pts (xy 9 9) (xy 15 9) (xy 15 11) (xy 9 11))))";
        let c = path("crystal", &format!("{Y1_TOP} {zone}"));
        assert_eq!(c.status, "unverified");
        assert!(c.detail.contains("zone"), "{}", c.detail);
        // A routed path under the limit passes even when a zone is present.
        let c = path("crystal", &format!("{Y1_TOP} {zone} {}", seg("F.Cu", 0.2, (10.0, 10.0), (14.0, 10.0))));
        assert_eq!(c.status, "pass", "{c:?}");
    }
}

/// Review fixes: pair-level fields on one net only, conservative edge-rate
/// velocity, stack-up basis of get/set, more tie-path and drop shapes.
#[cfg(test)]
mod review_tests {
    use super::*;

    fn half_pair(p: NetIntent) -> DesignIntent {
        // Metadata on the P net only; the N net sorts first.
        let mut i = DesignIntent { schema: 1, ..Default::default() };
        i.nets.insert("ETH_TX_P".into(), NetIntent { role: "diff_pair".into(), pair: Some("ETH_TX_N".into()), ..p });
        i.nets.insert("ETH_TX_N".into(), NetIntent { role: "diff_pair".into(), pair: Some("ETH_TX_P".into()), ..Default::default() });
        i
    }

    #[test]
    fn a_standard_on_one_half_of_a_pair_applies_to_both_and_to_the_pair_rule() {
        let i = half_pair(NetIntent { standard: Some("ethernet_100base_tx".into()), ..Default::default() });
        assert!(i.validate().is_ok());
        let reqs = requirements(&i);
        let (p, n) = (reqs.iter().find(|r| r.net == "ETH_TX_P").unwrap(), reqs.iter().find(|r| r.net == "ETH_TX_N").unwrap());
        assert_eq!((n.max_skew_mm, n.effective.impedance_ohm), (Some(1.27), Some(100.0)));
        assert_eq!((p.max_skew_mm, p.effective.impedance_ohm, p.max_length_mm), (n.max_skew_mm, n.effective.impedance_ohm, n.max_length_mm));
        assert!(n.sources["standard"].contains("pair partner ETH_TX_P"), "{:?}", n.sources);
        assert!(n.sources["max_skew_mm"].contains("SNLA387"), "the standard's citation is kept");
        let rules = dru_rules(&i, &reqs);
        let pair = &rules["intent: pair ETH_TX_N/ETH_TX_P"];
        assert!(pair.contains("(constraint skew (max 1.27mm))"), "{pair}");
        assert!(rules.keys().any(|k| k.starts_with("intent: impedance ETH_TX_N/ETH_TX_P")), "{:?}", rules.keys());
        // A rise time or a skew on one half only is shared the same way.
        let r = requirements(&half_pair(NetIntent { rise_time_ns: Some(1.0), ..Default::default() }));
        assert_eq!(r[0].max_skew_mm, r[1].max_skew_mm);
        assert!(r[0].max_skew_mm.is_some());
        let r = requirements(&half_pair(NetIntent { max_skew_mm: Some(2.0), ..Default::default() }));
        assert_eq!((r[0].max_skew_mm, r[1].max_skew_mm), (Some(2.0), Some(2.0)));
        assert!(r.iter().find(|r| r.net == "ETH_TX_N").unwrap().sources["max_skew_mm"].contains("pair partner"));
    }

    #[test]
    fn conflicting_pair_values_are_refused() {
        let mut i = half_pair(NetIntent { rise_time_ns: Some(1.0), ..Default::default() });
        i.nets.get_mut("ETH_TX_N").unwrap().rise_time_ns = Some(2.0);
        assert!(i.validate().unwrap_err().contains("rise_time_ns"));
        let mut i = half_pair(NetIntent { standard: Some("usb2_fs".into()), ..Default::default() });
        i.nets.get_mut("ETH_TX_N").unwrap().standard = Some("usb2_hs".into());
        assert!(i.validate().is_err());
    }

    #[test]
    fn the_edge_rate_skew_uses_the_slowest_routable_layer() {
        // Planes on In1 and B.Cu: In2 is a stripline between them, slower
        // than the F.Cu microstrip, so it sets the (tighter) bound.
        let mut i = half_pair(NetIntent { rise_time_ns: Some(1.0), ..Default::default() });
        i.plane_layers = vec!["In1.Cu".into(), "B.Cu".into()];
        let (layers, planes, _) = stack_and_planes(&i, None);
        let strip = layers.model("In2.Cu", &planes).unwrap().per_mm(0.2).delay_ps_per_mm;
        let micro = layers.model("F.Cu", &planes).unwrap().per_mm(0.2).delay_ps_per_mm;
        assert!(strip > micro);
        let r = &requirements(&i)[0];
        assert!((r.max_skew_mm.unwrap() - r3(100.0 / strip)).abs() < 1e-9, "{:?} vs {}", r.max_skew_mm, 100.0 / strip);
        assert!(r.sources["max_skew_mm"].contains("In2.Cu"), "{}", r.sources["max_skew_mm"]);
    }

    #[test]
    fn get_design_intent_resolves_against_the_saved_board() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.kicad_pro"), "{}").unwrap();
        let mut i = DesignIntent { schema: 1, ..Default::default() };
        i.ties.push(Tie { part: "Y1".into(), to: "U1".into(), kind: Some("crystal".into()), ..Default::default() });
        std::fs::write(dir.path().join("b.intent.json"), serde_json::to_string(&i).unwrap()).unwrap();
        let get = |dir: &Path| -> Value {
            let res = handle_get_design_intent(&json!({"project": dir.join("b.kicad_pro")})).unwrap();
            let crate::mcp::protocol::ToolContent::Text { text } = &res.content[0] else { panic!() };
            serde_json::from_str(text).unwrap()
        };
        let assumed = get(dir.path());
        assert!(assumed["stackup_basis"].as_str().unwrap().contains("assumed default stack-up"), "{}", assumed["stackup_basis"]);
        // A 2-layer board with a bottom ground pour: 1.6 mm to the plane, so
        // C′ is far lower and the crystal path limit far longer.
        std::fs::write(
            dir.path().join("b.kicad_pcb"),
            "(kicad_pcb (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal))
               (zone (net \"GND\") (layers \"B.Cu\") (polygon (pts (xy 0 0) (xy 50 0) (xy 50 50)))))",
        )
        .unwrap();
        let mut with_plane = i.clone();
        with_plane.plane_layers = vec!["B.Cu".into()];
        std::fs::write(dir.path().join("b.intent.json"), serde_json::to_string(&with_plane).unwrap()).unwrap();
        let real = get(dir.path());
        assert!(real["stackup_basis"].as_str().unwrap().contains("saved board file"), "{}", real["stackup_basis"]);
        // The budget's length equivalent follows the stack-up that was read;
        // the length guide does not.
        let equivalent = |v: &Value| -> f64 {
            let text = v["tie_requirements"][0]["sources"]["budget_pf"].as_str().unwrap().to_string();
            text.split("≈ ").nth(1).and_then(|t| t.split(" mm").next()).and_then(|n| n.parse().ok()).unwrap_or_else(|| panic!("{text}"))
        };
        let (a, r) = (equivalent(&assumed), equivalent(&real));
        assert!(r > 1.5 * a, "2-layer {r} mm vs assumed 4-layer {a} mm");
        assert_eq!((assumed["tie_requirements"][0]["max_path_mm"].clone(), real["tie_requirements"][0]["max_path_mm"].clone()), (json!(6.0), json!(6.0)));
    }

    fn tie_board(extra: &str, tie: Tie) -> Check {
        let board = konnect_sexp::parse_sexp(&format!(
            "(kicad_pcb (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (4 \"In1.Cu\" signal) (6 \"In2.Cu\" signal) (2 \"B.Cu\" signal))
              (zone (net \"GND\") (layers \"In1.Cu\" \"In2.Cu\") (polygon (pts (xy 0 0) (xy 50 0) (xy 50 50))))
              {extra})"
        ))
        .unwrap();
        let mut i = DesignIntent { schema: 1, ..Default::default() };
        i.ties.push(tie);
        check_board(&board, &i).into_iter().find(|c| c.kind == "tie_path").unwrap()
    }

    #[test]
    fn a_tie_without_a_pin_must_meet_the_limit_on_every_shared_net() {
        let extra = "(footprint \"J\" (layer \"F.Cu\") (at 10 10) (property \"Reference\" \"J1\")
              (pad \"1\" smd rect (at 0 0) (size 0.5 0.5) (layers \"F.Cu\") (net \"A\")) (pad \"2\" smd rect (at 0 5) (size 0.5 0.5) (layers \"F.Cu\") (net \"B\")))
            (footprint \"U\" (layer \"F.Cu\") (at 18 10) (property \"Reference\" \"U2\")
              (pad \"1\" smd rect (at 0 0) (size 0.5 0.5) (layers \"F.Cu\") (net \"A\")) (pad \"2\" smd rect (at -5 5) (size 0.5 0.5) (layers \"F.Cu\") (net \"B\")))
            (segment (start 10 10) (end 18 10) (width 0.2) (layer \"F.Cu\") (net \"A\"))
            (segment (start 10 15) (end 13 15) (width 0.2) (layer \"F.Cu\") (net \"B\"))";
        // Review B1: the 8 mm leg on A fails the tie; the 3 mm leg on B does
        // not rescue it. The top-level fields describe the failing leg.
        let c = tie_board(extra, Tie { part: "U2".into(), to: "J1".into(), max_path_mm: Some(5.0), ..Default::default() });
        assert_eq!(c.status, "fail", "{c:?}");
        assert_eq!((c.measured["net"].as_str(), c.measured["path_mm"].clone()), (Some("A"), json!(8.0)));
        let legs: Vec<(&str, &str)> = c.measured["legs"].as_array().unwrap().iter().map(|l| (l["net"].as_str().unwrap(), l["status"].as_str().unwrap())).collect();
        assert_eq!(legs, vec![("A", "fail"), ("B", "pass")]);
        assert!(c.detail.contains("A: path 8 mm > 5 mm"), "{}", c.detail);
        // With room for both, it passes and reports the longer leg.
        let c = tie_board(extra, Tie { part: "U2".into(), to: "J1".into(), max_path_mm: Some(9.0), ..Default::default() });
        assert_eq!((c.status, c.measured["net"].as_str()), ("pass", Some("A")), "{c:?}");
    }

    #[test]
    fn a_through_hole_pin_joins_a_bottom_track_without_a_via() {
        let extra = "(footprint \"U\" (layer \"F.Cu\") (at 10 10) (property \"Reference\" \"U1\")
              (pad \"5\" thru_hole circle (at 0 0) (size 1.6 1.6) (drill 0.8) (layers \"*.Cu\") (net \"XI\")))
            (footprint \"Y\" (layer \"B.Cu\") (at 13 10) (property \"Reference\" \"Y1\")
              (pad \"1\" smd rect (at 0 0) (size 0.5 0.5) (layers \"B.Cu\") (net \"XI\")))
            (segment (start 10 10) (end 13 10) (width 0.2) (layer \"B.Cu\") (net \"XI\"))";
        let c = tie_board(extra, Tie { part: "Y1".into(), to: "U1".into(), pin: Some("5".into()), kind: Some("crystal".into()), ..Default::default() });
        assert_eq!(c.status, "pass", "{c:?}");
        assert_eq!((c.measured["path_mm"].clone(), c.measured["vias"].clone(), c.measured["layers"].clone()), (json!(3.0), json!(0), json!(["B.Cu"])));
    }

    /// Pins voltage_drop on a fixed graph: the drop is exactly the shortest
    /// resistance path (10 mm top, barrel, 10 mm bottom, barrel), not the
    /// longer parallel top route.
    #[test]
    fn voltage_drop_follows_the_least_resistance_path_exactly() {
        let mut intent = DesignIntent { schema: 1, ..Default::default() };
        intent.nets.insert("+5V".into(), NetIntent { role: "power".into(), current_a: Some(1.0), max_drop_mv: Some(500.0), source: Some("J1.1".into()), ..Default::default() });
        let board = konnect_sexp::parse_sexp(
            "(kicad_pcb (general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal))
              (footprint \"J\" (layer \"F.Cu\") (at 0 0) (property \"Reference\" \"J1\") (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net \"+5V\")))
              (footprint \"U\" (layer \"F.Cu\") (at 20 0) (property \"Reference\" \"U1\") (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net \"+5V\")))
              (segment (start 0 0) (end 10 0) (width 0.5) (layer \"F.Cu\") (net \"+5V\"))
              (segment (start 10 0) (end 20 0) (width 0.5) (layer \"B.Cu\") (net \"+5V\"))
              (via (at 10 0) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (net \"+5V\"))
              (via (at 20 0) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (net \"+5V\"))
              (segment (start 0 0) (end 0 10) (width 0.2) (layer \"F.Cu\") (net \"+5V\"))
              (segment (start 0 10) (end 20 10) (width 0.2) (layer \"F.Cu\") (net \"+5V\"))
              (segment (start 20 10) (end 20 0) (width 0.2) (layer \"F.Cu\") (net \"+5V\")))",
        )
        .unwrap();
        let c = check_board(&board, &intent).into_iter().find(|c| c.kind == "voltage_drop").unwrap();
        let (layers, _, _) = stack_and_planes(&intent, Some(&board));
        let oz = layers.copper[0].1 / eecalc::MM_PER_OZ;
        let track = eecalc::trace_resistance(10.0, 0.5, oz, 35.0, None).result.resistance_ohm;
        let bottom = eecalc::trace_resistance(10.0, 0.5, layers.copper[1].1 / eecalc::MM_PER_OZ, 35.0, None).result.resistance_ohm;
        let barrel = eecalc::trace_resistance(layers.gaps[0].thickness_mm, std::f64::consts::PI * (0.3 + VIA_PLATING_MM), VIA_PLATING_MM / eecalc::MM_PER_OZ, 35.0, None).result.resistance_ohm;
        let want = (track + bottom + 2.0 * barrel) * 1000.0;
        assert_eq!(c.measured["drop_mv"], json!(r3(want)), "{c:?}");
        assert_eq!(c.measured["worst_pad"], json!("U1.1"));
    }
}

/// Pass 19: fixes from the second independent review of Tier 1 (B1–B5, D1–D6).
#[cfg(test)]
mod second_review_tests {
    use super::*;

    const LAYERS4: &str = "(general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (4 \"In1.Cu\" power) (6 \"In2.Cu\" power) (2 \"B.Cu\" signal))";
    /// Ground on In1 and +3V3 on In2, both over the whole 50 mm square.
    const PLANES: &str = "(zone (net \"GND\") (layers \"In1.Cu\") (polygon (pts (xy 0 0) (xy 50 0) (xy 50 50) (xy 0 50))))
        (zone (net \"+3V3\") (layers \"In2.Cu\") (polygon (pts (xy 0 0) (xy 50 0) (xy 50 50) (xy 0 50))))";

    fn seg(net: &str, layer: &str, w: f64, (x1, y1): (f64, f64), (x2, y2): (f64, f64)) -> String {
        format!("(segment (start {x1} {y1}) (end {x2} {y2}) (width {w}) (layer \"{layer}\") (net \"{net}\"))")
    }

    fn via(net: &str, (x, y): (f64, f64)) -> String {
        format!("(via (at {x} {y}) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (net \"{net}\"))")
    }

    /// A footprint with 0.5 mm square surface pads: (number, net, dx, dy).
    fn part(reference: &str, layer: &str, (x, y): (f64, f64), pads: &[(&str, &str, f64, f64)]) -> String {
        let pads: String = pads.iter().map(|(n, net, dx, dy)| format!("(pad \"{n}\" smd rect (at {dx} {dy}) (size 0.5 0.5) (layers \"{layer}\") (net \"{net}\"))")).collect();
        format!("(footprint \"X\" (layer \"{layer}\") (at {x} {y}) (property \"Reference\" \"{reference}\") {pads})")
    }

    fn board(layers: &str, body: &str) -> SexpNode {
        konnect_sexp::parse_sexp(&format!("(kicad_pcb {layers} {body})")).unwrap()
    }

    fn path_check(board: &SexpNode, tie: Tie) -> Check {
        let mut i = DesignIntent { schema: 1, ..Default::default() };
        i.ties.push(tie);
        check_board(board, &i).into_iter().find(|c| c.kind == "tie_path").expect("a tie_path check")
    }

    fn tie(part: &str, to: &str, pin: Option<&str>, kind: Option<&str>) -> Tie {
        Tie { part: part.into(), to: to.into(), pin: pin.map(str::to_string), kind: kind.map(str::to_string), ..Default::default() }
    }

    fn stack4() -> (LayerStack, BTreeSet<String>) {
        let names: Vec<String> = ["F.Cu", "In1.Cu", "In2.Cu", "B.Cu"].iter().map(|s| s.to_string()).collect();
        (LayerStack::from_intent(Stackup::default(), &names, 1.6), ["In1.Cu".to_string(), "In2.Cu".to_string()].into())
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.002
    }

    // ── B1 ──────────────────────────────────────────────────────────────

    /// A crystal tie without a pin: each node has its own budget, so a long
    /// XO leg fails the tie however short XI is. The ground pads are on a
    /// plane net and are not judged.
    #[test]
    fn a_pinless_crystal_fails_on_its_worse_leg() {
        let u1 = part("U1", "F.Cu", (10.0, 10.0), &[("5", "XI", 0.0, 0.0), ("6", "XO", 0.0, 1.0), ("7", "GND", 0.0, 2.0)]);
        let y1 = part("Y1", "F.Cu", (14.0, 10.0), &[("1", "XI", 0.0, 0.0), ("3", "XO", 0.0, 1.0), ("2", "GND", 0.0, 2.0)]);
        let xi = seg("XI", "F.Cu", 0.2, (10.0, 10.0), (14.0, 10.0));
        let long_xo = [seg("XO", "F.Cu", 0.2, (10.0, 11.0), (10.0, 31.0)), seg("XO", "F.Cu", 0.2, (10.0, 31.0), (14.0, 31.0)), seg("XO", "F.Cu", 0.2, (14.0, 31.0), (14.0, 11.0))].join(" ");
        let c = path_check(&board(LAYERS4, &format!("{PLANES} {u1} {y1} {xi} {long_xo}")), tie("Y1", "U1", None, Some("crystal")));
        assert_eq!(c.status, "fail", "{c:?}");
        assert_eq!((c.measured["net"].as_str(), c.measured["path_mm"].clone()), (Some("XO"), json!(44.0)));
        let legs: Vec<(&str, &str)> = c.measured["legs"].as_array().unwrap().iter().map(|l| (l["net"].as_str().unwrap(), l["status"].as_str().unwrap())).collect();
        assert_eq!(legs, vec![("GND", "not_judged"), ("XI", "pass"), ("XO", "fail")]);
        assert!(c.detail.contains("XO:"), "{}", c.detail);
        // Both legs short: a pass, reporting the longer one.
        let xo = seg("XO", "F.Cu", 0.2, (10.0, 11.0), (14.0, 11.0));
        let c = path_check(&board(LAYERS4, &format!("{PLANES} {u1} {y1} {xi} {xo}")), tie("Y1", "U1", None, Some("crystal")));
        assert_eq!(c.status, "pass", "{c:?}");
        // One leg not routed: nothing proves the tie, so it is not a pass.
        let c = path_check(&board(LAYERS4, &format!("{PLANES} {u1} {y1} {xi}")), tie("Y1", "U1", None, Some("crystal")));
        assert_eq!(c.status, "unverified", "{c:?}");
        assert!(c.detail.contains("XO:"), "{}", c.detail);
    }

    // ── B2 ──────────────────────────────────────────────────────────────

    #[test]
    fn an_invalid_intent_file_is_refused_by_every_reader() {
        for (body, reason) in [
            (r#"{"schema":1,"ties":[{"part":"C1","to":"U1"}]}"#, "give max_mm"),
            (r#"{"schema":1,"ties":[{"part":"C1","to":"U1","kind":"decoupling_cap"}]}"#, "kind must be one of"),
            (r#"{"schema":1,"ties":[{"part":"C1","to":"U1","max_mm":-1}]}"#, "max_mm must be > 0"),
            (r#"{"schema":1,"nets":{"A_P":{"role":"diff_pair","pair":"A_N","standard":"usb3"},"A_N":{"role":"diff_pair","pair":"A_P"}}}"#, "standard must be one of"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("p.kicad_pro"), "{}").unwrap();
            std::fs::write(dir.path().join("p.intent.json"), body).unwrap();
            let error = load_intent(&dir.path().join("p")).expect_err(body).to_string();
            assert!(error.contains(reason) && error.contains("not a valid design intent"), "{error}");
            // get_design_intent (and check/apply, which load the same way) fail too.
            assert!(handle_get_design_intent(&json!({"project": dir.path().join("p.kicad_pro")})).is_err());
        }
        // A valid file still loads.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.intent.json"), r#"{"schema":1,"ties":[{"part":"C1","to":"U1","max_mm":2.0}]}"#).unwrap();
        assert_eq!(load_intent(&dir.path().join("p")).unwrap().0.ties.len(), 1);
    }

    /// Even past validation, a tie with no limit is never a pass.
    #[test]
    fn a_tie_with_no_limit_is_unverified_not_passed() {
        let b = board(LAYERS4, &format!("{} {}", part("U1", "F.Cu", (10.0, 10.0), &[("1", "V", 0.0, 0.0)]), part("C1", "F.Cu", (90.0, 10.0), &[("1", "V", 0.0, 0.0)])));
        let mut i = DesignIntent { schema: 1, ..Default::default() };
        i.ties.push(tie("C1", "U1", None, None));
        assert!(i.validate().is_err());
        let checks = check_board(&b, &i);
        let t = checks.iter().find(|c| c.kind == "tie").unwrap();
        assert_eq!((t.status, t.required["max_mm"].clone()), ("unverified", Value::Null), "{t:?}");
        assert!(!checks.iter().any(|c| c.kind == "tie_path"));
        assert!(plan_ties(&b, &i, 0.2).0.is_empty());
    }

    // ── B3 ──────────────────────────────────────────────────────────────

    /// Copper KiCad treats as connected is connected in the graph, and the
    /// short piece that bridges an offset is in the length.
    #[test]
    fn overlapping_copper_is_joined_where_coordinates_do_not_coincide() {
        let u1 = part("U1", "F.Cu", (10.0, 10.0), &[("5", "XI", 0.0, 0.0)]);
        let y1 = part("Y1", "F.Cu", (14.0, 10.0), &[("1", "XI", 0.0, 0.0)]);
        let limit = |max: f64| Tie { max_path_mm: Some(max), ..tie("Y1", "U1", Some("5"), None) };
        let run = |body: String| path_check(&board(LAYERS4, &body), limit(9.0));
        // (a) A track through a 0.9 × 0.95 pad, 0.3 mm off its centre, that
        // does not end in it.
        let wide = "(footprint \"X\" (layer \"F.Cu\") (at 14 10) (property \"Reference\" \"Y1\") (pad \"1\" smd rect (at 0 0) (size 0.9 0.95) (layers \"F.Cu\") (net \"XI\")))";
        let c = run(format!("{u1} {wide} {} {}", seg("XI", "F.Cu", 0.2, (10.0, 10.0), (10.0, 10.3)), seg("XI", "F.Cu", 0.2, (10.0, 10.3), (18.0, 10.3))));
        assert_eq!((c.status, c.measured["path_mm"].clone()), ("pass", json!(4.3)), "{c:?}");
        // (b) Two track ends 0.05 mm apart, each inside the other's width.
        let c = run(format!("{u1} {y1} {} {}", seg("XI", "F.Cu", 0.25, (10.0, 10.0), (12.0, 10.0)), seg("XI", "F.Cu", 0.25, (12.0, 10.05), (14.0, 10.05))));
        assert_eq!((c.status, c.measured["path_mm"].clone()), ("pass", json!(4.05)), "{c:?}");
        // (c) A stub landing mid-trunk, 0.05 mm off the trunk's centreline.
        let c = run(format!("{u1} {y1} {} {}", seg("XI", "F.Cu", 0.25, (10.0, 10.0), (10.0, 14.0)), seg("XI", "F.Cu", 0.25, (10.05, 12.0), (14.0, 10.2))));
        let want = 2.0 + 0.05 + (3.95_f64).hypot(1.8);
        assert_eq!(c.status, "pass", "{c:?}");
        assert!(close(c.measured["path_mm"].as_f64().unwrap(), want), "{} vs {want}", c.measured["path_mm"]);
        // (d) A track ending on a via's annular ring, 0.1 mm from its centre.
        let below = part("Y1", "B.Cu", (12.0, 10.0), &[("1", "XI", 0.0, 0.0)]);
        let c = run(format!("{u1} {below} {} {}", seg("XI", "F.Cu", 0.25, (10.0, 10.0), (11.9, 10.0)), via("XI", (12.0, 10.0))));
        assert_eq!((c.status, c.measured["path_mm"].clone(), c.measured["vias"].clone()), ("pass", json!(2.0), json!(1)), "{c:?}");
        // Not joined: a track end 0.4 mm from the via centre is off its 0.3 mm ring…
        let c = run(format!("{u1} {below} {} {}", seg("XI", "F.Cu", 0.25, (10.0, 10.0), (11.6, 10.0)), via("XI", (12.0, 10.0))));
        assert_eq!(c.status, "unverified", "{c:?}");
        assert!(c.detail.contains("no path found in the track graph"), "{}", c.detail);
        // …and a stub 0.2 mm off a 0.25 mm trunk is outside its copper.
        let c = run(format!("{u1} {y1} {} {}", seg("XI", "F.Cu", 0.25, (10.0, 10.0), (10.0, 14.0)), seg("XI", "F.Cu", 0.25, (10.2, 12.0), (14.0, 10.2))));
        assert_eq!(c.status, "unverified", "{c:?}");
    }

    /// The graph itself, as the voltage-drop check and others use it: a load
    /// on a pad the feed track only crosses is reachable.
    #[test]
    fn net_graph_reaches_a_pad_a_track_only_crosses() {
        let body = format!(
            "{} (footprint \"X\" (layer \"F.Cu\") (at 14 10) (property \"Reference\" \"C1\") (pad \"1\" smd rect (at 0 0) (size 0.9 0.95) (layers \"F.Cu\") (net \"V\"))) {}",
            part("U1", "F.Cu", (10.0, 10.3), &[("1", "V", 0.0, 0.0)]),
            seg("V", "F.Cu", 0.2, (10.0, 10.3), (18.0, 10.3))
        );
        let tree = board(LAYERS4, &body);
        let (layers, _) = stack4();
        let graph = net_graph("V", &board_copper(&tree)["V"], &copper_pads(&tree), &[], &layers);
        let (dist, _) = graph.shortest(&[graph.pad("U1.1").unwrap()], length_weight);
        assert!(close(dist[graph.pad("C1.1").unwrap()], 4.0), "{:?}", dist[graph.pad("C1.1").unwrap()]);
        assert!(graph.vias.is_empty());
    }

    #[test]
    fn clipping_a_segment_to_a_pad_gives_the_part_inside() {
        let square = [(0.0, 0.0), (2.0, 0.0), (2.0, 2.0), (0.0, 2.0)];
        let (t0, t1) = clip_to_convex((-1.0, 1.0), (3.0, 1.0), &square).unwrap();
        assert!(close(t0, 0.25) && close(t1, 0.75), "{t0} {t1}");
        // Either winding.
        let reversed: Vec<(f64, f64)> = square.iter().rev().copied().collect();
        assert!(clip_to_convex((-1.0, 1.0), (3.0, 1.0), &reversed).is_some());
        assert!(clip_to_convex((-1.0, 3.0), (3.0, 3.0), &square).is_none());
        assert!(clip_to_convex((-1.0, 1.0), (-0.5, 1.0), &square).is_none());
    }

    // ── D1 ──────────────────────────────────────────────────────────────

    /// Johnson's own numbers: "Via Inductance II" predicts 0.49 nH and
    /// 1.29 nH for a via pair 0.017 in and 0.12 in apart (h 0.040 in, r
    /// 0.005 in); each via is half of the pair. With no return via the
    /// barrel is formula [7.9], 5.08·h·[ln(4h/d) + 1] nH with inches.
    #[test]
    fn via_barrel_inductance_matches_johnsons_published_values() {
        let mm = 25.4;
        for (s, pair_nh) in [(0.017, 0.49), (0.02, 0.56), (0.03, 0.73), (0.06, 1.00)] {
            let got = 2.0 * barrel_nh(0.040 * mm, 0.010 * mm, Some(s * mm));
            assert!((got - pair_nh).abs() < 0.011, "s {s} in: {got} vs {pair_nh}");
        }
        let (h, d): (f64, f64) = (0.063, 0.012);
        let formula_7_9 = 5.08 * h * ((4.0 * h / d).ln() + 1.0);
        assert!((barrel_nh(h * mm, d * mm, None) - formula_7_9).abs() < 1e-6, "{formula_7_9}");
        // A far return never gives more than [7.9].
        assert_eq!(barrel_nh(h * mm, d * mm, Some(500.0)), barrel_nh(h * mm, d * mm, None));
        // The plane term is the spreading inductance of UG483 Table 2-10:
        // μ0·h = 128 pH per square at 102 µm, i.e. (μ/2π)·h per e-fold of span.
        assert!((plane_nh(0.102, 0.15 * std::f64::consts::E.powf(2.0 * std::f64::consts::PI), 0.3) - 0.128).abs() < 0.001);
    }

    /// The verdict compares the estimate with the budget, not a length.
    #[test]
    fn a_crystal_leg_is_judged_on_its_capacitance() {
        let u1 = part("U1", "F.Cu", (10.0, 10.0), &[("5", "XI", 0.0, 0.0)]);
        let run = |w: f64, len: f64| {
            let y1 = part("Y1", "F.Cu", (10.0 + len, 10.0), &[("1", "XI", 0.0, 0.0)]);
            path_check(&board(LAYERS4, &format!("{PLANES} {u1} {y1} {}", seg("XI", "F.Cu", w, (10.0, 10.0), (10.0 + len, 10.0)))), tie("Y1", "U1", Some("5"), Some("crystal")))
        };
        let (layers, planes) = stack4();
        let per_mm = |w: f64| layers.model("F.Cu", &planes).unwrap().per_mm(w).c_pf_per_mm;
        // 0.3 mm × 5.7 mm is inside the 6 mm guide but over 0.5 pF.
        let wide = run(0.3, 5.7);
        assert!(5.7 * per_mm(0.3) > 0.5);
        assert_eq!(wide.status, "fail", "{wide:?}");
        assert!(wide.detail.contains("pF > 0.5 pF"), "{}", wide.detail);
        assert!(close(wide.measured["est_capacitance_pf"].as_f64().unwrap(), 5.7 * per_mm(0.3)));
        // 0.1 mm × 5.9 mm is under both.
        let thin = run(0.1, 5.9);
        assert!(5.9 * per_mm(0.1) < 0.5);
        assert_eq!(thin.status, "pass", "{thin:?}");
        // Under 0.5 pF but past the length guide still fails, on the guide.
        let long = run(0.1, 6.5);
        assert!(6.5 * per_mm(0.1) < 0.5);
        assert_eq!(long.status, "fail", "{long:?}");
        assert!(long.detail.contains("path 6.5 mm > 6 mm"), "{}", long.detail);
    }

    fn u1_supply() -> String {
        part("U1", "F.Cu", (10.0, 10.0), &[("1", "+3V3", 0.0, 0.0), ("2", "GND", 0.0, 1.0)])
    }

    /// A capacitor beside its pin, both legs routed as tracks: the loop is
    /// the two legs added up.
    #[test]
    fn a_decoupling_loop_adds_the_supply_and_return_legs() {
        let c1 = part("C1", "F.Cu", (12.0, 10.0), &[("1", "+3V3", 0.0, 0.0), ("2", "GND", 0.0, 1.0)]);
        let supply = |len: f64| seg("+3V3", "F.Cu", 0.3, (10.0, 10.0), (10.0 + len, 10.0));
        let ground = seg("GND", "F.Cu", 0.3, (10.0, 11.0), (12.0, 11.0));
        let (layers, planes) = stack4();
        let l = layers.model("F.Cu", &planes).unwrap().per_mm(0.3).l_nh_per_mm;
        let c = path_check(&board(LAYERS4, &format!("{PLANES} {} {c1} {} {ground}", u1_supply(), supply(2.0))), tie("C1", "U1", Some("1"), Some("decoupling")));
        assert_eq!(c.status, "pass", "{c:?}");
        assert!(close(c.measured["loop_inductance_nh"].as_f64().unwrap(), 4.0 * l), "{} vs {}", c.measured["loop_inductance_nh"], 4.0 * l);
        assert_eq!(c.required["budget_nh"], json!(4.0));
        let legs = c.measured["legs"].as_array().unwrap();
        assert_eq!(legs.iter().map(|l| (l["net"].as_str().unwrap(), l["carried_by"].as_str().unwrap(), l["to_pad"].as_str().unwrap())).collect::<Vec<_>>(), vec![("+3V3", "tracks", "U1.1"), ("GND", "tracks", "U1.2")]);
        // Without the pin the same two legs are found.
        let pinless = path_check(&board(LAYERS4, &format!("{PLANES} {} {c1} {} {ground}", u1_supply(), supply(2.0))), tie("C1", "U1", None, Some("decoupling")));
        assert_eq!((pinless.status, pinless.measured["loop_inductance_nh"].clone()), ("pass", c.measured["loop_inductance_nh"].clone()));
        // Review B1/D4: the ground leg missing is not a pass on the supply leg alone…
        let c = path_check(&board(LAYERS4, &format!("{PLANES} {} {c1} {}", u1_supply(), supply(2.0))), tie("C1", "U1", Some("1"), Some("decoupling")));
        assert_eq!(c.status, "unverified", "{c:?}");
        assert!(c.detail.contains("not verified: GND:"), "{}", c.detail);
        // …but a supply leg that alone is over the budget is a proven failure.
        let far = part("C1", "F.Cu", (24.0, 10.0), &[("1", "+3V3", 0.0, 0.0), ("2", "GND", 0.0, 1.0)]);
        assert!(14.0 * l > 4.0);
        let c = path_check(&board(LAYERS4, &format!("{PLANES} {} {far} {}", u1_supply(), supply(14.0))), tie("C1", "U1", Some("1"), Some("decoupling")));
        assert_eq!(c.status, "fail", "{c:?}");
        assert!(c.detail.contains("loop inductance") && c.detail.contains("> 4 nH"), "{}", c.detail);
        // A wide 7 mm pair of legs is under the budget where a length limit
        // of 6 mm used to fail it.
        let seven = part("C1", "F.Cu", (17.0, 10.0), &[("1", "+3V3", 0.0, 0.0), ("2", "GND", 0.0, 1.0)]);
        let lw = layers.model("F.Cu", &planes).unwrap().per_mm(1.0).l_nh_per_mm;
        assert!(14.0 * lw < 4.0);
        let body = format!("{PLANES} {} {seven} {} {}", u1_supply(), seg("+3V3", "F.Cu", 1.0, (10.0, 10.0), (17.0, 10.0)), seg("GND", "F.Cu", 1.0, (10.0, 11.0), (17.0, 11.0)));
        assert_eq!(path_check(&board(LAYERS4, &body), tie("C1", "U1", Some("1"), Some("decoupling"))).status, "pass");
    }

    /// The capacitor on the other side, each leg through its own via: the
    /// barrels are in the estimate, each with the other leg's via as return.
    #[test]
    fn via_barrels_are_part_of_the_loop_inductance() {
        let c1 = part("C1", "B.Cu", (11.2, 10.0), &[("1", "+3V3", 0.0, 0.0), ("2", "GND", 0.0, 1.0)]);
        let mut body = format!("{} {c1}", u1_supply());
        for (net, y) in [("+3V3", 10.0), ("GND", 11.0)] {
            body.push_str(&format!(" {} {} {}", seg(net, "F.Cu", 0.3, (10.0, y), (10.6, y)), seg(net, "B.Cu", 0.3, (10.6, y), (11.2, y)), via(net, (10.6, y))));
        }
        let c = path_check(&board(LAYERS4, &format!("{PLANES} {body}")), tie("C1", "U1", Some("1"), Some("decoupling")));
        let (layers, planes) = stack4();
        let height: f64 = layers.gaps.iter().map(|g| g.thickness_mm).sum();
        // The return via of each leg is the other leg's, 1 mm away.
        let barrel = MU_OVER_2PI_NH_PER_MM * height * (1.0_f64 / 0.15).ln();
        let tracks = 0.6 * (layers.model("F.Cu", &planes).unwrap().per_mm(0.3).l_nh_per_mm + layers.model("B.Cu", &planes).unwrap().per_mm(0.3).l_nh_per_mm);
        for leg in c.measured["legs"].as_array().unwrap() {
            assert_eq!((leg["vias"].clone(), leg["path_mm"].clone()), (json!(1), json!(1.2)), "{leg}");
            assert!(close(leg["via_barrel_mm"].as_f64().unwrap(), height), "{leg}");
            assert!(close(leg["via_inductance_nh"].as_f64().unwrap(), barrel), "{leg} vs {barrel}");
            assert!(close(leg["est_inductance_nh"].as_f64().unwrap(), tracks + barrel), "{leg}");
        }
        assert!(barrel > 0.5, "a through via is not negligible: {barrel}");
        assert!(close(c.measured["loop_inductance_nh"].as_f64().unwrap(), 2.0 * (tracks + barrel)), "{c:?}");
        assert_eq!(c.status, "pass", "{c:?}");
        // A lone leg has no return via to pair with: formula [7.9].
        let lone = tie_path(
            &net_graph("+3V3", &board_copper(&board(LAYERS4, &body))["+3V3"], &copper_pads(&board(LAYERS4, &body)), &konnect_sexp::board::vias(&board(LAYERS4, &body)).items, &layers),
            "+3V3",
            &copper_pads(&board(LAYERS4, &body)).iter().filter(|p| p.reference == "C1").collect::<Vec<_>>(),
            &copper_pads(&board(LAYERS4, &body)).iter().filter(|p| p.reference == "U1").collect::<Vec<_>>(),
            &layers,
            &planes,
        )
        .unwrap();
        assert!(close(lone.via_inductance_nh, barrel_nh(height, 0.3, None)) && lone.via_inductance_nh > barrel);
    }

    // ── D4 ──────────────────────────────────────────────────────────────

    /// Capacitor and pin each drop to the planes through their own vias: the
    /// legs are stub + via at each end with the plane between.
    #[test]
    fn a_leg_through_the_plane_is_measured_stub_via_plane_via_stub() {
        let c1 = part("C1", "F.Cu", (20.0, 10.0), &[("1", "+3V3", 0.0, 0.0), ("2", "GND", 0.0, 1.0)]);
        let mut body = format!("{} {c1}", u1_supply());
        // Pin side: vias 0.5 mm above / below the pads; capacitor side: 0.5 mm to the right.
        for (net, pin_via, pad_y) in [("+3V3", (10.0, 9.5), 10.0), ("GND", (10.0, 11.5), 11.0)] {
            body.push_str(&format!(" {} {} {} {}", seg(net, "F.Cu", 0.3, (10.0, pad_y), pin_via), via(net, pin_via), seg(net, "F.Cu", 0.3, (20.0, pad_y), (20.5, pad_y)), via(net, (20.5, pad_y))));
        }
        let c = path_check(&board(LAYERS4, &format!("{PLANES} {body}")), tie("C1", "U1", Some("1"), Some("decoupling")));
        let (layers, planes) = stack4();
        let core = layers.gaps[1].thickness_mm;
        let legs = c.measured["legs"].as_array().unwrap();
        assert_eq!(legs.len(), 2, "{c:?}");
        for leg in legs {
            let net = leg["net"].as_str().unwrap();
            assert_eq!((leg["carried_by"].as_str(), leg["path_mm"].clone(), leg["vias"].clone()), (Some("plane"), json!(1.0), json!(2)), "{leg}");
            let span = (10.5_f64).hypot(if net == "GND" { 0.5 } else { 0.5 });
            assert!(close(leg["plane_span_mm"].as_f64().unwrap(), span), "{leg}");
            // The loop height of the plane part is the In1–In2 core.
            assert!(close(leg["plane_inductance_nh"].as_f64().unwrap(), MU_OVER_2PI_NH_PER_MM * core * (span / 0.15).ln()), "{leg}");
            // Barrels run from F.Cu to the net's own plane only.
            let depth = if net == "GND" { layers.gaps[0].thickness_mm } else { layers.gaps[0].thickness_mm + core };
            assert!(close(leg["via_barrel_mm"].as_f64().unwrap(), 2.0 * depth), "{leg}");
            let tracks = layers.model("F.Cu", &planes).unwrap().per_mm(0.3).l_nh_per_mm;
            let sum = leg["track_inductance_nh"].as_f64().unwrap() + leg["via_inductance_nh"].as_f64().unwrap() + leg["plane_inductance_nh"].as_f64().unwrap();
            assert!(close(leg["track_inductance_nh"].as_f64().unwrap(), tracks) && close(leg["est_inductance_nh"].as_f64().unwrap(), sum), "{leg}");
        }
        let total: f64 = legs.iter().map(|l| l["est_inductance_nh"].as_f64().unwrap()).sum();
        assert!(close(c.measured["loop_inductance_nh"].as_f64().unwrap(), total));
        assert_eq!(c.status, if total <= 4.0 { "pass" } else { "fail" }, "{c:?}");
        // The pin's via missing: that end has no way into the plane.
        let cut = body.replace(&via("+3V3", (10.0, 9.5)), "");
        let c = path_check(&board(LAYERS4, &format!("{PLANES} {cut}")), tie("C1", "U1", Some("1"), Some("decoupling")));
        assert_eq!(c.status, "unverified", "{c:?}");
        assert!(c.detail.contains("+3V3: no routed track path, and no via"), "{}", c.detail);
        // A via outside the net's pour does not count as a drop into it.
        let small = PLANES.replace("(zone (net \"+3V3\") (layers \"In2.Cu\") (polygon (pts (xy 0 0) (xy 50 0) (xy 50 50) (xy 0 50))))", "(zone (net \"+3V3\") (layers \"In2.Cu\") (polygon (pts (xy 0 0) (xy 15 0) (xy 15 50) (xy 0 50))))");
        assert_ne!(small, PLANES);
        let c = path_check(&board(LAYERS4, &format!("{small} {body}")), tie("C1", "U1", Some("1"), Some("decoupling")));
        assert_eq!(c.status, "unverified", "{c:?}");
    }

    /// No reference plane: the inductance is not modelled, so a short leg is
    /// unverified (never a pass) and a long one fails the guide.
    #[test]
    fn a_loop_without_a_reference_plane_is_never_a_pass() {
        let two = "(general (thickness 1.6)) (layers (0 \"F.Cu\" signal) (2 \"B.Cu\" signal))";
        let run = |len: f64| {
            let c1 = part("C1", "F.Cu", (10.0 + len, 10.0), &[("1", "+3V3", 0.0, 0.0), ("2", "GND", 0.0, 1.0)]);
            let body = format!("{} {c1} {} {}", u1_supply(), seg("+3V3", "F.Cu", 0.3, (10.0, 10.0), (10.0 + len, 10.0)), seg("GND", "F.Cu", 0.3, (10.0, 11.0), (10.0 + len, 11.0)));
            path_check(&board(two, &body), tie("C1", "U1", Some("1"), Some("decoupling")))
        };
        let short = run(2.0);
        assert_eq!(short.status, "unverified", "{short:?}");
        assert!(short.detail.contains("no reference plane"), "{}", short.detail);
        let long = run(8.0);
        assert_eq!(long.status, "fail", "{long:?}");
        assert!(long.detail.contains("exceeds the 6 mm guide"), "{}", long.detail);
    }

    // ── B5, D6 ──────────────────────────────────────────────────────────

    #[test]
    fn a_length_limit_on_one_half_of_a_pair_applies_to_both() {
        let mut i = DesignIntent { schema: 1, ..Default::default() };
        i.nets.insert("M_P".into(), NetIntent { role: "diff_pair".into(), pair: Some("M_N".into()), standard: Some("ethernet_1000base_t".into()), ..Default::default() });
        i.nets.insert("M_N".into(), NetIntent { role: "diff_pair".into(), pair: Some("M_P".into()), max_length_mm: Some(20.0), ..Default::default() });
        assert!(i.validate().is_ok());
        let reqs = requirements(&i);
        let of = |net: &str| reqs.iter().find(|r| r.net == net).unwrap();
        assert_eq!((of("M_P").max_length_mm, of("M_N").max_length_mm), (Some(20.0), Some(20.0)));
        assert_eq!(of("M_P").sources["max_length_mm"], "user, set on pair partner M_N");
        assert_eq!(of("M_N").sources["max_length_mm"], "user");
    }

    #[test]
    fn full_speed_usb_takes_its_skew_from_the_edge_rate_not_the_high_speed_number() {
        let pair = |standard: &str| {
            let mut i = DesignIntent { schema: 1, ..Default::default() };
            i.nets.insert("D_P".into(), NetIntent { role: "diff_pair".into(), pair: Some("D_N".into()), standard: Some(standard.into()), ..Default::default() });
            i.nets.insert("D_N".into(), NetIntent { role: "diff_pair".into(), pair: Some("D_P".into()), ..Default::default() });
            requirements(&i).into_iter().find(|r| r.net == "D_P").unwrap()
        };
        let fs = pair("usb2_fs");
        // 10 % of 4 ns at about 6–7 ps/mm.
        assert!((55.0..75.0).contains(&fs.max_skew_mm.unwrap()), "{:?}", fs.max_skew_mm);
        assert!(fs.sources["max_skew_mm"].starts_with("edge-rate bound"), "{}", fs.sources["max_skew_mm"]);
        assert_eq!(fs.effective.impedance_ohm, Some(90.0));
        let hs = pair("usb2_hs");
        assert_eq!(hs.max_skew_mm, Some(1.27));
        assert!(hs.sources["max_skew_mm"].contains("ti-spraar7j"));
    }

    // ── D5 ──────────────────────────────────────────────────────────────

    #[test]
    fn a_plane_check_counts_the_filled_pieces_of_the_pour() {
        let fill = |layer: &str, x: f64| format!("(filled_polygon (layer \"{layer}\") (pts (xy {x} 0) (xy {} 0) (xy {} 10)))", x + 10.0, x + 10.0);
        let plane = |fills: &str, extra: &str| {
            let b = board(LAYERS4, &format!("(zone (net \"+3V3\") (layers \"In2.Cu\" \"F.Cu\") (polygon (pts (xy 0 0) (xy 50 0) (xy 50 50))) {fills}) {extra}"));
            let mut i = DesignIntent { schema: 1, ..Default::default() };
            i.nets.insert("+3V3".into(), NetIntent { role: "plane".into(), current_a: Some(1.0), ..Default::default() });
            check_board(&b, &i).into_iter().filter(|c| c.subject == "+3V3").collect::<Vec<_>>()
        };
        let of = |checks: &[Check], kind: &str| checks.iter().find(|c| c.kind == kind).cloned();
        // Not filled: it joins nothing yet.
        let c = of(&plane("", ""), "plane").unwrap();
        assert_eq!(c.status, "unverified", "{c:?}");
        assert!(c.detail.contains("not filled"), "{}", c.detail);
        // One piece on the inner layer; an outer pour in pieces is normal.
        let c = of(&plane(&format!("{} {} {}", fill("In2.Cu", 0.0), fill("F.Cu", 0.0), fill("F.Cu", 20.0)), ""), "plane").unwrap();
        assert_eq!(c.status, "pass", "{c:?}");
        assert_eq!(c.measured["filled_pieces"], json!({"F.Cu": 2, "In2.Cu": 1}));
        // The inner pour cut in two, rejoined by a 0.127 mm track: not a pass,
        // and the track is held to the net's current.
        let checks = plane(&format!("{} {}", fill("In2.Cu", 0.0), fill("In2.Cu", 20.0)), &seg("+3V3", "In2.Cu", 0.127, (9.0, 1.0), (21.0, 1.0)));
        let c = of(&checks, "plane").unwrap();
        assert_eq!(c.status, "unverified", "{c:?}");
        assert!(c.detail.contains("In2.Cu: 2 pieces") && c.detail.contains("narrowest 0.127 mm"), "{}", c.detail);
        assert_eq!(c.measured["narrowest_track_mm"], json!(0.127));
        let width = of(&checks, "track_width").expect("a plane net with current_a has its tracks checked");
        assert_eq!(width.status, "fail", "{width:?}");
        // No zone at all is still a failure.
        let mut i = DesignIntent { schema: 1, ..Default::default() };
        i.nets.insert("+3V3".into(), NetIntent { role: "plane".into(), ..Default::default() });
        assert_eq!(check_board(&board(LAYERS4, ""), &i).into_iter().find(|c| c.kind == "plane").unwrap().status, "fail");
    }
}
