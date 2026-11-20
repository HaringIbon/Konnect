use super::capacity::{self, Board, Obstacle, Rules};
use super::crossings::{self, Airwire, AirwireEnd, EndLayers};
use super::escape::{self, Package, PackagePad};
use super::geom::{self, Axis, Raster, Shape};
use super::*;
use konnect_sexp::parse_sexp;

const LAYERS: &str = "(layers (0 \"F.Cu\" signal) (4 \"In1.Cu\" power) (2 \"B.Cu\" signal) (25 \"Edge.Cuts\" user))";

/// A `w × h` board with F.Cu / In1.Cu (power) / B.Cu and `extra` items.
fn board_text(w: f64, h: f64, extra: &str) -> String {
    format!("(kicad_pcb (version 20260206) {LAYERS} (gr_rect (start 0 0) (end {w} {h}) (layer \"Edge.Cuts\")) {extra})")
}

fn model(w: f64, h: f64, extra: &str) -> BoardModel {
    BoardModel::read(&parse_sexp(&board_text(w, h, extra)).unwrap())
}

/// A two-pad 0603-ish part at (x, y) with nets n1, n2, pads 1.6 mm apart along x.
fn part(reference: &str, x: f64, y: f64, n1: &str, n2: &str) -> String {
    format!(
        "(footprint \"R\" (layer \"F.Cu\") (at {x} {y}) (property \"Reference\" \"{reference}\") \
         (pad \"1\" smd rect (at -0.8 0) (size 0.6 0.6) (layers \"F.Cu\") (net \"{n1}\")) \
         (pad \"2\" smd rect (at 0.8 0) (size 0.6 0.6) (layers \"F.Cu\") (net \"{n2}\")))"
    )
}

fn rules() -> NetRules {
    NetRules::from_project(None)
}

fn options() -> Options {
    Options { signal_layers: None, cell_mm: 1.0, efficiency: DEFAULT_LAYOUT_EFFICIENCY, efficiency_given: false }
}

fn end(x: f64, y: f64, layers: EndLayers) -> AirwireEnd {
    AirwireEnd { at: (x, y), layers, pad: None }
}

fn wire(net: &str, a: (f64, f64), b: (f64, f64)) -> Airwire {
    Airwire { net: net.into(), a: end(a.0, a.1, EndLayers::One("F.Cu".into())), b: end(b.0, b.1, EndLayers::One("F.Cu".into())) }
}

fn reasons(out: &Value) -> Vec<(String, String)> {
    out["reasons"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["metric"].as_str().unwrap().to_string(), r["level"].as_str().unwrap().to_string()))
        .collect()
}

// ─── Formulas ───────────────────────────────────────────────────────────────

#[test]
fn wang_channel_capacities_match_the_papers_worked_examples() {
    // 225/100/25/25 µm: C = 2, D = 3; 350/200/50/50 µm: C = 1, D = 2.
    assert_eq!(escape::channel_capacity(0.225 - 0.100, 0.025, 0.025), 2);
    assert_eq!(escape::diagonal_capacity(0.225, 0.100, 0.025, 0.025), 3);
    assert_eq!(escape::channel_capacity(0.350 - 0.200, 0.050, 0.050), 1);
    assert_eq!(escape::diagonal_capacity(0.350, 0.200, 0.050, 0.050), 2);
    // With 2p instead of √2·p the first example would give 6, not 3.
    assert_eq!(escape::channel_capacity(2.0 * 0.225 - 0.100, 0.025, 0.025), 6);
    // A 0.5 mm QFN with 0.25 mm pads at 0.2/0.2 rules: nothing between pads.
    assert_eq!(escape::channel_capacity(0.5 - 0.25, 0.2, 0.2), 0);
    // 1.0 mm BGA, 0.5 mm pads, 0.1/0.1: two traces.
    assert_eq!(escape::channel_capacity(0.5, 0.1, 0.1), 2);
}

#[test]
fn ug1099_row_rule_and_equation_2_1() {
    assert_eq!(escape::array_layers_needed(0, 1), Some(0));
    assert_eq!(escape::array_layers_needed(2, 1), Some(1)); // outermost row + one more
    assert_eq!(escape::array_layers_needed(3, 1), Some(2));
    assert_eq!(escape::array_layers_needed(5, 2), Some(2)); // 3 rows, then 2
    assert_eq!(escape::array_layers_needed(6, 2), Some(3));
    assert_eq!(escape::array_layers_needed(1, 0), Some(1));
    assert_eq!(escape::array_layers_needed(2, 0), None);
    // 120 signals, 10×10 array: 36 channels × 1 route → 4 layers.
    assert_eq!(escape::ug1099_layers(120, 10, 1), Some(4));
    assert_eq!(escape::ug1099_layers(120, 10, 2), Some(2));
}

#[test]
fn line_intervals_of_each_shape() {
    let square = Shape::Poly(vec![(0.0, 0.0), (2.0, 0.0), (2.0, 2.0), (0.0, 2.0)]);
    assert_eq!(square.line_intervals(Axis::Horizontal, 1.0), vec![(0.0, 2.0)]);
    assert_eq!(square.line_intervals(Axis::Vertical, 0.5), vec![(0.0, 2.0)]);
    assert!(square.line_intervals(Axis::Horizontal, 3.0).is_empty());
    let circle = Shape::Circle { c: (5.0, 5.0), r: 1.0 };
    let i = circle.line_intervals(Axis::Horizontal, 5.0);
    assert!((i[0].0 - 4.0).abs() < 1e-9 && (i[0].1 - 6.0).abs() < 1e-9);
    let track = Shape::Capsule { a: (0.0, 0.0), b: (10.0, 0.0), r: 0.1 };
    let i = track.line_intervals(Axis::Horizontal, 0.0);
    assert!((i[0].0 + 0.1).abs() < 1e-9 && (i[0].1 - 10.1).abs() < 1e-9, "{i:?}");
    let i = track.line_intervals(Axis::Vertical, 5.0);
    assert!((i[0].0 + 0.1).abs() < 1e-9 && (i[0].1 - 0.1).abs() < 1e-9, "{i:?}");
    let base = vec![(0.0, 10.0)];
    let cuts = geom::merge(vec![(2.0, 3.0), (2.5, 4.0), (8.0, 12.0)]);
    assert_eq!(geom::subtract(&base, &cuts), vec![(0.0, 2.0), (4.0, 8.0)]);
}

