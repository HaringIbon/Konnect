//! Board checkpoints, a semantic diff against them, and restore.
//!
//! An agent iterating on a layout needs to answer "what did that change, did
//! it help, keep it or go back?". KiCad's undo is not reachable headless and a
//! file copy says nothing about what changed. A checkpoint is the exact board
//! text (live snapshot when KiCad holds the board) kept under
//! `<project dir>/.konnect/checkpoints/<board stem>/`; the diff compares parts,
//! routing and zones by name, not by bytes.

use crate::mcp::error::ToolErrorKind;
use crate::mcp::protocol::CallToolResult;
use crate::tool;
use crate::tools::board_source::{self, BoardRead, BoardSource};
use crate::tools::{get_path, BoardAccess, ToolContext, ToolDef};
use anyhow::Context;
use konnect_sexp::SexpNode;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub(crate) fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "checkpoint_board",
            "Save the board's current state (the live snapshot when KiCad holds it, else the \
             saved file) as a named checkpoint under .konnect/checkpoints/, to diff against or \
             restore later. Returns the checkpoint id.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" },
                    "label": { "type": "string", "description": "Short name, e.g. 'before-autoroute'" },
                    "board_source": board_source::board_source_schema()
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_checkpoint(args, ctx).await }
        ),
        tool!(
            "diff_board",
            "What changed between a checkpoint (default: the latest) and the board now: parts \
             added, removed, moved, rotated or flipped; per net, routed length, segment and via \
             counts; zones. Also lists the board's checkpoints.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" },
                    "checkpoint": { "type": "string", "description": "Checkpoint id; default the latest" },
                    "board_source": board_source::board_source_schema()
                },
                "required": ["board"]
            }),
            |args, ctx| async move { handle_diff(args, ctx).await }
        ),
        tool!(
            "restore_checkpoint",
            "Write a checkpoint back to the board file. Refused while KiCad holds the board (close \
             it, or undo in KiCad); the current file is checkpointed first ('pre-restore') so the \
             restore itself can be undone. Revision-checked write.",
            json!({
                "type": "object",
                "properties": {
                    "board": { "type": "string" },
                    "checkpoint": { "type": "string" }
                },
                "required": ["board", "checkpoint"]
            }),
            |args, ctx| async move { handle_restore(args, ctx).await }
        )
        .with_board_access(BoardAccess::ClosedBoardOnly),
    ]
}

fn checkpoint_dir(board: &Path) -> PathBuf {
    let stem = board.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    board.parent().unwrap_or(Path::new(".")).join(".konnect").join("checkpoints").join(stem)
}

fn list(board: &Path) -> Vec<(String, Value)> {
    let dir = checkpoint_dir(board);
    let mut out: Vec<(String, Value)> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "json"))
                .filter_map(|p| {
                    // Only `<id>.json` is metadata: ids have no dots, and the
                    // saved `<id>.intent.json` sidecar is not a checkpoint.
                    let id = p.file_stem()?.to_string_lossy().to_string();
                    if !valid_id(&id) {
                        return None;
                    }
                    let meta: Value = serde_json::from_str(&std::fs::read_to_string(&p).ok()?).ok()?;
                    (meta["id"].as_str() == Some(id.as_str())).then_some((id, meta))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn sanitize(label: &str) -> String {
    label.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).take(40).collect()
}

/// The project files saved beside each checkpoint, by suffix: the rules and
/// intent the design tools write change as much as the board does.
const SIDECARS: [&str; 3] = ["kicad_pro", "kicad_dru", "intent.json"];

fn sidecar_path(board: &Path, suffix: &str) -> PathBuf {
    let stem = board.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    board.with_file_name(format!("{stem}.{suffix}"))
}

/// A checkpoint id names files in the checkpoint directory only.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 80 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn write_checkpoint(board: &Path, content: &str, label: &str, source: &str) -> anyhow::Result<String> {
    let dir = checkpoint_dir(board);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let stamp = chrono_like_stamp();
    let mut id = format!("{stamp}-{}", sanitize(label));
    let mut n = 1;
    while dir.join(format!("{id}.kicad_pcb")).exists() {
        n += 1;
        id = format!("{stamp}-{}-{n}", sanitize(label));
    }
    konnect_sexp::write_new_atomic(&dir.join(format!("{id}.kicad_pcb")), content)?;
    let mut sidecars = serde_json::Map::new();
    for suffix in SIDECARS {
        if let Ok(text) = std::fs::read_to_string(sidecar_path(board, suffix)) {
            konnect_sexp::write_new_atomic(&dir.join(format!("{id}.{suffix}")), &text)?;
            sidecars.insert(suffix.to_string(), json!(sha256(&text)));
        }
    }
    let meta = json!({
        "id": id,
        "label": label,
        "source": source,
        "sha256": sha256(content),
        "bytes": content.len(),
        "sidecars": sidecars,
    });
    konnect_sexp::write_new_atomic(&dir.join(format!("{id}.json")), &serde_json::to_string_pretty(&meta)?)?;
    Ok(id)
}

