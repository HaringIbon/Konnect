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

// ─── The decision loop ───────────────────────────────────────────────────────

/// What one trial DRC found: the board with the routes of a set of nets on it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TrialResult {
    pub blame: Attribution,
    /// Unconnected items per net on that board.
    pub unconnected: BTreeMap<String, usize>,
}

/// A DRC of the board with only some nets' routes on it. The live
/// implementation removes the other nets' imported items from the imported
/// board's text by KIID and runs kicad-cli on it, so trials touch neither the
/// live board nor its undo history.
pub(crate) trait Trial {
    fn run(&mut self, nets: &BTreeSet<String>) -> anyhow::Result<TrialResult>;
}

/// Why a net's routes were not kept, with the evidence.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NetRejection {
    pub reason: String,
    pub errors: Vec<ErrorBrief>,
    /// Unconnected items on the net in the trial that rejected it.
    pub unconnected_after: Option<usize>,
    /// `judged` (its own evidence), `group` (a pair or skew-group partner
    /// failed) or `bisection` (it alone brings an error no net could be
    /// blamed for, or cuts another net).
    pub by: &'static str,
}

/// The outcome of [`decide`].
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Decision {
    pub accepted: BTreeSet<String>,
    pub rejected: BTreeMap<String, NetRejection>,
    pub rounds: usize,
    pub trials: usize,
    /// The trial of exactly `accepted` (the last one run); `None` when
    /// nothing is accepted.
    pub accepted_trial: Option<TrialResult>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DecideOptions {
    pub require_complete: bool,
    /// Rounds of "check, drop the failing units" before giving up.
    pub max_rounds: usize,
    /// Extra trials bisection may spend over the whole decision.
    pub max_bisection_trials: usize,
}

/// Something in the trial no net can be blamed for: an unattributed error,
/// or a net outside the trial with more unconnected items than before.
fn global_fault(result: &TrialResult, before: &BTreeMap<String, usize>, nets: &BTreeSet<String>) -> Option<String> {
    if let Some(error) = result.blame.unattributed.first() {
        return Some(format!("a new DRC error no imported net is named in: {} ({})", error.description, error.rule));
    }
    other_nets_worse(before, &result.unconnected, nets).first().map(|(net, was, now)| {
        format!("{net}, which is not imported, goes from {was} to {now} unconnected items once zones refill")
    })
}

fn union(units: &[BTreeSet<String>], pick: &[usize]) -> BTreeSet<String> {
    pick.iter().flat_map(|&i| units[i].iter().cloned()).collect()
}

/// Decide which nets' routes to keep. `units` are the nets with routes,
/// grouped so that a differential pair or skew group is kept or dropped as a
/// whole. Each round runs one trial of every remaining unit:
/// - a fault no net can be blamed for is isolated by bisection (about two
///   trials per halving, within `max_bisection_trials`) and the units that
///   bring it are dropped;
/// - otherwise each net is judged ([`judge_net`]) and a failing net drops its
///   whole unit.
///
/// Fails when the rounds run out, or when bisection runs out of trials
/// before it has isolated the fault (the import is then rolled back whole).
pub(crate) fn decide(
    units: &[BTreeSet<String>],
    before: &BTreeMap<String, usize>,
    options: DecideOptions,
    trial: &mut dyn Trial,
) -> anyhow::Result<Decision> {
    let mut decision = Decision::default();
    let mut remaining: Vec<usize> = (0..units.len()).filter(|&i| !units[i].is_empty()).collect();
    let mut bisection_left = options.max_bisection_trials;
    while !remaining.is_empty() {
        decision.rounds += 1;
        if decision.rounds > options.max_rounds {
            anyhow::bail!(
                "nets kept failing after {} rounds of dropping the failing ones; nothing was imported",
                options.max_rounds
            );
        }
        let nets = union(units, &remaining);
        let result = trial.run(&nets)?;
        decision.trials += 1;
        if let Some(fault) = global_fault(&result, before, &nets) {
            let culprits = bisect(units, &remaining, &[], fault.clone(), before, trial, &mut bisection_left, &mut decision.trials)?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "the import cannot be kept net by net: {fault}; bisection ran out of its {} trials before \
                         it isolated the nets that cause it",
                        options.max_bisection_trials
                    )
                })?;
            for (unit, why) in culprits {
                for net in &units[unit] {
                    decision.rejected.insert(
                        net.clone(),
                        NetRejection { reason: why.clone(), errors: Vec::new(), unconnected_after: None, by: "bisection" },
                    );
                }
                remaining.retain(|&i| i != unit);
            }
            continue;
        }
        let mut failed_units = Vec::new();
        for &unit in &remaining {
            let mut verdicts = Vec::new();
            for net in &units[unit] {
                let errors = result.blame.by_net.get(net).cloned().unwrap_or_default();
                let was = before.get(net).copied().unwrap_or(0);
                let now = result.unconnected.get(net).copied().unwrap_or(0);
                if let Some(rejection) = judge_net(errors.len(), was, now, options.require_complete) {
                    verdicts.push((net.clone(), rejection.reason(was, now), errors, now));
                }
            }
            if verdicts.is_empty() {
                continue;
            }
            failed_units.push(unit);
            let summary = verdicts.iter().map(|(net, reason, _, _)| format!("{net}: {reason}")).collect::<Vec<_>>().join("; ");
            for net in &units[unit] {
                match verdicts.iter().find(|(failed, ..)| failed == net) {
                    Some((_, reason, errors, now)) => decision.rejected.insert(
                        net.clone(),
                        NetRejection {
                            reason: reason.clone(),
                            errors: errors.iter().take(3).cloned().collect(),
                            unconnected_after: Some(*now),
                            by: "judged",
                        },
                    ),
                    None => decision.rejected.insert(
                        net.clone(),
                        NetRejection {
                            reason: format!("kept or dropped with its group, and the group failed ({summary})"),
                            errors: Vec::new(),
                            unconnected_after: result.unconnected.get(net).copied(),
                            by: "group",
                        },
                    ),
                };
            }
        }
        if failed_units.is_empty() {
            decision.accepted = nets;
            decision.accepted_trial = Some(result);
            return Ok(decision);
        }
        remaining.retain(|unit| !failed_units.contains(unit));
    }
    Ok(decision)
}