#[test]
fn the_raster_measures_free_area() {
    let edges = vec![((0.0, 0.0), (10.0, 0.0)), ((10.0, 0.0), (10.0, 10.0)), ((10.0, 10.0), (0.0, 10.0)), ((0.0, 10.0), (0.0, 0.0))];
    let mut r = Raster::new((0.0, 0.0, 10.0, 10.0), 0.05, 2);
    r.mark_board(&edges);
    assert!((r.total_inside_area() - 100.0).abs() < 0.5);
    r.mark_obstacle(&Shape::Poly(vec![(2.0, 2.0), (4.0, 2.0), (4.0, 7.0), (2.0, 7.0)]), &[0]);
    assert!((r.total_free_area(0) - 90.0).abs() < 0.5, "{}", r.total_free_area(0));
    assert!((r.total_free_area(1) - 100.0).abs() < 0.5);
    assert!((r.free_area_in(0, (0.0, 0.0, 5.0, 5.0)) - 19.0).abs() < 0.3);
}

// ─── Crossings ──────────────────────────────────────────────────────────────

#[test]
fn crossings_are_proper_and_between_different_nets() {
    let ws = vec![
        wire("A", (0.0, 0.0), (10.0, 10.0)),
        wire("B", (0.0, 10.0), (10.0, 0.0)),
        wire("A", (10.0, 10.0), (20.0, 10.0)), // shares an end with the first A
        wire("C", (10.0, 10.0), (10.0, 20.0)), // touches A's end, does not cross
    ];
    let c = crossings::airwire_crossings(&ws);
    assert_eq!(c.len(), 1, "{c:?}");
    assert_eq!((c[0].i, c[0].j), (0, 1));
    assert!((c[0].at.0 - 5.0).abs() < 1e-9 && (c[0].at.1 - 5.0).abs() < 1e-9);
}

#[test]
fn dsatur_colours_triangles_and_bipartite_graphs() {
    assert_eq!(crossings::dsatur(3, &[(0, 1), (1, 2), (0, 2)]).iter().max(), Some(&2));
    let c = crossings::dsatur(4, &[(0, 1), (1, 2), (2, 3), (3, 0)]);
    assert_eq!(c.iter().max(), Some(&1));
    for (a, b) in [(0, 1), (1, 2), (2, 3), (3, 0)] {
        assert_ne!(c[a], c[b]);
    }
}

#[test]
fn a_crossing_bundle_needs_vias_and_more_layers_than_two() {
    // Three mutually crossing SMD airwires (a star through one point region).
    let ws = vec![
        wire("A", (0.0, 5.0), (10.0, 5.2)),
        wire("B", (5.0, 0.0), (5.2, 10.0)),
        wire("C", (0.0, 0.0), (10.0, 10.0)),
    ];
    let c = crossings::airwire_crossings(&ws);
    assert_eq!(c.len(), 3);
    let layers = vec!["F.Cu".to_string(), "B.Cu".to_string()];
    let plan = crossings::plan_layers(&ws, &c, &layers);
    assert_eq!(plan.colours, 3);
    // Best mapping: two classes on F (one unresolved crossing, 2 vias) or
    // one class on B (2 end vias each): 2 + 2 + … ; at least 2 vias.
    assert!(plan.vias_estimate >= 2, "{plan:?}");
    // Two crossing wires only: one goes to B.Cu with 2 end vias.
    let two = &ws[..2];
    let plan = crossings::plan_layers(two, &crossings::airwire_crossings(two), &layers);
    assert_eq!((plan.colours, plan.vias_estimate, plan.unresolved_crossings), (2, 2, 0));
    // Through-hole ends cost nothing on any layer.
    let tht: Vec<Airwire> = two
        .iter()
        .map(|w| Airwire { net: w.net.clone(), a: end(w.a.at.0, w.a.at.1, EndLayers::All), b: end(w.b.at.0, w.b.at.1, EndLayers::All) })
        .collect();
    assert_eq!(crossings::plan_layers(&tht, &crossings::airwire_crossings(&tht), &layers).vias_estimate, 0);
}