/// `YYYYMMDD-HHMMSS` in UTC, sortable.
fn chrono_like_stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn sha256(text: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

async fn current(args: &Value, ctx: &ToolContext, board: &Path) -> anyhow::Result<Result<(String, String), CallToolResult>> {
    let source = match BoardSource::from_args(args) {
        Ok(s) => s,
        Err(e) => return Ok(Err(e)),
    };
    Ok(match board_source::read_board(ctx, board, source, "board checkpoint", |c, d| c.save_document_to_string_in(d)).await? {
        BoardRead::Live(text) => Ok((text, "live".into())),
        BoardRead::Saved(saved) => Ok((
            std::fs::read_to_string(board).with_context(|| format!("read {}", board.display()))?,
            format!("saved file ({})", saved.detail()),
        )),
        BoardRead::Refused(result) => Err(result),
    })
}

async fn handle_checkpoint(args: &Value, ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let label = args["label"].as_str().unwrap_or("checkpoint");
    let (content, source) = match current(args, ctx, &board).await? {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let id = write_checkpoint(&board, &content, label, &source)?;
    Ok(CallToolResult::json(&json!({
        "checkpoint": id,
        "source": source,
        "path": checkpoint_dir(&board).join(format!("{id}.kicad_pcb")),
        "checkpoints": list(&board).len(),
    })))
}

// ─── Semantic diff ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
struct Part {
    reference: String,
    footprint: String,
    x: f64,
    y: f64,
    rotation: f64,
    back: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct NetRouting {
    segments: usize,
    vias: usize,
    length_mm: f64,
    /// Routed length per `layer@width`, so a reroute at another width or on
    /// another layer shows even when counts and length are unchanged.
    copper: BTreeMap<String, f64>,
}

/// Footprints keyed by their UUID (references repeat: `REF**`, `H1`
/// twice, an unannotated `R?`), falling back to the reference.
fn parts(tree: &SexpNode) -> BTreeMap<String, Part> {
    let mut out = BTreeMap::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for fp in konnect_sexp::board::footprints(tree) {
        let Some(reference) = fp
            .find_all("property")
            .into_iter()
            .find(|p| p.get(1).and_then(SexpNode::as_str) == Some("Reference"))
            .and_then(|p| p.get(2))
            .and_then(SexpNode::as_str)
        else {
            continue;
        };
        let at = fp.find("at");
        let key = match fp.find_str("uuid") {
            Some(uuid) => uuid.to_string(),
            None => {
                let n = seen.entry(reference.to_string()).or_insert(0);
                *n += 1;
                if *n == 1 { reference.to_string() } else { format!("{reference}#{n}") }
            }
        };
        out.insert(
            key,
            Part {
                reference: reference.to_string(),
                footprint: fp.get(1).and_then(SexpNode::as_str).unwrap_or("").to_string(),
                x: at.and_then(|a| a.get_f64(1)).unwrap_or(0.0),
                y: at.and_then(|a| a.get_f64(2)).unwrap_or(0.0),
                rotation: at.and_then(|a| a.get_f64(3)).unwrap_or(0.0),
                back: fp.find_str("layer") == Some("B.Cu"),
            },
        );
    }
    out
}

fn routing(tree: &SexpNode) -> BTreeMap<String, NetRouting> {
    let mut out: BTreeMap<String, NetRouting> = BTreeMap::new();
    for t in konnect_sexp::board::tracks(tree).items {
        let e = out.entry(t.net.unwrap_or_default()).or_default();
        let len = (t.x2 - t.x1).hypot(t.y2 - t.y1);
        e.segments += 1;
        e.length_mm += len;
        *e.copper.entry(format!("{}@{:.3}", t.layer, t.width)).or_insert(0.0) += len;
    }
    for arc in tree.find_all("arc") {
        let p = |tag: &str| arc.find(tag).and_then(|n| Some((n.get_f64(1)?, n.get_f64(2)?)));
        if let (Some(s), Some(m), Some(e)) = (p("start"), p("mid"), p("end")) {
            let net = arc.find("net").and_then(konnect_sexp::net::net_name).unwrap_or("").to_string();
            let pts = crate::tools::fanout::sample_arc(s, m, e, 16);
            let entry = out.entry(net).or_default();
            let len = pts.windows(2).map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1)).sum::<f64>();
            entry.segments += 1;
            entry.length_mm += len;
            let key = format!("{}@{:.3}", arc.find_str("layer").unwrap_or(""), arc.find_f64("width").unwrap_or(0.0));
            *entry.copper.entry(key).or_insert(0.0) += len;
        }
    }
    for v in konnect_sexp::board::vias(tree).items {
        out.entry(v.net.unwrap_or_default()).or_default().vias += 1;
    }
    out
}

fn zones(tree: &SexpNode) -> BTreeSet<String> {
    konnect_sexp::board::zones(tree)
        .items
        .into_iter()
        .map(|z| format!("{} on {}", z.net.unwrap_or_else(|| "(keep-out)".into()), z.layers.join(",")))
        .collect()
}

fn r3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Hashes of what the part, routing and zone summaries do not cover. Zone
/// fills are left out (a refill is not a design change).
fn other_digests(tree: &SexpNode) -> BTreeMap<&'static str, String> {
    let digest = |parts: Vec<String>| sha256(&parts.join("\n"));
    let on_edge = |n: &SexpNode| n.find_str("layer") == Some("Edge.Cuts");
    let mut edge = Vec::new();
    for node in tree.children().unwrap_or(&[]) {
        if node.head().is_some_and(|h| h.starts_with("gr_")) && on_edge(node) {
            edge.push(format!("{node:?}"));
        }
    }
    for fp in konnect_sexp::board::footprints(tree) {
        for node in fp.children().unwrap_or(&[]) {
            if node.head().is_some_and(|h| h.starts_with("fp_")) && on_edge(node) {
                edge.push(format!("{:?}|{node:?}", fp.find("at")));
            }
        }
    }
    let zones = tree
        .find_all("zone")
        .iter()
        .map(|z| {
            let kept: Vec<String> = z
                .children()
                .unwrap_or(&[])
                .iter()
                .filter(|c| !matches!(c.head(), Some("filled_polygon" | "fill_segments")))
                .map(|c| format!("{c:?}"))
                .collect();
            kept.join("|")
        })
        .collect();
    let mut pads: Vec<String> = crate::tools::fanout::copper_pads(tree)
        .into_iter()
        .map(|p| format!("{}.{}={}", p.reference, p.number, p.net.unwrap_or_default()))
        .collect();
    pads.sort();
    let setup = ["setup", "layers", "general"].iter().map(|t| format!("{:?}", tree.find(t))).collect();
    BTreeMap::from([
        ("edge_cuts", digest(edge)),
        ("zone_outlines_and_settings", digest(zones)),
        ("pad_nets", digest(pads)),
        ("board_setup_and_stackup", digest(setup)),
    ])
}

