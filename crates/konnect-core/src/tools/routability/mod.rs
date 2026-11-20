//! `check_routability` (Pass 23): a read-only pre-check that says, before a
//! ten-minute autoroute, whether a placement is `doomed`, `tight` or `fine`,
//! with the numbers, places and nets behind each reason.
//!
//! The metrics, each in its own module so later tools can call them:
//! 1. [`crossings`]: airwire crossings, a DSatur layer estimate and a via
//!    estimate. Can only make a board `tight`.
//! 2. [`escape`]: pin-escape capacity per IC side (Wang's channel capacity,
//!    UG1099's row rule). `doomed` when the escape needs more signal layers
//!    than the board has.
//! 3. [`capacity`]: section-crossing cuts across the whole board (a
//!    necessary condition, so a failing full-board cut is a proof of doom),
//!    cuts through the channels between parts, and a RUDY congestion grid.
//! 4. [`capacity::density`]: Holden's wiring demand over substrate capacity.
//!
//! Airwires come from KiCad itself when possible: an offline `kicad-cli pcb
//! drc` (zones refilled) reports every missing connection with both items,
//! which already accounts for partial routing and pours. Without kicad-cli
//! the fallback is KiCad's own construction: a minimum spanning tree per net
//! over its copper, with existing copper joined at zero weight
//! (`kicad-10-0-8-ratsnest-data`, `RN_NET::kruskalMST`).

pub(crate) mod capacity;
pub(crate) mod crossings;
pub(crate) mod escape;
pub(crate) mod geom;
#[cfg(test)]
mod tests;

use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::fanout::{copper_pads, BoardOutline, CopperPad};
use crate::tools::{get_path, with_board_ipc_classified, ToolContext, ToolDef};
use capacity::{Board, Obstacle, PartBox, Rules};
use crossings::{Airwire, AirwireEnd, EndLayers, TrackSeg};
use escape::{round3, Package, PackagePad};
use geom::{Pt, Raster, Shape};
use konnect_sexp::parser::SexpNode;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// Layout efficiency used when the caller gives none. Holden
/// (`holden-tt26-density-equation`) defines the factor but its table of
/// typical values is an image not in the doc DB, and `holden-tt42-density-equations`
/// says to fit it on your own boards. Calibrated (Pass 23, FORK_CHANGES.md):
/// routed signal length over raw capacity (free area / pitch) on the
/// benchmark boards was 0.126 for the densest completed one (`stm32_demo`),
/// 0.109 / 0.072 for the nearly complete module and Ethernet boards and
/// 0.026 / 0.008 for the easy PoC boards; E is the densest completed board,
/// rounded up. U > 1 therefore means "denser than any board routed so far".
pub(crate) const DEFAULT_LAYOUT_EFFICIENCY: f64 = 0.13;
const CUT_STEP_MM: f64 = 0.5;
const CHANNEL_MAX_GAP_MM: f64 = 3.0;
const MIN_ESCAPE_PADS: usize = 8;

// ─── Thresholds ─────────────────────────────────────────────────────────────

/// Every threshold the verdict uses, with where it comes from.
pub(crate) struct Threshold {
    pub id: &'static str,
    pub rule: &'static str,
    pub level: &'static str,
    pub value: f64,
    pub kind: &'static str,
    pub source: &'static str,
}

pub(crate) const THRESHOLDS: &[Threshold] = &[
    Threshold { id: "escape.layers", rule: "escape needs more signal layers than the board has", level: "doomed", value: 0.0, kind: "sourced", source: "wang-iccad06-escape-layer-minimization; xilinx-ug1099-bga-design-rules (Eq. 2-1 and the outermost-row rule)" },
    Threshold { id: "cut.lower_bound", rule: "nets that must cross a full-board cut need more lanes than it has", level: "doomed", value: 1.0, kind: "sourced", source: "holden-tt42-density-equations (section crossing method); a necessary condition: every route between the two sides crosses the line" },
    Threshold { id: "density.u_doomed", rule: "U = Wd / Wc above 1", level: "doomed", value: 1.0, kind: "sourced", source: "holden-tt26-density-equation (Wd > Wc: not enough wires); E itself is calibrated, not sourced" },
    Threshold { id: "density.u_tight", rule: "U above 1/1.2 (less than about 20 % spare)", level: "tight", value: 0.833, kind: "sourced", source: "holden-tt26-density-equation (Wd < Wc: about 20 % is a good target, especially if autorouting)" },
    Threshold { id: "crossings.layers", rule: "DSatur colours of the crossing graph exceed the signal layers by more than this", level: "tight", value: 1.0, kind: "judgement", source: "Konnect judgement, calibrated: the PoC mcu_board needs 3 colours on 2 layers and Freerouting completes it" },
    Threshold { id: "crossings.k_over_e", rule: "crossings per airwire", level: "tight", value: 0.3, kind: "judgement", source: "Konnect judgement" },
    Threshold { id: "crossings.vias", rule: "via estimate per airwire", level: "tight", value: 1.0, kind: "judgement", source: "Konnect judgement (2 vias per hop)" },
    Threshold { id: "escape.channel", rule: "interior nets over a side's channel capacity", level: "tight", value: 0.8, kind: "judgement", source: "Konnect judgement" },
    Threshold { id: "escape.fanout_pitch", rule: "neighbouring pads that both need a trace closer than the fab-minimum track pitch (narrower than the class pitch is reported as needs_neck_down only: necking down in the breakout is allowed, xilinx-ug1099-bga-design-rules)", level: "tight", value: 1.0, kind: "judgement", source: "Konnect judgement, from Wang's C formula" },
    Threshold { id: "cut.utilization", rule: "airwire demand over a full-board cut's capacity", level: "tight", value: 0.8, kind: "judgement", source: "Konnect judgement" },
    Threshold { id: "channel.utilization", rule: "airwires through the gap between two parts over its capacity, with at least 3 airwires (a route can go round)", level: "tight", value: 1.0, kind: "judgement", source: "Konnect judgement" },
    Threshold { id: "congestion.overflow", rule: "share of grid cells whose RUDY demand exceeds supply", level: "tight", value: 0.05, kind: "judgement", source: "Konnect judgement; overflow as defined by circuitnet-routability-features, supply per lou-ispd2001-probabilistic-congestion-slides" },
];

fn threshold(id: &str) -> &'static Threshold {
    THRESHOLDS.iter().find(|t| t.id == id).expect("threshold id")
}

fn threshold_json(t: &Threshold) -> Value {
    json!({ "id": t.id, "rule": t.rule, "level": t.level, "value": t.value, "kind": t.kind, "source": t.source })
}

// ─── Tool ───────────────────────────────────────────────────────────────────

pub fn tools() -> Vec<ToolDef> {
    vec![tool!(
        "check_routability",
        "Routability pre-check before autorouting (read-only, seconds instead of a 10-minute \
         route). Verdict 'doomed' | 'tight' | 'fine' with reasons, each naming the threshold \
         and whether it is sourced or a judgement, plus the numbers, coordinates and nets \
         behind it: airwire crossings (DSatur layer and via estimate, worst net pairs, \
         rotation what-ifs for the parts most involved), pin-escape capacity per IC side \
         (Wang channel capacity, UG1099 row rule; nets behind the part, interior nets needing \
         a via), full-board section cuts (a failing cut proves the placement cannot route) \
         and cuts through the gaps between parts, a RUDY congestion grid with obstacles \
         subtracted, Holden's wiring density U = Wd/Wc and pin density. Airwires are KiCad's \
         own (offline kicad-cli DRC, zones refilled, existing copper counted); without \
         kicad-cli a per-net minimum spanning tree. Nets carried by planes or pours are not \
         demand. 'nets' / 'net_classes' pre-check one routing group (same meaning as \
         export_specctra_dsn). A 'tight' verdict recommends a short grouped Freerouting pass \
         of the worst nets as the tiebreaker.",
        json!({
            "type": "object",
            "properties": {
                "board": { "type": "string", "description": "Path to .kicad_pcb file" },
                "nets": { "type": "array", "items": { "type": "string" }, "description": "Only these nets are demand (a routing group)" },
                "net_classes": { "type": "array", "items": { "type": "string" }, "description": "Every net of these classes is demand ('Default' = nets with no other class)" },
                "signal_layers": { "type": "array", "items": { "type": "string" }, "description": "Copper layers to route on (default: signal and mixed layers of the stack-up)" },
                "cell_mm": { "type": "number", "default": 1.0, "description": "Congestion grid cell size, 0.25 to 5 mm" },
                "layout_efficiency": { "type": "number", "description": "Holden's E, 0 < E <= 1 (default 0.13, calibrated on the benchmark boards: FORK_CHANGES Pass 23)" },
                "airwires": { "type": "string", "enum": ["auto", "kicad", "mst"], "default": "auto", "description": "'kicad': KiCad's DRC ratsnest only; 'mst': per-net spanning tree only; 'auto': KiCad, else the tree" }
            },
            "required": ["board"]
        }),
        |args, ctx| async move { handle_check_routability(args, ctx).await }
    )]
}