#[test]
fn a_crossing_bundle_makes_the_board_tight_never_doomed() {
    // Four parts on a column at x = 5 wired to four parts at x = 15 in
    // reversed order: every pair of airwires crosses.
    let mut extra = String::new();
    for i in 0..4 {
        extra.push_str(&part(&format!("L{i}"), 5.0, 4.0 + 4.0 * i as f64, &format!("N{i}"), "X"));
        extra.push_str(&part(&format!("R{i}"), 15.0, 16.0 - 4.0 * i as f64, &format!("N{i}"), "Y"));
    }
    let m = model(20.0, 20.0, &extra);
    let airwires: Vec<Airwire> = mst_airwires(&m).into_iter().filter(|w| w.net.starts_with('N')).collect();
    assert_eq!(airwires.len(), 4);
    let out = analyse(&m, &rules(), &airwires, None, &options()).unwrap();
    assert_eq!(out["crossings"]["crossings"], 6, "{}", out["crossings"]);
    assert_eq!(out["verdict"], "tight", "{:?}", reasons(&out));
    assert!(reasons(&out).iter().any(|r| r == &("crossings".to_string(), "tight".to_string())));
    assert!(!reasons(&out).iter().any(|r| r.1 == "doomed"));
    let pairs = out["crossings"]["worst_net_pairs"].as_array().unwrap();
    assert_eq!(pairs.len(), 6);
    // Two-pad parts get no rotation what-if.
    assert!(out["rotation_hints"].as_array().unwrap().is_empty());
}

#[test]
fn a_rotation_what_if_finds_the_turn_that_uncrosses_a_part() {
    // U1's west pad goes east and its east pad goes west: the two airwires
    // cross over the part. Turned 180° they do not.
    let extra = format!("{}{}{}", qfn("U1", 10.0, 10.0, 4, "GNDX"), part("R1", 17.0, 9.0, "U1_W1", "Q"), part("R2", 3.0, 9.0, "U1_E2", "Q2"));
    let m = model(20.0, 20.0, &extra);
    let out = analyse(&m, &rules(), &mst_airwires(&m), None, &options()).unwrap();
    assert_eq!(out["crossings"]["crossings"], 1, "{}", out["crossings"]);
    let hint = out["rotation_hints"].as_array().unwrap().iter().find(|h| h["reference"] == "U1").expect("a hint for U1");
    assert_eq!(hint["best"]["rotate_by_deg"], 180.0, "{hint}");
    assert_eq!(hint["best"]["crossings"], 0);
    assert!(hint["best"]["airwire_mm"].as_f64().unwrap() < hint["now"]["airwire_mm"].as_f64().unwrap());
}

// ─── Escape ─────────────────────────────────────────────────────────────────

/// A QFN-like part: `n` pads per side at 0.5 mm pitch, 0.25 × 0.8 mm pads,
/// a 2 × 2 mm centre pad on net `ep`. Pad k of the south side is net Sk.
fn qfn(reference: &str, x: f64, y: f64, n: usize, ep: &str) -> String {
    let mut s = format!("(footprint \"QFN\" (layer \"F.Cu\") (at {x} {y}) (property \"Reference\" \"{reference}\")");
    let span = (n as f64 - 1.0) * 0.5;
    let mut number = 1;
    for k in 0..n {
        let t = -span / 2.0 + k as f64 * 0.5;
        let sides = [
            (-2.0, t, 0.8, 0.25, format!("W{k}")),
            (t, 2.0, 0.25, 0.8, format!("S{k}")),
            (2.0, -t, 0.8, 0.25, format!("E{k}")),
            (-t, -2.0, 0.25, 0.8, format!("N{k}")),
        ];
        for (px, py, w, h, net) in sides {
            s.push_str(&format!(" (pad \"{number}\" smd rect (at {px} {py}) (size {w} {h}) (layers \"F.Cu\") (net \"{reference}_{net}\"))"));
            number += 1;
        }
    }
    s.push_str(&format!(" (pad \"{number}\" smd rect (at 0 0) (size 2 2) (layers \"F.Cu\") (net \"{ep}\")))"));
    s
}

#[test]
fn a_qfn_with_no_channel_sends_its_interior_net_down_a_via() {
    // EP on a signal net that goes to a resistor west of the part.
    let extra = format!("{}{}", qfn("U1", 10.0, 10.0, 4, "SIG"), part("R1", 3.0, 10.0, "SIG", "U1_W0"));
    let m = model(20.0, 20.0, &extra);
    let airwires = mst_airwires(&m);
    let out = analyse(&m, &rules(), &airwires, None, &options()).unwrap();
    let u1 = out["escape"].as_array().unwrap().iter().find(|e| e["reference"] == "U1").expect("U1 analysed");
    assert_eq!(u1["kind"], "perimeter");
    assert_eq!(u1["sides"].as_array().unwrap().len(), 4);
    for side in u1["sides"].as_array().unwrap() {
        assert_eq!(side["channel_capacity"], 0, "{side}");
        assert_eq!(side["pitch_mm"], 0.5);
    }
    assert_eq!(u1["interior_demand"][0]["net"], "SIG");
    assert_eq!(u1["interior_needs_via"][0]["net"], "SIG", "{u1}");
    assert_eq!(u1["layers_needed"], 2);
    assert!(!reasons(&out).iter().any(|r| r.1 == "doomed"), "{:?}", reasons(&out));
    // On one signal layer the same part is doomed (sourced threshold).
    let one = Options { signal_layers: Some(vec!["F.Cu".into()]), ..options() };
    let out = analyse(&m, &rules(), &airwires, None, &one).unwrap();
    assert_eq!(out["verdict"], "doomed");
    let reason = out["reasons"].as_array().unwrap().iter().find(|r| r["metric"] == "escape").unwrap();
    assert_eq!(reason["threshold"]["kind"], "sourced");
    assert!(reason["threshold"]["source"].as_str().unwrap().contains("wang-iccad06"));
}

