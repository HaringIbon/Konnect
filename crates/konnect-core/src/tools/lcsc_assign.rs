//! Assign JLCPCB/LCSC part numbers to schematic symbols and check the ones
//! already assigned, against the local JLCPCB database.
//!
//! Passive values are normalised to the spelling LCSC descriptions use
//! (`100n` → `100nF`, `4k7` → `4.7kΩ`), and ratings written in the value
//! (`100n 16V X7R 10%`) become filters. Everything else is matched by
//! manufacturer part number (an `MPN` field, else the value). What could not
//! be checked is listed per group, so the caller knows what to verify.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use konnect_schematic_editor as cse;
use serde_json::{json, Value};

use super::integration::{
    query_alternative_candidates, rank_alternatives, resolve_db_path, row_to_part_json,
    AlternativeCandidate, AlternativePolicy,
};
use super::lcsc_packages;
use crate::mcp::{error::ToolErrorKind, protocol::CallToolResult};
use crate::tool;
use crate::tools::{get_path, ToolContext, ToolDef};

/// Field names that already hold an LCSC number; the first present wins.
const LCSC_FIELDS: [&str; 5] = ["LCSC", "LCSC Part #", "LCSC Part", "JLCPCB Part #", "JLC"];
const MPN_FIELDS: [&str; 4] = ["MPN", "Manufacturer_Part_Number", "MFR_Part", "Part Number"];

pub fn tools() -> Vec<ToolDef> {
    vec![tool!(
        "assign_lcsc_parts",
        "Give every in-BOM, non-DNP schematic symbol an LCSC part number from the local JLCPCB \
         database, and check the numbers already there (exists, stock for the order, package \
         against the footprint). Passive values are normalised (100n → 100nF, 4k7 → 4.7kΩ); \
         ratings in the value (16V, X7R, 1%) are enforced; other parts match by MPN field or \
         value. Ranks Basic before Extended, then price and stock. Dry run by default; with \
         dry_run=false the numbers are written to each symbol's LCSC field in one atomic write.",
        json!({
            "type": "object",
            "properties": {
                "schematic": { "type": "string", "description": "Path to the .kicad_sch file" },
                "boards": { "type": "integer", "minimum": 1, "default": 5, "description": "Boards to be assembled; stock must cover quantity × boards" },
                "parts": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Explicit choices, reference → LCSC number (e.g. {\"J1\": \"C165948\"}); checked, not searched" },
                "prefer_basic": { "type": "boolean", "default": true },
                "overwrite": { "type": "boolean", "default": false, "description": "Replace LCSC numbers that are already present and pass their checks" },
                "dry_run": { "type": "boolean", "default": true },
                "db_path": { "type": "string" }
            },
            "required": ["schematic"]
        }),
        |args, ctx| async move { handle_assign(args, ctx).await }
    )]
}

// ─── Values ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Capacitor,
    Resistor,
    Inductor,
    Other,
}

fn kind_of(reference: &str) -> Kind {
    let prefix: String = reference.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    match prefix.as_str() {
        "C" => Kind::Capacitor,
        "R" | "RN" => Kind::Resistor,
        "L" => Kind::Inductor,
        _ => Kind::Other,
    }
}

fn multiplier(c: char) -> Option<f64> {
    Some(match c {
        'p' => 1e-12,
        'n' => 1e-9,
        'u' | 'µ' | 'μ' => 1e-6,
        'm' => 1e-3,
        'k' | 'K' => 1e3,
        'M' => 1e6,
        'G' => 1e9,
        'R' | 'r' => 1.0,
        _ => return None,
    })
}

/// Parse an engineering value such as `100n`, `0.1uF`, `4k7`, `2R2`, `10K`,
/// `1M`, `100`, `10mH`, `4.7µF` into base units. The unit letter is
/// optional; `kind` decides what a trailing `F`/`H`/`Ω` may be.
fn parse_value(token: &str, kind: Kind) -> Option<f64> {
    let mut t = token.trim().to_string();
    for unit in ["ohms", "ohm", "Ohm", "Ω", "Ω"] {
        if let Some(stripped) = t.strip_suffix(unit) {
            t = stripped.to_string();
        }
    }
    let unit = match kind {
        Kind::Capacitor => Some('F'),
        Kind::Inductor => Some('H'),
        _ => None,
    };
    if let Some(u) = unit {
        if let Some(stripped) = t.strip_suffix(u) {
            t = stripped.to_string();
        }
    }
    if t.is_empty() {
        return None;
    }
    let chars: Vec<char> = t.chars().collect();
    let letter_positions: Vec<usize> = chars
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.is_ascii_digit() && **c != '.')
        .map(|(i, _)| i)
        .collect();
    match letter_positions.as_slice() {
        [] => t.parse::<f64>().ok(),
        [i] => {
            let m = multiplier(chars[*i])?;
            // `R` and the mutiplier letters stand for the decimal point.
            let head: String = chars[..*i].iter().collect();
            let tail: String = chars[*i + 1..].iter().collect();
            if head.is_empty() && tail.is_empty() {
                return None;
            }
            if !tail.is_empty() && head.contains('.') {
                return None;
            }
            let number = if tail.is_empty() {
                head.parse::<f64>().ok()?
            } else {
                format!("{}.{}", if head.is_empty() { "0" } else { &head }, tail).parse::<f64>().ok()?
            };
            Some(number * m)
        }
        _ => None,
    }
}