/// The units among `suspects` that bring `fault` (seen with `context` and
/// all of `suspects` on the board), each with the fault its own trial
/// showed. A delta-debugging split: each half is trialled with the context;
/// a half without the fault is cleared. When neither half shows it alone,
/// the fault needs both (two routes that together cut a pour into an
/// island), so the second half is searched again with the first half added
/// to the context, and only its part of the pair is dropped. `None` when the
/// trial budget runs out.
#[allow(clippy::too_many_arguments)]
fn bisect(
    units: &[BTreeSet<String>],
    suspects: &[usize],
    context: &[usize],
    fault: String,
    before: &BTreeMap<String, usize>,
    trial: &mut dyn Trial,
    budget: &mut usize,
    trials: &mut usize,
) -> anyhow::Result<Option<Vec<(usize, String)>>> {
    if suspects.len() == 1 {
        let why = if context.is_empty() {
            format!("bisection: importing it brings {fault}")
        } else {
            format!("bisection: importing it together with other kept nets brings {fault}")
        };
        return Ok(Some(vec![(suspects[0], why)]));
    }
    let (first, second) = suspects.split_at(suspects.len() / 2);
    let mut faults = Vec::new();
    for half in [first, second] {
        if *budget == 0 {
            return Ok(None);
        }
        *budget -= 1;
        *trials += 1;
        let picked: Vec<usize> = context.iter().chain(half).copied().collect();
        let nets = union(units, &picked);
        faults.push(global_fault(&trial.run(&nets)?, before, &nets));
    }
    let mut out = Vec::new();
    match (&faults[0], &faults[1]) {
        (None, None) => {
            let wider: Vec<usize> = context.iter().chain(first).copied().collect();
            match bisect(units, second, &wider, fault, before, trial, budget, trials)? {
                Some(culprits) => out.extend(culprits),
                None => return Ok(None),
            }
        }
        (a, b) => {
            for (half, found) in [(first, a), (second, b)] {
                if let Some(found) = found {
                    match bisect(units, half, context, found.clone(), before, trial, budget, trials)? {
                        Some(culprits) => out.extend(culprits),
                        None => return Ok(None),
                    }
                }
            }
        }
    }
    Ok(Some(out))
}