#[test]
fn a_ring_pad_wired_to_a_part_underneath_needs_a_via_outside_the_ring() {
    let under = "(footprint \"C\" (layer \"B.Cu\") (at 10 10) (property \"Reference\" \"C9\") \
        (pad \"1\" smd rect (at -0.5 0) (size 0.5 0.5) (layers \"B.Cu\") (net \"U1_S1\")) \
        (pad \"2\" smd rect (at 0.5 0) (size 0.5 0.5) (layers \"B.Cu\") (net \"Z\")))";
    let m = model(20.0, 20.0, &format!("{}{under}", qfn("U1", 10.0, 10.0, 4, "GNDX")));
    let out = analyse(&m, &rules(), &mst_airwires(&m), None, &options()).unwrap();
    let u1 = out["escape"].as_array().unwrap().iter().find(|e| e["reference"] == "U1").unwrap();
    assert_eq!(u1["via_outside_ring"][0]["net"], "U1_S1", "{u1}");
}

#[test]
fn a_net_whose_far_end_is_behind_the_part_is_flagged() {
    // U1's west pad W0 goes to a part east of U1.
    let extra = format!("{}{}", qfn("U1", 8.0, 10.0, 4, "GNDX"), part("R1", 16.0, 10.0, "U1_W0", "Q"));
    let m = model(20.0, 20.0, &extra);
    let out = analyse(&m, &rules(), &mst_airwires(&m), None, &options()).unwrap();
    let u1 = out["escape"].as_array().unwrap().iter().find(|e| e["reference"] == "U1").unwrap();
    let west = u1["sides"].as_array().unwrap().iter().find(|s| s["faces"] == "west").unwrap();
    assert_eq!(west["behind"][0]["net"], "U1_W0", "{west}");
}

#[test]
fn a_bga_grid_counts_rows_and_layers() {
    // 6×6 array at 0.8 mm, 0.4 mm round pads, every ball a signal: C at
    // 0.2/0.2 is 0, so the inner rows cannot escape at all.
    let mut pads = Vec::new();
    for j in 0..6 {
        for i in 0..6 {
            pads.push(PackagePad { number: format!("{i}{j}"), net: Some(format!("B{i}{j}")), local: (i as f64 * 0.8, j as f64 * 0.8), half: (0.2, 0.2), demand: true, round: true, rule: None });
        }
    }
    let package = Package { reference: "U9".into(), pads, rotation: 0.0 };
    let report = escape::analyse_package(&package, &vec![Vec::new(); 36], 0.2, 0.2);
    let grid = report.grid.as_ref().unwrap();
    assert_eq!((grid.columns, grid.rows, grid.signal_rows, grid.c), (6, 6, 3, 0));
    assert_eq!(report.layers_needed, None);
    // At 0.1/0.1, C = 1 and D = floor((1.131 − 0.4 − 0.1)/0.2) = 3: 3 rows → 2 layers.
    let report = escape::analyse_package(&package, &vec![Vec::new(); 36], 0.1, 0.1);
    let grid = report.grid.as_ref().unwrap();
    assert_eq!((grid.c, grid.d), (1, 3));
    assert_eq!(report.layers_needed, Some(2));
    assert_eq!(grid.boundary_channels, 20);
}

// ─── Cuts ───────────────────────────────────────────────────────────────────

/// A wall keep-out across the board at y 9.5..10.5 from x = 0 to `to`.
fn wall(to: f64) -> String {
    format!(
        "(zone (layers \"F.Cu\" \"In1.Cu\" \"B.Cu\") (keepout (tracks not_allowed) (vias not_allowed)) \
         (polygon (pts (xy -1 9.5) (xy {to} 9.5) (xy {to} 10.5) (xy -1 10.5))))"
    )
}

fn crossing_nets(n: usize) -> String {
    (0..n).map(|i| format!("{}{}", part(&format!("T{i}"), 2.0 + 2.0 * i as f64, 4.0, &format!("V{i}"), "TA"), part(&format!("B{i}"), 2.0 + 2.0 * i as f64, 16.0, &format!("V{i}"), "BA"))).collect()
}

#[test]
fn a_keepout_wall_with_a_narrow_gap_proves_doom() {
    // Gap of 1 mm at x 19..20: ⌊(1 − 0.2)/0.4⌋ = 2 lanes per layer, 4 in all.
    // Six nets must get from the top half to the bottom half.
    let m = model(20.0, 20.0, &format!("{}{}", wall(19.0), crossing_nets(6)));
    let airwires: Vec<Airwire> = mst_airwires(&m).into_iter().filter(|w| w.net.starts_with('V')).collect();
    assert_eq!(airwires.len(), 6);
    let out = analyse(&m, &rules(), &airwires, None, &options()).unwrap();
    assert_eq!(out["verdict"], "doomed", "{:?}", reasons(&out));
    let doomed = &out["cuts"]["doomed"][0];
    assert_eq!(doomed["axis"], "horizontal");
    assert_eq!(doomed["demand_nets_lower_bound"], 6);
    assert_eq!(doomed["capacity_lanes"], 4, "{doomed}");
    let reason = out["reasons"].as_array().unwrap().iter().find(|r| r["metric"] == "cuts").unwrap();
    assert_eq!(reason["threshold"]["kind"], "sourced");
    // Four nets fit through the same gap.
    let m = model(20.0, 20.0, &format!("{}{}", wall(19.0), crossing_nets(4)));
    let airwires: Vec<Airwire> = mst_airwires(&m).into_iter().filter(|w| w.net.starts_with('V')).collect();
    let out = analyse(&m, &rules(), &airwires, None, &options()).unwrap();
    assert!(out["cuts"]["doomed"].as_array().unwrap().is_empty(), "{}", out["cuts"]);
}