fn trim_number(x: f64) -> String {
    let s = format!("{:.3}", (x * 1000.0).round() / 1000.0);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

/// The spellings LCSC descriptions use for a value, best first: the
/// engineering-prefix form (`100nF`, `4.7kΩ`) and, for capacitors and
/// inductors, the next smaller prefix (`1000pF` as well as `1nF`).
fn lcsc_spellings(value: f64, kind: Kind) -> Vec<String> {
    let (prefixes, unit): (&[(&str, f64)], &str) = match kind {
        Kind::Capacitor => (&[("p", 1e-12), ("n", 1e-9), ("u", 1e-6), ("m", 1e-3)], "F"),
        Kind::Inductor => (&[("n", 1e-9), ("u", 1e-6), ("m", 1e-3), ("", 1.0)], "H"),
        Kind::Resistor => (&[("m", 1e-3), ("", 1.0), ("k", 1e3), ("M", 1e6), ("G", 1e9)], "Ω"),
        Kind::Other => return Vec::new(),
    };
    if value == 0.0 {
        return vec![format!("0{unit}")];
    }
    let mut best = None;
    for (index, (_, scale)) in prefixes.iter().enumerate() {
        let mantissa = value / scale;
        if (0.9995..999.5).contains(&mantissa) {
            best = Some(index);
        }
    }
    let Some(best) = best else { return Vec::new() };
    let spell = |index: usize| {
        let (prefix, scale) = prefixes[index];
        format!("{}{prefix}{unit}", trim_number(value / scale))
    };
    let mut out = vec![spell(best)];
    if kind != Kind::Resistor && best > 0 && value / prefixes[best - 1].1 < 1e5 {
        out.push(spell(best - 1));
    }
    out
}

/// Ratings written into a value after the value itself.
#[derive(Debug, Default, Clone, PartialEq)]
struct Ratings {
    min_voltage: Option<f64>,
    dielectric: Option<String>,
    max_tolerance_pct: Option<f64>,
    min_power_w: Option<f64>,
}

fn parse_ratings(tokens: &[&str]) -> (Ratings, Vec<String>) {
    let mut ratings = Ratings::default();
    let mut unknown = Vec::new();
    for token in tokens {
        let upper = token.to_ascii_uppercase();
        if let Some(v) = upper.strip_suffix('V').and_then(|n| n.parse::<f64>().ok()) {
            ratings.min_voltage = Some(v);
        } else if ["X7R", "X5R", "C0G", "NP0", "X7S", "X6S", "Y5V", "X8R"].contains(&upper.as_str()) {
            ratings.dielectric = Some(if upper == "NP0" { "C0G".into() } else { upper });
        } else if let Some(p) = upper.trim_start_matches('±').strip_suffix('%').and_then(|n| n.parse::<f64>().ok()) {
            ratings.max_tolerance_pct = Some(p);
        } else if let Some(w) = upper.strip_suffix("MW").and_then(|n| n.parse::<f64>().ok()) {
            ratings.min_power_w = Some(w / 1000.0);
        } else if let Some(w) = upper.strip_suffix('W').and_then(|n| {
            n.split_once('/')
                .and_then(|(a, b)| Some(a.parse::<f64>().ok()? / b.parse::<f64>().ok()?))
                .or_else(|| n.parse::<f64>().ok())
        }) {
            ratings.min_power_w = Some(w);
        } else {
            unknown.push(token.to_string());
        }
    }
    (ratings, unknown)
}

/// Numbers in a description followed by `suffix` (`50V`, `100mW`, `±10%`).
fn numbers_before(description: &str, suffix: &str) -> Vec<f64> {
    let mut out = Vec::new();
    for word in description.split(|c: char| c.is_whitespace() || c == ',') {
        let word = word.trim_start_matches('±');
        if let Some(n) = word.strip_suffix(suffix) {
            if let Ok(v) = n.parse::<f64>() {
                out.push(v);
            }
        }
    }
    out
}

/// Whether a part's description satisfies the ratings. `None` means the
/// description does not state the rating, so it was not checked.
fn rating_failures(description: &str, ratings: &Ratings) -> Vec<String> {
    let mut failures = Vec::new();
    if let Some(min) = ratings.min_voltage {
        let volts = numbers_before(description, "V");
        if !volts.iter().any(|v| *v >= min) {
            failures.push(format!("voltage < {min} V or not stated"));
        }
    }
    if let Some(d) = &ratings.dielectric {
        let has = description.to_ascii_uppercase().contains(d.as_str())
            || (d == "C0G" && description.to_ascii_uppercase().contains("NP0"));
        if !has {
            failures.push(format!("dielectric not {d}"));
        }
    }
    if let Some(max) = ratings.max_tolerance_pct {
        let tol = numbers_before(description, "%");
        if !tol.iter().any(|t| *t <= max) {
            failures.push(format!("tolerance > {max}% or not stated"));
        }
    }
    if let Some(min) = ratings.min_power_w {
        let mw = numbers_before(description, "mW");
        let w = numbers_before(description, "W");
        if !mw.iter().any(|p| p / 1000.0 >= min) && !w.iter().any(|p| *p >= min) {
            failures.push(format!("power < {min} W or not stated"));
        }
    }
    failures
}

// ─── Schematic ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Line {
    references: Vec<String>,
    value: String,
    footprint: String,
    mpn: Option<String>,
    lcsc: Option<(String, String)>,
}