/// Group the nets with routes into units kept or dropped together: each set
/// in `groups` (a pair, a skew group, a caller's group) joins its members
/// that have routes; every other net is a unit of its own.
pub(crate) fn units(nets: &BTreeSet<String>, groups: &[BTreeSet<String>]) -> Vec<BTreeSet<String>> {
    let mut out: Vec<BTreeSet<String>> = nets.iter().map(|net| BTreeSet::from([net.clone()])).collect();
    for group in groups {
        let (joined, rest): (Vec<_>, Vec<_>) = out.into_iter().partition(|unit| unit.iter().any(|net| group.contains(net)));
        out = rest;
        if !joined.is_empty() {
            out.push(joined.into_iter().flatten().collect());
        }
    }
    out.sort();
    out
}

/// Differential pairs by name among `nets`: `X_P`/`X_N`, `XP`/`XN` with a
/// separator before (`USB_DP`/`USB_DN`), and `X+`/`X-`.
pub(crate) fn name_pairs(nets: &BTreeSet<String>) -> Vec<BTreeSet<String>> {
    let mut out = Vec::new();
    for net in nets {
        for (p, n) in [("_P", "_N"), ("+", "-"), ("_DP", "_DN")] {
            if let Some(stem) = net.strip_suffix(p) {
                let partner = format!("{stem}{n}");
                if nets.contains(&partner) {
                    out.push(BTreeSet::from([net.clone(), partner]));
                }
            }
        }
    }
    out
}

/// Pairs and skew groups in a `.kicad_dru`: the nets a rule with a `skew`,
/// `diff_pair_gap` or `diff_pair_uncoupled` constraint names in
/// `NetName == '…'` conditions (the form `apply_design_intent` writes).
pub(crate) fn dru_groups(dru: &str) -> Vec<BTreeSet<String>> {
    let mut out = Vec::new();
    let mut rest = dru;
    while let Some(at) = rest.find("(rule ") {
        let body = &rest[at..];
        let end = body[1..].find("(rule ").map_or(body.len(), |next| next + 1);
        let rule = &body[..end];
        if ["(constraint skew", "(constraint diff_pair_gap", "(constraint diff_pair_uncoupled"].iter().any(|c| rule.contains(c)) {
            let mut nets = BTreeSet::new();
            let mut scan = rule;
            while let Some(i) = scan.find("NetName == '") {
                let after = &scan[i + "NetName == '".len()..];
                if let Some(close) = after.find('\'') {
                    nets.insert(after[..close].to_string());
                    scan = &after[close..];
                } else {
                    break;
                }
            }
            if nets.len() > 1 {
                out.push(nets);
            }
        }
        rest = &body[end..];
    }
    out
}

/// Pairs in a design-intent file: every net whose `pair` names another.
pub(crate) fn intent_pairs(intent: &serde_json::Value) -> Vec<BTreeSet<String>> {
    intent["nets"]
        .as_object()
        .map(|nets| {
            nets.iter()
                .filter_map(|(net, spec)| spec["pair"].as_str().map(|pair| BTreeSet::from([net.clone(), pair.to_string()])))
                .collect()
        })
        .unwrap_or_default()
}