#[test]
fn a_wall_that_routes_can_go_round_is_not_a_proof() {
    // The wall stops 6 mm short of the right edge: plenty of room round it.
    let m = model(20.0, 20.0, &format!("{}{}", wall(14.0), crossing_nets(6)));
    let airwires: Vec<Airwire> = mst_airwires(&m).into_iter().filter(|w| w.net.starts_with('V')).collect();
    let out = analyse(&m, &rules(), &airwires, None, &options()).unwrap();
    assert!(out["cuts"]["doomed"].as_array().unwrap().is_empty());
    assert_ne!(out["verdict"], "doomed", "{:?}", reasons(&out));
}

#[test]
fn a_partly_routed_net_already_across_the_cut_owes_no_crossing() {
    // V0 has its own track across y = 10 (through the gap): it can join on
    // either side, so only the other five must cross.
    let track = "(segment (start 19.5 4) (end 19.5 16) (width 0.2) (layer \"B.Cu\") (net \"V0\"))";
    let m = model(20.0, 20.0, &format!("{}{}{track}", wall(19.0), crossing_nets(6)));
    let airwires: Vec<Airwire> = mst_airwires(&m).into_iter().filter(|w| w.net.starts_with('V')).collect();
    let out = analyse(&m, &rules(), &airwires, None, &options()).unwrap();
    let doomed = &out["cuts"]["doomed"][0];
    assert_eq!(doomed["demand_nets_lower_bound"], 5, "{doomed}");
    // The track splits B.Cu's 1 mm gap into two 0.4 mm slivers: no lane left there.
    assert_eq!(doomed["capacity_lanes_per_layer"], json!([2, 0]), "{doomed}");
}

#[test]
fn channel_cuts_name_the_parts_and_never_doom() {
    // A1 is an 8-pad part (channels are examined next to ICs and connectors).
    let pads: String = (0..8).map(|k| format!("(pad \"{}\" smd rect (at -2 {}) (size 0.5 0.3) (layers \"F.Cu\") (net \"Q\"))", k + 1, -2.0 + 0.5 * k as f64)).collect();
    let a = format!("(footprint \"A\" (layer \"F.Cu\") (at 5 10) (property \"Reference\" \"A1\") (fp_rect (start -3 -3) (end 3 3) (layer \"F.CrtYd\")) {pads})");
    let b = "(footprint \"A\" (layer \"F.Cu\") (at 13 10) (property \"Reference\" \"A2\") (fp_rect (start -3 -3) (end 3 3) (layer \"F.CrtYd\")) \
        (pad \"1\" smd rect (at 0 0) (size 1 1) (layers \"F.Cu\") (net \"Q\")))";
    let m = model(20.0, 20.0, &format!("{a}{b}"));
    let airwires: Vec<Airwire> = (0..5).map(|i| wire(&format!("W{i}"), (8.6 + 0.1 * i as f64, 1.0), (8.7 + 0.1 * i as f64, 19.0))).collect();
    let out = analyse(&m, &rules(), &airwires, None, &options()).unwrap();
    let channel = &out["channels"][0];
    assert_eq!(channel["between"], json!(["A1", "A2"]), "{}", out["channels"]);
    assert!((channel["gap_mm"].as_f64().unwrap() - 2.0).abs() < 1e-6);
    assert_eq!(channel["demand_airwires"], 5);
    assert_eq!(channel["proves_doom"], false);
}

// ─── Partial routing, airwires ──────────────────────────────────────────────

#[test]
fn the_spanning_tree_joins_existing_copper_at_zero_weight() {
    // Three pads of N; R1.2 and R2.1 joined by a track: one airwire left,
    // from the joined pair's nearest anchor to R3.
    let extra = format!(
        "{}{}{}(segment (start 5.8 5) (end 9.2 5) (width 0.2) (layer \"F.Cu\") (net \"N\"))",
        part("R1", 5.0, 5.0, "A", "N"),
        part("R2", 10.0, 5.0, "N", "B"),
        part("R3", 10.0, 12.0, "N", "C")
    );
    let m = model(20.0, 20.0, &extra);
    let ws: Vec<Airwire> = mst_airwires(&m).into_iter().filter(|w| w.net == "N").collect();
    assert_eq!(ws.len(), 1, "{ws:?}");
    let length = ws[0].length();
    assert!(length < 7.5, "{ws:?}");
}

#[test]
fn airwires_crossing_existing_copper_are_counted_per_layer() {
    let track = "(segment (start 10 0.5) (end 10 19.5) (width 0.2) (layer \"F.Cu\") (net \"OTHER\"))";
    let m = model(20.0, 20.0, &format!("{}{}{track}", part("R1", 4.0, 10.0, "A", "N"), part("R2", 16.0, 10.0, "N", "B")));
    let out = analyse(&m, &rules(), &mst_airwires(&m), None, &options()).unwrap();
    assert_eq!(out["crossings"]["crossing_existing_copper_per_layer"]["F.Cu"], 1, "{}", out["crossings"]);
    assert_eq!(out["crossings"]["crossing_existing_copper_per_layer"]["B.Cu"], 0);
}

