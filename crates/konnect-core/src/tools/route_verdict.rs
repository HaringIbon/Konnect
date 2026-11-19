//! Per-net verdicts for a Specctra SES import (`apply_specctra_ses` with
//! `accept: "per_net"`).
//!
//! Freerouting 2.3.0 ends every job `COMPLETED` even with connections open
//! (FORK_CHANGES Pass 15), so whether a net is routed is never taken from the
//! router. It is read from KiCad's own DRC of the board with the routes on it:
//! the `unconnected_items` KiCad reports per net, and the DRC errors the routes
//! added, by identity (rule + item KIIDs). KiCad's own SES import is all or
//! nothing: it deletes every unlocked track and re-creates the session
//! (`kicad-10-0-8-specctra-import`, `ImportSpecctraSession`), so there is
//! nothing to reuse there for a per-net decision.
//!
//! Everything here is pure: the live IPC flow lives in `pcb_routing`.

use super::cli::{DrcReport, DrcViolation, ReportItem};
use konnect_sexp::SexpNode;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Every board item that carries both a `(uuid …)` and a `(net …)`: pads,
/// tracks, arcs, vias and zones, by KIID.
pub(crate) fn item_nets(board: &SexpNode) -> HashMap<String, String> {
    let mut out = HashMap::new();
    collect_item_nets(board, &mut out);
    out
}

fn collect_item_nets(node: &SexpNode, out: &mut HashMap<String, String>) {
    let Some(children) = node.children() else { return };
    if let (Some(uuid), Some(net)) = (
        node.find_str("uuid"),
        node.find("net").and_then(konnect_sexp::net::net_name),
    ) {
        if !net.is_empty() {
            out.insert(uuid.to_string(), net.to_string());
        }
    }
    for child in children {
        collect_item_nets(child, out);
    }
}

/// The net a DRC report item is on: by its KIID, else the `[NET]` KiCad
/// writes into every item description ("Pad 1 [+5V] of C10 on F.Cu").
pub(crate) fn report_item_net(item: &ReportItem, nets: &HashMap<String, String>) -> Option<String> {
    if let Some(net) = item.uuid.as_ref().and_then(|uuid| nets.get(uuid)) {
        return Some(net.clone());
    }
    let open = item.description.find('[')?;
    let close = open + item.description[open..].find(']')?;
    let net = &item.description[open + 1..close];
    (!net.is_empty() && net != "<no net>").then(|| net.to_string())
}

/// KiCad's unconnected items (ratsnest edges still open) per net.
pub(crate) fn unconnected_by_net(report: &DrcReport, nets: &HashMap<String, String>) -> Option<BTreeMap<String, usize>> {
    let items = report.unconnected_items.as_ref()?;
    let mut out = BTreeMap::new();
    for violation in items {
        let net = violation
            .items
            .iter()
            .find_map(|item| report_item_net(item, nets))
            .unwrap_or_else(|| "<unknown>".to_string());
        *out.entry(net).or_insert(0) += 1;
    }
    Some(out)
}

/// A DRC error as the verdict reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct ErrorBrief {
    pub rule: String,
    pub description: String,
}

impl From<&DrcViolation> for ErrorBrief {
    fn from(violation: &DrcViolation) -> Self {
        ErrorBrief { rule: violation.rule.clone(), description: violation.description.clone() }
    }
}

/// New DRC errors, each blamed on the imported nets it involves.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Attribution {
    pub by_net: BTreeMap<String, Vec<ErrorBrief>>,
    /// New errors that touch no imported item and no net being imported (a
    /// zone refill that changed elsewhere): no net can be blamed, so the
    /// import cannot be kept net by net.
    pub unattributed: Vec<ErrorBrief>,
}