pub(crate) fn diff_trees(before: &SexpNode, after: &SexpNode) -> Value {
    let (pa, pb) = (parts(before), parts(after));
    let added: Vec<&String> = pb.iter().filter(|(k, _)| !pa.contains_key(*k)).map(|(_, p)| &p.reference).collect();
    let removed: Vec<&String> = pa.iter().filter(|(k, _)| !pb.contains_key(*k)).map(|(_, p)| &p.reference).collect();
    let mut moved = Vec::new();
    for (key, a) in &pa {
        let Some(b) = pb.get(key) else { continue };
        let distance = (b.x - a.x).hypot(b.y - a.y);
        let rotated = (b.rotation - a.rotation).rem_euclid(360.0) > 1e-6;
        if distance > 1e-4 || rotated || a.back != b.back || a.footprint != b.footprint || a.reference != b.reference {
            moved.push(json!({
                "reference": b.reference,
                "renamed_from": (a.reference != b.reference).then(|| a.reference.clone()),
                "from": { "x": a.x, "y": a.y, "rotation": a.rotation },
                "to": { "x": b.x, "y": b.y, "rotation": b.rotation },
                "distance_mm": r3(distance),
                "flipped": a.back != b.back,
                "footprint_changed": (a.footprint != b.footprint).then(|| json!({"from": a.footprint, "to": b.footprint})),
            }));
        }
    }
    let (ra, rb) = (routing(before), routing(after));
    let nets: BTreeSet<&String> = ra.keys().chain(rb.keys()).collect();
    let mut routed = Vec::new();
    for net in nets {
        let a = ra.get(net).cloned().unwrap_or_default();
        let b = rb.get(net).cloned().unwrap_or_default();
        let keys: BTreeSet<&String> = a.copper.keys().chain(b.copper.keys()).collect();
        let copper_changed = keys.iter().any(|k| (a.copper.get(*k).unwrap_or(&0.0) - b.copper.get(*k).unwrap_or(&0.0)).abs() > 1e-3);
        if a.segments != b.segments || a.vias != b.vias || (a.length_mm - b.length_mm).abs() > 1e-3 || copper_changed {
            let per = |r: &NetRouting| r.copper.iter().map(|(k, v)| (k.clone(), json!(r3(*v)))).collect::<serde_json::Map<_, _>>();
            routed.push(json!({
                "net": net,
                "segments": [a.segments, b.segments],
                "vias": [a.vias, b.vias],
                "length_mm": [r3(a.length_mm), r3(b.length_mm)],
                "length_by_layer_and_width": [per(&a), per(&b)],
            }));
        }
    }
    let (za, zb) = (zones(before), zones(after));
    let total = |r: &BTreeMap<String, NetRouting>| {
        (r.values().map(|n| n.segments).sum::<usize>(), r.values().map(|n| n.vias).sum::<usize>(), r3(r.values().map(|n| n.length_mm).sum::<f64>()))
    };
    let (ta, tb) = (total(&ra), total(&rb));
    let (da, db) = (other_digests(before), other_digests(after));
    let other: Vec<&str> = da.iter().filter(|(k, v)| db.get(*k) != Some(*v)).map(|(k, _)| *k).collect();
    json!({
        "summary": {
            "parts_added": added.len(),
            "parts_removed": removed.len(),
            "parts_moved": moved.len(),
            "nets_with_routing_changes": routed.len(),
            "segments": [ta.0, tb.0],
            "vias": [ta.1, tb.1],
            "routed_length_mm": [ta.2, tb.2],
            "zones_added": zb.difference(&za).count(),
            "zones_removed": za.difference(&zb).count(),
            "other_changes": other.len(),
        },
        "other_changes": other,
        "parts": { "added": added, "removed": removed, "moved": moved },
        "routing": routed,
        "zones": { "added": zb.difference(&za).collect::<Vec<_>>(), "removed": za.difference(&zb).collect::<Vec<_>>() },
        "note": "pairs are [checkpoint, now]",
    })
}