/// What the live board's undo history holds after the import, newest last.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct UndoPlan {
    /// Undo entries this call added.
    pub entries_added: usize,
    /// The newest entry: what one Ctrl-Z undoes.
    pub last_entry: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// The commits an import makes: the whole session once (trials run offline
/// on its text), then, unless every net is kept, a compensating deletion and,
/// when some are kept, one re-import of the kept nets.
pub(crate) fn undo_plan(kept: usize, with_routes: usize) -> UndoPlan {
    let partial = format!("Import Freerouting SES ({kept} of {with_routes} nets)");
    match (kept, with_routes) {
        (_, 0) => UndoPlan { entries_added: 0, last_entry: "nothing".into(), warning: None },
        (k, n) if k == n => UndoPlan { entries_added: 1, last_entry: "Import Freerouting SES".into(), warning: None },
        (0, _) => UndoPlan {
            entries_added: 2,
            last_entry: "Rollback failed Freerouting SES import".into(),
            warning: Some(
                "Nothing was kept, but the undo history holds the trial import and its rollback: one Ctrl-Z re-creates \
                 every rejected route. Do not undo past this point."
                    .into(),
            ),
        },
        _ => UndoPlan {
            entries_added: 3,
            last_entry: partial.clone(),
            warning: Some(format!(
                "One Ctrl-Z removes the kept routes ('{partial}'). A second one re-creates every route of the session, \
                 the rejected ones included (it undoes the rollback of the trial import). Do not undo past '{partial}'."
            )),
        },
    }
}