/// Blame each new error on the nets of the imported items it names (`created`:
/// KIID → net). An error naming no imported item is blamed on any net of
/// `importing` its items are on (a group net's zone refilled around the new
/// copper); failing that it is unattributed.
pub(crate) fn attribute_errors(
    added: &[&DrcViolation],
    created: &HashMap<String, String>,
    importing: &BTreeSet<String>,
    nets: &HashMap<String, String>,
) -> Attribution {
    let mut out = Attribution::default();
    for violation in added {
        let mut blamed: BTreeSet<String> = violation
            .items
            .iter()
            .filter_map(|item| item.uuid.as_ref().and_then(|uuid| created.get(uuid)))
            .cloned()
            .collect();
        if blamed.is_empty() {
            blamed = violation
                .items
                .iter()
                .filter_map(|item| report_item_net(item, nets))
                .filter(|net| importing.contains(net))
                .collect();
        }
        if blamed.is_empty() {
            // Last resort: a rule-level error KiCad reports without the net in
            // its items, such as a pair's skew ("rule 'intent: pair
            // /USB_D_N//USB_D_P'"), names the net in its description.
            blamed = importing
                .iter()
                .filter(|net| mentions_net(&violation.description, net))
                .cloned()
                .collect();
        }
        if blamed.is_empty() {
            out.unattributed.push(ErrorBrief::from(*violation));
        }
        for net in blamed {
            out.by_net.entry(net).or_default().push(ErrorBrief::from(*violation));
        }
    }
    out
}

/// Whether `text` names `net` as a whole name: not as part of a longer one
/// (`/PA1` is not in `/PA10`, `VBUS` is not in `/VBUS_F`).
fn mentions_net(text: &str, net: &str) -> bool {
    if net.is_empty() {
        return false;
    }
    let name_char = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(net).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + net.len()..].chars().next();
        // A name that starts or ends with a separator ('/x') bounds itself.
        let starts_open = net.starts_with(|c: char| !name_char(c));
        let ends_open = net.ends_with(|c: char| !name_char(c));
        (starts_open || before.is_none_or(|c| !name_char(c)))
            && (ends_open || after.is_none_or(|c| !name_char(c)))
    })
}

/// Why a net's routes were not kept.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Rejection {
    /// The routes add DRC errors the board did not have.
    DrcErrors,
    /// The net has more unconnected items with its routes than without.
    MoreUnconnected,
    /// The routes join nothing that was not already joined.
    NoImprovement,
    /// `require_complete` and the net is still not fully connected.
    Incomplete,
}

impl Rejection {
    pub(crate) fn reason(&self, before: usize, after: usize) -> String {
        match self {
            Rejection::DrcErrors => "its routes add DRC errors (see new_drc_errors)".to_string(),
            Rejection::MoreUnconnected => format!("its routes leave {after} unconnected items where there were {before}"),
            Rejection::NoImprovement => format!("its routes join nothing new: still {after} unconnected"),
            Rejection::Incomplete => format!("require_complete, and it still has {after} unconnected items"),
        }
    }
}

/// The verdict on one net in one round.
pub(crate) fn judge_net(
    errors: usize,
    unconnected_before: usize,
    unconnected_after: usize,
    require_complete: bool,
) -> Option<Rejection> {
    if errors > 0 {
        Some(Rejection::DrcErrors)
    } else if unconnected_after > unconnected_before {
        Some(Rejection::MoreUnconnected)
    } else if require_complete && unconnected_after > 0 {
        Some(Rejection::Incomplete)
    } else if unconnected_after > 0 && unconnected_after == unconnected_before {
        Some(Rejection::NoImprovement)
    } else {
        None
    }
}

/// Nets outside `importing` with more unconnected items after the import than
/// before: `(net, before, after)`. Routes can do that only by cutting another
/// net's zone once it refills; which imported net did it is not recorded.
pub(crate) fn other_nets_worse(
    before: &BTreeMap<String, usize>,
    after: &BTreeMap<String, usize>,
    importing: &BTreeSet<String>,
) -> Vec<(String, usize, usize)> {
    after
        .iter()
        .filter(|(net, _)| !importing.contains(*net))
        .filter_map(|(net, &now)| {
            let was = before.get(net).copied().unwrap_or(0);
            (now > was).then(|| (net.clone(), was, now))
        })
        .collect()
}