#[test]
fn drc_unconnected_items_become_airwires_between_the_nearest_anchors() {
    use crate::tools::cli::{DrcViolation, ReportItem, ReportPos};
    let text = board_text(
        20.0,
        20.0,
        "(footprint \"R\" (layer \"F.Cu\") (at 5 5) (property \"Reference\" \"R1\") \
           (pad \"1\" smd rect (at 0 0) (size 0.6 0.6) (layers \"F.Cu\") (net \"N\") (uuid \"pad-1\"))) \
         (segment (start 9 5) (end 15 5) (width 0.2) (layer \"B.Cu\") (net \"N\") (uuid \"trk-1\"))",
    );
    let m = BoardModel::read(&parse_sexp(&text).unwrap());
    let item = |description: &str, uuid: &str, x: f64, y: f64| ReportItem {
        description: description.into(),
        pos: Some(ReportPos { x, y }),
        uuid: Some(uuid.into()),
        kicad_reported_pos: None,
        ownership_status: None,
        item_kind: None,
        layer: None,
        owner: None,
    };
    let violation = DrcViolation {
        severity: "error".into(),
        description: "Missing connection between items".into(),
        rule: "unconnected_items".into(),
        pos: None,
        items: vec![item("Pad 1 [N] of R1 on F.Cu", "pad-1", 5.0, 5.0), item("Track [N] on B.Cu, length 6 mm", "trk-1", 15.0, 5.0)],
    };
    let ws = airwires_from_drc(&[violation], &m);
    assert_eq!(ws.len(), 1);
    assert_eq!(ws[0].net, "N");
    assert_eq!(ws[0].a.pad, Some(("R1".to_string(), "1".to_string())));
    assert_eq!(ws[0].b.at, (9.0, 5.0), "the track end nearest the pad, not KiCad's reported start");
    assert_eq!(ws[0].b.layers, EndLayers::One("B.Cu".into()));
}

// ─── Planes, group, density, congestion ─────────────────────────────────────

#[test]
fn plane_nets_are_not_demand() {
    let extra = format!(
        "{}{}(zone (net \"GND\") (layers \"In1.Cu\") (polygon (pts (xy 0 0) (xy 20 0) (xy 20 20) (xy 0 20))))",
        part("R1", 4.0, 10.0, "GND", "N"),
        part("R2", 16.0, 10.0, "N", "GND")
    );
    let m = model(20.0, 20.0, &extra);
    let out = analyse(&m, &rules(), &mst_airwires(&m), None, &options()).unwrap();
    assert_eq!(out["airwires"]["plane_net_airwires"], 1);
    assert_eq!(out["airwires"]["demand"], 1);
    assert_eq!(out["airwires"]["plane_nets"]["GND"], "plane on In1.Cu");
}

#[test]
fn a_mixed_layer_pour_is_blocked_and_its_net_left_to_it() {
    let text = format!(
        "(kicad_pcb (version 20260206) (layers (0 \"F.Cu\" signal) (4 \"In1.Cu\" mixed) (2 \"B.Cu\" signal) (25 \"Edge.Cuts\" user)) \
         (gr_rect (start 0 0) (end 20 20) (layer \"Edge.Cuts\")) {}{} \
         (zone (net \"P\") (layers \"In1.Cu\") (polygon (pts (xy 0 0) (xy 20 0) (xy 20 10) (xy 0 10))))",
        part("R1", 4.0, 15.0, "P", "N"),
        part("R2", 16.0, 15.0, "N", "P")
    );
    let m = BoardModel::read(&parse_sexp(&format!("{text})")).unwrap());
    let out = analyse(&m, &rules(), &mst_airwires(&m), None, &options()).unwrap();
    assert_eq!(out["rules"]["signal_layers"], json!(["F.Cu", "In1.Cu", "B.Cu"]));
    assert_eq!(out["airwires"]["plane_net_airwires"], 1);
    let caps = &out["density"]["per_layer_capacity_mm"];
    assert!(caps["In1.Cu"].as_f64().unwrap() < 0.55 * caps["B.Cu"].as_f64().unwrap(), "{caps}");
}

#[test]
fn a_group_limits_demand_and_unknown_names_are_refused() {
    let m = model(20.0, 20.0, &format!("{}{}", part("R1", 4.0, 10.0, "A", "N"), part("R2", 16.0, 10.0, "N", "A")));
    let r = rules();
    let group = Group::resolve(vec!["N".into()], vec![], &m, &r).unwrap();
    let out = analyse(&m, &r, &mst_airwires(&m), Some(&group), &options()).unwrap();
    assert_eq!(out["airwires"]["demand"], 1);
    assert_eq!(out["airwires"]["outside_group"], 1);
    assert!(Group::resolve(vec!["NOPE".into()], vec![], &m, &r).unwrap_err().contains("NOPE"));
    assert!(Group::resolve(vec![], vec!["Power".into()], &m, &r).unwrap_err().contains("Power"));
    let all = Group::resolve(vec![], vec!["Default".into()], &m, &r).unwrap();
    assert!(all.nets.contains("A") && all.nets.contains("N"));
}

#[test]
fn net_classes_come_from_the_project_patterns() {
    let project = json!({
        "net_settings": {
            "classes": [ { "name": "Default", "track_width": 0.2, "clearance": 0.2 }, { "name": "Power", "track_width": 0.5, "clearance": 0.25 } ],
            "netclass_patterns": [ { "pattern": "+5V", "netclass": "Power" }, { "pattern": "/VBUS*", "netclass": "Power" } ]
        },
        "board": { "design_settings": { "rules": { "min_track_width": 0.127, "min_clearance": 0.15, "min_copper_edge_clearance": 0.5 } } }
    });
    let r = NetRules::from_project(Some(&project));
    assert_eq!(r.rule("+5V"), (0.5, 0.25));
    assert_eq!(r.rule("/VBUS_F"), (0.5, 0.25));
    assert_eq!(r.rule("/SDA"), (0.2, 0.2));
    assert_eq!(r.class_of("/SDA"), "Default");
    assert_eq!((r.fab_w, r.fab_s, r.edge_clearance), (0.127, 0.15, 0.5));
}

