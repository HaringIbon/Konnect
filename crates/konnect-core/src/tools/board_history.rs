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
                    let meta: Value = serde_json::from_str(&std::fs::read_to_string(&p).ok()?).ok()?;
                    Some((p.file_stem()?.to_string_lossy().to_string(), meta))
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
    let meta = json!({
        "id": id,
        "label": label,
        "source": source,
        "sha256": sha256(content),
        "bytes": content.len(),
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
}

fn parts(tree: &SexpNode) -> BTreeMap<String, Part> {
    let mut out = BTreeMap::new();
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
        out.insert(
            reference.to_string(),
            Part {
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
        e.segments += 1;
        e.length_mm += (t.x2 - t.x1).hypot(t.y2 - t.y1);
    }
    for arc in tree.find_all("arc") {
        let p = |tag: &str| arc.find(tag).and_then(|n| Some((n.get_f64(1)?, n.get_f64(2)?)));
        if let (Some(s), Some(m), Some(e)) = (p("start"), p("mid"), p("end")) {
            let net = arc.find("net").and_then(konnect_sexp::net::net_name).unwrap_or("").to_string();
            let pts = crate::tools::fanout::sample_arc(s, m, e, 16);
            let entry = out.entry(net).or_default();
            entry.segments += 1;
            entry.length_mm += pts.windows(2).map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1)).sum::<f64>();
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

pub(crate) fn diff_trees(before: &SexpNode, after: &SexpNode) -> Value {
    let (pa, pb) = (parts(before), parts(after));
    let added: Vec<&String> = pb.keys().filter(|k| !pa.contains_key(*k)).collect();
    let removed: Vec<&String> = pa.keys().filter(|k| !pb.contains_key(*k)).collect();
    let mut moved = Vec::new();
    for (reference, a) in &pa {
        let Some(b) = pb.get(reference) else { continue };
        let distance = (b.x - a.x).hypot(b.y - a.y);
        let rotated = (b.rotation - a.rotation).rem_euclid(360.0) > 1e-6;
        if distance > 1e-4 || rotated || a.back != b.back || a.footprint != b.footprint {
            moved.push(json!({
                "reference": reference,
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
        if a.segments != b.segments || a.vias != b.vias || (a.length_mm - b.length_mm).abs() > 1e-3 {
            routed.push(json!({
                "net": net,
                "segments": [a.segments, b.segments],
                "vias": [a.vias, b.vias],
                "length_mm": [r3(a.length_mm), r3(b.length_mm)],
            }));
        }
    }
    let (za, zb) = (zones(before), zones(after));
    let total = |r: &BTreeMap<String, NetRouting>| {
        (r.values().map(|n| n.segments).sum::<usize>(), r.values().map(|n| n.vias).sum::<usize>(), r3(r.values().map(|n| n.length_mm).sum::<f64>()))
    };
    let (ta, tb) = (total(&ra), total(&rb));
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
        },
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
    body["checkpoints"] = json!(checkpoints.iter().map(|(i, m)| json!({"id": i, "label": m["label"]})).collect::<Vec<_>>());
    Ok(CallToolResult::json(&body))
}

async fn handle_restore(args: &Value, ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let board = get_path(args, "board")?;
    let id = args["checkpoint"].as_str().unwrap_or_default().to_string();
    let path = checkpoint_dir(&board).join(format!("{id}.kicad_pcb"));
    let Ok(restored) = std::fs::read_to_string(&path) else {
        return Ok(CallToolResult::error_kind(
            ToolErrorKind::InvalidArgument { field: "checkpoint".into(), reason: format!("no checkpoint '{id}'") },
            format!("No checkpoint '{id}' for this board"),
        ));
    };
    if let Some(refusal) = crate::tools::pcb_board::refuse_if_board_open_in_kicad(ctx, &board, "checkpoint restore").await? {
        return Ok(refusal);
    }
    let current = konnect_sexp::writer::read_consistent(&board)?;
    let safety = write_checkpoint(&board, &current, "pre-restore", "saved file")?;
    match konnect_sexp::write_atomic_if_unchanged(&board, &current, &restored) {
        Ok(()) => Ok(CallToolResult::json(&json!({
            "restored": id,
            "board": board,
            "previous_state_checkpoint": safety,
        }))),
        Err(konnect_sexp::SexpError::Conflict { .. }) => Ok(CallToolResult::error_kind(
            ToolErrorKind::Conflict { paths: vec![board.display().to_string()] },
            "The board file changed while restoring; nothing was written. Retry.",
        )),
        Err(e) => Err(e.into()),
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
}