/// Total routed copper per net on a board: track length (mm) and via count.
pub(crate) fn copper_by_net(board: &SexpNode) -> BTreeMap<String, (f64, usize)> {
    let mut out: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    for track in konnect_sexp::board::tracks(board).items {
        if let Some(net) = track.net {
            out.entry(net).or_default().0 += (track.x2 - track.x1).hypot(track.y2 - track.y1);
        }
    }
    for via in konnect_sexp::board::vias(board).items {
        if let Some(net) = via.net {
            out.entry(net).or_default().1 += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::cli::ReportPos;

    fn item(description: &str, uuid: Option<&str>) -> ReportItem {
        ReportItem {
            description: description.to_string(),
            pos: Some(ReportPos { x: 0.0, y: 0.0 }),
            uuid: uuid.map(str::to_string),
            kicad_reported_pos: None,
            ownership_status: None,
            item_kind: None,
            layer: None,
            owner: None,
        }
    }

    fn violation(rule: &str, items: Vec<ReportItem>) -> DrcViolation {
        DrcViolation {
            severity: "error".to_string(),
            description: format!("{rule} violation"),
            rule: rule.to_string(),
            pos: None,
            items,
        }
    }

    const BOARD: &str = r#"(kicad_pcb
      (footprint "R" (uuid "fp1")
        (pad "1" smd rect (at 0 0) (size 1 1) (layers "F.Cu") (net "SDA") (uuid "pad-sda"))
        (pad "2" smd rect (at 2 0) (size 1 1) (layers "F.Cu") (net "GND") (uuid "pad-gnd")))
      (segment (start 0 0) (end 3 4) (width 0.2) (layer "F.Cu") (net "SDA") (uuid "seg1"))
      (segment (start 3 4) (end 3 5) (width 0.2) (layer "F.Cu") (net "SDA") (uuid "seg2"))
      (via (at 3 5) (size 0.6) (drill 0.3) (layers "F.Cu" "B.Cu") (net "SDA") (uuid "via1"))
      (zone (net "GND") (net_name "GND") (layer "In1.Cu") (uuid "zone1")))"#;

    #[test]
    fn items_map_to_their_nets_and_copper_is_summed_per_net() {
        let board = konnect_sexp::parse_sexp(BOARD).unwrap();
        let nets = item_nets(&board);
        assert_eq!(nets["pad-sda"], "SDA");
        assert_eq!(nets["pad-gnd"], "GND");
        assert_eq!(nets["seg2"], "SDA");
        assert_eq!(nets["via1"], "SDA");
        assert_eq!(nets["zone1"], "GND");
        assert!(!nets.contains_key("fp1"), "a footprint has no net of its own");
        let copper = copper_by_net(&board);
        let (length, vias) = copper["SDA"];
        assert!((length - 6.0).abs() < 1e-9, "{length}");
        assert_eq!(vias, 1);
        assert!(!copper.contains_key("GND"));
    }

    #[test]
    fn an_item_net_comes_from_its_kiid_else_from_its_description() {
        let nets = HashMap::from([("seg1".to_string(), "SDA".to_string())]);
        assert_eq!(report_item_net(&item("Track [X] on F.Cu", Some("seg1")), &nets).as_deref(), Some("SDA"));
        assert_eq!(report_item_net(&item("Pad 1 [+5V] of C10 on F.Cu", Some("other")), &nets).as_deref(), Some("+5V"));
        assert_eq!(report_item_net(&item("Board edge", None), &nets), None);
        assert_eq!(report_item_net(&item("Pad 3 [<no net>] of U1", None), &nets), None);
    }

    #[test]
    fn unconnected_items_are_counted_per_net() {
        let report = DrcReport {
            violations: vec![],
            unconnected_items: Some(vec![
                violation("unconnected_items", vec![item("Pad 1 [SDA] of R1", None), item("Pad 2 [SDA] of R2", None)]),
                violation("unconnected_items", vec![item("Pad 1 [SDA] of R3", None), item("Pad 2 [SDA] of R2", None)]),
                violation("unconnected_items", vec![item("Pad 1 [GND] of C1", None), item("Via [GND]", None)]),
            ]),
            schematic_parity: None,
            ownership_diagnostic: None,
            schematic_parity_diagnostic: None,
        };
        let counts = unconnected_by_net(&report, &HashMap::new()).unwrap();
        assert_eq!(counts, BTreeMap::from([("GND".to_string(), 1), ("SDA".to_string(), 2)]));
        let unknown = DrcReport { unconnected_items: None, ..report };
        assert!(unconnected_by_net(&unknown, &HashMap::new()).is_none(), "not reported is not zero");
    }

    #[test]
    fn errors_are_blamed_on_the_imported_nets_they_involve() {
        let created = HashMap::from([("new-a".to_string(), "A".to_string()), ("new-b".to_string(), "B".to_string())]);
        let importing: BTreeSet<String> = ["A", "B", "C"].iter().map(|s| s.to_string()).collect();
        let nets = HashMap::from([("zone-c".to_string(), "C".to_string())]);
        let short = violation("shorting_items", vec![item("Track [A]", Some("new-a")), item("Pad 1 [GND] of U1", Some("p"))]);
        let crossing = violation("tracks_crossing", vec![item("Track [A]", Some("new-a")), item("Track [B]", Some("new-b"))]);
        let refill = violation("clearance", vec![item("Zone [C]", Some("zone-c")), item("Pad 2 [GND] of U1", None)]);
        let elsewhere = violation("starved_thermal", vec![item("Pad 3 [GND] of U1", None)]);
        let added = [&short, &crossing, &refill, &elsewhere];
        let blame = attribute_errors(&added, &created, &importing, &nets);
        assert_eq!(blame.by_net["A"].len(), 2);
        assert_eq!(blame.by_net["B"].len(), 1, "a crossing between two imported nets blames both");
        assert_eq!(blame.by_net["C"][0].rule, "clearance");
        assert_eq!(blame.unattributed.len(), 1);
        assert_eq!(blame.unattributed[0].rule, "starved_thermal");
    }

    #[test]
    fn a_rule_error_without_net_items_is_blamed_on_the_nets_it_names() {
        let importing: BTreeSet<String> = ["/USB_D_N", "/USB_D_P", "/PA1", "VBUS"].iter().map(|s| s.to_string()).collect();
        let mut skew = violation("skew_out_of_range", vec![item("Board", None)]);
        skew.description = "Skew between tracks out of range (rule 'intent: pair /USB_D_N//USB_D_P' max skew 1.27 mm)".into();
        let mut other = violation("length_out_of_range", vec![]);
        other.description = "Length out of range (rule 'intent: length /PA10' max 5 mm) on /VBUS_F".into();
        let blame = attribute_errors(&[&skew, &other], &HashMap::new(), &importing, &HashMap::new());
        assert_eq!(blame.by_net.keys().cloned().collect::<Vec<_>>(), vec!["/USB_D_N", "/USB_D_P"]);
        assert_eq!(blame.unattributed.len(), 1, "/PA10 is not /PA1 and /VBUS_F is not VBUS");
        assert!(mentions_net("net VBUS.", "VBUS") && !mentions_net("VBUS2", "VBUS"));
    }

    #[test]
    fn a_net_is_kept_only_when_it_adds_no_error_and_joins_something() {
        assert_eq!(judge_net(0, 3, 0, false), None, "completed");
        assert_eq!(judge_net(0, 3, 1, false), None, "partial progress is kept");
        assert_eq!(judge_net(0, 3, 1, true), Some(Rejection::Incomplete));
        assert_eq!(judge_net(1, 3, 0, false), Some(Rejection::DrcErrors));
        assert_eq!(judge_net(0, 3, 3, false), Some(Rejection::NoImprovement));
        assert_eq!(judge_net(0, 1, 2, false), Some(Rejection::MoreUnconnected));
        assert_eq!(judge_net(0, 0, 0, false), None, "already complete and still complete");
        assert!(Rejection::NoImprovement.reason(3, 3).contains("still 3"));
    }

    #[test]
    fn only_nets_outside_the_import_that_got_worse_are_reported() {
        let before = BTreeMap::from([("GND".to_string(), 1), ("A".to_string(), 4)]);
        let after = BTreeMap::from([("GND".to_string(), 3), ("A".to_string(), 5), ("+3V3".to_string(), 1)]);
        let importing: BTreeSet<String> = ["A".to_string()].into();
        assert_eq!(
            other_nets_worse(&before, &after, &importing),
            vec![("+3V3".to_string(), 0, 1), ("GND".to_string(), 1, 3)]
        );
    }
}