fn refuse(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(message.into())
}

fn string_list(args: &Value, key: &str) -> Result<Option<Vec<String>>, CallToolResult> {
    match &args[key] {
        Value::Null => Ok(None),
        Value::Array(items) => items
            .iter()
            .map(|v| v.as_str().map(str::to_string).ok_or_else(|| refuse(format!("'{key}' must be an array of strings"))))
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        _ => Err(refuse(format!("'{key}' must be an array of strings"))),
    }
}

async fn handle_check_routability(args: &Value, ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let board_path = get_path(args, "board")?;
    let nets = match string_list(args, "nets") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let classes = match string_list(args, "net_classes") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let signal_layers = match string_list(args, "signal_layers") {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let cell_mm = args["cell_mm"].as_f64().unwrap_or(1.0);
    if !(0.25..=5.0).contains(&cell_mm) {
        return Ok(refuse("'cell_mm' must be between 0.25 and 5"));
    }
    let efficiency = match args["layout_efficiency"].as_f64() {
        None if args["layout_efficiency"].is_null() => None,
        Some(e) if e > 0.0 && e <= 1.0 => Some(e),
        _ => return Ok(refuse("'layout_efficiency' must be a number with 0 < E <= 1")),
    };
    let source_mode = args["airwires"].as_str().unwrap_or("auto");
    if !matches!(source_mode, "auto" | "kicad" | "mst") {
        return Ok(refuse("'airwires' must be 'auto', 'kicad' or 'mst'"));
    }

    // Live board when KiCad has it open, else the saved file (as score_placement).
    let live = with_board_ipc_classified(ctx, &board_path, move |client, document| client.save_document_to_string_in(document)).await?;
    let (content, board_source) = match live {
        Ok(content) => (content, "ipc"),
        Err(konnect_ipc::IpcFailure::Unreachable(_)) => match read_saved(&board_path)? {
            Ok(c) => (c, "saved_file"),
            Err(r) => return Ok(r),
        },
        Err(konnect_ipc::IpcFailure::Target { error, .. }) if error.proves_not_open() => match read_saved(&board_path)? {
            Ok(c) => (c, "saved_file"),
            Err(r) => return Ok(r),
        },
        Err(konnect_ipc::IpcFailure::Target { error, .. }) => return Ok(crate::tools::ipc_target_error_result(&error)),
        Err(konnect_ipc::IpcFailure::Rejected(message)) => return Ok(CallToolResult::error(message)),
        Err(konnect_ipc::IpcFailure::Recovered(message)) => return Ok(crate::tools::ipc_recovered_result(&message)),
        Err(konnect_ipc::IpcFailure::Uncertain(message)) => return Ok(crate::tools::ipc_uncertain_result(&message)),
    };
    let tree = konnect_sexp::parse_sexp(&content)?;
    let project = std::fs::read_to_string(board_path.with_extension("kicad_pro"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok());
    let rules = NetRules::from_project(project.as_ref());
    let model = BoardModel::read(&tree);

    let group = if nets.is_some() || classes.is_some() {
        match Group::resolve(nets.unwrap_or_default(), classes.unwrap_or_default(), &model, &rules) {
            Ok(g) => Some(g),
            Err(message) => return Ok(refuse(message)),
        }
    } else {
        None
    };

    // Airwires: KiCad's own ratsnest, else the spanning tree.
    let mut fallback_reason = None;
    let mut airwires = None;
    if source_mode != "mst" {
        let cli = if ctx.config.kicad_cli.is_empty() { "kicad-cli".to_string() } else { ctx.config.kicad_cli.clone() };
        match crate::tools::pcb_routing::drc_with_project_rules(&cli, &board_path, &content, true).await {
            Ok((report, _)) => match report.unconnected_items {
                Some(items) => airwires = Some(airwires_from_drc(&items, &model)),
                None => fallback_reason = Some("kicad-cli reported no unconnected_items category".to_string()),
            },
            Err(error) => fallback_reason = Some(format!("offline kicad-cli DRC failed: {error:#}")),
        }
        if airwires.is_none() && source_mode == "kicad" {
            return Ok(refuse(format!(
                "airwires: 'kicad' needs KiCad's DRC ratsnest: {}",
                fallback_reason.unwrap_or_default()
            )));
        }
    }
    let (airwires, airwire_source) = match airwires {
        Some(a) => (a, "kicad_drc"),
        None => (mst_airwires(&model), "mst"),
    };
    let options = Options { signal_layers, cell_mm, efficiency: efficiency.unwrap_or(DEFAULT_LAYOUT_EFFICIENCY), efficiency_given: efficiency.is_some() };
    let mut report = match analyse(&model, &rules, &airwires, group.as_ref(), &options) {
        Ok(r) => r,
        Err(message) => return Ok(refuse(message)),
    };
    report["board_source"] = json!(board_source);
    report["airwires"]["source"] = json!(airwire_source);
    if let Some(reason) = fallback_reason.filter(|_| airwire_source == "mst") {
        report["airwires"]["fallback_reason"] = json!(reason);
    }
    Ok(CallToolResult::json(&report))
}

fn read_saved(board: &std::path::Path) -> anyhow::Result<Result<String, CallToolResult>> {
    match konnect_sexp::writer::read_consistent(board) {
        Ok(content) => Ok(Ok(content)),
        Err(error) if !board.exists() => {
            let _ = error;
            Ok(Err(CallToolResult::error_kind(
                crate::mcp::error::ToolErrorKind::FileNotFound { path: board.display().to_string() },
                format!("Board file not found: {}", board.display()),
            )))
        }
        Err(error) => Err(error.into()),
    }
}

// ─── Board model ────────────────────────────────────────────────────────────

/// Net classes and the project's minimums, from the `.kicad_pro`.
#[derive(Debug, Clone)]
pub(crate) struct NetRules {
    classes: BTreeMap<String, (f64, f64)>,
    patterns: Vec<(String, String)>,
    pub fab_w: f64,
    pub fab_s: f64,
    pub edge_clearance: f64,
}

impl NetRules {
    pub(crate) fn from_project(project: Option<&Value>) -> Self {
        let null = Value::Null;
        let settings = project.map(|p| &p["net_settings"]).unwrap_or(&null);
        let mut classes = BTreeMap::new();
        let default = settings["classes"]
            .as_array()
            .and_then(|cs| cs.iter().find(|c| c["name"] == "Default"))
            .map(|c| (c["track_width"].as_f64().unwrap_or(0.2), c["clearance"].as_f64().unwrap_or(0.2)))
            .unwrap_or((0.2, 0.2));
        for class in settings["classes"].as_array().into_iter().flatten() {
            if let Some(name) = class["name"].as_str() {
                classes.insert(
                    name.to_string(),
                    (class["track_width"].as_f64().unwrap_or(default.0), class["clearance"].as_f64().unwrap_or(default.1)),
                );
            }
        }
        classes.entry("Default".to_string()).or_insert(default);
        let patterns = settings["netclass_patterns"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| Some((p["pattern"].as_str()?.to_string(), p["netclass"].as_str()?.to_string())))
            .collect();
        let design = project.map(|p| &p["board"]["design_settings"]["rules"]).unwrap_or(&null);
        Self {
            fab_w: design["min_track_width"].as_f64().unwrap_or(default.0).min(default.0),
            fab_s: design["min_clearance"].as_f64().unwrap_or(default.1).min(default.1),
            edge_clearance: design["min_copper_edge_clearance"].as_f64().unwrap_or(0.0),
            classes,
            patterns,
        }
    }

    pub(crate) fn class_of(&self, net: &str) -> String {
        self.patterns
            .iter()
            .find(|(pattern, _)| crate::tools::pcb_routing::glob_matches(pattern, net))
            .map(|(_, class)| class.clone())
            .filter(|class| self.classes.contains_key(class))
            .unwrap_or_else(|| "Default".to_string())
    }

    /// Track width and clearance of a net's class.
    pub(crate) fn rule(&self, net: &str) -> (f64, f64) {
        self.classes[&self.class_of(net)]
    }

    pub(crate) fn default_rule(&self) -> (f64, f64) {
        self.classes["Default"]
    }

    pub(crate) fn has_class(&self, class: &str) -> bool {
        self.classes.contains_key(class)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TrackItem {
    pub seg: TrackSeg,
    pub width: f64,
    pub uuid: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ViaItem {
    pub at: Pt,
    pub size: f64,
    pub net: Option<String>,
    pub uuid: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ZoneItem {
    pub net: Option<String>,
    pub layers: Vec<String>,
    pub outline: Vec<Pt>,
    /// Filled copper per layer.
    pub fills: Vec<(String, Vec<Pt>)>,
    pub uuid: Option<String>,
}

pub(crate) struct BoardModel {
    /// Copper layers `(name, kind)` in stack order.
    pub copper: Vec<(String, String)>,
    pub pads: Vec<CopperPad>,
    pub tracks: Vec<TrackItem>,
    pub vias: Vec<ViaItem>,
    pub zones: Vec<ZoneItem>,
    pub outline: BoardOutline,
    pub parts: Vec<konnect_sexp::board::FootprintCourtyard>,
    pub nets: BTreeSet<String>,
}

fn pts(node: &SexpNode) -> Vec<Pt> {
    node.find("polygon")
        .and_then(|p| p.find("pts"))
        .or_else(|| node.find("pts"))
        .map(|pts| pts.find_all("xy").iter().filter_map(|xy| Some((xy.get_f64(1)?, xy.get_f64(2)?))).collect())
        .unwrap_or_default()
}

fn net_of(node: &SexpNode) -> Option<String> {
    node.find("net").and_then(konnect_sexp::net::net_name).filter(|n| !n.is_empty()).map(str::to_string)
}

impl BoardModel {
    pub(crate) fn read(tree: &SexpNode) -> Self {
        let copper = konnect_sexp::layers::copper(&konnect_sexp::layers::layers(tree))
            .into_iter()
            .map(|l| (l.name.clone(), l.kind.clone()))
            .collect();
        let mut tracks: Vec<TrackItem> = konnect_sexp::board::tracks(tree)
            .items
            .into_iter()
            .map(|t| TrackItem {
                seg: TrackSeg { a: (t.x1, t.y1), b: (t.x2, t.y2), layer: t.layer, net: t.net },
                width: t.width,
                uuid: t.uuid,
            })
            .collect();
        for arc in tree.find_all("arc") {
            let point = |tag: &str| arc.find(tag).and_then(|n| Some((n.get_f64(1)?, n.get_f64(2)?)));
            let (Some(s), Some(m), Some(e)) = (point("start"), point("mid"), point("end")) else { continue };
            let width = arc.find_f64("width").unwrap_or(0.2);
            let layer = arc.find_str("layer").unwrap_or("").to_string();
            let net = net_of(arc);
            let uuid = arc.find_str("uuid").map(str::to_string);
            for w in crate::tools::fanout::sample_arc(s, m, e, 8).windows(2) {
                tracks.push(TrackItem { seg: TrackSeg { a: w[0], b: w[1], layer: layer.clone(), net: net.clone() }, width, uuid: uuid.clone() });
            }
        }
        let vias = konnect_sexp::board::vias(tree)
            .items
            .into_iter()
            .map(|v| ViaItem { at: (v.x, v.y), size: v.size, net: v.net, uuid: v.uuid })
            .collect();
        let mut zone_nodes: Vec<&SexpNode> = tree.find_all("zone");
        for footprint in konnect_sexp::board::footprints(tree) {
            zone_nodes.extend(footprint.find_all("zone"));
        }
        let mut zones = Vec::new();
        for zone in zone_nodes {
            if zone.find("keepout").is_some() {
                continue;
            }
            let mut layers = Vec::new();
            for tag in ["layers", "layer"] {
                if let Some(node) = zone.find(tag) {
                    layers.extend(node.children().unwrap_or_default().iter().skip(1).filter_map(SexpNode::as_str).map(str::to_string));
                }
            }
            let fills = zone
                .find_all("filled_polygon")
                .into_iter()
                .filter_map(|f| Some((f.find_str("layer")?.to_string(), pts(f))))
                .filter(|(_, p)| p.len() >= 3)
                .collect();
            zones.push(ZoneItem { net: net_of(zone), layers, outline: pts(zone), fills, uuid: zone.find_str("uuid").map(str::to_string) });
        }
        let pads = copper_pads(tree);
        let mut nets: BTreeSet<String> = pads.iter().filter_map(|p| p.net.clone()).collect();
        nets.extend(konnect_sexp::board::PcbConnectivityIndex::build(tree).nets().into_iter().map(str::to_string));
        Self {
            copper,
            pads,
            tracks,
            vias,
            zones,
            outline: BoardOutline::read(tree),
            parts: konnect_sexp::board::footprint_courtyards(tree).items,
            nets,
        }
    }
}

fn polygon_area(points: &[Pt]) -> f64 {
    let n = points.len();
    (0..n).map(|i| points[i].0 * points[(i + 1) % n].1 - points[(i + 1) % n].0 * points[i].1).sum::<f64>().abs() / 2.0
}

/// Nets whose connections planes or pours make, and why. Their airwires are
/// fan-out work (a via per pad), not routing demand.
pub(crate) fn plane_nets(model: &BoardModel, board_area: f64) -> BTreeMap<String, String> {
    let kind_of = |layer: &str| model.copper.iter().find(|(n, _)| n == layer).map(|(_, k)| k.as_str());
    let mut out = BTreeMap::new();
    for zone in &model.zones {
        let Some(net) = &zone.net else { continue };
        for layer in &zone.layers {
            let reason = match kind_of(layer) {
                Some("power") => Some(format!("plane on {layer}")),
                Some("mixed") => Some(format!("pour on mixed layer {layer} (its area counts as blocked for routing)")),
                Some(_) if (layer == "F.Cu" || layer == "B.Cu") && board_area > 0.0 && polygon_area(&zone.outline) >= 0.5 * board_area => {
                    Some(format!("pour on {layer} over at least half the board (judgement)"))
                }
                _ => None,
            };
            if let Some(reason) = reason {
                out.entry(net.clone()).or_insert(reason);
            }
        }
    }
    out
}

// ─── Airwires ───────────────────────────────────────────────────────────────

fn bracketed_net(description: &str) -> Option<String> {
    let start = description.find('[')?;
    let end = description[start..].find(']')? + start;
    Some(description[start + 1..end].to_string())
}

fn pad_end(pad: &CopperPad) -> AirwireEnd {
    AirwireEnd {
        at: pad.center,
        layers: if pad.smd { EndLayers::One(pad.layer.to_string()) } else { EndLayers::All },
        pad: Some((pad.reference.clone(), pad.number.clone())),
    }
}

/// Candidate anchor points of one DRC item, by UUID, else its reported position.
fn item_anchors(item: &crate::tools::cli::ReportItem, model: &BoardModel) -> Vec<AirwireEnd> {
    if let Some(uuid) = &item.uuid {
        if let Some(pad) = model.pads.iter().find(|p| p.uuid.as_deref() == Some(uuid)) {
            return vec![pad_end(pad)];
        }
        if let Some(via) = model.vias.iter().find(|v| v.uuid.as_deref() == Some(uuid)) {
            return vec![AirwireEnd { at: via.at, layers: EndLayers::All, pad: None }];
        }
        let segments: Vec<&TrackItem> = model.tracks.iter().filter(|t| t.uuid.as_deref() == Some(uuid)).collect();
        if !segments.is_empty() {
            return segments
                .iter()
                .flat_map(|t| [t.seg.a, t.seg.b].map(|at| AirwireEnd { at, layers: EndLayers::One(t.seg.layer.clone()), pad: None }))
                .collect();
        }
        if let Some(zone) = model.zones.iter().find(|z| z.uuid.as_deref() == Some(uuid)) {
            let layer = zone.layers.first().cloned().unwrap_or_default();
            let mut anchors: Vec<AirwireEnd> = zone
                .fills
                .iter()
                .flat_map(|(l, poly)| poly.iter().map(move |&at| AirwireEnd { at, layers: EndLayers::One(l.clone()), pad: None }))
                .collect();
            if anchors.is_empty() {
                anchors = zone.outline.iter().map(|&at| AirwireEnd { at, layers: EndLayers::One(layer.clone()), pad: None }).collect();
            }
            if !anchors.is_empty() {
                return anchors;
            }
        }
    }
    let Some(pos) = &item.pos else { return Vec::new() };
    let layers = if item.description.contains(" - ") {
        EndLayers::All
    } else {
        item.description
            .rsplit(" on ")
            .next()
            .and_then(|tail| tail.split([',', ' ']).next())
            .filter(|l| l.ends_with(".Cu"))
            .map(|l| EndLayers::One(l.to_string()))
            .unwrap_or(EndLayers::All)
    };
    vec![AirwireEnd { at: (pos.x, pos.y), layers, pad: None }]
}

/// KiCad's ratsnest as an offline DRC reports it: one airwire per
/// `unconnected_items` entry, between the closest anchors of its two items.
pub(crate) fn airwires_from_drc(items: &[crate::tools::cli::DrcViolation], model: &BoardModel) -> Vec<Airwire> {
    let mut out = Vec::new();
    for violation in items {
        if violation.items.len() < 2 {
            continue;
        }
        let Some(net) = violation.items.iter().find_map(|i| bracketed_net(&i.description)) else { continue };
        let a = item_anchors(&violation.items[0], model);
        let b = item_anchors(&violation.items[1], model);
        let best = a
            .iter()
            .flat_map(|x| b.iter().map(move |y| (x, y)))
            .min_by(|p, q| {
                let d = |(x, y): &(&AirwireEnd, &AirwireEnd)| (x.at.0 - y.at.0).hypot(x.at.1 - y.at.1);
                d(p).total_cmp(&d(q))
            });
        if let Some((x, y)) = best {
            out.push(Airwire { net, a: x.clone(), b: y.clone() });
        }
    }
    out
}

struct DisjointSet(Vec<usize>);

impl DisjointSet {
    fn new(n: usize) -> Self {
        Self((0..n).collect())
    }
    fn find(&mut self, x: usize) -> usize {
        let mut r = x;
        while self.0[r] != r {
            r = self.0[r];
        }
        let mut y = x;
        while self.0[y] != r {
            let next = self.0[y];
            self.0[y] = r;
            y = next;
        }
        r
    }
    fn union(&mut self, a: usize, b: usize) -> bool {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return false;
        }
        self.0[ra] = rb;
        true
    }
}

/// KiCad's construction without KiCad: per net, join copper that touches
/// (weight 0), then a minimum spanning tree (Kruskal) between the resulting
/// clusters over their anchors (`kicad-10-0-8-ratsnest-data`). Zones are
/// not followed: plane and pour nets are not demand anyway.
pub(crate) fn mst_airwires(model: &BoardModel) -> Vec<Airwire> {
    let mut out = Vec::new();
    let mut nets: BTreeSet<&str> = BTreeSet::new();
    nets.extend(model.pads.iter().filter_map(|p| p.net.as_deref()));
    for net in nets {
        // Items: pads, then track segments, then vias.
        let pads: Vec<&CopperPad> = model.pads.iter().filter(|p| p.net.as_deref() == Some(net)).collect();
        let tracks: Vec<&TrackItem> = model.tracks.iter().filter(|t| t.seg.net.as_deref() == Some(net)).collect();
        let vias: Vec<&ViaItem> = model.vias.iter().filter(|v| v.net.as_deref() == Some(net)).collect();
        let n = pads.len() + tracks.len() + vias.len();
        if pads.len() < 2 {
            continue;
        }
        let mut set = DisjointSet::new(n);
        let t0 = pads.len();
        let v0 = t0 + tracks.len();
        let pad_on = |p: &CopperPad, layer: &str| !p.smd || p.layer == layer;
        for (ti, t) in tracks.iter().enumerate() {
            for (pi, p) in pads.iter().enumerate() {
                if pad_on(p, &t.seg.layer)
                    && [t.seg.a, t.seg.b].iter().any(|&e| crate::tools::fanout::point_in_polygon(e, &p.outline) || (e.0 - p.center.0).hypot(e.1 - p.center.1) <= 1e-3)
                {
                    set.union(pi, t0 + ti);
                }
            }
            for (ui, u) in tracks.iter().enumerate().skip(ti + 1) {
                if u.seg.layer != t.seg.layer {
                    continue;
                }
                let reach = t.width.max(u.width) / 2.0 + 1e-3;
                let touches = [t.seg.a, t.seg.b].iter().any(|&e| geom::point_segment_distance(e, u.seg.a, u.seg.b) <= reach)
                    || [u.seg.a, u.seg.b].iter().any(|&e| geom::point_segment_distance(e, t.seg.a, t.seg.b) <= reach);
                if touches {
                    set.union(t0 + ti, t0 + ui);
                }
            }
            for (vi, v) in vias.iter().enumerate() {
                if geom::point_segment_distance(v.at, t.seg.a, t.seg.b) <= v.size / 2.0 {
                    set.union(t0 + ti, v0 + vi);
                }
            }
        }
        for (vi, v) in vias.iter().enumerate() {
            for (pi, p) in pads.iter().enumerate() {
                if crate::tools::fanout::point_in_polygon(v.at, &p.outline) {
                    set.union(pi, v0 + vi);
                }
            }
        }
        // Anchors with their item index.
        let mut anchors: Vec<(usize, AirwireEnd)> = pads.iter().enumerate().map(|(i, p)| (i, pad_end(p))).collect();
        for (ti, t) in tracks.iter().enumerate() {
            for at in [t.seg.a, t.seg.b] {
                anchors.push((t0 + ti, AirwireEnd { at, layers: EndLayers::One(t.seg.layer.clone()), pad: None }));
            }
        }
        for (vi, v) in vias.iter().enumerate() {
            anchors.push((v0 + vi, AirwireEnd { at: v.at, layers: EndLayers::All, pad: None }));
        }
        let mut edges: Vec<(f64, usize, usize)> = Vec::new();
        for i in 0..anchors.len() {
            for j in i + 1..anchors.len() {
                if set.find(anchors[i].0) != set.find(anchors[j].0) {
                    let (a, b) = (anchors[i].1.at, anchors[j].1.at);
                    edges.push(((a.0 - b.0).hypot(a.1 - b.1), i, j));
                }
            }
        }
        edges.sort_by(|a, b| a.0.total_cmp(&b.0));
        for (_, i, j) in edges {
            if set.union(anchors[i].0, anchors[j].0) {
                out.push(Airwire { net: net.to_string(), a: anchors[i].1.clone(), b: anchors[j].1.clone() });
            }
        }
    }
    out
}

// ─── Group ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct Group {
    pub nets: BTreeSet<String>,
    pub requested_nets: Vec<String>,
    pub classes: Vec<String>,
}

impl Group {
    /// Same meaning as `export_specctra_dsn`'s group: named nets plus every
    /// net of the named classes; `Default` is the nets with no other class.
    /// Unknown nets or classes are refused.
    pub(crate) fn resolve(nets: Vec<String>, classes: Vec<String>, model: &BoardModel, rules: &NetRules) -> Result<Self, String> {
        let unknown: Vec<&String> = nets.iter().filter(|n| !model.nets.contains(*n)).collect();
        if !unknown.is_empty() {
            return Err(format!("net(s) {} are not on the board", unknown.iter().map(|n| format!("'{n}'")).collect::<Vec<_>>().join(", ")));
        }
        let unknown: Vec<&String> = classes.iter().filter(|c| !rules.has_class(c)).collect();
        if !unknown.is_empty() {
            return Err(format!("net class(es) {} are not in the project", unknown.iter().map(|c| format!("'{c}'")).collect::<Vec<_>>().join(", ")));
        }
        let mut set: BTreeSet<String> = nets.iter().cloned().collect();
        for net in &model.nets {
            if classes.contains(&rules.class_of(net)) {
                set.insert(net.clone());
            }
        }
        Ok(Self { nets: set, requested_nets: nets, classes })
    }
}

// ─── Analysis ───────────────────────────────────────────────────────────────

pub(crate) struct Options {
    pub signal_layers: Option<Vec<String>>,
    pub cell_mm: f64,
    pub efficiency: f64,
    pub efficiency_given: bool,
}

struct Reason {
    metric: &'static str,
    level: &'static str,
    threshold: &'static str,
    detail: String,
    measured: Value,
}

impl Reason {
    fn json(&self) -> Value {
        let t = threshold(self.threshold);
        json!({
            "metric": self.metric,
            "level": self.level,
            "detail": self.detail,
            "measured": self.measured,
            "threshold": { "id": t.id, "value": t.value, "kind": t.kind, "source": t.source },
        })
    }
}

fn layer_indices(layers: &[String], names: &[String]) -> Vec<usize> {
    names.iter().filter_map(|n| layers.iter().position(|l| l == n)).collect()
}

fn keepout_applies(spec: &str, layer: &str) -> bool {
    spec == layer || spec == "*.Cu" || (spec == "F&B.Cu" && (layer == "F.Cu" || layer == "B.Cu"))
}

/// Inverse of the footprint transform: board point into the footprint frame.
fn to_local(p: Pt, centre: Pt, rotation: f64) -> Pt {
    konnect_sexp::geometry::transform_pad(p.0 - centre.0, p.1 - centre.1, 0.0, 0.0, -rotation)
}

/// The whole pre-check on a parsed board and its airwires. Pure: the tests
/// call it with synthetic boards and airwires.
pub(crate) fn analyse(model: &BoardModel, rules: &NetRules, all_airwires: &[Airwire], group: Option<&Group>, options: &Options) -> Result<Value, String> {
    // Signal layers.
    let layers: Vec<String> = match &options.signal_layers {
        Some(names) => {
            let unknown: Vec<&String> = names.iter().filter(|n| !model.copper.iter().any(|(c, _)| c == *n)).collect();
            if !unknown.is_empty() || names.is_empty() {
                return Err(format!(
                    "signal_layers must name copper layers of the board (board copper: {}); unknown: {}",
                    model.copper.iter().map(|(c, _)| c.as_str()).collect::<Vec<_>>().join(", "),
                    unknown.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                ));
            }
            names.clone()
        }
        None => model.copper.iter().filter(|(_, k)| k == "signal" || k == "mixed").map(|(n, _)| n.clone()).collect(),
    };
    if layers.is_empty() {
        return Err("the board has no signal or mixed copper layer to route on".into());
    }
    if !model.outline.has_edges() {
        return Err("the board has no Edge.Cuts outline: capacity cannot be measured".into());
    }
    let all: Vec<usize> = (0..layers.len()).collect();
    let edges = model.outline.edges().to_vec();

    // Raster resolution: about 3 million pixels per layer at most.
    let bbox = edges.iter().fold((f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY), |(a, b, c, d), &(p, q)| {
        (a.min(p.0).min(q.0), b.min(p.1).min(q.1), c.max(p.0).max(q.0), d.max(p.1).max(q.1))
    });
    let res = (((bbox.2 - bbox.0) * (bbox.3 - bbox.1) / 3.0e6).sqrt()).max(0.05);
    let mut raster = Raster::new(bbox, res, layers.len());
    raster.mark_board(&edges);
    let board_area = raster.total_inside_area();
    let planes = plane_nets(model, board_area);

    // Obstacles: pads, tracks, vias, keep-outs, mixed-layer pours, edge clearance.
    let mut obstacles: Vec<Obstacle> = Vec::new();
    for pad in &model.pads {
        let on = if pad.smd { layer_indices(&layers, &[pad.layer.to_string()]) } else { all.clone() };
        if !on.is_empty() {
            obstacles.push(Obstacle { shape: Shape::Poly(pad.outline.clone()), layers: on, net: pad.net.clone(), pad: true });
        }
    }
    for t in &model.tracks {
        let on = layer_indices(&layers, std::slice::from_ref(&t.seg.layer));
        if !on.is_empty() {
            obstacles.push(Obstacle { shape: Shape::Capsule { a: t.seg.a, b: t.seg.b, r: t.width / 2.0 }, layers: on, net: t.seg.net.clone(), pad: false });
        }
    }
    for v in &model.vias {
        obstacles.push(Obstacle { shape: Shape::Circle { c: v.at, r: v.size / 2.0 }, layers: all.clone(), net: v.net.clone(), pad: false });
    }
    for (polygon, specs) in model.outline.track_keepouts() {
        let on: Vec<usize> = (0..layers.len()).filter(|&l| specs.iter().any(|s| keepout_applies(s, &layers[l]))).collect();
        if !on.is_empty() && polygon.len() >= 3 {
            obstacles.push(Obstacle { shape: Shape::Poly(polygon.to_vec()), layers: on, net: None, pad: false });
        }
    }
    let mixed: BTreeSet<&str> = model.copper.iter().filter(|(_, k)| k == "mixed").map(|(n, _)| n.as_str()).collect();
    let mut blocked_pours = Vec::new();
    for zone in &model.zones {
        for layer in zone.layers.iter().filter(|l| mixed.contains(l.as_str())) {
            let Some(l) = layers.iter().position(|x| x == layer) else { continue };
            let fills: Vec<&Vec<Pt>> = zone.fills.iter().filter(|(fl, _)| fl == layer).map(|(_, p)| p).collect();
            let polygons: Vec<Vec<Pt>> = if fills.is_empty() { vec![zone.outline.clone()] } else { fills.into_iter().cloned().collect() };
            for polygon in polygons.into_iter().filter(|p| p.len() >= 3) {
                obstacles.push(Obstacle { shape: Shape::Poly(polygon), layers: vec![l], net: zone.net.clone(), pad: false });
            }
            blocked_pours.push(json!({ "net": zone.net, "layer": layer }));
        }
    }
    if rules.edge_clearance > 0.0 {
        for &(a, b) in &edges {
            obstacles.push(Obstacle { shape: Shape::Capsule { a, b, r: rules.edge_clearance }, layers: all.clone(), net: None, pad: false });
        }
    }
    for o in &obstacles {
        raster.mark_obstacle(&o.shape, &o.layers);
    }

    // Demand: airwires of routable nets in the group.
    let mut plane_airwires = 0usize;
    let mut outside_group = 0usize;
    let mut demand: Vec<Airwire> = Vec::new();
    for w in all_airwires {
        if planes.contains_key(&w.net) {
            plane_airwires += 1;
        } else if group.is_some_and(|g| !g.nets.contains(&w.net)) {
            outside_group += 1;
        } else {
            demand.push(w.clone());
        }
    }
    let (w0, s0) = rules.default_rule();
    let pitch: Vec<f64> = demand.iter().map(|w| {
        let (tw, ts) = rules.rule(&w.net);
        tw + ts
    }).collect();
    let s_min = demand.iter().map(|w| rules.rule(&w.net).1).fold(s0, f64::min);
    let cut_rules = Rules { w: w0, s: s0, s_min, fab_w: rules.fab_w, fab_s: rules.fab_s };
    let tracks: Vec<TrackSeg> = model.tracks.iter().map(|t| t.seg.clone()).collect();
    let board = Board { layers: &layers, edges: &edges, obstacles: &obstacles, tracks: &tracks, raster: &raster, rules: cut_rules };
    let mut reasons: Vec<Reason> = Vec::new();

    // 1. Crossings.
    let crossing_list = crossings::airwire_crossings(&demand);
    let plan = crossings::plan_layers(&demand, &crossing_list, &layers);
    let blocks = crossings::track_blocks(&demand, &tracks, &layers);
    let e = demand.len();
    let k = crossing_list.len();
    let k_over_e = if e > 0 { k as f64 / e as f64 } else { 0.0 };
    let mut pairs: BTreeMap<(String, String), (usize, Pt)> = BTreeMap::new();
    let mut by_part: BTreeMap<String, usize> = BTreeMap::new();
    for c in &crossing_list {
        let (a, b) = (&demand[c.i].net, &demand[c.j].net);
        let key = if a <= b { (a.clone(), b.clone()) } else { (b.clone(), a.clone()) };
        pairs.entry(key).or_insert((0, c.at)).0 += 1;
        for w in [&demand[c.i], &demand[c.j]] {
            let refs: BTreeSet<&String> = [&w.a.pad, &w.b.pad].into_iter().flatten().map(|(r, _)| r).collect();
            for r in refs {
                *by_part.entry(r.clone()).or_default() += 1;
            }
        }
    }
    let mut pair_list: Vec<_> = pairs.into_iter().collect();
    pair_list.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(a.0.cmp(&b.0)));
    let top_pairs: Vec<Value> = pair_list
        .iter()
        .take(10)
        .map(|((a, b), (n, at))| json!({ "nets": [a, b], "crossings": n, "at": [round3(at.0), round3(at.1)] }))
        .collect();
    let blocked_per_layer: BTreeMap<&String, usize> = layers.iter().enumerate().map(|(l, name)| (name, blocks.iter().filter(|b| b[l] > 0).count())).collect();
    let blocked_everywhere: Vec<Value> = demand
        .iter()
        .zip(&blocks)
        .filter(|(_, b)| b.iter().all(|&n| n > 0))
        .take(10)
        .map(|(w, _)| airwire_json(w))
        .collect();
    let blocked_everywhere_count = blocks.iter().filter(|b| b.iter().all(|&n| n > 0)).count();
    let vias_per_airwire = if e > 0 { plan.vias_estimate as f64 / e as f64 } else { 0.0 };
    if plan.colours as f64 > layers.len() as f64 + threshold("crossings.layers").value {
        reasons.push(Reason { metric: "crossings", level: "tight", threshold: "crossings.layers", detail: format!("the crossing graph needs {} colours (straight-line layers) and the board routes on {}", plan.colours, layers.len()), measured: json!(plan.colours) });
    }
    if e > 0 && k_over_e >= threshold("crossings.k_over_e").value {
        reasons.push(Reason { metric: "crossings", level: "tight", threshold: "crossings.k_over_e", detail: format!("{k} crossings over {e} airwires (K/E = {:.2})", k_over_e), measured: json!(round3(k_over_e)) });
    }
    if e > 0 && vias_per_airwire > threshold("crossings.vias").value {
        reasons.push(Reason { metric: "crossings", level: "tight", threshold: "crossings.vias", detail: format!("about {} vias for {e} airwires", plan.vias_estimate), measured: json!(round3(vias_per_airwire)) });
    }
    let crossings_json = json!({
        "airwires": e,
        "crossings": k,
        "k_over_e": round3(k_over_e),
        "layers_estimate": plan.colours,
        "vias_estimate": plan.vias_estimate,
        "end_vias": plan.end_vias,
        "unresolved_crossings": plan.unresolved_crossings,
        "smd_smd_airwires": demand.iter().filter(|w| w.smd_smd()).count(),
        "airwires_on_layer": layers.iter().enumerate().map(|(l, n)| (n.clone(), plan.layer_of.iter().filter(|&&x| x == l).count())).collect::<BTreeMap<_, _>>(),
        "crossing_existing_copper_per_layer": blocked_per_layer,
        "crossing_existing_copper_on_every_layer": blocked_everywhere_count,
        "crossing_existing_copper_on_every_layer_examples": blocked_everywhere,
        "worst_net_pairs": top_pairs,
        "crossings_by_part": top_n(&by_part, 10),
    });

    // 2. Escape.
    let mut packages = Vec::new();
    let mut part_frames: BTreeMap<String, (Pt, f64)> = BTreeMap::new();
    let mut by_footprint: BTreeMap<(String, i64, i64), Vec<&CopperPad>> = BTreeMap::new();
    for pad in &model.pads {
        let key = (pad.reference.clone(), (pad.footprint_center.0 * 1000.0).round() as i64, (pad.footprint_center.1 * 1000.0).round() as i64);
        by_footprint.entry(key).or_default().push(pad);
    }
    let demand_pads: BTreeSet<(String, String)> = demand.iter().flat_map(|w| [w.a.pad.clone(), w.b.pad.clone()]).flatten().collect();
    for ((reference, _, _), pads) in &by_footprint {
        let (centre, rotation) = (pads[0].footprint_center, pads[0].footprint_rotation);
        part_frames.insert(reference.clone(), (centre, rotation));
        if pads.iter().filter(|p| p.smd).count() < MIN_ESCAPE_PADS {
            continue;
        }
        let mut package_pads = Vec::new();
        let mut destinations = Vec::new();
        for pad in pads {
            let relative = (pad.angle - rotation).to_radians();
            let (hw, hh) = (pad.size.0 / 2.0, pad.size.1 / 2.0);
            let half = ((hw * relative.cos()).abs() + (hh * relative.sin()).abs(), (hw * relative.sin()).abs() + (hh * relative.cos()).abs());
            let key = (pad.reference.clone(), pad.number.clone());
            let is_demand = demand_pads.contains(&key);
            let far: Vec<Pt> = demand
                .iter()
                .filter_map(|w| {
                    let own = |end: &AirwireEnd| end.pad.as_ref() == Some(&key) && (end.at.0 - pad.center.0).hypot(end.at.1 - pad.center.1) < 1e-3;
                    if own(&w.a) {
                        Some(w.b.at)
                    } else if own(&w.b) {
                        Some(w.a.at)
                    } else {
                        None
                    }
                })
                .map(|p| to_local(p, centre, rotation))
                .collect();
            package_pads.push(PackagePad {
                number: pad.number.clone(),
                net: pad.net.clone(),
                local: to_local(pad.center, centre, rotation),
                half,
                demand: is_demand && !far.is_empty(),
                round: pad.outline.len() > 4 && (pad.size.0 - pad.size.1).abs() <= 0.05 * pad.size.0.max(pad.size.1),
                rule: pad.net.as_deref().map(|n| rules.rule(n)),
            });
            destinations.push(far);
        }
        let package = Package { reference: reference.clone(), pads: package_pads, rotation };
        let (w, s) = dominant_rule(&package, rules);
        packages.push((escape::analyse_package(&package, &destinations, w, s), w, s));
    }
    let mut escape_json = Vec::new();
    for (report, w, s) in &packages {
        if let Some(needed) = report.layers_needed.filter(|&n| n > layers.len()) {
            reasons.push(Reason { metric: "escape", level: "doomed", threshold: "escape.layers", detail: format!("{} needs {needed} signal layers to escape its pins and the board routes on {}", report.reference, layers.len()), measured: json!(needed) });
        } else if report.layers_needed.is_none() {
            reasons.push(Reason { metric: "escape", level: "doomed", threshold: "escape.layers", detail: format!("{}: inner rows cannot escape (no trace fits between pads at w = {w} mm, s = {s} mm); needs via-in-pad/HDI or narrower tracks", report.reference), measured: Value::Null });
        }
        for side in &report.sides {
            if side.interior_assigned > 0 && (side.channel_utilization > threshold("escape.channel").value) {
                reasons.push(Reason { metric: "escape", level: "tight", threshold: "escape.channel", detail: format!("{} {} side: {} interior nets through {} channel(s)", report.reference, side.faces, side.interior_assigned, side.channel_total), measured: json!(side.channel_utilization) });
            }
            // Necking down in the breakout area is allowed
            // (`xilinx-ug1099-bga-design-rules`): only pads too close even
            // for fab-minimum traces make the board tight.
            let fab_pitch = rules.fab_w + rules.fab_s;
            if side.tightest_demand_pitch_mm.is_some_and(|p| p + 1e-9 < fab_pitch) {
                reasons.push(Reason { metric: "escape", level: "tight", threshold: "escape.fanout_pitch", detail: format!("{} {} side: neighbouring pads that both need a trace are {} mm apart; even fab-minimum traces need {} mm (their net classes need {} mm): stagger, swap pins or use a finer process", report.reference, side.faces, side.tightest_demand_pitch_mm.unwrap_or(0.0), round3(fab_pitch), side.needed_pitch_mm.unwrap_or(0.0)), measured: json!(side.tightest_demand_pitch_mm) });
            }
        }
        let mut value = serde_json::to_value(report).unwrap_or(Value::Null);
        value["rule"] = json!({ "track_mm": w, "clearance_mm": s });
        escape_json.push(value);
    }

    // 3a. Full-board cuts.
    let cuts = capacity::full_board_cuts(&board, &demand, &pitch, CUT_STEP_MM);
    let doomed_cuts: Vec<&capacity::Cut> = cuts.iter().filter(|c| c.proves_doom()).collect();
    let worst_cuts: Vec<&capacity::Cut> = {
        let mut sorted: Vec<&capacity::Cut> = cuts.iter().filter(|c| c.demand_airwires > 0).collect();
        sorted.sort_by(|a, b| b.utilization.total_cmp(&a.utilization));
        sorted.into_iter().take(5).collect()
    };
    if let Some(worst) = doomed_cuts.iter().max_by(|a, b| a.lower_bound_utilization.total_cmp(&b.lower_bound_utilization)) {
        reasons.push(Reason {
            metric: "cuts",
            level: "doomed",
            threshold: "cut.lower_bound",
            detail: format!(
                "the {} line {}={} crosses the whole board; {} nets must cross it ({}) but it has {} lanes ({} at the fab minimum)",
                axis_name(worst.axis), if worst.axis == geom::Axis::Horizontal { "y" } else { "x" }, worst.at,
                worst.demand_nets_lower_bound, worst.lower_bound_nets.iter().take(6).cloned().collect::<Vec<_>>().join(", "),
                worst.capacity_lanes, worst.capacity_lanes_at_fab_minimum
            ),
            measured: json!(worst.lower_bound_utilization),
        });
    }
    if let Some(worst) = worst_cuts.first().filter(|c| c.utilization > threshold("cut.utilization").value && !c.proves_doom()) {
        reasons.push(Reason {
            metric: "cuts",
            level: "tight",
            threshold: "cut.utilization",
            detail: format!("{} cut at {}: {} airwires against {} lanes", axis_name(worst.axis), worst.at, worst.demand_airwires, worst.capacity_lanes),
            measured: json!(worst.utilization),
        });
    }

    // 3a'. Channels between parts.
    let parts: Vec<PartBox> = model
        .parts
        .iter()
        .filter(|p| p.bbox_source != konnect_sexp::board::CourtyardSource::AnchorOnly)
        .map(|p| {
            let reference = p.reference.clone().unwrap_or_default();
            let pads = model.pads.iter().filter(|q| q.reference == reference).count();
            PartBox { reference, bbox: p.bbox, pads }
        })
        .collect();
    let channels = capacity::channel_cuts(&board, &demand, &pitch, &parts, CHANNEL_MAX_GAP_MM);
    if let Some(worst) = channels.iter().find(|c| c.cut.utilization > threshold("channel.utilization").value && c.cut.demand_airwires >= 3) {
        reasons.push(Reason {
            metric: "channels",
            level: "tight",
            threshold: "channel.utilization",
            detail: format!("between {} and {} (gap {} mm): {} airwires against {} lanes", worst.between[0], worst.between[1], worst.gap_mm, worst.cut.demand_airwires, worst.cut.capacity_lanes),
            measured: json!(worst.cut.utilization),
        });
    }

    // 3b. Congestion grid.
    let congestion = capacity::congestion(&board, &demand, &pitch, options.cell_mm);
    if congestion.overflow_fraction > threshold("congestion.overflow").value {
        reasons.push(Reason {
            metric: "congestion",
            level: "tight",
            threshold: "congestion.overflow",
            detail: format!("{} of {} cells ({:.1} %) have more RUDY demand than free track", congestion.overflow_cells, congestion.cells, 100.0 * congestion.overflow_fraction),
            measured: json!(congestion.overflow_fraction),
        });
    }

    // 4. Density.
    let density = capacity::density(&board, &demand, &pitch, options.efficiency);
    if density.utilization > threshold("density.u_doomed").value {
        reasons.push(Reason { metric: "density", level: "doomed", threshold: "density.u_doomed", detail: format!("wiring demand {} mm exceeds capacity {} mm (E = {}; with the calibrated E this means denser than any board routed so far)", density.demand_mm, density.capacity_mm, options.efficiency), measured: json!(density.utilization) });
    } else if density.utilization > threshold("density.u_tight").value {
        reasons.push(Reason { metric: "density", level: "tight", threshold: "density.u_tight", detail: format!("wiring demand {} mm against capacity {} mm leaves less than 20 % spare (E = {})", density.demand_mm, density.capacity_mm, options.efficiency), measured: json!(density.utilization) });
    }

    // Pin density (informational, `cadence-burkhert-pcb-density-layer-count`).
    let pins = model.pads.len();
    let sides: BTreeSet<String> = model.parts.iter().map(|p| format!("{:?}", p.layer_side)).collect();
    let pin_density = json!({
        "pins": pins,
        "board_area_mm2": round3(board_area),
        "pins_per_cm2": if board_area > 0.0 { round3(pins as f64 / (board_area / 100.0)) } else { 0.0 },
        "pins_per_in2": if board_area > 0.0 { round3(pins as f64 / (board_area / 645.16)) } else { 0.0 },
        "sides_with_parts": sides.len(),
        "note": "informational, no sourced threshold: \"The first metric that comes to mind for PCB circuit density is the number of pins per square inch\" (cadence-burkhert-pcb-density-layer-count); compare with earlier boards",
    });

    // Rotation what-ifs for the parts most involved in crossings.
    let rotation_hints = rotation_hints(&demand, &by_part, &part_frames, &by_footprint, k);

    // Verdict.
    let verdict = if reasons.iter().any(|r| r.level == "doomed") {
        "doomed"
    } else if reasons.iter().any(|r| r.level == "tight") {
        "tight"
    } else {
        "fine"
    };
    let worst_nets = worst_nets(&demand, &crossing_list, worst_cuts.first().copied(), &congestion);
    let recommendation = match verdict {
        "doomed" => format!(
            "Do not autoroute yet: a doomed reason is a necessary condition that fails, so no router can finish. Act on the named place first ({}), then check again.",
            reasons.iter().filter(|r| r.level == "doomed").map(|r| r.detail.clone()).collect::<Vec<_>>().join("; ")
        ),
        "tight" => format!(
            "Tight, not proven either way: estimators miss what a real router sees (kim-iccad11-simplr), so the tiebreaker is a short time-boxed Freerouting pass of the worst nets first: export_specctra_dsn with nets {:?}, route_specctra_dsn with about 10-20 passes, apply_specctra_ses accept 'per_net'. If they route, route the rest; if not, act on the reasons ({}).",
            worst_nets,
            reasons.iter().map(|r| r.metric).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>().join(", ")
        ),
        _ => "Fine: no metric is near its limit; autoroute.".to_string(),
    };

    let group_json = group.map(|g| {
        let plane_in_group: Vec<Value> = g.nets.iter().filter_map(|n| planes.get(n).map(|r| json!({ "net": n, "reason": r }))).collect();
        json!({ "nets": g.nets, "requested_nets": g.requested_nets, "classes": g.classes, "not_demand": plane_in_group })
    });
    let demand_nets: BTreeSet<&String> = demand.iter().map(|w| &w.net).collect();
    let mut out = json!({
        "verdict": verdict,
        "reasons": reasons.iter().map(Reason::json).collect::<Vec<_>>(),
        "recommendation": recommendation,
        "airwires": {
            "total": all_airwires.len(),
            "demand": e,
            "demand_nets": demand_nets.len(),
            "plane_net_airwires": plane_airwires,
            "outside_group": outside_group,
            "plane_nets": planes,
            "note": "plane-net airwires are fan-out work (a via per pad to the plane), not routing demand",
        },
        "rules": {
            "signal_layers": layers,
            "default_track_mm": w0,
            "default_clearance_mm": s0,
            "smallest_clearance_mm": s_min,
            "fab_minimum": { "track_mm": rules.fab_w, "clearance_mm": rules.fab_s },
            "edge_clearance_mm": rules.edge_clearance,
            "blocked_pours": blocked_pours,
            "raster_mm": round3(res),
        },
        "crossings": crossings_json,
        "escape": escape_json,
        "cuts": {
            "step_mm": CUT_STEP_MM,
            "count": cuts.len(),
            "doomed": doomed_cuts.iter().take(5).map(|c| cut_json(c)).collect::<Vec<_>>(),
            "worst": worst_cuts.iter().map(|c| cut_json(c)).collect::<Vec<_>>(),
        },
        "channels": channels.iter().take(8).map(|c| {
            let mut v = cut_json(&c.cut);
            v["between"] = json!(c.between);
            v["gap_mm"] = json!(c.gap_mm);
            v
        }).collect::<Vec<_>>(),
        "congestion": congestion,
        "density": density,
        "pin_density": pin_density,
        "rotation_hints": rotation_hints,
        "thresholds": THRESHOLDS.iter().map(threshold_json).collect::<Vec<_>>(),
    });
    if let Some(g) = group_json {
        out["group"] = g;
    }
    out["density"]["layout_efficiency_source"] = json!(if options.efficiency_given {
        "caller"
    } else {
        "default 0.13: unsourced in the literature (Holden gives no number in the doc DB); calibrated as the densest completed benchmark board (stm32_demo, routed signal length / raw capacity = 0.126), so U > 1 means denser than any board routed so far (FORK_CHANGES Pass 23)"
    });
    Ok(out)
}

fn axis_name(axis: geom::Axis) -> &'static str {
    match axis {
        geom::Axis::Horizontal => "horizontal",
        geom::Axis::Vertical => "vertical",
    }
}

fn cut_json(cut: &capacity::Cut) -> Value {
    let mut v = serde_json::to_value(cut).unwrap_or(Value::Null);
    v["proves_doom"] = json!(cut.proves_doom());
    v["lower_bound_nets"] = json!(cut.lower_bound_nets.iter().take(12).collect::<Vec<_>>());
    v
}

fn airwire_json(w: &Airwire) -> Value {
    let end = |e: &AirwireEnd| match &e.pad {
        Some((r, n)) => json!({ "at": [round3(e.at.0), round3(e.at.1)], "pad": format!("{r}.{n}") }),
        None => json!({ "at": [round3(e.at.0), round3(e.at.1)] }),
    };
    json!({ "net": w.net, "from": end(&w.a), "to": end(&w.b) })
}

fn top_n(map: &BTreeMap<String, usize>, n: usize) -> Vec<Value> {
    let mut v: Vec<(&String, &usize)> = map.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    v.into_iter().take(n).map(|(k, c)| json!({ "reference": k, "crossings": c })).collect()
}

/// The (width, clearance) most of a package's demand nets use.
fn dominant_rule(package: &Package, rules: &NetRules) -> (f64, f64) {
    let mut counts: BTreeMap<(i64, i64), usize> = BTreeMap::new();
    for pad in package.pads.iter().filter(|p| p.demand) {
        if let Some(net) = &pad.net {
            let (w, s) = rules.rule(net);
            *counts.entry(((w * 1e4) as i64, (s * 1e4) as i64)).or_default() += 1;
        }
    }
    counts
        .into_iter()
        .max_by_key(|(_, n)| *n)
        .map(|((w, s), _)| (w as f64 / 1e4, s as f64 / 1e4))
        .unwrap_or_else(|| rules.default_rule())
}

/// Nets to try first in a time-boxed route: those in the most crossings,
/// on the worst cut and in the worst congestion cells.
fn worst_nets(demand: &[Airwire], crossings: &[crossings::Crossing], cut: Option<&capacity::Cut>, congestion: &capacity::Congestion) -> Vec<String> {
    let mut score: BTreeMap<&str, usize> = BTreeMap::new();
    for c in crossings {
        *score.entry(&demand[c.i].net).or_default() += 1;
        *score.entry(&demand[c.j].net).or_default() += 1;
    }
    if let Some(cut) = cut {
        for (net, n) in &cut.nets {
            *score.entry(net).or_default() += n * 2;
        }
    }
    for cell in &congestion.worst_cells {
        for net in &cell.nets {
            *score.entry(net).or_default() += 2;
        }
    }
    let mut v: Vec<(&str, usize)> = score.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    v.into_iter().take(8).map(|(n, _)| n.to_string()).collect()
}

/// For the parts most involved in crossings: crossings and airwire length if
/// the part were turned 90°, 180° or 270° about its anchor, airwires kept
/// (the ratsnest is not rebuilt; judgement).
fn rotation_hints(
    demand: &[Airwire],
    by_part: &BTreeMap<String, usize>,
    frames: &BTreeMap<String, (Pt, f64)>,
    footprints: &BTreeMap<(String, i64, i64), Vec<&CopperPad>>,
    current_k: usize,
) -> Vec<Value> {
    let mut parts: Vec<(&String, &usize)> = by_part.iter().collect();
    parts.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let big = |r: &str| footprints.iter().any(|((reference, _, _), pads)| reference == r && pads.len() >= 4);
    let total_len = |wires: &[Airwire]| wires.iter().map(Airwire::length).sum::<f64>();
    let current_len = total_len(demand);
    let mut out = Vec::new();
    for (reference, involved) in parts.into_iter().filter(|(r, _)| big(r)).take(3) {
        let Some(&(centre, _)) = frames.get(reference) else { continue };
        let mut options = Vec::new();
        for angle in [90.0, 180.0, 270.0] {
            let moved: Vec<Airwire> = demand
                .iter()
                .map(|w| {
                    let mut w = w.clone();
                    for end in [&mut w.a, &mut w.b] {
                        if end.pad.as_ref().is_some_and(|(r, _)| r == reference) {
                            end.at = konnect_sexp::geometry::transform_pad(end.at.0 - centre.0, end.at.1 - centre.1, centre.0, centre.1, angle);
                        }
                    }
                    w
                })
                .collect();
            let k = crossings::airwire_crossings(&moved).len();
            options.push(json!({ "rotate_by_deg": angle, "crossings": k, "airwire_mm": round3(total_len(&moved)) }));
        }
        // Fewest crossings, then the shortest airwires.
        let best = options
            .iter()
            .min_by(|a, b| {
                let key = |o: &Value| (o["crossings"].as_u64().unwrap_or(u64::MAX), o["airwire_mm"].as_f64().unwrap_or(f64::INFINITY));
                let (ka, kb) = (key(a), key(b));
                ka.0.cmp(&kb.0).then(ka.1.total_cmp(&kb.1))
            })
            .cloned();
        let improves = best.as_ref().is_some_and(|b| b["crossings"].as_u64().is_some_and(|k| (k as usize) < current_k));
        out.push(json!({
            "reference": reference,
            "improves": improves,
            "crossings_involving_part": involved,
            "now": { "crossings": current_k, "airwire_mm": round3(current_len) },
            "rotations": options,
            "best": best,
            "note": "airwires kept and their part ends turned with the part; the ratsnest is not rebuilt (judgement)",
        }));
    }
    out
}