/// One BOM line per (value, footprint, MPN, existing LCSC); multi-unit
/// symbols count once.
fn bom_lines(sheets: &[(PathBuf, cse::Schematic)]) -> (Vec<Line>, Vec<String>, BTreeMap<String, PathBuf>) {
    let mut lines: BTreeMap<(String, String, String, String), Line> = BTreeMap::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut skipped = Vec::new();
    let mut file_of = BTreeMap::new();
    for (file, symbol) in sheets.iter().flat_map(|(f, sch)| sch.symbols.iter().map(move |s| (f, s))) {
        let Some(reference) = symbol.reference() else { continue };
        if reference.starts_with('#') || !seen.insert(reference.to_string()) {
            continue;
        }
        file_of.insert(reference.to_string(), file.clone());
        if !symbol.in_bom || symbol.dnp {
            skipped.push(reference.to_string());
            continue;
        }
        let value = symbol.value_str().unwrap_or("").trim().to_string();
        let footprint = symbol.footprint().unwrap_or("").trim().to_string();
        let mpn = MPN_FIELDS
            .iter()
            .find_map(|f| symbol.property(f).map(str::trim).filter(|v| !v.is_empty() && *v != "~"))
            .map(str::to_string);
        let lcsc = LCSC_FIELDS.iter().find_map(|f| {
            symbol
                .property(f)
                .map(str::trim)
                .filter(|v| !v.is_empty() && *v != "~")
                .map(|v| (f.to_string(), v.to_string()))
        });
        let key = (
            value.clone(),
            footprint.clone(),
            mpn.clone().unwrap_or_default(),
            lcsc.as_ref().map(|l| l.1.clone()).unwrap_or_default(),
        );
        lines
            .entry(key)
            .or_insert_with(|| Line { references: Vec::new(), value, footprint, mpn, lcsc })
            .references
            .push(reference.to_string());
    }
    let mut lines: Vec<Line> = lines.into_values().collect();
    for line in &mut lines {
        line.references.sort_by_key(|r| natural(r));
    }
    lines.sort_by(|a, b| natural(&a.references[0]).cmp(&natural(&b.references[0])));
    (lines, skipped, file_of)
}

/// The root sheet and every sub-sheet file below it. A file placed more than
/// once holds per-instance references this tool does not rewrite, so it is
/// returned separately and left alone.
type Sheets = Vec<(PathBuf, cse::Schematic)>;

fn load_hierarchy(root: &Path) -> anyhow::Result<(Sheets, Vec<String>)> {
    let mut loaded: Sheets = Vec::new();
    let mut uses: BTreeMap<PathBuf, usize> = BTreeMap::new();
    let mut queue = vec![root.to_path_buf()];
    while let Some(path) = queue.pop() {
        let count = uses.entry(path.clone()).or_insert(0);
        *count += 1;
        if *count > 1 {
            continue;
        }
        let sch = cse::Schematic::load(&path)?;
        let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
        for sheet in sch.sheets.iter() {
            queue.push(dir.join(sheet.file()));
        }
        loaded.push((path, sch));
    }
    let multi: Vec<String> = uses.iter().filter(|(_, n)| **n > 1).map(|(p, _)| p.display().to_string()).collect();
    loaded.retain(|(p, _)| uses.get(p) == Some(&1));
    Ok((loaded, multi))
}