#[test]
fn holden_density_verdicts_follow_u() {
    let m = model(20.0, 20.0, &format!("{}{}", part("R1", 2.0, 2.0, "A", "N"), part("R2", 18.0, 18.0, "N", "B")));
    let ws = mst_airwires(&m);
    let out = analyse(&m, &rules(), &ws, None, &options()).unwrap();
    let raw = out["density"]["raw_utilization"].as_f64().unwrap();
    assert!(raw > 0.0 && raw < 0.1, "{}", out["density"]);
    assert_eq!(out["verdict"], "fine", "{:?}", reasons(&out));
    assert!(out["density"]["layout_efficiency_source"].as_str().unwrap().contains("calibrated"));
    // E chosen so U is 0.9 → tight (sourced 20 % margin), then 1.5 → doomed.
    for (u, level) in [(0.9, "tight"), (1.5, "doomed")] {
        let o = Options { efficiency: raw / u, efficiency_given: true, ..options() };
        let out = analyse(&m, &rules(), &ws, None, &o).unwrap();
        assert_eq!(out["verdict"], level, "{}", out["density"]);
        let reason = out["reasons"].as_array().unwrap().iter().find(|r| r["metric"] == "density").unwrap();
        assert_eq!(reason["threshold"]["kind"], "sourced");
    }
}

#[test]
fn rudy_conserves_wire_length_and_finds_overflow() {
    let edges = vec![((0.0, 0.0), (20.0, 0.0)), ((20.0, 0.0), (20.0, 20.0)), ((20.0, 20.0), (0.0, 20.0)), ((0.0, 20.0), (0.0, 0.0))];
    let mut raster = Raster::new((0.0, 0.0, 20.0, 20.0), 0.1, 1);
    raster.mark_board(&edges);
    let layers = vec!["F.Cu".to_string()];
    let rules = Rules { w: 0.2, s: 0.2, s_min: 0.2, fab_w: 0.2, fab_s: 0.2 };
    let board = Board { layers: &layers, edges: &edges, obstacles: &[], tracks: &[], raster: &raster, rules };
    let ws = vec![wire("A", (2.0, 2.0), (12.0, 7.0))];
    let c = capacity::congestion(&board, &ws, &[0.4], 1.0);
    // One wire, 15 mm of RUDY over 50 mm² of 400 cells: no overflow.
    assert_eq!(c.overflow_cells, 0);
    assert_eq!(c.cells, 400);
    // 40 parallel wires through one 1 mm row: 40 tracks against 2.5 per cell.
    let many: Vec<Airwire> = (0..40).map(|i| wire(&format!("W{i}"), (1.0, 10.5), (19.0, 10.5))).collect();
    let c = capacity::congestion(&board, &many, &vec![0.4; 40], 1.0);
    assert!(c.overflow_cells >= 18, "{c:?}");
    assert!(c.worst_cells[0].utilization > 5.0);
    assert!((c.worst_cells[0].y - 10.5).abs() < 1e-9);
    // A pad covering a cell takes its supply away (SimPLR: obstacles count).
    let pad = Obstacle { shape: Shape::Poly(vec![(5.0, 5.0), (6.0, 5.0), (6.0, 6.0), (5.0, 6.0)]), layers: vec![0], net: None, pad: true };
    raster.mark_obstacle(&pad.shape, &pad.layers);
    let board = Board { layers: &layers, edges: &edges, obstacles: std::slice::from_ref(&pad), tracks: &[], raster: &raster, rules };
    let c = capacity::congestion(&board, &[wire("A", (5.2, 5.5), (5.8, 5.5))], &[0.4], 1.0);
    assert_eq!((c.overflow_cells, c.blocked_cells_with_demand), (0, 1), "a wholly blocked cell is a pin field, not overflow: {c:?}");
    // A pad covering 90 % of a cell leaves 0.25 mm of track; 3 wires through it overflow.
    let mut raster = Raster::new((0.0, 0.0, 20.0, 20.0), 0.1, 1);
    raster.mark_board(&edges);
    raster.mark_obstacle(&Shape::Poly(vec![(5.0, 5.0), (6.0, 5.0), (6.0, 5.9), (5.0, 5.9)]), &[0]);
    let board = Board { layers: &layers, edges: &edges, obstacles: &[], tracks: &[], raster: &raster, rules };
    let three: Vec<Airwire> = (0..3).map(|i| wire(&format!("W{i}"), (5.0, 5.95), (6.0, 5.95))).collect();
    let c = capacity::congestion(&board, &three, &[0.4; 3], 1.0);
    assert_eq!(c.overflow_cells, 1, "{c:?}");
}

#[test]
fn every_threshold_is_labelled() {
    for t in THRESHOLDS {
        assert!(matches!(t.kind, "sourced" | "judgement"), "{}", t.id);
        assert!(matches!(t.level, "doomed" | "tight"), "{}", t.id);
        if t.kind == "sourced" {
            assert!(t.source.contains('-'), "{}: a sourced threshold names a doc-DB slug", t.id);
        }
        if t.level == "doomed" {
            assert_eq!(t.kind, "sourced", "{}: only sourced thresholds may doom a board", t.id);
        }
    }
}

#[test]
fn signal_layers_override_is_checked() {
    let m = model(20.0, 20.0, &part("R1", 4.0, 10.0, "A", "N"));
    let bad = Options { signal_layers: Some(vec!["In9.Cu".into()]), ..options() };
    assert!(analyse(&m, &rules(), &[], None, &bad).unwrap_err().contains("In9.Cu"));
    let empty = model(20.0, 20.0, "");
    let out = analyse(&empty, &rules(), &[], None, &options()).unwrap();
    assert_eq!(out["verdict"], "fine");
}

