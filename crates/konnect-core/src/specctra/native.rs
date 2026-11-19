//! Specctra export through KiCad's own exporter, for boards outside the strict
//! profile of [`super::export_dsn`].
//!
//! KiCad's `ExportSpecctraDSN` already represents what the strict exporter
//! refuses: roundrect, oval, trapezoid and custom pads, unplated holes, arc
//! outlines, back-side parts, repeated pad numbers and any copper layer count.
//! It runs here on the exact IPC snapshot the strict exporter would have read,
//! written to a private temporary directory, so the live board file is never
//! touched and the manifest stays bound to that revision.
//!
//! Zones need one change. KiCad writes every copper zone as a Specctra
//! `(plane …)` covering the zone outline, and Freerouting 2.1 treats those as
//! obstacles for every other net: on a 4-layer board with GND and +3V3 inner
//! planes, no via of another net can cross them, and the router stalls with
//! most connections unrouted (measured: 35 of 41 left after 999 passes). With
//! the planes removed the same board routes completely. So the planes are
//! stripped from the DSN and reported. The layers stay, typed `power` when
//! KiCad typed them so, and Freerouting does not route on a power layer; the
//! zone nets are routed as ordinary nets on the signal layers. Refilling the
//! zones after import reconnects the planes, and the import's DRC runs with
//! refill.
//!
//! The reverse manifest is built from this DSN itself (schema 2), not by
//! correlating it with the strict exporter's output, which cannot exist for
//! these boards.

use super::{
    dsn_atom, parse_dsn_sexp, sha256_hex, ExportBundle, Manifest,
    ManifestComponent, ManifestLayer, ManifestNet, ManifestPadstack, ManifestPin, PadShape,
    SupportedProfile,
};
use anyhow::{bail, Context, Result};
use konnect_sexp::{parse_sexp, SexpNode};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Version 2 manifests come from a KiCad-native DSN.
pub(crate) const NATIVE_SCHEMA_VERSION: u32 = 2;

const EXPORT_TIMEOUT: Duration = Duration::from_secs(180);

/// The only code this runs: load the board, export the DSN. Paths arrive as
/// arguments, never interpolated into the script.
const EXPORT_SCRIPT: &str = "import sys, pcbnew\n\
board = pcbnew.LoadBoard(sys.argv[1])\n\
sys.exit(0 if pcbnew.ExportSpecctraDSN(board, sys.argv[2]) else 3)\n";

/// A plane `(plane NET (polygon LAYER …))` removed from the DSN.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct RemovedPlane {
    pub net: String,
    pub layer: String,
}

#[derive(Debug, Clone)]
pub(crate) struct NativeExport {
    pub bundle: ExportBundle,
    pub removed_planes: Vec<RemovedPlane>,
    /// Plane nets whose every SMD pad already drops to the plane (see
    /// `fanout_to_plane`): left out of the routing job entirely, because the
    /// refilled planes connect them.
    pub left_to_planes: Vec<String>,
    /// Plane nets that are fully fanned out but kept in the routing job,
    /// because every inner pour of theirs is on a `mixed` layer: routes on
    /// that layer can cut the pour into islands, so the net's own traces must
    /// connect its pads.
    pub kept_for_mixed_planes: Vec<String>,
    pub copper_layers: usize,
    pub power_layers: Vec<String>,
    pub custom_rules: bool,
}