/// `board` without the top-level items (tracks, arcs, vias) whose KIID is in
/// `drop`. The items are KiCad's own serialisation, so what is left is the
/// board as KiCad would write it without them.
pub(crate) fn without_items(board: &str, drop: &std::collections::HashSet<String>) -> anyhow::Result<String> {
    if drop.is_empty() {
        return Ok(board.to_string());
    }
    let mut out = String::with_capacity(board.len());
    let bytes = board.as_bytes();
    let (mut depth, mut i, mut quoted, mut escaped) = (0usize, 0usize, false, false);
    let mut form_start: Option<usize> = None;
    let mut copied = 0usize;
    let mut removed = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if quoted {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                quoted = false;
            }
        } else if c == b'"' {
            quoted = true;
        } else if c == b'(' {
            depth += 1;
            if depth == 2 {
                form_start = Some(i);
            }
        } else if c == b')' {
            if depth == 2 {
                if let Some(start) = form_start.take() {
                    let form = &board[start..=i];
                    let routed = ["(segment", "(arc", "(via"].iter().any(|head| form.starts_with(head));
                    let id = form.find("(uuid \"").map(|at| &form[at + 7..]).and_then(|s| s.find('"').map(|end| &s[..end]));
                    if routed && id.is_some_and(|id| drop.contains(id)) {
                        let keep_to = board[..start].trim_end_matches([' ', '\t']).len();
                        out.push_str(&board[copied..keep_to]);
                        copied = i + 1;
                        if board[copied..].starts_with('\n') {
                            copied += 1;
                        }
                        removed += 1;
                    }
                }
            }
            depth = depth.checked_sub(1).ok_or_else(|| anyhow::anyhow!("unbalanced board text"))?;
        }
        i += 1;
    }
    out.push_str(&board[copied..]);
    if removed != drop.len() {
        anyhow::bail!("found {removed} of the {} imported items to leave out of the trial board", drop.len());
    }
    Ok(out)
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

    // ─── The decision loop, over a fake DRC ──────────────────────────────

    /// A fake board: each net is complete when imported unless listed in
    /// `incomplete`; `blamed` nets bring an error blamed on them; `faulty`
    /// nets bring an error nobody can be blamed for; `cutter` nets cut GND;
    /// `together` brings a fault only when all its nets are in.
    #[derive(Default)]
    struct FakeBoard {
        blamed: Vec<&'static str>,
        faulty: Vec<&'static str>,
        cutter: Vec<&'static str>,
        incomplete: Vec<&'static str>,
        together: Vec<&'static str>,
        runs: Vec<BTreeSet<String>>,
    }

    impl Trial for FakeBoard {
        fn run(&mut self, nets: &BTreeSet<String>) -> anyhow::Result<TrialResult> {
            self.runs.push(nets.clone());
            let mut result = TrialResult::default();
            result.unconnected.insert("GND".into(), 0);
            for net in nets {
                let brief = |rule: &str| ErrorBrief { rule: rule.into(), description: format!("{rule} on {net}") };
                if self.blamed.contains(&net.as_str()) {
                    result.blame.by_net.entry(net.clone()).or_default().push(brief("clearance"));
                }
                if self.faulty.contains(&net.as_str()) {
                    result.blame.unattributed.push(brief("starved_thermal"));
                }
                if self.cutter.contains(&net.as_str()) {
                    *result.unconnected.get_mut("GND").unwrap() += 1;
                }
                if self.incomplete.contains(&net.as_str()) {
                    result.unconnected.insert(net.clone(), 1);
                }
            }
            if !self.together.is_empty() && self.together.iter().all(|n| nets.contains(*n)) {
                result.blame.unattributed.push(ErrorBrief { rule: "hole_to_hole".into(), description: "pair of vias".into() });
            }
            Ok(result)
        }
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn singles(names: &[&str]) -> Vec<BTreeSet<String>> {
        names.iter().map(|n| set(&[n])).collect()
    }

    /// Every net open once before the import (one unconnected item each).
    fn open_before(names: &[&str]) -> BTreeMap<String, usize> {
        names.iter().map(|n| (n.to_string(), 1)).chain([("GND".to_string(), 0)]).collect()
    }

    const OPTIONS: DecideOptions = DecideOptions { require_complete: false, max_rounds: 4, max_bisection_trials: 12 };

    #[test]
    fn a_clean_import_is_decided_in_one_trial() {
        let nets = ["A", "B", "C"];
        let mut board = FakeBoard::default();
        let d = decide(&singles(&nets), &open_before(&nets), OPTIONS, &mut board).unwrap();
        assert_eq!(d.accepted, set(&nets));
        assert_eq!((d.rounds, d.trials), (1, 1));
        assert!(d.rejected.is_empty() && d.accepted_trial.is_some());
    }

    #[test]
    fn a_failing_net_is_dropped_and_the_rest_rechecked() {
        let nets = ["A", "B", "C"];
        let mut board = FakeBoard { blamed: vec!["B"], incomplete: vec!["C"], ..Default::default() };
        let d = decide(&singles(&nets), &open_before(&nets), OPTIONS, &mut board).unwrap();
        assert_eq!(d.accepted, set(&["A"]));
        assert_eq!(d.rejected["B"].by, "judged");
        assert_eq!(d.rejected["B"].errors[0].rule, "clearance");
        assert!(d.rejected["C"].reason.contains("join nothing new"), "{:?}", d.rejected["C"]);
        assert_eq!(d.rounds, 2, "one round finds both, the second confirms the rest");
        assert_eq!(board.runs.last().unwrap(), &set(&["A"]));
    }

    #[test]
    fn a_pair_is_kept_or_dropped_as_a_unit() {
        let nets = ["USB_D_P", "USB_D_N", "LED"];
        let groups = vec![set(&["USB_D_P", "USB_D_N"])];
        let units = units(&set(&nets), &groups);
        assert_eq!(units.len(), 2);
        let mut board = FakeBoard { blamed: vec!["USB_D_N"], ..Default::default() };
        let d = decide(&units, &open_before(&nets), OPTIONS, &mut board).unwrap();
        assert_eq!(d.accepted, set(&["LED"]));
        assert_eq!(d.rejected["USB_D_P"].by, "group");
        assert!(d.rejected["USB_D_P"].reason.contains("USB_D_N"), "{:?}", d.rejected["USB_D_P"]);
    }

    #[test]
    fn bisection_isolates_the_net_behind_an_unblamable_error_or_a_cut_plane() {
        let nets = ["A", "B", "C", "D", "E", "F", "G", "H"];
        let mut board = FakeBoard { faulty: vec!["F"], cutter: vec!["B"], ..Default::default() };
        let d = decide(&singles(&nets), &open_before(&nets), OPTIONS, &mut board).unwrap();
        assert_eq!(d.accepted, set(&["A", "C", "D", "E", "G", "H"]));
        assert_eq!(d.rejected["F"].by, "bisection");
        assert!(d.rejected["F"].reason.contains("starved_thermal"), "{:?}", d.rejected["F"]);
        assert!(d.rejected["B"].reason.contains("GND"), "{:?}", d.rejected["B"]);
        // 8 units: 2 trials per level over 3 levels on two branches, well
        // under the cap, plus one trial per round.
        assert!(d.trials <= 1 + 12 + 1, "{}", d.trials);
        assert_eq!(d.rounds, 2);
    }

    #[test]
    fn a_fault_that_needs_two_nets_drops_one_side_and_the_next_round_checks() {
        let nets = ["A", "B", "C", "D"];
        let mut board = FakeBoard { together: vec!["A", "D"], ..Default::default() };
        let d = decide(&singles(&nets), &open_before(&nets), OPTIONS, &mut board).unwrap();
        assert!(d.rejected.contains_key("D") && !d.accepted.contains("D"));
        assert!(d.accepted.contains("A"));
        assert!(d.rejected["D"].reason.contains("together with other kept nets"), "{:?}", d.rejected["D"]);
        assert_eq!(d.accepted, set(&["A", "B", "C"]), "only the half of the pair that was searched goes");
    }

    #[test]
    fn running_out_of_bisection_trials_or_rounds_fails_the_import() {
        let nets = ["A", "B", "C", "D", "E", "F", "G", "H"];
        let mut board = FakeBoard { faulty: vec!["H"], ..Default::default() };
        let tight = DecideOptions { max_bisection_trials: 2, ..OPTIONS };
        let error = decide(&singles(&nets), &open_before(&nets), tight, &mut board).unwrap_err();
        assert!(format!("{error}").contains("bisection ran out"), "{error}");

        // A net that fails every round, one at a time: each round drops one.
        let nets = ["A", "B", "C", "D", "E", "F"];
        struct OneAtATime;
        impl Trial for OneAtATime {
            fn run(&mut self, nets: &BTreeSet<String>) -> anyhow::Result<TrialResult> {
                let mut r = TrialResult::default();
                let worst = nets.iter().next_back().unwrap();
                r.blame.by_net.insert(worst.clone(), vec![ErrorBrief { rule: "clearance".into(), description: String::new() }]);
                Ok(r)
            }
        }
        let error = decide(&singles(&nets), &open_before(&nets), OPTIONS, &mut OneAtATime).unwrap_err();
        assert!(format!("{error}").contains("after 4 rounds"), "{error}");
    }

    #[test]
    fn require_complete_drops_partial_nets() {
        let nets = ["A", "B"];
        let before = BTreeMap::from([("A".to_string(), 3), ("B".to_string(), 1)]);
        let mut board = FakeBoard { incomplete: vec!["A"], ..Default::default() };
        let d = decide(&singles(&nets), &before, OPTIONS, &mut board).unwrap();
        assert_eq!(d.accepted, set(&nets), "3 -> 1 is progress");
        let strict = DecideOptions { require_complete: true, ..OPTIONS };
        let d = decide(&singles(&nets), &before, strict, &mut FakeBoard { incomplete: vec!["A"], ..Default::default() }).unwrap();
        assert_eq!(d.accepted, set(&["B"]));
    }

    #[test]
    fn groups_come_from_names_rules_and_intent() {
        let nets = set(&["/USB_D_P", "/USB_D_N", "ETH_TX+", "ETH_TX-", "CLK_P", "LED"]);
        let pairs = name_pairs(&nets);
        assert!(pairs.contains(&set(&["/USB_D_P", "/USB_D_N"])));
        assert!(pairs.contains(&set(&["ETH_TX+", "ETH_TX-"])));
        assert_eq!(pairs.len(), 2, "CLK_P has no partner");
        let dru = "(version 1)\n(rule \"intent: length X\"\n  (condition \"A.NetName == 'X'\")\n  (constraint length (max 5mm)))\n\
(rule \"intent: pair /USB_D_N//USB_D_P\"\n  (condition \"A.NetName == '/USB_D_N' || A.NetName == '/USB_D_P'\")\n  (constraint diff_pair_gap (min 0.1mm))\n  (constraint skew (max 1.27mm)))\n";
        assert_eq!(dru_groups(dru), vec![set(&["/USB_D_N", "/USB_D_P"])]);
        let intent = serde_json::json!({ "nets": { "/USB_C_P": { "pair": "/USB_C_N" }, "+5V": { "role": "power" } } });
        assert_eq!(intent_pairs(&intent), vec![set(&["/USB_C_P", "/USB_C_N"])]);
        // Units join overlapping groups; members without routes stay out.
        let u = units(&set(&["A", "B", "C"]), &[set(&["A", "B"]), set(&["B", "C"]), set(&["Z", "C"])]);
        assert_eq!(u, vec![set(&["A", "B", "C"])]);
    }

    #[test]
    fn the_undo_history_is_described_as_it_is() {
        assert_eq!(undo_plan(5, 5).entries_added, 1);
        assert_eq!(undo_plan(5, 5).last_entry, "Import Freerouting SES");
        assert!(undo_plan(5, 5).warning.is_none());
        let partial = undo_plan(3, 5);
        assert_eq!((partial.entries_added, partial.last_entry.as_str()), (3, "Import Freerouting SES (3 of 5 nets)"));
        assert!(partial.warning.unwrap().contains("second one re-creates"));
        let none = undo_plan(0, 5);
        assert_eq!(none.entries_added, 2);
        assert!(none.warning.unwrap().contains("re-creates every rejected route"));
        assert_eq!(undo_plan(0, 0).entries_added, 0);
    }

    #[test]
    fn trial_boards_leave_out_exactly_the_named_items() {
        let board = "(kicad_pcb\n\t(net \"A\")\n\t(segment\n\t\t(start 0 0)\n\t\t(end 1 0)\n\t\t(net \"A\")\n\t\t(uuid \"s1\")\n\t)\n\t(gr_text \"a ) ( \\\" b\"\n\t\t(uuid \"t1\")\n\t)\n\t(via\n\t\t(at 1 0)\n\t\t(uuid \"v1\")\n\t)\n\t(arc\n\t\t(start 0 0)\n\t\t(uuid \"a1\")\n\t)\n)\n";
        let drop: std::collections::HashSet<String> = ["s1".to_string(), "a1".to_string()].into();
        let out = without_items(board, &drop).unwrap();
        assert!(!out.contains("\"s1\"") && !out.contains("\"a1\""), "{out}");
        assert!(out.contains("\"v1\"") && out.contains("\"t1\"") && out.contains("a ) ( \\\" b"), "{out}");
        konnect_sexp::parse_sexp(&out).unwrap();
        assert_eq!(without_items(board, &Default::default()).unwrap(), board);
        let missing: std::collections::HashSet<String> = ["nope".to_string()].into();
        assert!(without_items(board, &missing).is_err(), "a KIID not found is an error, not a silent no-op");
        // A graphic with a matching uuid is never removed.
        let graphic: std::collections::HashSet<String> = ["t1".to_string()].into();
        assert!(without_items(board, &graphic).is_err());
    }
}