#[tokio::test]
async fn the_tool_answers_from_a_saved_board_with_the_spanning_tree() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.kicad_pcb");
    std::fs::write(&path, board_text(20.0, 20.0, &format!("{}{}", part("R1", 4.0, 10.0, "A", "N"), part("R2", 16.0, 10.0, "N", "A")))).unwrap();
    let ctx = crate::tools::ToolContext::new(crate::tools::ServerConfig::default(), std::sync::Arc::new(crate::router::ToolRouter::new()));
    let result = handle_check_routability(&json!({ "board": path, "airwires": "mst" }), &ctx).await.unwrap();
    assert!(!result.is_error, "{result:?}");
    let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else { panic!() };
    let out: Value = serde_json::from_str(text).unwrap();
    assert_eq!(out["verdict"], "fine");
    assert_eq!(out["board_source"], "saved_file");
    assert_eq!(out["airwires"]["source"], "mst");
    assert_eq!(out["airwires"]["demand"], 2);
    let refused = handle_check_routability(&json!({ "board": path, "airwires": "mst", "nets": ["NOPE"] }), &ctx).await.unwrap();
    assert!(refused.is_error);
}

/// Prints the full report for `ROUTABILITY_BOARD` (KiCad's ratsnest via
/// kicad-cli). Used for the Pass 23 calibration on the benchmark boards.
#[tokio::test]
#[ignore = "needs kicad-cli and ROUTABILITY_BOARD"]
async fn report_for_a_real_board() {
    let board = std::env::var("ROUTABILITY_BOARD").expect("set ROUTABILITY_BOARD");
    let mut args = json!({ "board": board });
    if let Ok(extra) = std::env::var("ROUTABILITY_ARGS") {
        for (k, v) in serde_json::from_str::<serde_json::Map<String, Value>>(&extra).unwrap() {
            args[k] = v;
        }
    }
    let ctx = crate::tools::ToolContext::new(crate::tools::ServerConfig::default(), std::sync::Arc::new(crate::router::ToolRouter::new()));
    let started = std::time::Instant::now();
    let result = handle_check_routability(&args, &ctx).await.unwrap();
    let crate::mcp::protocol::ToolContent::Text { text } = &result.content[0] else { panic!() };
    println!("ELAPSED {:.2}", started.elapsed().as_secs_f64());
    println!("REPORT {text}");
}

#[test]
fn fanout_pitch_uses_each_neighbours_own_track_width() {
    // An SOIC-like column of 8 pads at 0.5 mm pitch on each side.
    let column = |x: f64, power: Option<usize>| -> Vec<PackagePad> {
        (0..8)
            .map(|k| PackagePad {
                number: format!("{x}{k}"),
                net: Some(format!("N{x}{k}")),
                local: (x, k as f64 * 0.5),
                half: (0.6, 0.15),
                demand: true,
                round: false,
                rule: Some(if Some(k) == power { (0.53, 0.2) } else { (0.2, 0.2) }),
            })
            .collect()
    };
    let far = vec![Vec::new(); 16];
    let plain = Package { reference: "U5".into(), pads: [column(-3.0, None), column(3.0, None)].concat(), rotation: 0.0 };
    let report = escape::analyse_package(&plain, &far, 0.2, 0.2);
    assert!(report.sides.iter().all(|s| s.fanout_pitch_ok), "{:?}", report.sides);
    // One 0.53 mm power trace beside a 0.2 mm one needs 0.565 mm: too tight at 0.5.
    let power = Package { reference: "U5".into(), pads: [column(-3.0, Some(3)), column(3.0, None)].concat(), rotation: 0.0 };
    let report = escape::analyse_package(&power, &far, 0.2, 0.2);
    let west = report.sides.iter().find(|s| s.faces == "west").unwrap();
    assert!(!west.fanout_pitch_ok);
    assert_eq!((west.tightest_demand_pitch_mm, west.needed_pitch_mm), (Some(0.5), Some(0.565)));
    assert!(report.sides.iter().find(|s| s.faces == "east").unwrap().fanout_pitch_ok);
}

#[test]
fn pads_too_close_even_for_fab_minimum_traces_make_the_board_tight() {
    // Two neighbouring south pads (0.5 mm apart) both wired out.
    let extra = format!("{}{}{}", qfn("U1", 10.0, 8.0, 4, "GNDX"), part("R1", 9.0, 16.0, "U1_S1", "Q1"), part("R2", 12.0, 16.0, "U1_S2", "Q2"));
    let m = model(20.0, 20.0, &extra);
    let airwires = mst_airwires(&m);
    // Default rules (0.2/0.2, fab minimum the same): 0.5 mm is enough.
    let out = analyse(&m, &rules(), &airwires, None, &options()).unwrap();
    assert!(!out["reasons"].as_array().unwrap().iter().any(|r| r["threshold"]["id"] == "escape.fanout_pitch"), "{}", out["reasons"]);
    // A 0.3/0.3 process: 0.6 mm needed even necked down.
    let project = json!({
        "net_settings": { "classes": [ { "name": "Default", "track_width": 0.3, "clearance": 0.3 } ] },
        "board": { "design_settings": { "rules": { "min_track_width": 0.3, "min_clearance": 0.3 } } }
    });
    let coarse = NetRules::from_project(Some(&project));
    let out = analyse(&m, &coarse, &airwires, None, &options()).unwrap();
    let reason = out["reasons"].as_array().unwrap().iter().find(|r| r["threshold"]["id"] == "escape.fanout_pitch").expect("fan-out reason");
    assert_eq!(reason["level"], "tight");
    assert!(reason["detail"].as_str().unwrap().contains("U1 south"), "{reason}");
}