/// The Python that can `import pcbnew`: `KONNECT_KICAD_PYTHON` when set, else
/// the interpreter KiCad ships beside `kicad-cli` (Windows, macOS), else
/// `python3` (Linux packages install `pcbnew` into the system Python).
pub(crate) fn kicad_python(kicad_cli: &str) -> PathBuf {
    if let Some(explicit) = std::env::var_os("KONNECT_KICAD_PYTHON") {
        return PathBuf::from(explicit);
    }
    if let Some(dir) = Path::new(kicad_cli).parent() {
        for name in ["python.exe", "python3"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from("python3")
}

/// Run KiCad's exporter on `snapshot` and build the bundle.
pub(crate) async fn export(
    python: &Path,
    board_path: &Path,
    snapshot: &str,
) -> Result<NativeExport> {
    let dsn = run_kicad_exporter(python, board_path, snapshot).await?;
    lower(board_path, snapshot, &dsn)
}

async fn run_kicad_exporter(python: &Path, board_path: &Path, snapshot: &str) -> Result<String> {
    let temp = tempfile::tempdir().context("create private export directory")?;
    let stem = board_path
        .file_stem()
        .context("board path has no file name")?;
    let board = temp.path().join(stem).with_extension("kicad_pcb");
    std::fs::write(&board, snapshot).context("write board snapshot for KiCad's exporter")?;
    // The project file carries the net classes the exporter turns into
    // Specctra classes; without it every net would get the defaults.
    let project = board_path.with_extension("kicad_pro");
    if project.is_file() {
        std::fs::copy(&project, board.with_extension("kicad_pro"))
            .context("copy project file for KiCad's exporter")?;
    }
    let dsn_path = board.with_extension("dsn");
    let mut command = tokio::process::Command::new(python);
    command
        .arg("-I")
        .arg("-c")
        .arg(EXPORT_SCRIPT)
        .arg(&board)
        .arg(&dsn_path)
        .current_dir(temp.path())
        .kill_on_drop(true);
    let output = tokio::time::timeout(EXPORT_TIMEOUT, command.output())
        .await
        .context("KiCad's Specctra exporter timed out")?
        .with_context(|| format!("start {} to run KiCad's Specctra exporter", python.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr
            .lines()
            .filter(|line| !line.contains("Debug:") && !line.starts_with("swig/python"))
            .collect::<Vec<_>>()
            .join("\n");
        bail!(
            "KiCad's Specctra exporter failed ({}) using {}: {}",
            output.status,
            python.display(),
            tail.chars().rev().take(600).collect::<String>().chars().rev().collect::<String>()
        );
    }
    std::fs::read_to_string(&dsn_path).context("read the DSN KiCad exported")
}

/// Strip planes and build the schema-2 manifest. Separate from the process
/// call so it can be tested on a stored KiCad DSN.
pub(crate) fn lower(board_path: &Path, snapshot: &str, raw_dsn: &str) -> Result<NativeExport> {
    let board = parse_sexp(snapshot).context("parse KiCad IPC board snapshot")?;
    if board.head() != Some("kicad_pcb") {
        bail!("KiCad IPC snapshot root is not 'kicad_pcb'");
    }
    let (dsn, removed_planes) = strip_planes(raw_dsn)?;
    // A layer the board types `mixed` carries a pour *and* signals: it stays
    // routable, and the pour refills around the routes after import.
    let mixed = konnect_sexp::layers::layers(&board)
        .into_iter()
        .filter(|layer| layer.kind == "mixed")
        .map(|layer| layer.name)
        .collect::<BTreeSet<_>>();
    let plane_layers = removed_planes
        .iter()
        .filter(|plane| plane.layer.starts_with("In") && !mixed.contains(&plane.layer))
        .map(|plane| plane.layer.clone())
        .collect::<BTreeSet<_>>();
    let dsn = retype_as_power(&dsn, &plane_layers)?;
    let fully_fanned = fully_fanned_plane_nets(&board, &removed_planes);
    let (left_to_planes, kept_for_mixed_planes) =
        split_by_plane_layer(fully_fanned, &removed_planes, &mixed);
    let dsn = drop_nets(&dsn, &left_to_planes)?;
    let dsn = edge_clearance_keepouts(&dsn, board_edge_clearance_mm(board_path))?;
    // Not `validate_dsn_syntax`: Topola's reader rejects KiCad's own image
    // outlines (`(outline (polygon …))`, which every stock footprint with a
    // courtyard polygon produces). The structural parse below plus the
    // manifest checks are the gate; the router is the real consumer.
    let root = parse_dsn_sexp(&dsn)?;
    if root.head() != Some("pcb") {
        bail!("KiCad's Specctra export root is not 'pcb'");
    }
    let (locked_tracks, locked_vias) = locked_routing(&board)?;

    // Layers: every DSN layer must be a copper layer of the board.
    let board_copper = konnect_sexp::layers::copper(&konnect_sexp::layers::layers(&board))
        .into_iter()
        .map(|layer| layer.name.clone())
        .collect::<BTreeSet<_>>();
    let structure = root.find("structure").context("DSN has no structure")?;
    let mut layers = Vec::new();
    let mut power_layers = Vec::new();
    for layer in structure.find_all("layer") {
        let name = dsn_atom(layer, 1, "layer name")?.to_string();
        if !board_copper.contains(&name) {
            bail!("KiCad's DSN names layer '{name}', which is not a copper layer of the board");
        }
        let index = layer
            .find("property")
            .and_then(|property| property.find("index"))
            .and_then(|index| index.get(1))
            .and_then(SexpNode::as_str)
            .context("DSN layer has no index")?
            .parse::<usize>()
            .context("DSN layer index is not an integer")?;
        if layer.find("type").and_then(|kind| kind.get(1)).and_then(SexpNode::as_str)
            == Some("power")
        {
            power_layers.push(name.clone());
        }
        layers.push(ManifestLayer {
            kicad_name: name.clone(),
            dsn_name: name,
            index,
        });
    }
    if layers.len() < 2 || layers.len() != board_copper.len() {
        bail!(
            "KiCad's DSN has {} copper layers but the board has {}",
            layers.len(),
            board_copper.len()
        );
    }
    let layer_names = layers
        .iter()
        .map(|layer| layer.dsn_name.clone())
        .collect::<BTreeSet<_>>();

    // Padstacks. Vias are the ones the structure lists; their names encode
    // KiCad's size and drill (`Via[0-3]_600:300_um`), and the import needs both.
    let via_names = structure
        .find("via")
        .and_then(SexpNode::children)
        .unwrap_or_default()
        .iter()
        .skip(1)
        .filter_map(SexpNode::as_str)
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    if via_names.is_empty() {
        bail!("KiCad's DSN declares no via padstack");
    }
    let library = root.find("library").context("DSN has no library")?;
    let mut padstacks = Vec::new();
    for padstack in library.find_all("padstack") {
        let name = dsn_atom(padstack, 1, "padstack name")?.to_string();
        let shape_layers = padstack
            .find_all("shape")
            .iter()
            .filter_map(|shape| shape.get(1))
            .filter_map(|geometry| geometry.get(1).and_then(SexpNode::as_str))
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        if via_names.contains(&name) {
            let (size_um, drill_um) = via_geometry(&name)?;
            if shape_layers != layer_names {
                bail!("via padstack '{name}' does not span every copper layer");
            }
            padstacks.push(ManifestPadstack {
                name,
                purpose: "via".to_string(),
                shape: PadShape::Circle,
                layers: shape_layers.into_iter().collect(),
                size_x_um: size_um,
                size_y_um: size_um,
                drill_um: Some(drill_um),
            });
        } else {
            // Pad geometry is KiCad's; the import only needs the name to exist.
            padstacks.push(ManifestPadstack {
                name,
                purpose: "pad".to_string(),
                shape: PadShape::Native,
                layers: shape_layers.into_iter().collect(),
                size_x_um: 0,
                size_y_um: 0,
                drill_um: None,
            });
        }
    }
    if let Some(missing) = via_names
        .iter()
        .find(|name| !padstacks.iter().any(|padstack| &padstack.name == *name))
    {
        bail!("KiCad's DSN lists via '{missing}' but defines no padstack for it");
    }

    // Images: pin name → padstack.
    let mut images = BTreeMap::<String, Vec<(String, String)>>::new();
    for image in library.find_all("image") {
        let name = dsn_atom(image, 1, "image name")?.to_string();
        let mut pins = Vec::new();
        for pin in image.find_all("pin") {
            // `(pin <padstack> [(rotate <deg>)] <pin id> <x> <y>)`: a pad
            // rotated within its footprint puts a list before the id.
            let name = pin
                .children()
                .unwrap_or_default()
                .iter()
                .skip(2)
                .find_map(SexpNode::as_str)
                .context("DSN is missing pin name")?;
            pins.push((
                name.to_string(),
                dsn_atom(pin, 1, "pin padstack")?.to_string(),
            ));
        }
        if images.insert(name.clone(), pins).is_some() {
            bail!("KiCad's DSN repeats image '{name}'");
        }
    }

    // Network: pin → net, net → class.
    let network = root.find("network").context("DSN has no network")?;
    let mut pin_nets = BTreeMap::<String, String>::new();
    let mut net_pins = BTreeMap::<String, Vec<String>>::new();
    for net in network.find_all("net") {
        let name = dsn_atom(net, 1, "net name")?.to_string();
        let pins = net
            .find("pins")
            .and_then(SexpNode::children)
            .unwrap_or_default()
            .iter()
            .skip(1)
            .filter_map(SexpNode::as_str)
            .map(str::to_string)
            .collect::<Vec<_>>();
        for pin in &pins {
            if pin_nets.insert(pin.clone(), name.clone()).is_some() {
                bail!("KiCad's DSN puts pin '{pin}' on two nets");
            }
        }
        if net_pins.insert(name.clone(), pins).is_some() {
            bail!("KiCad's DSN repeats net '{name}'");
        }
    }
    let mut net_classes = BTreeMap::<String, String>::new();
    for class in network.find_all("class") {
        let class_name = dsn_atom(class, 1, "class name")?.to_string();
        for net in class
            .children()
            .unwrap_or_default()
            .iter()
            .skip(2)
            .filter_map(SexpNode::as_str)
        {
            net_classes.insert(net.to_string(), class_name.clone());
        }
    }
    // A net with no pins (a zone-only or track-only net) gives the router
    // nothing to connect, so it is not part of the job.
    net_pins.retain(|_, pins| !pins.is_empty());
    let nets = net_pins
        .iter()
        .map(|(name, pins)| {
            Ok(ManifestNet {
                name: name.clone(),
                pins: pins.clone(),
                class_name: net_classes
                    .get(name)
                    .cloned()
                    .with_context(|| format!("KiCad's DSN puts net '{name}' in no class"))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // Placement, bound to the board's footprints by reference.
    let kiids = footprint_kiids(&board)?;
    let placement = root.find("placement").context("DSN has no placement")?;
    let mut components = Vec::new();
    let mut sides = BTreeSet::new();
    for component in placement.find_all("component") {
        let image_name = dsn_atom(component, 1, "component image")?.to_string();
        let image_pins = images
            .get(&image_name)
            .with_context(|| format!("DSN placement uses unknown image '{image_name}'"))?;
        for place in component.find_all("place") {
            let reference = dsn_atom(place, 1, "place reference")?.to_string();
            let x_um = whole_um(place, 2, &reference)?;
            let y_um = whole_um(place, 3, &reference)?;
            let side = dsn_atom(place, 4, "place side")?.to_string();
            if side != "front" && side != "back" {
                bail!("component '{reference}' has unknown side '{side}'");
            }
            sides.insert(side.clone());
            let rotation_degrees: f64 = dsn_atom(place, 5, "place rotation")?
                .parse()
                .with_context(|| format!("component '{reference}' rotation is not a number"))?;
            // KiCad writes -90 where Freerouting writes 270.
            let rotation_degrees = rotation_degrees.rem_euclid(360.0);
            let kiid = kiids
                .get(&reference)
                .cloned()
                .with_context(|| format!("DSN component '{reference}' is not on the board"))?;
            let pads = image_pins
                .iter()
                .map(|(pin, padstack)| {
                    let dsn_pin = format!("{reference}-{pin}");
                    ManifestPin {
                        pad_number: pin.clone(),
                        net: pin_nets.get(&dsn_pin).cloned(),
                        dsn_pin,
                        padstack_name: padstack.clone(),
                    }
                })
                .collect();
            components.push(ManifestComponent {
                reference,
                kiid,
                image_name: image_name.clone(),
                x_um,
                y_um,
                rotation_degrees,
                side,
                pads,
            });
        }
    }
    if components.is_empty() {
        bail!("KiCad's DSN places no components");
    }
    let mut seen = BTreeSet::new();
    if let Some(repeated) = components
        .iter()
        .find(|component| !seen.insert(component.reference.as_str()))
    {
        bail!(
            "reference '{}' is on more than one footprint; the SES cannot be mapped back",
            repeated.reference
        );
    }

    let custom_rules = board_path.with_extension("kicad_dru").is_file();
    let copper_zones = !board.find_all("zone").is_empty();
    let source_sha256 = sha256_hex(snapshot.as_bytes());
    let pad_count = components.iter().map(|component| component.pads.len()).sum();
    let manifest = Manifest {
        schema_version: NATIVE_SCHEMA_VERSION,
        board_path: board_path.display().to_string(),
        source_sha256: source_sha256.clone(),
        coordinate_unit: "um".to_string(),
        resolution: super::DSN_RESOLUTION as u32,
        supported_profile: SupportedProfile {
            copper_layers: layers.len() as u32,
            component_side: if sides.contains("back") { "both" } else { "front" }.to_string(),
            pad_shapes: vec!["kicad_native".to_string()],
            existing_routing: locked_tracks + locked_vias > 0,
            locked_track_count: locked_tracks,
            locked_via_count: locked_vias,
            copper_zones,
            custom_rules,
            outline: "kicad_native".to_string(),
            plane_layers: power_layers.clone(),
        },
        layers,
        components,
        nets,
        padstacks,
    };
    let component_count = manifest.components.len();
    let net_count = manifest.nets.len();
    let class_count = net_classes.values().collect::<BTreeSet<_>>().len();
    let copper_layers = manifest.layers.len();
    let manifest = serde_json::to_string_pretty(&manifest).context("serialize routing manifest")?;
    Ok(NativeExport {
        bundle: ExportBundle {
            dsn,
            manifest,
            source_sha256,
            component_count,
            pad_count,
            net_count,
            class_count,
        },
        removed_planes,
        left_to_planes,
        kept_for_mixed_planes,
        copper_layers,
        power_layers,
        custom_rules,
    })
}

/// Remove every `(plane …)` form, keeping the rest of the DSN byte-for-byte.
fn strip_planes(dsn: &str) -> Result<(String, Vec<RemovedPlane>)> {
    let mut out = String::with_capacity(dsn.len());
    let mut removed = Vec::new();
    let mut rest = dsn;
    while let Some(start) = rest.find("(plane ") {
        let end = start + balanced_len(&rest[start..]).context("unbalanced (plane …) in DSN")?;
        let form = parse_dsn_sexp(&rest[start..end]).context("parse DSN plane")?;
        let net = dsn_atom(&form, 1, "plane net")?.to_string();
        let layer = form
            .find("polygon")
            .or_else(|| form.find("rect"))
            .or_else(|| form.find("circle"))
            .or_else(|| form.find("path"))
            .and_then(|shape| shape.get(1))
            .and_then(SexpNode::as_str)
            .unwrap_or_default()
            .to_string();
        removed.push(RemovedPlane { net, layer });
        // Drop the indentation in front of the form along with it.
        let keep = rest[..start].trim_end_matches([' ', '\t']);
        out.push_str(keep);
        rest = rest[end..].strip_prefix('\n').unwrap_or(&rest[end..]);
    }
    out.push_str(rest);
    Ok((out, removed))
}

/// Plane nets (a removed plane on an inner layer) whose every surface-mount
/// pad is already joined to a same-net track or via. Through-hole pads reach
/// the plane on their own.
fn fully_fanned_plane_nets(board: &SexpNode, planes: &[RemovedPlane]) -> Vec<String> {
    let nets = planes
        .iter()
        .filter(|plane| plane.layer.starts_with("In"))
        .map(|plane| plane.net.clone())
        .collect::<BTreeSet<_>>();
    nets.into_iter()
        .filter(|net| {
            let defaults = |_: &str| crate::tools::fanout::FanoutOptions::default();
            crate::tools::fanout::plan(board, std::slice::from_ref(net), &defaults)
                .all_pads_reach_planes()
        })
        .collect()
}

/// Split fully fanned plane nets into those left to their planes and those
/// kept in the routing job.
///
/// A net is left out only when at least one of its inner pours is on a layer
/// that stays a solid plane (KiCad `power`, written `(type power)` in the
/// DSN). Freerouting never routes on such a layer (`read_layer_scope` sets
/// `is_signal = false` for `(type power)`; doc DB
/// `freerouting-2-3-0-structure-java`), so the refilled pour joins every
/// fan-out via. A `mixed` layer is written `(type signal)` and Freerouting
/// routes other nets across it; a net dropped from the network
/// (`freerouting-2-3-0-network-java`) has nothing that rejoins the pour
/// islands those routes cut. Such a net stays in the job and is routed as
/// an ordinary net (its stripped plane gives no `contains_plane` via
/// discount, `freerouting-2-3-0-layerstructure-java`).
fn split_by_plane_layer(
    fully_fanned: Vec<String>,
    planes: &[RemovedPlane],
    mixed: &BTreeSet<String>,
) -> (Vec<String>, Vec<String>) {
    fully_fanned.into_iter().partition(|net| {
        planes
            .iter()
            .any(|plane| &plane.net == net && plane.layer.starts_with("In") && !mixed.contains(&plane.layer))
    })
}

/// Remove nets from a DSN: their `(net …)` in the network, their names in
/// every `(class …)` (a class left with no nets is removed), and every
/// `(wire …)` / `(via …)` of theirs in the wiring.
fn drop_nets(dsn: &str, nets: &[String]) -> Result<String> {
    if nets.is_empty() {
        return Ok(dsn.to_string());
    }
    let mut removed_wiring = Vec::new();
    let quoted = |name: &str| {
        if name.chars().any(|c| c.is_whitespace() || "()\"".contains(c)) {
            format!("\"{name}\"")
        } else {
            name.to_string()
        }
    };
    let names: BTreeSet<String> = nets.iter().flat_map(|n| [n.clone(), format!("\"{n}\"")]).collect();
    let mut out = String::with_capacity(dsn.len());
    let mut rest = dsn;
    loop {
        // The next form that may mention a dropped net.
        let next = ["(net ", "(class ", "(wire", "(via "]
            .iter()
            .filter_map(|tag| rest.find(tag).map(|at| (at, *tag)))
            .min_by_key(|(at, _)| *at);
        let Some((start, tag)) = next else {
            out.push_str(rest);
            break;
        };
        let end = start + balanced_len(&rest[start..]).context("unbalanced DSN form")?;
        let form = &rest[start..end];
        let keep_prefix = |out: &mut String| out.push_str(rest[..start].trim_end_matches([' ', '\t']));
        match tag {
            "(net " => {
                let first = dsn_tokens(&form[5..]);
                let name = first.first().map(String::as_str).unwrap_or("");
                // `(net NAME (pins …))` in the network; `(net NAME)` inside a
                // wire or via is handled with its parent below.
                if names.contains(name) && form.contains("(pins") {
                    keep_prefix(&mut out);
                    rest = rest[end..].strip_prefix('\n').unwrap_or(&rest[end..]);
                    continue;
                }
            }
            "(class " => {
                let body_end = form.find("(circuit").or_else(|| form.find("(rule")).unwrap_or(form.len() - 1);
                let head = &form[..body_end];
                let all = dsn_tokens(&head[1..]);
                let class_tag = "(class";
                let class_name = all.get(1).cloned().unwrap_or_default();
                let members = all.iter().skip(2).cloned().collect::<Vec<_>>();
                let kept = members.iter().filter(|token| !names.contains(*token)).cloned().collect::<Vec<_>>();
                if kept.len() != members.len() {
                    if kept.is_empty() {
                        keep_prefix(&mut out);
                        rest = rest[end..].strip_prefix('\n').unwrap_or(&rest[end..]);
                        continue;
                    }
                    out.push_str(&rest[..start]);
                    out.push_str(&format!("{class_tag} {class_name} {}\n      ", kept.join(" ")));
                    out.push_str(&form[body_end..]);
                    rest = &rest[end..];
                    continue;
                }
            }
            _ => {
                // A wire or via of a dropped net.
                if nets.iter().any(|n| form.contains(&format!("(net {})", quoted(n)))) {
                    removed_wiring.push(form.to_string());
                    keep_prefix(&mut out);
                    rest = rest[end..].strip_prefix('\n').unwrap_or(&rest[end..]);
                    continue;
                }
            }
        }
        // Keep this form; continue scanning inside it.
        let step = start + tag.len();
        out.push_str(&rest[..step]);
        rest = &rest[step..];
    }
    keep_copper_as_obstacles(&out, &removed_wiring)
}

/// The board's copper-to-edge clearance: the largest `edge_clearance` minimum
/// in the project's `.kicad_dru` (the fab rules write one) or the
/// `min_copper_edge_clearance` design setting in the `.kicad_pro`.
fn board_edge_clearance_mm(board_path: &Path) -> f64 {
    let mut best: f64 = 0.0;
    if let Ok(dru) = std::fs::read_to_string(board_path.with_extension("kicad_dru")) {
        let needle = "edge_clearance (min ";
        let mut rest = dru.as_str();
        while let Some(at) = rest.find(needle) {
            let after = &rest[at + needle.len()..];
            let number: String = after.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
            if let Ok(v) = number.parse::<f64>() {
                best = best.max(v);
            }
            rest = after;
        }
    }
    if let Ok(pro) = std::fs::read_to_string(board_path.with_extension("kicad_pro")) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&pro) {
            if let Some(v) = value["board"]["design_settings"]["rules"]["min_copper_edge_clearance"].as_f64() {
                best = best.max(v);
            }
        }
    }
    best
}

/// KiCad's Specctra export carries the board outline but not the
/// copper-to-edge clearance, so Freerouting keeps only its net-class
/// clearance from the edge (0.24 mm against JLCPCB's 0.3 mm on the demo
/// board, which the import's DRC gate then refused). The outline goes back in
/// as a keep-out band on every signal layer, `2 × clearance` wide.
fn edge_clearance_keepouts(dsn: &str, clearance_mm: f64) -> Result<String> {
    if clearance_mm <= 0.0 {
        return Ok(dsn.to_string());
    }
    let root = parse_dsn_sexp(dsn)?;
    let structure = root.find("structure").context("DSN has no structure")?;
    let unit_per_mm = match root.find("resolution").and_then(|r| r.get(1)).and_then(SexpNode::as_str) {
        Some("mil") => 1000.0 / 25.4,
        Some("mm") => 1.0,
        _ => 1000.0, // KiCad writes um
    };
    let mut keepouts = String::new();
    let signal_layers: Vec<String> = structure
        .find_all("layer")
        .iter()
        .filter(|layer| layer.find("type").and_then(|t| t.get(1)).and_then(SexpNode::as_str) == Some("signal"))
        .filter_map(|layer| layer.get(1).and_then(SexpNode::as_str).map(str::to_string))
        .collect();
    let width = 2.0 * clearance_mm * unit_per_mm;
    for boundary in structure.find_all("boundary") {
        for shape in boundary.children().unwrap_or_default() {
            let points: Vec<&str> = match shape.head() {
                Some("path") | Some("polygon") => shape.children().unwrap_or_default().iter().skip(3).filter_map(SexpNode::as_str).collect(),
                Some("rect") => continue,
                _ => continue,
            };
            if points.len() < 4 {
                continue;
            }
            // One keep-out per edge: Freerouting reads a closed keep-out path
            // as the area it encloses — the whole board (found on the demo
            // board: nothing routed).
            let xy: Vec<(&str, &str)> = points.chunks(2).filter(|c| c.len() == 2).map(|c| (c[0], c[1])).collect();
            for layer in &signal_layers {
                for pair in xy.windows(2) {
                    let ((x1, y1), (x2, y2)) = (pair[0], pair[1]);
                    if (x1, y1) != (x2, y2) {
                        keepouts.push_str(&format!("    (keepout \"\" (path {layer} {width} {x1} {y1} {x2} {y2}))\n"));
                    }
                }
            }
        }
    }
    if keepouts.is_empty() {
        return Ok(dsn.to_string());
    }
    let start = dsn.find("(structure").context("DSN has no structure")?;
    let close = start + balanced_len(&dsn[start..]).context("unbalanced DSN structure")? - 1;
    let insert_at = dsn[..close].trim_end_matches([' ', '\t']).len();
    Ok(format!("{}{}{}", &dsn[..insert_at], keepouts, &dsn[insert_at..]))
}

/// The copper of a net left to its plane (its fan-out stubs and vias) is
/// still on the board. Without it the router routes straight through it, so
/// it goes back into the structure as keep-outs of the copper's own size:
/// Freerouting already keeps its clearance from a keep-out. Widening them by
/// the clearance as well walled in USB-C pins on the PoC board (1 connection
/// left unrouted); at copper size it routes completely and KiCad's DRC is
/// clean.
fn keep_copper_as_obstacles(dsn: &str, wiring: &[String]) -> Result<String> {
    if wiring.is_empty() {
        return Ok(dsn.to_string());
    }
    let root = parse_dsn_sexp(dsn)?;
    let structure = root.find("structure").context("DSN has no structure")?;
    let layers = structure
        .find_all("layer")
        .iter()
        .filter_map(|layer| layer.get(1).and_then(SexpNode::as_str))
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut keepouts = String::new();
    for form in wiring {
        let node = parse_dsn_sexp(form)?;
        match node.head() {
            Some("wire") => {
                let Some(path) = node.find("path") else { continue };
                let atoms = path
                    .children()
                    .unwrap_or_default()
                    .iter()
                    .skip(1)
                    .filter_map(SexpNode::as_str)
                    .collect::<Vec<_>>();
                if atoms.len() < 4 {
                    continue;
                }
                let width: f64 = atoms[1].parse().context("DSN wire width")?;
                keepouts.push_str(&format!(
                    "    (keepout \"\" (path {} {} {}))\n",
                    atoms[0],
                    width,
                    atoms[2..].join(" ")
                ));
            }
            Some("via") => {
                let padstack = dsn_atom(&node, 1, "via padstack")?;
                let (size, _) = via_geometry(padstack)?;
                let (x, y) = (dsn_atom(&node, 2, "via x")?, dsn_atom(&node, 3, "via y")?);
                for layer in &layers {
                    keepouts.push_str(&format!(
                        "    (keepout \"\" (circle {layer} {} {x} {y}))\n",
                        size
                    ));
                }
            }
            _ => {}
        }
    }
    let start = dsn.find("(structure").context("DSN has no structure")?;
    let close = start + balanced_len(&dsn[start..]).context("unbalanced DSN structure")? - 1;
    let insert_at = dsn[..close].trim_end_matches([' ', '\t']).len();
    Ok(format!("{}{}{}", &dsn[..insert_at], keepouts, &dsn[insert_at..]))
}

/// Whitespace-separated atoms up to the first `(` or `)` outside quotes;
/// a quoted atom keeps its quotes (`"GND A"`), as DSN names are written.
fn dsn_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in text.chars() {
        match ch {
            '"' => {
                current.push(ch);
                quoted = !quoted;
            }
            c if !quoted && (c == '(' || c == ')') => break,
            c if !quoted && c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Mark these layers `power` in the DSN, so Freerouting does not route on
/// them whatever KiCad typed them. An inner layer carrying a copper zone is a
/// plane on this board even when its type is `signal` (the default for a
/// layer `add_layer` creates).
fn retype_as_power(dsn: &str, layers: &BTreeSet<String>) -> Result<String> {
    let mut out = dsn.to_string();
    for layer in layers {
        let Some(start) = out.find(&format!("(layer {layer}")) else { continue };
        let end = start + balanced_len(&out[start..]).context("unbalanced DSN layer")?;
        let form = out[start..end].replace("(type signal)", "(type power)").replace("(type mixed)", "(type power)");
        out.replace_range(start..end, &form);
    }
    Ok(out)
}

/// Length of the parenthesised form at the start of `text`, honouring quotes.
fn balanced_len(text: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut quoted = false;
    for (index, ch) in text.char_indices() {
        match ch {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// `Via[0-3]_600:300_um` → (600, 300): KiCad's via padstack name carries the
/// diameter and drill in micrometres; the DSN shapes carry only the diameter.
fn via_geometry(name: &str) -> Result<(i64, i64)> {
    let parse = || -> Option<(i64, i64)> {
        let sizes = name.rsplit_once(']')?.1.strip_prefix('_')?.strip_suffix("_um")?;
        let (size, drill) = sizes.split_once(':')?;
        Some((size.parse().ok()?, drill.parse().ok()?))
    };
    match parse() {
        Some((size, drill)) if size > 0 && drill > 0 && drill < size => Ok((size, drill)),
        _ => bail!("cannot read via size and drill from KiCad padstack name '{name}'"),
    }
}

fn whole_um(place: &SexpNode, index: usize, reference: &str) -> Result<i64> {
    let value: f64 = dsn_atom(place, index, "place coordinate")?
        .parse()
        .with_context(|| format!("component '{reference}' coordinate is not a number"))?;
    if !value.is_finite() || value.fract() != 0.0 {
        bail!(
            "component '{reference}' is not on the 1 µm grid ({value}); the SES placement \
             check cannot match it"
        );
    }
    Ok(value as i64)
}

fn footprint_kiids(board: &SexpNode) -> Result<BTreeMap<String, String>> {
    let mut kiids = BTreeMap::new();
    for footprint in board.find_all("footprint") {
        let Some(reference) = footprint
            .find_all("property")
            .into_iter()
            .find(|property| property.get(1).and_then(SexpNode::as_str) == Some("Reference"))
            .and_then(|property| property.get(2))
            .and_then(SexpNode::as_str)
        else {
            continue;
        };
        let kiid = footprint
            .find_str("uuid")
            .or_else(|| footprint.find_str("tstamp"))
            .with_context(|| format!("footprint '{reference}' has no uuid"))?;
        kiids.insert(reference.to_string(), kiid.to_string());
    }
    Ok(kiids)
}

/// Existing routing must be locked, as on the strict path: the SES is applied
/// on top of the board, and an unlocked track the router moved would be added
/// a second time. The counts bind the manifest to the board.
fn locked_routing(board: &SexpNode) -> Result<(usize, usize)> {
    let locked = |node: &&SexpNode| {
        node.find_str("locked") == Some("yes")
            || node.children().unwrap_or_default().iter().any(|c| c.as_str() == Some("locked"))
    };
    if !board.find_all("arc").is_empty() {
        bail!("board has a routed arc; existing routing must be straight, locked tracks");
    }
    let tracks = board.find_all("segment");
    let vias = board.find_all("via");
    let unlocked_tracks = tracks.iter().filter(|track| !locked(track)).count();
    let unlocked_vias = vias.iter().filter(|via| !locked(via)).count();
    if unlocked_tracks + unlocked_vias > 0 {
        bail!(
            "board has {unlocked_tracks} unlocked track(s) and {unlocked_vias} unlocked via(s); \
             lock existing routing (or delete it) before exporting for the autorouter"
        );
    }
    Ok((tracks.len(), vias.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn via_names_carry_size_and_drill() {
        assert_eq!(via_geometry("Via[0-3]_600:300_um").unwrap(), (600, 300));
        assert_eq!(via_geometry("Via[0-1]_800:400_um").unwrap(), (800, 400));
        assert!(via_geometry("Via[0-1]_300:600_um").is_err());
        assert!(via_geometry("MyVia").is_err());
    }

    #[test]
    fn dropped_nets_leave_network_classes_and_wiring() {
        let dsn = "(pcb x\n  (structure\n    (layer F.Cu (type signal))\n  )\n  (network\n    (net GND\n      (pins U1-1 C1-2)\n    )\n    (net SDA\n      (pins U1-2 R1-1)\n    )\n    (class Power GND\n      (circuit (use_via V))\n      (rule (width 300))\n    )\n    (class kicad_default SDA GND\n      (circuit (use_via V))\n    )\n  )\n  (wiring\n    (wire (path F.Cu 300  0 0  1 1)(net GND)(type protect))\n    (via \"Via[0-1]_600:300_um\" 1 1 (net GND)(type protect))\n    (wire (path F.Cu 200  0 0  2 2)(net SDA)(type protect))\n  )\n)\n";
        let out = drop_nets(dsn, &["GND".to_string()]).unwrap();
        let root = super::super::parse_dsn_sexp(&out).unwrap();
        let network = root.find("network").unwrap();
        assert_eq!(network.find_all("net").len(), 1, "{out}");
        assert_eq!(network.find_all("class").len(), 1, "a class left empty is removed: {out}");
        assert!(!out.contains("GND"), "{out}");
        assert!(out.contains("(class kicad_default SDA"), "{out}");
        assert_eq!(root.find("wiring").unwrap().find_all("wire").len(), 1, "{out}");
    }

    #[test]
    fn dropped_copper_stays_as_keepouts() {
        let dsn = "(pcb x\n  (structure\n    (layer F.Cu (type signal))\n    (layer B.Cu (type signal))\n    (rule (width 200) (clearance 200) (clearance 50 (type smd_smd)))\n  )\n  (network\n    (net GND\n      (pins U1-1)\n    )\n  )\n  (wiring\n    (wire (path F.Cu 300  0 0  800 0)(net GND)(type protect))\n    (via \"Via[0-1]_600:300_um\" 800 0 (net GND)(type protect))\n  )\n)\n";
        let out = drop_nets(dsn, &["GND".to_string()]).unwrap();
        let root = super::super::parse_dsn_sexp(&out).unwrap();
        let keepouts = root.find("structure").unwrap().find_all("keepout");
        // One path plus one circle per layer, at the copper's own size.
        assert_eq!(keepouts.len(), 3, "{out}");
        assert!(out.contains("(path F.Cu 300 0 0 800 0)"), "{out}");
        assert!(out.contains("(circle B.Cu 600 800 0)"), "{out}");
        assert!(!out.contains("(net GND"), "{out}");
    }

    #[test]
    fn names_with_spaces_or_brackets_are_dropped_whole() {
        let dsn = "(pcb x\n  (structure\n    (layer F.Cu (type signal))\n  )\n  (network\n    (net \"GND A\"\n      (pins U1-1)\n    )\n    (net \"GND(A)\"\n      (pins U1-2)\n    )\n    (net SDA\n      (pins U1-3)\n    )\n    (class Power \"GND A\" \"GND(A)\" SDA\n      (circuit (use_via V))\n    )\n  )\n  (wiring\n  )\n)\n";
        let out = drop_nets(dsn, &["GND A".to_string(), "GND(A)".to_string()]).unwrap();
        let root = super::super::parse_dsn_sexp(&out).unwrap();
        let network = root.find("network").unwrap();
        assert_eq!(network.find_all("net").len(), 1, "{out}");
        assert!(!out.contains("GND"), "{out}");
        assert!(out.contains("(class Power SDA"), "{out}");
    }

    #[test]
    fn inner_layers_with_planes_become_power_layers() {
        let dsn = "(pcb x\n  (structure\n    (layer F.Cu\n      (type signal)\n    )\n    (layer In1.Cu\n      (type signal)\n    )\n  )\n)\n";
        let out = retype_as_power(dsn, &["In1.Cu".to_string()].into_iter().collect()).unwrap();
        assert!(out.contains("(layer In1.Cu\n      (type power)"), "{out}");
        assert!(out.contains("(layer F.Cu\n      (type signal)"), "{out}");
    }

    #[test]
    fn planes_are_removed_and_reported() {
        let dsn = "(pcb x\n  (structure\n    (layer F.Cu)\n    (plane GND (polygon In1.Cu 0  1 2  3 4))\n    (plane \"+3V3\" (polygon In2.Cu 0  1 2  3 4))\n    (via V)\n  )\n)\n";
        let (out, removed) = strip_planes(dsn).unwrap();
        assert_eq!(out, "(pcb x\n  (structure\n    (layer F.Cu)\n    (via V)\n  )\n)\n");
        assert_eq!(
            removed,
            vec![
                RemovedPlane { net: "GND".into(), layer: "In1.Cu".into() },
                RemovedPlane { net: "+3V3".into(), layer: "In2.Cu".into() },
            ]
        );
    }
}

#[cfg(test)]
mod round_trip_tests {
    //! The real KiCad 10.0.8 + Freerouting 2.1.0 round trip in
    //! `tests/fixtures/specctra_native_4layer.README.md`.
    use super::{balanced_len, parse_dsn_sexp, split_by_plane_layer, RemovedPlane};
    use konnect_sexp::SexpNode;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    const BOARD: &str = include_str!("../../tests/fixtures/specctra_native_4layer.kicad_pcb");
    const DSN: &str = include_str!("../../tests/fixtures/specctra_native_4layer.dsn");
    const SES: &str = include_str!("../../tests/fixtures/specctra_native_4layer.ses");

    fn board() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        // The SES names its base design after the exported file.
        let path = dir.path().join("mcu_board_unrouted.kicad_pcb");
        std::fs::write(&path, BOARD).unwrap();
        (dir, path)
    }

    #[test]
    fn kicad_dsn_lowers_to_a_four_layer_manifest_without_planes() {
        let (_dir, path) = board();
        let export = super::lower(&path, BOARD, DSN).unwrap();
        assert!(!export.bundle.dsn.contains("(plane "));
        assert_eq!(export.removed_planes.len(), 3);
        assert_eq!(export.copper_layers, 4);
        assert_eq!(export.power_layers, vec!["In1.Cu", "In2.Cu"]);
        assert_eq!(export.bundle.component_count, 32);
        let manifest: serde_json::Value = serde_json::from_str(&export.bundle.manifest).unwrap();
        assert_eq!(manifest["schema_version"], 2);
        assert_eq!(manifest["supported_profile"]["copper_zones"], true);
        let via = manifest["padstacks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|padstack| padstack["name"] == "Via[0-3]_600:300_um")
            .unwrap();
        assert_eq!((via["size_x_um"].as_i64(), via["drill_um"].as_i64()), (Some(600), Some(300)));
    }

    /// A `mixed` inner layer keeps its pour out of the DSN but stays a
    /// routing layer for Freerouting.
    #[test]
    fn a_mixed_inner_layer_stays_routable() {
        let (_dir, path) = board();
        let line = BOARD
            .lines()
            .find(|line| line.contains("\"In2.Cu\""))
            .expect("fixture has In2.Cu");
        let kind = line.trim().trim_end_matches(')').rsplit(' ').next().unwrap();
        let mixed_board = BOARD.replacen(line, &line.replacen(kind, "mixed", 1), 1);
        // KiCad's exporter writes a `mixed` layer as `(type signal)`.
        let dsn = DSN.replacen("(layer In2.Cu\n      (type power)", "(layer In2.Cu\n      (type signal)", 1);
        assert_ne!(dsn, DSN);
        let export = super::lower(&path, &mixed_board, &dsn).unwrap();
        assert_eq!(export.power_layers, vec!["In1.Cu"]);
        assert!(export.bundle.dsn.contains("(layer In2.Cu\n      (type signal)"));
        assert!(!export.bundle.dsn.contains("(plane "));
    }

    /// The fixture board with every `+3V3` pad made through-hole: such pads
    /// reach the planes on their own, so the fan-out planner reports the net
    /// fully fanned without a via per pad.
    fn board_with_fanned_3v3(board: &str) -> String {
        let mut out = String::with_capacity(board.len());
        let mut rest = board;
        while let Some(start) = rest.find("(pad ") {
            let end = start + balanced_len(&rest[start..]).unwrap();
            let form = &rest[start..end];
            out.push_str(&rest[..start]);
            if form.contains("(net \"+3V3\")") {
                out.push_str(&form.replacen(" smd ", " thru_hole ", 1));
            } else {
                out.push_str(form);
            }
            rest = &rest[end..];
        }
        out.push_str(rest);
        out
    }

    fn with_in2_typed(board: &str, kind: &str) -> String {
        let line = board.lines().find(|line| line.contains("(6 \"In2.Cu\"")).expect("fixture has In2.Cu");
        board.replacen(line, &line.replacen("power", kind, 1), 1)
    }

    /// On a `power` plane layer a fully fanned plane net leaves the routing
    /// job (the solid plane connects every via): Pass 4 behaviour.
    #[test]
    fn a_fully_fanned_net_on_a_power_plane_leaves_the_job() {
        let (_dir, path) = board();
        let fanned = board_with_fanned_3v3(BOARD);
        assert_ne!(fanned, BOARD);
        let export = super::lower(&path, &fanned, DSN).unwrap();
        assert_eq!(export.left_to_planes, vec!["+3V3"]);
        assert!(export.kept_for_mixed_planes.is_empty());
        let root = super::super::parse_dsn_sexp(&export.bundle.dsn).unwrap();
        let network = root.find("network").unwrap();
        assert!(network.find_all("net").iter().all(|net| net.get(1).and_then(SexpNode::as_str) != Some("+3V3")));
    }

    /// Tier 1 fix: when the net's pour is on a `mixed` layer Freerouting may
    /// cut it into islands, so the net stays in the job with all its pins.
    #[test]
    fn a_fully_fanned_net_on_a_mixed_layer_stays_in_the_job() {
        let (_dir, path) = board();
        let mixed_board = with_in2_typed(&board_with_fanned_3v3(BOARD), "mixed");
        let dsn = DSN.replacen("(layer In2.Cu\n      (type power)", "(layer In2.Cu\n      (type signal)", 1);
        let export = super::lower(&path, &mixed_board, &dsn).unwrap();
        assert!(export.left_to_planes.is_empty(), "{:?}", export.left_to_planes);
        assert_eq!(export.kept_for_mixed_planes, vec!["+3V3"]);
        assert_eq!(export.power_layers, vec!["In1.Cu"]);
        let root = super::super::parse_dsn_sexp(&export.bundle.dsn).unwrap();
        let net = root
            .find("network")
            .unwrap()
            .find_all("net")
            .into_iter()
            .find(|net| net.get(1).and_then(SexpNode::as_str) == Some("+3V3"))
            .expect("+3V3 stays in the network");
        let pins = net.find("pins").and_then(SexpNode::children).unwrap().len() - 1;
        let raw = parse_dsn_sexp(&dsn).unwrap();
        let raw_pins = raw
            .find("network")
            .unwrap()
            .find_all("net")
            .into_iter()
            .find(|net| net.get(1).and_then(SexpNode::as_str) == Some("+3V3"))
            .and_then(|net| net.find("pins").and_then(SexpNode::children))
            .unwrap()
            .len()
            - 1;
        assert_eq!(pins, raw_pins);
        let manifest: serde_json::Value = serde_json::from_str(&export.bundle.manifest).unwrap();
        assert!(manifest["nets"].as_array().unwrap().iter().any(|net| net["name"] == "+3V3"));
    }

    #[test]
    fn a_net_with_a_pour_on_a_power_layer_too_still_leaves_the_job() {
        let planes = [
            RemovedPlane { net: "GND".into(), layer: "In1.Cu".into() },
            RemovedPlane { net: "GND".into(), layer: "In2.Cu".into() },
            RemovedPlane { net: "+3V3".into(), layer: "In2.Cu".into() },
            RemovedPlane { net: "+5V".into(), layer: "B.Cu".into() },
            RemovedPlane { net: "+5V".into(), layer: "In2.Cu".into() },
        ];
        let mixed = BTreeSet::from(["In2.Cu".to_string()]);
        let fanned = vec!["+3V3".to_string(), "+5V".to_string(), "GND".to_string()];
        let (left, kept) = split_by_plane_layer(fanned, &planes, &mixed);
        assert_eq!(left, vec!["GND"]);
        // An outer-layer pour is never a solid plane.
        assert_eq!(kept, vec!["+3V3", "+5V"]);
    }

    /// A pad rotated within its footprint is written
    /// `(pin <padstack> (rotate <deg>) <id> <x> <y>)`; the id is the first
    /// atom after the padstack, not index 2.
    #[test]
    fn rotated_pins_keep_their_names() {
        let (_dir, path) = board();
        let pin = "(pin RoundRect[T]Pad_900.000000x950.000000_225.856000_um_0.000000_0 1 -775 0)";
        assert!(DSN.contains(pin));
        let rotated = DSN.replacen(
            pin,
            "(pin RoundRect[T]Pad_900.000000x950.000000_225.856000_um_0.000000_0 (rotate 90) 1 -775 0)",
            1,
        );
        let plain = super::lower(&path, BOARD, DSN).unwrap();
        let export = super::lower(&path, BOARD, &rotated).unwrap();
        assert_eq!(export.bundle.component_count, plain.bundle.component_count);
        assert_eq!(export.bundle.manifest, plain.bundle.manifest);
    }

    #[test]
    fn freerouting_session_plans_onto_the_signal_layers() {
        let (_dir, path) = board();
        let export = super::lower(&path, BOARD, DSN).unwrap();
        let plan =
            crate::specctra_ses::parse_import_plan(&path, BOARD, &export.bundle.manifest, SES)
                .unwrap();
        assert_eq!((plan.tracks.len(), plan.vias.len(), plan.arcs.len()), (244, 12, 0));
        assert!(plan.duplicate_wires_dropped > 0);
        assert!(plan.refill_zones);
        assert!(plan.tracks.iter().all(|track| track.layer == "F.Cu" || track.layer == "B.Cu"));
        assert!(plan.vias.iter().all(|via| via.drill_mm > 0.0 && via.size_mm > via.drill_mm));
    }

    /// Freerouting 2.3.0 writes KiCad's variant images (`…::1`) under their
    /// base name.
    #[test]
    fn a_session_naming_variants_by_base_image_is_accepted() {
        let (_dir, path) = board();
        let export = super::lower(&path, BOARD, DSN).unwrap();
        let base_named = SES.replace("C_0603_1608Metric::1", "C_0603_1608Metric");
        let renamed = SES.replace("C_0603_1608Metric::1", "C_0805_2012Metric");
        assert_ne!(base_named, SES, "the 2.1.0 fixture keeps variant names");
        crate::specctra_ses::parse_import_plan(&path, BOARD, &export.bundle.manifest, &base_named)
            .unwrap();
        assert!(crate::specctra_ses::parse_import_plan(
            &path,
            BOARD,
            &export.bundle.manifest,
            &renamed
        )
        .is_err());
    }

    #[test]
    fn a_session_routing_on_a_plane_layer_is_refused() {
        let (_dir, path) = board();
        let export = super::lower(&path, BOARD, DSN).unwrap();
        let cut = SES.replacen("(path B.Cu ", "(path In1.Cu ", 1);
        assert_ne!(cut, SES);
        let error =
            crate::specctra_ses::parse_import_plan(&path, BOARD, &export.bundle.manifest, &cut)
                .unwrap_err();
        assert!(format!("{error:#}").contains("plane layer"), "{error:#}");
    }

    /// The second regression board types its inner planes `signal`; they must
    /// still be treated as planes.
    #[test]
    fn inner_planes_typed_signal_become_unroutable_plane_layers() {
        const BOARD2: &str = include_str!("../../tests/fixtures/fanout_regress2.kicad_pcb");
        const DSN2: &str = include_str!("../../tests/fixtures/fanout_regress2.dsn");
        assert!(DSN2.contains("(layer In1.Cu\n      (type signal)"), "fixture premise");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("regress2.kicad_pcb");
        std::fs::write(&path, BOARD2).unwrap();
        let export = super::lower(&path, BOARD2, DSN2).unwrap();
        assert_eq!(export.power_layers, vec!["In1.Cu", "In2.Cu"]);
        assert!(export.bundle.dsn.contains("(layer In1.Cu\n      (type power)"));
        let manifest: serde_json::Value = serde_json::from_str(&export.bundle.manifest).unwrap();
        assert_eq!(manifest["supported_profile"]["plane_layers"], serde_json::json!(["In1.Cu", "In2.Cu"]));
    }

    #[test]
    fn a_session_for_another_revision_is_refused() {
        let (_dir, path) = board();
        let export = super::lower(&path, BOARD, DSN).unwrap();
        let edited = BOARD.replacen("(paper \"A4\")", "(paper \"A3\")", 1);
        assert_ne!(edited, BOARD);
        assert!(crate::specctra_ses::parse_import_plan(&path, &edited, &export.bundle.manifest, SES)
            .is_err());
    }
}

#[cfg(test)]
mod edge_clearance_tests {
    use super::edge_clearance_keepouts;

    const DSN: &str = "(pcb x\n  (resolution um 10)\n  (unit um)\n  (structure\n    (layer F.Cu (type signal))\n    (layer In1.Cu (type power))\n    (layer B.Cu (type signal))\n    (boundary\n      (path pcb 0  0 0  1000 0  1000 -1000  0 -1000  0 0)\n    )\n  )\n)\n";

    #[test]
    fn the_outline_becomes_a_keepout_band_on_each_signal_layer() {
        let out = edge_clearance_keepouts(DSN, 0.3).unwrap();
        // Four open edges on each of the two signal layers, never a closed loop.
        assert_eq!(out.matches("(keepout").count(), 8, "{out}");
        assert!(out.contains("(keepout \"\" (path F.Cu 600 0 0 1000 0))"), "{out}");
        assert!(out.contains("(keepout \"\" (path B.Cu 600 0 -1000 0 0))"), "{out}");
        assert!(!out.contains("(path In1.Cu"), "planes are not routed");
        // Still parses, and the keep-outs sit inside the structure.
        let root = super::parse_dsn_sexp(&out).unwrap();
        assert_eq!(root.find("structure").unwrap().find_all("keepout").len(), 8);
        assert_eq!(edge_clearance_keepouts(DSN, 0.0).unwrap(), DSN);
    }
}