async fn handle_diff(args: &Value, ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let checkpoints = list(&board);
    let wanted = args["checkpoint"].as_str();
    let Some((id, meta)) = (match wanted {
        Some(id) => checkpoints.iter().find(|(i, _)| i == id).cloned(),
        None => checkpoints.last().cloned(),
    }) else {
        return Ok(CallToolResult::error_kind(
            ToolErrorKind::InvalidArgument { field: "checkpoint".into(), reason: "no such checkpoint; call checkpoint_board first".into() },
            "No such checkpoint for this board",
        ));
    };
    let (now, source) = match current(args, ctx, &board).await? {
        Ok(v) => v,
        Err(e) => return Ok(e),
    };
    let before_text = std::fs::read_to_string(checkpoint_dir(&board).join(format!("{id}.kicad_pcb")))?;
    let before = konnect_sexp::parse_sexp(&before_text).context("parse checkpoint")?;
    let after = konnect_sexp::parse_sexp(&now).context("parse board")?;
    let mut body = diff_trees(&before, &after);
    body["checkpoint"] = json!(id);
    body["checkpoint_label"] = meta["label"].clone();
    body["source"] = json!(source);
    body["identical"] = json!(sha256(&now) == meta["sha256"].as_str().unwrap_or(""));
    let mut files = serde_json::Map::new();
    for suffix in SIDECARS {
        let then = meta["sidecars"][suffix].as_str();
        let current = std::fs::read_to_string(sidecar_path(&board, suffix)).ok().map(|t| sha256(&t));
        let state = match (then, current.as_deref()) {
            (None, None) => continue,
            (Some(a), Some(b)) if a == b => "unchanged",
            (Some(_), Some(_)) => "changed",
            (None, Some(_)) => if meta.get("sidecars").is_some() { "added" } else { "not in checkpoint" },
            (Some(_), None) => "removed",
        };
        files.insert(suffix.to_string(), json!(state));
    }
    body["project_files"] = Value::Object(files);
    body["checkpoints"] = json!(checkpoints.iter().map(|(i, m)| json!({"id": i, "label": m["label"]})).collect::<Vec<_>>());
    Ok(CallToolResult::json(&body))
}