fn natural(reference: &str) -> (String, u64) {
    let prefix: String = reference.chars().take_while(|c| !c.is_ascii_digit()).collect();
    let number = reference[prefix.len()..].parse().unwrap_or(0);
    (prefix, number)
}

// ─── Database ──────────────────────────────────────────────────────────────

fn part_by_lcsc(db: &Path, lcsc: &str) -> anyhow::Result<Option<Value>> {
    let conn = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = conn.prepare(
        "SELECT LCSC, MFR_Part, Package, Manufacturer, Library_Type, Description, Datasheet, Price, Stock \
         FROM components WHERE LCSC = ?1 COLLATE NOCASE",
    )?;
    let mut rows = statement.query([lcsc])?;
    Ok(match rows.next()? {
        Some(row) => Some(row_to_part_json(row)?),
        None => None,
    })
}

fn parts_by_mpn(db: &Path, mpn: &str) -> anyhow::Result<Vec<AlternativeCandidate>> {
    let conn = rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = conn.prepare(
        "SELECT LCSC, MFR_Part, Package, Manufacturer, Library_Type, Description, Datasheet, Price, Stock \
         FROM components WHERE MFR_Part = ?1 COLLATE NOCASE",
    )?;
    let rows = statement
        .query_map([mpn], |row| {
            Ok(AlternativeCandidate {
                part: row_to_part_json(row)?,
                lcsc_id: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                library_type: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                price: row.get::<_, Option<f64>>(7)?.unwrap_or(0.0),
                stock_count: row.get::<_, Option<i64>>(8)?.unwrap_or(0),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn package_ok(part: &Value, footprint: &str) -> Option<bool> {
    let packages = lcsc_packages::lcsc_packages(footprint)?.packages;
    let package = part["package"].as_str().unwrap_or("");
    Some(packages.iter().any(|p| p.eq_ignore_ascii_case(package)))
}

// ─── Decisions ─────────────────────────────────────────────────────────────

struct Settings {
    db: PathBuf,
    boards: u64,
    prefer_basic: bool,
    overwrite: bool,
}

fn need(line: &Line, settings: &Settings) -> u64 {
    (line.references.len() as u64 * settings.boards).max(1)
}

/// Check one chosen LCSC number: exists, stock, package.
fn check_number(lcsc: &str, line: &Line, settings: &Settings) -> anyhow::Result<(Option<Value>, Vec<String>)> {
    let mut problems = Vec::new();
    if lcsc.len() < 2 || !lcsc.starts_with(['C', 'c']) || !lcsc[1..].chars().all(|c| c.is_ascii_digit()) {
        problems.push(format!("'{lcsc}' is not an LCSC number (C followed by digits)"));
        return Ok((None, problems));
    }
    let Some(part) = part_by_lcsc(&settings.db, lcsc)? else {
        problems.push(format!("{lcsc} is not in the JLCPCB database"));
        return Ok((None, problems));
    };
    let stock = part["stock"].as_i64().unwrap_or(0);
    if (stock as u64) < need(line, settings) {
        problems.push(format!("stock {stock} < {} needed", need(line, settings)));
    }
    if package_ok(&part, &line.footprint) == Some(false) {
        problems.push(format!(
            "package '{}' does not match footprint '{}'",
            part["package"].as_str().unwrap_or(""),
            line.footprint
        ));
    }
    Ok((Some(part), problems))
}

fn search(line: &Line, settings: &Settings) -> anyhow::Result<(Vec<AlternativeCandidate>, Value)> {
    let kind = kind_of(&line.references[0]);
    let policy = AlternativePolicy {
        min_stock_count: need(line, settings).max(100),
        prefer_basic: settings.prefer_basic,
        max_price: None,
        limit: 3,
    };
    let package_match = lcsc_packages::lcsc_packages(&line.footprint);
    let packages = package_match.as_ref().map(|m| m.packages.clone());

    // A part number names the part, whatever kind it is.
    let mpn = line.mpn.clone().or_else(|| {
        (kind == Kind::Other && !line.value.is_empty() && !line.value.contains(' ')).then(|| line.value.clone())
    });
    if let Some(mpn) = mpn {
        let mut found = parts_by_mpn(&settings.db, &mpn)?;
        let total = found.len();
        if let Some(packages) = &packages {
            found.retain(|c| packages.iter().any(|p| c.part["package"].as_str().is_some_and(|q| q.eq_ignore_ascii_case(p))));
        }
        let (ranked, counts) = rank_alternatives(found, &policy);
        return Ok((
            ranked,
            json!({ "method": "mpn", "mpn": mpn, "matched": total, "after_package_filter": counts.matched,
                    "below_stock": counts.below_min_stock,
                    "package_filter": packages }),
        ));
    }

    let mut tokens = line.value.split_whitespace();
    let head = tokens.next().unwrap_or("");
    let rest: Vec<&str> = tokens.collect();
    let Some(base) = parse_value(head, kind) else {
        return Ok((Vec::new(), json!({ "method": "none", "reason": format!("value '{}' is neither a passive value nor a part number; set an MPN field or pass parts", line.value) })));
    };
    let Some(packages) = packages else {
        return Ok((Vec::new(), json!({ "method": "none", "reason": format!("no LCSC package known for footprint '{}'", line.footprint) })));
    };
    let (ratings, unknown) = parse_ratings(&rest);
    let spellings = lcsc_spellings(base, kind);
    let mut candidates: Vec<AlternativeCandidate> = Vec::new();
    for spelling in &spellings {
        for c in query_alternative_candidates(&settings.db, spelling, &packages)? {
            if !candidates.iter().any(|k| k.lcsc_id == c.lcsc_id) {
                candidates.push(c);
            }
        }
    }
    // Keep only parts of the right kind: the value also appears in, say,
    // a capacitor-array or LED description.
    let kind_word = match kind {
        Kind::Resistor => Some("resistor"),
        Kind::Inductor => Some("induct"),
        _ => None,
    };
    let matched = candidates.len();
    candidates.retain(|c| {
        let d = c.part["description"].as_str().unwrap_or("").to_ascii_lowercase();
        kind_word.is_none_or(|w| d.contains(w) || matches!(kind, Kind::Inductor) && d.contains("inductor"))
    });
    let of_kind = candidates.len();
    candidates.retain(|c| rating_failures(c.part["description"].as_str().unwrap_or(""), &ratings).is_empty());
    let rated = candidates.len();
    let (ranked, counts) = rank_alternatives(candidates, &policy);
    let mut unchecked = Vec::new();
    match kind {
        Kind::Capacitor => {
            if ratings.min_voltage.is_none() {
                unchecked.push("voltage rating (write it in the value, e.g. '100n 16V')");
            }
            if ratings.dielectric.is_none() {
                unchecked.push("dielectric (and its DC-bias derating)");
            }
        }
        Kind::Resistor => {
            if ratings.min_power_w.is_none() {
                unchecked.push("power rating (write it in the value, e.g. '10k 1/10W')");
            }
        }
        Kind::Inductor => unchecked.push("saturation and rated current"),
        Kind::Other => {}
    }
    Ok((
        ranked,
        json!({
            "method": "value",
            "spellings": spellings,
            "packages": packages,
            "ratings": { "min_voltage": ratings.min_voltage, "dielectric": ratings.dielectric,
                         "max_tolerance_pct": ratings.max_tolerance_pct, "min_power_w": ratings.min_power_w },
            "ignored_value_words": unknown,
            "matched": matched, "of_kind": of_kind, "meeting_ratings": rated,
            "below_stock": counts.below_min_stock,
            "unchecked": unchecked,
        }),
    ))
}

fn decide(line: &Line, overrides: &BTreeMap<String, String>, settings: &Settings) -> anyhow::Result<Value> {
    let base = json!({ "references": line.references, "value": line.value, "footprint": line.footprint,
                       "quantity": line.references.len(), "needed": need(line, settings) });
    let with = |mut v: Value, extra: Value| {
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    };
    // An explicit choice for any reference in the line.
    if let Some(choice) = line.references.iter().find_map(|r| overrides.get(r)) {
        let (part, problems) = check_number(choice, line, settings)?;
        // Already written: nothing to assign.
        let already = line.lcsc.as_ref().is_some_and(|(_, existing)| existing.eq_ignore_ascii_case(choice));
        let status = match (problems.is_empty(), already) {
            (false, _) => "rejected",
            (true, true) => "ok",
            (true, false) => "assign",
        };
        return Ok(with(base, json!({ "status": status,
            "source": "parts argument", "lcsc": choice, "part": part, "problems": problems })));
    }
    if let Some((field, existing)) = &line.lcsc {
        let (part, problems) = check_number(existing, line, settings)?;
        if problems.is_empty() && !settings.overwrite {
            return Ok(with(base, json!({ "status": "ok", "source": format!("existing {field} field"), "lcsc": existing, "part": part })));
        }
        if !problems.is_empty() {
            let (ranked, search) = search(line, settings)?;
            let suggestion = ranked.first().map(|c| c.part.clone());
            return Ok(with(base, json!({ "status": "problem", "source": format!("existing {field} field"),
                "lcsc": existing, "part": part, "problems": problems, "suggestion": suggestion, "search": search })));
        }
    }
    let (ranked, search) = search(line, settings)?;
    match ranked.split_first() {
        Some((best, others)) => Ok(with(base, json!({
            "status": "assign", "source": "database", "lcsc": best.lcsc_id, "part": best.part,
            "alternatives": others.iter().map(|c| json!({"lcsc": c.lcsc_id, "library_type": c.library_type, "stock": c.stock_count, "price": c.price})).collect::<Vec<_>>(),
            "search": search }))),
        None => Ok(with(base, json!({ "status": "unresolved", "search": search }))),
    }
}

async fn handle_assign(args: &Value, ctx: &ToolContext) -> anyhow::Result<CallToolResult> {
    let sch_path = get_path(args, "schematic")?;
    let mut db_args = args.clone();
    if let Some(p) = args["db_path"].as_str() {
        db_args["output_path"] = json!(p);
    }
    let db = resolve_db_path(&db_args, ctx);
    if !db.exists() {
        return Ok(CallToolResult::error("JLCPCB database not found. Run download_jlcpcb_database first."));
    }
    let overrides: BTreeMap<String, String> = match args.get("parts") {
        None | Some(Value::Null) => BTreeMap::new(),
        Some(Value::Object(map)) => map
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.trim().to_string())))
            .collect(),
        Some(_) => {
            return Ok(CallToolResult::error_kind(
                ToolErrorKind::InvalidArgument { field: "parts".into(), reason: "must be an object".into() },
                "parts must map references to LCSC numbers",
            ))
        }
    };
    let settings = Settings {
        db,
        boards: args["boards"].as_u64().unwrap_or(5).max(1),
        prefer_basic: args["prefer_basic"].as_bool().unwrap_or(true),
        overwrite: args["overwrite"].as_bool().unwrap_or(false),
    };
    let dry_run = args["dry_run"].as_bool().unwrap_or(true);

    let path = sch_path.clone();
    let (decisions, skipped, multi, file_of, scanned) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let (sheets, multi) = load_hierarchy(&path)?;
        let scanned: Vec<String> = sheets.iter().map(|(p, _)| p.display().to_string()).collect();
        let (lines, skipped, file_of) = bom_lines(&sheets);
        let decisions = lines
            .iter()
            .map(|line| decide(line, &overrides, &settings))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok((decisions, skipped, multi, file_of, scanned))
    })
    .await??;

    let count = |status: &str| decisions.iter().filter(|d| d["status"] == status).count();
    let extended: std::collections::BTreeSet<String> = decisions
        .iter()
        .filter(|d| matches!(d["status"].as_str(), Some("assign" | "ok")))
        .filter(|d| d["part"]["library_type"] == "Extended")
        .filter_map(|d| d["lcsc"].as_str().map(str::to_string))
        .collect();
    let edits: Vec<Value> = decisions
        .iter()
        .filter(|d| d["status"] == "assign")
        .flat_map(|d| {
            let lcsc = d["lcsc"].as_str().unwrap_or("").to_string();
            d["references"].as_array().cloned().unwrap_or_default().into_iter().map(move |r| {
                json!({ "reference": r, "fields": { "LCSC": lcsc } })
            })
        })
        .collect();
    let mut body = json!({
        "dry_run": dry_run,
        "schematic": sch_path,
        "summary": {
            "lines": decisions.len(), "assign": count("assign"), "ok": count("ok"),
            "problem": count("problem"), "rejected": count("rejected"), "unresolved": count("unresolved"),
            "extended_parts": extended.len(),
        },
        "extended_parts": extended,
        "extended_note": "JLCPCB charges a one-off loading fee per unique Extended part; Basic and Preferred parts have none. Prefer a Basic equivalent where the design allows.",
        "lines": decisions,
        "not_in_bom": skipped,
        "sheets_scanned": scanned,
        "sheets_not_scanned": multi,
        "sheets_note": "A sheet file placed more than once keeps per-instance references; assign those parts by hand.",
        "ready_to_order": count("problem") + count("rejected") + count("unresolved") == 0,
    });
    if !dry_run && !edits.is_empty() {
        // One atomic write per sheet file.
        let mut by_file: BTreeMap<PathBuf, Vec<Value>> = BTreeMap::new();
        for edit in edits {
            let reference = edit["reference"].as_str().unwrap_or("");
            let file = file_of.get(reference).cloned().unwrap_or_else(|| sch_path.clone());
            by_file.entry(file).or_default().push(edit);
        }
        let mut written = Vec::new();
        for (file, edits) in by_file {
            let count = edits.len();
            let result = super::sch_batch::handle_batch_edit(
                &json!({ "schematic": file, "create_missing": true, "edits": edits }),
                ctx,
            )
            .await?;
            if result.is_error {
                // Earlier sheets were written; say which before failing.
                return Ok(CallToolResult::error(format!(
                    "assign_lcsc_parts wrote {} and then failed on {}: {:?}",
                    serde_json::to_string(&written).unwrap_or_default(),
                    file.display(),
                    result.content.first()
                )));
            }
            written.push(json!({ "file": file, "symbols": count }));
        }
        body["written"] = json!(written);
    }
    Ok(CallToolResult::json(&body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_parse_in_every_common_spelling() {
        let cases = [
            ("100n", Kind::Capacitor, 100e-9),
            ("100nF", Kind::Capacitor, 100e-9),
            ("0.1u", Kind::Capacitor, 100e-9),
            ("0.1µF", Kind::Capacitor, 100e-9),
            ("4u7", Kind::Capacitor, 4.7e-6),
            ("10p", Kind::Capacitor, 10e-12),
            ("4k7", Kind::Resistor, 4700.0),
            ("4.7k", Kind::Resistor, 4700.0),
            ("10K", Kind::Resistor, 10_000.0),
            ("5.1kΩ", Kind::Resistor, 5100.0),
            ("2R2", Kind::Resistor, 2.2),
            ("0R", Kind::Resistor, 0.0),
            ("100", Kind::Resistor, 100.0),
            ("1M", Kind::Resistor, 1e6),
            ("10uH", Kind::Inductor, 10e-6),
        ];
        for (text, kind, want) in cases {
            let got = parse_value(text, kind).unwrap_or_else(|| panic!("{text}"));
            assert!((got - want).abs() <= want.abs() * 1e-9, "{text}: {got} vs {want}");
        }
        for bad in ["", "STM32F072", "4k7k", "1.2k3", "k"] {
            assert!(parse_value(bad, Kind::Resistor).is_none(), "{bad}");
        }
    }

    #[test]
    fn spellings_match_lcsc_descriptions() {
        assert_eq!(lcsc_spellings(100e-9, Kind::Capacitor), ["100nF"]);
        assert_eq!(lcsc_spellings(4.7e-6, Kind::Capacitor), ["4.7uF", "4700nF"]);
        assert_eq!(lcsc_spellings(1e-9, Kind::Capacitor), ["1nF", "1000pF"]);
        assert_eq!(lcsc_spellings(10e-12, Kind::Capacitor), ["10pF"]);
        assert_eq!(lcsc_spellings(4700.0, Kind::Resistor), ["4.7kΩ"]);
        assert_eq!(lcsc_spellings(5100.0, Kind::Resistor), ["5.1kΩ"]);
        assert_eq!(lcsc_spellings(0.0, Kind::Resistor), ["0Ω"]);
        assert_eq!(lcsc_spellings(1e6, Kind::Resistor), ["1MΩ"]);
    }

    #[test]
    fn ratings_filter_on_the_description() {
        let (r, unknown) = parse_ratings(&["16V", "X7R", "10%", "foo"]);
        assert_eq!(unknown, ["foo"]);
        assert!(rating_failures("100nF 50V X7R ±10%", &r).is_empty());
        assert_eq!(rating_failures("100nF 10V X5R ±10%", &r).len(), 2);
        assert_eq!(rating_failures("100nF X7R ±20%", &r).len(), 2, "unstated voltage fails closed");
        let (p, _) = parse_ratings(&["1/4W"]);
        assert_eq!(p.min_power_w, Some(0.25));
        assert!(rating_failures("-55℃~+155℃ 250mW 10kΩ", &p).is_empty());
        assert!(!rating_failures("-55℃~+155℃ 100mW 10kΩ", &p).is_empty());
    }

    fn seed(dir: &Path) -> PathBuf {
        let db = dir.join("jlc.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE components (LCSC TEXT, MFR_Part TEXT, Package TEXT, Manufacturer TEXT, Library_Type TEXT, Description TEXT, Datasheet TEXT, Price REAL, Stock INTEGER, Category TEXT);
             INSERT INTO components VALUES
              ('C14663','CC0603KRX7R9BB104','0603','YAGEO','Basic','100nF 50V X7R ±10%','',0.001,34000000,''),
              ('C30926','0603B104K500NT','0603','FH','Extended','100nF 50V X7R ±10%','',0.0009,5000000,''),
              ('C1591','CL10B104KO8NNNC','0603','SAMSUNG','Basic','100nF 16V X7R ±10%','',0.0008,9000000,''),
              ('C25804','0603WAF1002T5E','0603','UNI-ROYAL','Basic','-55℃~+155℃ 100mW 10kΩ 75V Thick Film Resistor ±1% ±100ppm/℃','',0.0005,42000000,''),
              ('C99999','CAP-ARRAY-10K','0603','X','Basic','10kΩ capacitor array ±1%','',0.0001,90000000,''),
              ('C81720','STM32F072CBT6','LQFP-48(7x7)','ST','Extended','ARM MCU','',3.2,4000,''),
              ('C165948','TYPE-C-31-M-12','SMD','HRO','Extended','USB-C 16P','',0.2,200000,''),
              ('C12345','LOWSTOCK','0603','X','Basic','100nF 50V X7R ±10%','',0.0001,3,'');",
        )
        .unwrap();
        db
    }

    fn line(refs: &[&str], value: &str, footprint: &str) -> Line {
        Line { references: refs.iter().map(|s| s.to_string()).collect(), value: value.into(), footprint: footprint.into(), mpn: None, lcsc: None }
    }

    #[test]
    fn decisions_against_a_seeded_database() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings { db: seed(dir.path()), boards: 5, prefer_basic: true, overwrite: false };
        let none = BTreeMap::new();
        let d = |l: &Line, o: &BTreeMap<String, String>| decide(l, o, &settings).unwrap();

        // 100n → 100nF; Basic beats the cheaper Extended; 16V part is cheapest Basic.
        let caps = d(&line(&["C1", "C2"], "100n", "Capacitor_SMD:C_0603_1608Metric"), &none);
        assert_eq!(caps["status"], "assign");
        assert_eq!(caps["lcsc"], "C1591");
        // A 25 V requirement excludes the 16 V part.
        let rated = d(&line(&["C3"], "100n 25V", "Capacitor_SMD:C_0603_1608Metric"), &none);
        assert_eq!(rated["lcsc"], "C14663");
        // Resistor: the capacitor-array row with the same value is not a resistor.
        let res = d(&line(&["R1"], "10k", "Resistor_SMD:R_0603_1608Metric"), &none);
        assert_eq!(res["lcsc"], "C25804");
        // MPN by value, package checked through the footprint.
        let mcu = d(&line(&["U1"], "STM32F072CBT6", "Package_QFP:LQFP-48_7x7mm_P0.5mm"), &none);
        assert_eq!(mcu["lcsc"], "C81720");
        // Stock for 5 boards × 1 = 5 > 3: the existing number is a problem.
        let mut low = line(&["C9"], "100n", "Capacitor_SMD:C_0603_1608Metric");
        low.lcsc = Some(("LCSC".into(), "C12345".into()));
        let low = d(&low, &none);
        assert_eq!(low["status"], "problem");
        assert_eq!(low["suggestion"]["lcsc"], "C1591");
        // An explicit choice is checked, not searched; a wrong package is rejected.
        let mut o = BTreeMap::new();
        o.insert("C4".to_string(), "C25804".to_string());
        let wrong = d(&line(&["C4"], "100n", "Capacitor_SMD:C_0402_1005Metric"), &o);
        assert_eq!(wrong["status"], "rejected");
        // Unknown footprint and not a part number: unresolved with a reason.
        let j = d(&line(&["J1"], "USB_C_Receptacle", "Connector_USB:USB_C_Receptacle_HRO_TYPE-C-31-M-12"), &none);
        assert_eq!(j["status"], "unresolved");
        let mut o = BTreeMap::new();
        o.insert("J1".to_string(), "C165948".to_string());
        let j = d(&line(&["J1"], "USB_C_Receptacle", "Connector_USB:USB_C_Receptacle_HRO_TYPE-C-31-M-12"), &o);
        assert_eq!(j["status"], "assign");
        let mut written = line(&["J1"], "USB_C_Receptacle", "Connector_USB:USB_C_Receptacle_HRO_TYPE-C-31-M-12");
        written.lcsc = Some(("LCSC".into(), "C165948".into()));
        assert_eq!(d(&written, &o)["status"], "ok", "an explicit choice already written is not re-assigned");
    }
}