async fn handle_restore(args: &Value, ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let id = args["checkpoint"].as_str().unwrap_or_default().to_string();
    let refuse = |reason: String| {
        CallToolResult::error_kind(
            ToolErrorKind::InvalidArgument { field: "checkpoint".into(), reason: reason.clone() },
            format!("checkpoint '{id}': {reason}"),
        )
    };
    if !valid_id(&id) {
        return Ok(refuse("not a checkpoint id (letters, digits, '-' and '_' only)".into()));
    }
    let dir = checkpoint_dir(&board);
    let Ok(restored) = std::fs::read_to_string(dir.join(format!("{id}.kicad_pcb"))) else {
        return Ok(refuse("no such checkpoint for this board".into()));
    };
    let meta: Value = std::fs::read_to_string(dir.join(format!("{id}.json")))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    // The checkpoint must be the one recorded, and a board KiCad can read.
    if meta["sha256"].as_str() != Some(sha256(&restored).as_str()) {
        return Ok(refuse("its content does not match the recorded sha256; it was changed or truncated".into()));
    }
    if !konnect_sexp::parse_sexp(&restored).map(|t| t.head() == Some("kicad_pcb")).unwrap_or(false) {
        return Ok(refuse("it does not parse as a KiCad board".into()));
    }
    let mut sidecars: Vec<(PathBuf, String)> = Vec::new();
    for suffix in SIDECARS {
        let Some(recorded) = meta["sidecars"][suffix].as_str() else { continue };
        let text = std::fs::read_to_string(dir.join(format!("{id}.{suffix}")))
            .with_context(|| format!("read the checkpoint's {suffix}"))?;
        if sha256(&text) != recorded {
            return Ok(refuse(format!("its {suffix} does not match the recorded sha256")));
        }
        sidecars.push((sidecar_path(&board, suffix), text));
    }
    if let Some(refusal) = crate::tools::pcb_board::refuse_if_board_open_in_kicad(ctx, &board, "checkpoint restore").await? {
        return Ok(refusal);
    }
    if let Some(refusal) = crate::tools::pcb_board::refuse_if_project_locked_by_kicad(&sidecar_path(&board, "kicad_pro"), "checkpoint restore") {
        return Ok(refusal);
    }
    let current = konnect_sexp::writer::read_consistent(&board)?;
    let safety = write_checkpoint(&board, &current, "pre-restore", "saved file")?;
    // Files already equal to the checkpoint are left alone: the journal
    // refuses a no-op transition, and there is nothing to write.
    let mut transitions = Vec::new();
    let mut restored_files = Vec::new();
    if current != restored {
        transitions.push(konnect_sexp::transaction::FileTransition::replace(&board, current, restored));
        restored_files.push(board.display().to_string());
    }
    for (path, text) in sidecars {
        match std::fs::read_to_string(&path) {
            Ok(now) if now == text => continue,
            Ok(now) => transitions.push(konnect_sexp::transaction::FileTransition::replace(&path, now, text)),
            Err(_) => transitions.push(konnect_sexp::transaction::FileTransition::create(&path, text)),
        }
        restored_files.push(path.display().to_string());
    }
    if transitions.is_empty() {
        return Ok(CallToolResult::json(&json!({
            "restored": id,
            "files": [],
            "previous_state_checkpoint": safety,
            "note": "the board and its project files already match this checkpoint",
        })));
    }
    let journal_dir = board.parent().unwrap_or(Path::new("."));
    match konnect_sexp::transaction::try_commit_file_transaction(journal_dir, transitions) {
        Ok(_) => Ok(CallToolResult::json(&json!({
            "restored": id,
            "files": restored_files,
            "previous_state_checkpoint": safety,
            "note": if meta.get("sidecars").is_some() { "board, project, rules and intent restored together" } else { "an older checkpoint: only the board was saved, so only the board was restored" },
        }))),
        Err(failure) if failure.journal.is_none() => Ok(CallToolResult::error_kind(
            ToolErrorKind::Conflict { paths: restored_files },
            format!("Nothing was written: {}", failure.error),
        )),
        Err(failure) => Err(anyhow::anyhow!(
            "restore failed after its journal reached disk ({}); recovery will complete or roll it back: {:?}",
            failure.error,
            failure.journal
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_names_moved_parts_and_routing_changes() {
        let before = konnect_sexp::parse_sexp(
            "(kicad_pcb
              (footprint \"C\" (layer \"F.Cu\") (at 10 10) (property \"Reference\" \"C1\"))
              (footprint \"R\" (layer \"F.Cu\") (at 20 10) (property \"Reference\" \"R1\"))
              (segment (start 0 0) (end 10 0) (width 0.2) (layer \"F.Cu\") (net \"A\")))",
        )
        .unwrap();
        let after = konnect_sexp::parse_sexp(
            "(kicad_pcb
              (footprint \"C\" (layer \"F.Cu\") (at 12 10 90) (property \"Reference\" \"C1\"))
              (footprint \"U\" (layer \"F.Cu\") (at 30 10) (property \"Reference\" \"U1\"))
              (segment (start 0 0) (end 10 0) (width 0.2) (layer \"F.Cu\") (net \"A\"))
              (segment (start 10 0) (end 10 5) (width 0.2) (layer \"F.Cu\") (net \"A\"))
              (via (at 10 5) (size 0.6) (drill 0.3) (layers \"F.Cu\" \"B.Cu\") (net \"A\")))",
        )
        .unwrap();
        let d = diff_trees(&before, &after);
        assert_eq!(d["parts"]["added"], json!(["U1"]));
        assert_eq!(d["parts"]["removed"], json!(["R1"]));
        assert_eq!(d["parts"]["moved"][0]["reference"], "C1");
        assert_eq!(d["parts"]["moved"][0]["distance_mm"], 2.0);
        assert_eq!(d["routing"][0]["net"], "A");
        assert_eq!(d["routing"][0]["segments"], json!([1, 2]));
        assert_eq!(d["routing"][0]["vias"], json!([0, 1]));
        assert_eq!(d["routing"][0]["length_mm"], json!([10.0, 15.0]));
    }

    #[test]
    fn stamps_sort_and_look_like_dates() {
        let s = chrono_like_stamp();
        assert_eq!(s.len(), 15, "{s}");
        assert!(s.starts_with("20"), "{s}");
    }

    #[tokio::test]
    async fn checkpoint_diff_and_restore_round_trip_on_a_closed_board() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("b.kicad_pcb");
        let original = "(kicad_pcb (footprint \"C\" (layer \"F.Cu\") (at 10 10) (property \"Reference\" \"C1\")))\n";
        std::fs::write(&board, original).unwrap();
        let ctx = crate::tools::pcb_board::board_mock::ctx_talking_to(String::new());
        let args = json!({ "board": board, "label": "start" });
        let made = handle_checkpoint(&args, &ctx).await.unwrap();
        assert!(!made.is_error, "{made:?}");
        std::fs::write(&board, original.replace("(at 10 10)", "(at 15 10)")).unwrap();
        let diff = handle_diff(&json!({ "board": board }), &ctx).await.unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &diff.content[0] else { panic!() };
        let body: Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["summary"]["parts_moved"], 1, "{body}");
        let id = body["checkpoint"].as_str().unwrap().to_string();
        let restored = handle_restore(&json!({ "board": board, "checkpoint": id }), &ctx).await.unwrap();
        assert!(!restored.is_error, "{restored:?}");
        assert_eq!(std::fs::read_to_string(&board).unwrap(), original);
        // The pre-restore state was kept, so the restore can be undone.
        assert_eq!(list(&board).len(), 2);
    }

    #[test]
    fn diff_sees_width_layer_outline_and_duplicate_references() {
        let before = konnect_sexp::parse_sexp(
            "(kicad_pcb
              (gr_rect (start 0 0) (end 50 50) (layer \"Edge.Cuts\"))
              (footprint \"H\" (layer \"F.Cu\") (uuid \"a\") (at 5 5) (property \"Reference\" \"H1\"))
              (footprint \"H\" (layer \"F.Cu\") (uuid \"b\") (at 45 5) (property \"Reference\" \"H1\"))
              (segment (start 0 0) (end 10 0) (width 0.2) (layer \"F.Cu\") (net \"A\")))",
        )
        .unwrap();
        let after = konnect_sexp::parse_sexp(
            "(kicad_pcb
              (gr_rect (start 0 0) (end 60 50) (layer \"Edge.Cuts\"))
              (footprint \"H\" (layer \"F.Cu\") (uuid \"a\") (at 5 5) (property \"Reference\" \"H1\"))
              (footprint \"H\" (layer \"F.Cu\") (uuid \"b\") (at 55 5) (property \"Reference\" \"H1\"))
              (segment (start 0 0) (end 10 0) (width 0.4) (layer \"B.Cu\") (net \"A\")))",
        )
        .unwrap();
        let d = diff_trees(&before, &after);
        // The second H1 moved; with reference keys it collapsed into the first.
        assert_eq!(d["summary"]["parts_moved"], 1, "{d}");
        assert_eq!(d["parts"]["moved"][0]["distance_mm"], 10.0);
        // Same count and length, other width and layer.
        assert_eq!(d["routing"][0]["net"], "A", "{d}");
        assert_eq!(d["other_changes"], json!(["edge_cuts"]));
        assert_eq!(diff_trees(&before, &before)["summary"]["other_changes"], 0);
        assert_eq!(diff_trees(&before, &before)["routing"], json!([]));
    }

    #[tokio::test]
    async fn restore_refuses_tampered_or_escaping_checkpoints_and_restores_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let board = dir.path().join("b.kicad_pcb");
        let dru = dir.path().join("b.kicad_dru");
        let intent = dir.path().join("b.intent.json");
        std::fs::write(&board, "(kicad_pcb (version 1))\n").unwrap();
        std::fs::write(&dru, "(version 1)\n").unwrap();
        std::fs::write(&intent, "{\"schema\": 1}\n").unwrap();
        let ctx = crate::tools::pcb_board::board_mock::ctx_talking_to(String::new());
        let made = handle_checkpoint(&json!({ "board": board, "label": "x" }), &ctx).await.unwrap();
        let crate::mcp::protocol::ToolContent::Text { text } = &made.content[0] else { panic!() };
        let id = serde_json::from_str::<Value>(text).unwrap()["checkpoint"].as_str().unwrap().to_string();
        std::fs::write(&dru, "(version 1)\n(rule \"later\" (constraint clearance (min 1mm)))\n").unwrap();
        std::fs::write(&board, "(kicad_pcb (version 2))\n").unwrap();
        // The intent sidecar must not be mistaken for a checkpoint.
        assert_eq!(list(&board).len(), 1);
        let diff = handle_diff(&json!({ "board": board }), &ctx).await.unwrap();
        assert!(!diff.is_error, "{diff:?}");

        for bad in ["../../etc/passwd", "a/b", ""] {
            let r = handle_restore(&json!({ "board": board, "checkpoint": bad }), &ctx).await.unwrap();
            assert!(r.is_error, "{bad}");
        }
        let stored = checkpoint_dir(&board).join(format!("{id}.kicad_pcb"));
        let original = std::fs::read_to_string(&stored).unwrap();
        std::fs::write(&stored, "(kicad_pcb (version 9))\n").unwrap();
        assert!(handle_restore(&json!({ "board": board, "checkpoint": id }), &ctx).await.unwrap().is_error, "hash mismatch");
        std::fs::write(&stored, original).unwrap();

        let r = handle_restore(&json!({ "board": board, "checkpoint": id }), &ctx).await.unwrap();
        assert!(!r.is_error, "{r:?}");
        assert_eq!(std::fs::read_to_string(&board).unwrap(), "(kicad_pcb (version 1))\n");
        assert_eq!(std::fs::read_to_string(&dru).unwrap(), "(version 1)\n", "rules restored with the board");
    }
}
