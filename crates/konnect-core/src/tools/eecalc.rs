//! Engineering calculators for the AI's own design decisions: trace width
//! from current, copper resistance and voltage drop, transmission-line
//! impedance, and standard component values.
//!
//! Every result carries the method, the assumptions it rests on, the range
//! where that method holds, and warnings when an input leaves that range, so
//! a decision made from it can be traced and its limits respected. Nothing
//! here is formatted for people; the tool returns these structs as JSON.
//!
//! Units: lengths in mm, current in A, temperature in °C, resistance in Ω.

use serde::Serialize;

/// 1 oz/ft² of copper is 34.79 µm thick (IPC-2221 uses 1.37 mil).
pub(crate) const MM_PER_OZ: f64 = 0.03479;
const MIL_PER_MM: f64 = 1000.0 / 25.4;
/// IACS standard annealed copper at 20 °C, Ω·m (1/58 Ω·mm²/m). PCB copper is
/// specified against this (IPC-2221 §6.2 / IEC 60028).
const RHO_CU_20: f64 = 1.7241e-8;
/// Free-space wave impedance, Ω.
const ETA0: f64 = 376.730_313;

/// Copper resistivity relative to 20 °C, from the NIST reference table
/// (Matula 1979, J. Phys. Chem. Ref. Data 8 p.1147; as tabulated in the
/// EELearn materials data, `content/data/parts.json`), linearly interpolated.
/// Temperature in K, value ρ in Ω·m for pure copper; only the ratio is used.
const MATULA_CU: &[(f64, f64)] = &[
    (200.0, 1.046e-8),
    (250.0, 1.387e-8),
    (273.15, 1.543e-8),
    (293.0, 1.678e-8),
    (300.0, 1.725e-8),
    (350.0, 2.063e-8),
    (400.0, 2.402e-8),
    (500.0, 3.090e-8),
];

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Calc<T: Serialize> {
    pub result: T,
    pub method: &'static str,
    pub assumptions: Vec<String>,
    pub valid_range: &'static str,
    pub warnings: Vec<String>,
}

fn rho_cu(temp_c: f64) -> f64 {
    let k = temp_c + 273.15;
    let at = |k: f64| {
        let table = MATULA_CU;
        let k = k.clamp(table[0].0, table[table.len() - 1].0);
        let i = table.windows(2).position(|w| k <= w[1].0).unwrap_or(table.len() - 2);
        let ((k0, r0), (k1, r1)) = (table[i], table[i + 1]);
        r0 + (r1 - r0) * (k - k0) / (k1 - k0)
    };
    RHO_CU_20 * at(k) / at(293.15)
}

// ─── Current capacity (IPC-2152, Brooks & Adam fit) ─────────────────────────

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WidthForCurrent {
    pub width_mm: f64,
    pub cross_section_mil2: f64,
    pub layer: &'static str,
    pub copper_oz: f64,
    pub temp_rise_c: f64,
}

/// Brooks & Adam's fit ΔT = K·I^a·W^b·Th^c to the IPC-2152 data (W, Th in
/// mil, I in A), as KiCad's calculator uses it since commit 7f558e2779
/// (master, 2026-07; KiCad 10.0.x still uses IPC-2221). External: one fit for
/// all copper weights. Internal: per weight, picked by the nearest nominal
/// thickness (0.5/1/2/3 oz = 0.689/1.378/2.756/4.134 mil), exactly as KiCad.
fn brooks_adam(inner: bool, thickness_mil: f64) -> (f64, f64, f64, f64) {
    const HALF: f64 = 0.689;
    const ONE: f64 = 1.378;
    const TWO: f64 = 2.756;
    const THREE: f64 = 4.134;
    if !inner {
        (215.3, 2.0, -1.15, -1.0)
    } else if thickness_mil < (HALF + ONE) / 2.0 {
        (120.0, 2.0, -1.10, -1.52)
    } else if thickness_mil < (ONE + TWO) / 2.0 {
        (200.0, 1.9, -1.10, -1.52)
    } else if thickness_mil < (TWO + THREE) / 2.0 {
        (300.0, 2.0, -1.15, -1.52)
    } else {
        (262.5, 1.9, -1.15, -1.52)
    }
}

/// IPC-2152 width in mil for a copper thickness in mil.
pub(crate) fn ipc2152_width_mil(current_a: f64, temp_rise_c: f64, thickness_mil: f64, inner: bool) -> f64 {
    let (k, a, b, c) = brooks_adam(inner, thickness_mil);
    (temp_rise_c / (k * current_a.powf(a) * thickness_mil.powf(c))).powf(1.0 / b)
}

/// IPC-2152 current in A that a `width_mil` trace carries for the rise.
#[cfg(test)]
pub(crate) fn ipc2152_current_a(width_mil: f64, temp_rise_c: f64, thickness_mil: f64, inner: bool) -> f64 {
    let (k, a, b, c) = brooks_adam(inner, thickness_mil);
    (temp_rise_c / (k * width_mil.powf(b) * thickness_mil.powf(c))).powf(1.0 / a)
}

/// Minimum trace width for `current_a` with a `temp_rise_c` rise, by IPC-2152
/// (Brooks & Adam closed-form fit). IPC-2152 (2009) replaced IPC-2221's
/// 1956 NBS charts; its data show internal traces cool about as well as
/// external ones, so there is no factor-of-two internal derating — an inner
/// trace is wider only because its copper is usually thinner. The fit is the
/// conservative IPC-2152 case (no copper plane near the trace, still air): a
/// plane under the trace runs it cooler.
pub(crate) fn width_for_current(current_a: f64, temp_rise_c: f64, copper_oz: f64, inner: bool) -> Calc<WidthForCurrent> {
    let thickness_mil = copper_oz * MM_PER_OZ * MIL_PER_MM;
    let width_mil = ipc2152_width_mil(current_a, temp_rise_c, thickness_mil, inner);
    let width_mm = width_mil / MIL_PER_MM;
    let mut warnings = Vec::new();
    if !(0.0..=30.0).contains(&current_a) {
        warnings.push(format!("current {current_a} A is beyond the IPC-2152 charts (≈30 A)"));
    }
    if !(1.0..=100.0).contains(&temp_rise_c) {
        warnings.push(format!("temperature rise {temp_rise_c} °C is outside the IPC-2152 charts' 1–100 °C"));
    }
    if width_mil > 400.0 {
        warnings.push("width exceeds 400 mil; use a plane or pour".to_string());
    }
    Calc {
        result: WidthForCurrent {
            width_mm: round(width_mm, 4),
            cross_section_mil2: round(width_mil * thickness_mil, 2),
            layer: if inner { "inner" } else { "outer" },
            copper_oz,
            temp_rise_c,
        },
        method: "IPC-2152 via the Brooks & Adam fit ΔT = K·I^a·W^b·Th^c (W, Th in mil); external K 215.3, a 2, b −1.15, c −1.0; internal per copper weight (0.5 oz: 120, 2, −1.10, −1.52; 1 oz: 200, 1.9, −1.10, −1.52; 2 oz: 300, 2, −1.15, −1.52; 3 oz: 262.5, 1.9, −1.15, −1.52) — the same equation and coefficients as KiCad's calculator (commit 7f558e2779)",
        assumptions: {
            let mut assumptions = vec![
                "single trace, still air, no copper plane near it (the conservative IPC-2152 case); planes lower the rise".to_string(),
                format!("copper thickness {copper_oz} oz = {:.1} µm", copper_oz * MM_PER_OZ * 1000.0),
                "IPC-2152 found internal traces cool about as well as external ones: no internal derating factor".to_string(),
            ];
            // Which fit was used is a fact about the method, not a warning.
            if inner && copper_oz < 0.75 {
                assumptions.push("0.5 oz inner copper: the half-ounce internal fit applies".to_string());
            }
            assumptions
        },
        valid_range: "IPC-2152 chart range: ΔT 1–100 °C, up to ≈30 A",
        warnings,
    }
}

/// The superseded IPC-2221 §6.2 curve fit I = k·ΔT^0.44·A^0.725 (A in mil²;
/// k = 0.048 outer, 0.024 inner), kept for comparison with older tools
/// (KiCad 10.0.x's calculator uses it).
pub(crate) fn width_for_current_ipc2221(current_a: f64, temp_rise_c: f64, copper_oz: f64, inner: bool) -> Calc<WidthForCurrent> {
    let k = if inner { 0.024 } else { 0.048 };
    let area_mil2 = (current_a / (k * temp_rise_c.powf(0.44))).powf(1.0 / 0.725);
    let thickness_mil = copper_oz * MM_PER_OZ * MIL_PER_MM;
    let width_mm = area_mil2 / thickness_mil / MIL_PER_MM;
    let mut warnings = vec!["IPC-2221's charts were superseded by IPC-2152 (2009); the internal chart is too conservative and the external one not always conservative".to_string()];
    if !(0.0..=35.0).contains(&current_a) {
        warnings.push(format!("current {current_a} A is outside the curve fit's 0–35 A"));
    }
    if !(10.0..=100.0).contains(&temp_rise_c) {
        warnings.push(format!("temperature rise {temp_rise_c} °C is outside the fit's 10–100 °C"));
    }
    Calc {
        result: WidthForCurrent {
            width_mm: round(width_mm, 4),
            cross_section_mil2: round(area_mil2, 2),
            layer: if inner { "inner" } else { "outer" },
            copper_oz,
            temp_rise_c,
        },
        method: "IPC-2221 §6.2 curve fit I = k·ΔT^0.44·A^0.725 (k 0.048 outer, 0.024 inner)",
        assumptions: vec![
            "single isolated trace, still air".to_string(),
            format!("copper thickness {copper_oz} oz = {:.1} µm", copper_oz * MM_PER_OZ * 1000.0),
        ],
        valid_range: "0–35 A, ΔT 10–100 °C, width ≤ 400 mil",
        warnings,
    }
}

// ─── Copper resistance and voltage drop ─────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub(crate) struct TraceResistance {
    pub resistance_ohm: f64,
    pub voltage_drop_v: Option<f64>,
    pub power_w: Option<f64>,
    pub resistivity_ohm_m: f64,
}

/// DC resistance of a trace (and, given a current, its voltage drop and
/// dissipation): R = ρ(T)·L / (w·t).
pub(crate) fn trace_resistance(length_mm: f64, width_mm: f64, copper_oz: f64, temp_c: f64, current_a: Option<f64>) -> Calc<TraceResistance> {
    let rho = rho_cu(temp_c);
    let area_m2 = (width_mm * 1e-3) * (copper_oz * MM_PER_OZ * 1e-3);
    let r = rho * (length_mm * 1e-3) / area_m2;
    let mut warnings = Vec::new();
    if !(-73.0..=226.0).contains(&temp_c) {
        warnings.push(format!("temperature {temp_c} °C is outside the resistivity table (−73…227 °C); clamped"));
    }
    Calc {
        result: TraceResistance {
            resistance_ohm: round_sig(r, 5),
            voltage_drop_v: current_a.map(|i| round_sig(i * r, 5)),
            power_w: current_a.map(|i| round_sig(i * i * r, 5)),
            resistivity_ohm_m: round_sig(rho, 5),
        },
        method: "R = ρ(T)·L/(w·t); ρ(20 °C) = 1.7241e-8 Ω·m (IACS annealed copper), temperature ratio from the NIST pure-copper table (Matula 1979)",
        assumptions: vec![
            "rectangular cross-section, uniform current (DC; no skin effect)".to_string(),
            format!("copper thickness {copper_oz} oz = {:.1} µm, no plating added", copper_oz * MM_PER_OZ * 1000.0),
        ],
        valid_range: "DC and low frequency; −73…227 °C",
        warnings,
    }
}

// ─── Transmission-line impedance ────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize)]
pub(crate) struct LineGeometry {
    pub width_mm: f64,
    /// Dielectric height to the reference plane (microstrip), or the total
    /// plane-to-plane spacing (stripline), mm.
    pub height_mm: f64,
    pub thickness_mm: f64,
    pub er: f64,
    /// Edge-to-edge gap of a differential pair, mm; `None` for single-ended.
    pub gap_mm: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Impedance {
    pub z0_ohm: f64,
    pub zdiff_ohm: Option<f64>,
    pub er_eff: Option<f64>,
    pub geometry: LineGeometry,
}

/// Hammerstad–Jensen (1980) microstrip, with their finite-thickness width
/// correction: the form KiCad's own calculator and Wadell (§3.5.1) use.
fn microstrip_z0(w: f64, h: f64, t: f64, er: f64) -> (f64, f64) {
    let u = w / h;
    let tn = t / h;
    let z_air = |u: f64| {
        let f = 6.0 + (2.0 * std::f64::consts::PI - 6.0) * (-(30.666 / u).powf(0.7528)).exp();
        ETA0 / (2.0 * std::f64::consts::PI) * (f / u + (1.0 + 4.0 / (u * u)).sqrt()).ln()
    };
    let er_eff = |u: f64| {
        let a = 1.0
            + (1.0 / 49.0) * ((u.powi(4) + (u / 52.0).powi(2)) / (u.powi(4) + 0.432)).ln()
            + (1.0 / 18.7) * (1.0 + (u / 18.1).powi(3)).ln();
        let b = 0.564 * ((er - 0.9) / (er + 3.0)).powf(0.053);
        (er + 1.0) / 2.0 + (er - 1.0) / 2.0 * (1.0 + 10.0 / u).powf(-a * b)
    };
    let (u1, ur) = if tn > 0.0 {
        let coth = |x: f64| x.cosh() / x.sinh();
        let du1 = tn / std::f64::consts::PI * (1.0 + 4.0 * std::f64::consts::E / (tn * coth((6.517 * u).sqrt()).powi(2))).ln();
        let dur = 0.5 * (1.0 + 1.0 / (er - 1.0).sqrt().cosh()) * du1;
        (u + du1, u + dur)
    } else {
        (u, u)
    };
    let e_eff = er_eff(ur) * (z_air(u1) / z_air(ur)).powi(2);
    (z_air(ur) / er_eff(ur).sqrt(), e_eff)
}

/// Single-ended or edge-coupled differential microstrip.
pub(crate) fn microstrip(g: LineGeometry) -> Calc<Impedance> {
    let (z0, e_eff) = microstrip_z0(g.width_mm, g.height_mm, g.thickness_mm, g.er);
    // IPC-2141A edge-coupled microstrip: Zdiff = 2·Z0·(1 − 0.48·e^(−0.96·s/h)).
    let zdiff = g.gap_mm.map(|s| 2.0 * z0 * (1.0 - 0.48 * (-0.96 * s / g.height_mm).exp()));
    let mut warnings = Vec::new();
    let u = g.width_mm / g.height_mm;
    if !(0.01..=100.0).contains(&u) {
        warnings.push(format!("w/h = {u:.3} is outside Hammerstad–Jensen's 0.01–100"));
    }
    if !(1.0..=128.0).contains(&g.er) {
        warnings.push(format!("εr = {} is outside 1–128", g.er));
    }
    if let Some(s) = g.gap_mm {
        if !(0.1..=3.0).contains(&(s / g.height_mm)) {
            warnings.push(format!("s/h = {:.2} is outside the coupled-pair fit's ~0.1–3", s / g.height_mm));
        }
    }
    Calc {
        result: Impedance {
            z0_ohm: round(z0, 2),
            zdiff_ohm: zdiff.map(|z| round(z, 2)),
            er_eff: Some(round(e_eff, 4)),
            geometry: g,
        },
        method: if g.gap_mm.is_some() {
            "Z0: Hammerstad–Jensen microstrip with thickness correction (~1 %); Zdiff: IPC-2141A coupled-microstrip approximation (~±5 %)"
        } else {
            "Hammerstad–Jensen microstrip with thickness correction (~1 % vs. full-wave for typical PCB geometry)"
        },
        assumptions: vec![
            "outer-layer trace over one solid reference plane, no solder mask (mask lowers Z0 ~1–3 Ω)".to_string(),
            "quasi-static (frequency-independent) εr".to_string(),
        ],
        valid_range: "0.01 ≤ w/h ≤ 100, 1 ≤ εr ≤ 128",
        warnings,
    }
}

/// Symmetric stripline (trace centred between two planes `height_mm` apart),
/// IPC-2141A: Z0 = 60/√εr · ln(1.9·b / (0.8·w + t)).
pub(crate) fn stripline(g: LineGeometry) -> Calc<Impedance> {
    let b = g.height_mm;
    let z0 = 60.0 / g.er.sqrt() * (1.9 * b / (0.8 * g.width_mm + g.thickness_mm)).ln();
    // IPC-2141A edge-coupled stripline: Zdiff = 2·Z0·(1 − 0.347·e^(−2.9·s/b)).
    let zdiff = g.gap_mm.map(|s| 2.0 * z0 * (1.0 - 0.347 * (-2.9 * s / b).exp()));
    let mut warnings = Vec::new();
    if g.width_mm / (b - g.thickness_mm) > 0.35 {
        warnings.push("w/(b−t) > 0.35: the IPC-2141A stripline fit loses accuracy".to_string());
    }
    if g.thickness_mm / b > 0.25 {
        warnings.push("t/b > 0.25: outside the fit".to_string());
    }
    Calc {
        result: Impedance { z0_ohm: round(z0, 2), zdiff_ohm: zdiff.map(|z| round(z, 2)), er_eff: Some(g.er), geometry: g },
        method: "IPC-2141A symmetric stripline Z0 = 60/√εr·ln(1.9b/(0.8w+t)); Zdiff = 2Z0(1−0.347e^(−2.9s/b)) (~±5 %)",
        assumptions: vec!["trace centred between two solid planes; homogeneous dielectric".to_string()],
        valid_range: "w/(b−t) < 0.35, t/b < 0.25",
        warnings,
    }
}

/// The width that gives `target_ohm` (Z0, or Zdiff when `gap_mm` is set),
/// by bisection on the forward model. Impedance falls monotonically with width.
pub(crate) fn width_for_impedance(target_ohm: f64, mut g: LineGeometry, strip: bool) -> Option<Calc<Impedance>> {
    let eval = |w: f64, g: &LineGeometry| {
        let probe = LineGeometry { width_mm: w, ..*g };
        let c = if strip { stripline(probe) } else { microstrip(probe) };
        if g.gap_mm.is_some() { c.result.zdiff_ohm.unwrap() } else { c.result.z0_ohm }
    };
    let (mut lo, mut hi) = (0.01, 20.0 * g.height_mm.max(0.05));
    if eval(lo, &g) < target_ohm || eval(hi, &g) > target_ohm {
        return None;
    }
    for _ in 0..80 {
        let mid = 0.5 * (lo + hi);
        if eval(mid, &g) > target_ohm { lo = mid } else { hi = mid }
    }
    g.width_mm = round(0.5 * (lo + hi), 4);
    let mut calc = if strip { stripline(g) } else { microstrip(g) };
    calc.assumptions.push(format!("width solved for {target_ohm} Ω by bisection"));
    Some(calc)
}

// ─── Standard values ────────────────────────────────────────────────────────

const E12: &[f64] = &[1.0, 1.2, 1.5, 1.8, 2.2, 2.7, 3.3, 3.9, 4.7, 5.6, 6.8, 8.2];
const E24: &[f64] = &[
    1.0, 1.1, 1.2, 1.3, 1.5, 1.6, 1.8, 2.0, 2.2, 2.4, 2.7, 3.0, 3.3, 3.6, 3.9, 4.3, 4.7, 5.1, 5.6, 6.2, 6.8, 7.5, 8.2, 9.1,
];
const E96: &[f64] = &[
    1.00, 1.02, 1.05, 1.07, 1.10, 1.13, 1.15, 1.18, 1.21, 1.24, 1.27, 1.30, 1.33, 1.37, 1.40, 1.43, 1.47, 1.50, 1.54, 1.58,
    1.62, 1.65, 1.69, 1.74, 1.78, 1.82, 1.87, 1.91, 1.96, 2.00, 2.05, 2.10, 2.15, 2.21, 2.26, 2.32, 2.37, 2.43, 2.49, 2.55,
    2.61, 2.67, 2.74, 2.80, 2.87, 2.94, 3.01, 3.09, 3.16, 3.24, 3.32, 3.40, 3.48, 3.57, 3.65, 3.74, 3.83, 3.92, 4.02, 4.12,
    4.22, 4.32, 4.42, 4.53, 4.64, 4.75, 4.87, 4.99, 5.11, 5.23, 5.36, 5.49, 5.62, 5.76, 5.90, 6.04, 6.19, 6.34, 6.49, 6.65,
    6.81, 6.98, 7.15, 7.32, 7.50, 7.68, 7.87, 8.06, 8.25, 8.45, 8.66, 8.87, 9.09, 9.31, 9.53, 9.76,
];

pub(crate) fn series(name: &str) -> Option<&'static [f64]> {
    match name {
        "E12" => Some(E12),
        "E24" => Some(E24),
        "E96" => Some(E96),
        _ => None,
    }
}

/// Every value of a series from `lo` to `hi`, in order.
fn series_values(series: &[f64], lo: f64, hi: f64) -> Vec<f64> {
    let mut out = Vec::new();
    let mut decade = 10f64.powf(lo.log10().floor() - 1.0);
    while decade <= hi * 10.0 {
        for &m in series {
            let v = round_sig(m * decade, 3);
            if v >= lo * 0.999 && v <= hi * 1.001 {
                out.push(v);
            }
        }
        decade *= 10.0;
    }
    out
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Nearest {
    pub target: f64,
    pub below: f64,
    pub above: f64,
    pub nearest: f64,
    pub error_percent: f64,
}

pub(crate) fn nearest_standard(target: f64, series_name: &'static str, values: &[f64]) -> Calc<Nearest> {
    let candidates = series_values(values, target / 10.0, target * 10.0);
    let below = candidates.iter().copied().filter(|v| *v <= target * 1.000_001).fold(f64::NAN, f64::max);
    let above = candidates.iter().copied().filter(|v| *v >= target * 0.999_999).fold(f64::NAN, f64::min);
    let nearest = if (target - below).abs() / target <= (above - target).abs() / target { below } else { above };
    Calc {
        result: Nearest { target, below, above, nearest, error_percent: round((nearest - target) / target * 100.0, 3) },
        method: "IEC 60063 preferred-number series",
        assumptions: vec![format!("series {series_name}")],
        valid_range: "any positive value",
        warnings: vec![],
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Divider {
    pub r_top: f64,
    pub r_bottom: f64,
    pub ratio: f64,
    pub vout_v: Option<f64>,
    pub error_percent: f64,
    pub divider_current_a: Option<f64>,
}

/// The standard-value pair whose ratio Rb/(Rt+Rb) is closest to `ratio`, with
/// Rt + Rb near `total_ohm` (sets divider current and source impedance).
pub(crate) fn divider(ratio: f64, total_ohm: f64, values: &[f64], series_name: &'static str, vin_v: Option<f64>) -> Calc<Divider> {
    let candidates = series_values(values, total_ohm / 100.0, total_ohm);
    let mut best: Option<(f64, f64, f64)> = None;
    for &rb in &candidates {
        for &rt in &candidates {
            let total = rt + rb;
            if total < total_ohm * 0.5 || total > total_ohm * 2.0 {
                continue;
            }
            let err = (rb / total - ratio).abs() / ratio;
            if best.is_none_or(|(_, _, e)| err < e - 1e-12) {
                best = Some((rt, rb, err));
            }
        }
    }
    let (rt, rb, _) = best.unwrap_or((f64::NAN, f64::NAN, f64::NAN));
    let actual = rb / (rt + rb);
    Calc {
        result: Divider {
            r_top: rt,
            r_bottom: rb,
            ratio: round_sig(actual, 6),
            vout_v: vin_v.map(|v| round_sig(v * actual, 6)),
            error_percent: round((actual - ratio) / ratio * 100.0, 3),
            divider_current_a: vin_v.map(|v| round_sig(v / (rt + rb), 4)),
        },
        method: "exhaustive search over the series within 0.5–2× the requested total resistance",
        assumptions: vec![format!("series {series_name}"), "unloaded output; resistor tolerance not included".to_string()],
        valid_range: "0 < ratio < 1",
        warnings: if !(0.0..1.0).contains(&ratio) { vec!["ratio must be between 0 and 1".to_string()] } else { vec![] },
    }
}

fn round(value: f64, places: i32) -> f64 {
    let f = 10f64.powi(places);
    (value * f).round() / f
}

fn round_sig(value: f64, sig: i32) -> f64 {
    if value == 0.0 || !value.is_finite() {
        return value;
    }
    let places = sig - 1 - value.abs().log10().floor() as i32;
    round(value, places)
}

#[cfg(test)]
mod tests {
    //! Oracle tests: each formula against an independently published value,
    //! not against itself.
    use super::*;

    fn close(a: f64, b: f64, rel: f64) -> bool {
        ((a - b) / b).abs() <= rel
    }

    #[test]
    fn microstrip_matches_pozar_example_3_7() {
        // Pozar, Microwave Engineering 4e, Example 3.7: εr 2.20, d 0.158 cm,
        // 50 Ω needs W = 0.487 cm (W/d 3.081), εe 1.87 (zero thickness).
        let c = microstrip(LineGeometry { width_mm: 4.87, height_mm: 1.58, thickness_mm: 0.0, er: 2.2, gap_mm: None });
        assert!(close(c.result.z0_ohm, 50.0, 0.015), "{:?}", c.result);
        assert!(close(c.result.er_eff.unwrap(), 1.87, 0.01), "{:?}", c.result);
    }

    #[test]
    fn air_microstrip_matches_wheeler() {
        // Wheeler's exact conformal-map value for w/h = 1 in air is 126.5 Ω
        // (Wadell, Transmission Line Design Handbook, Table 3.5.1).
        let c = microstrip(LineGeometry { width_mm: 1.0, height_mm: 1.0, thickness_mm: 0.0, er: 1.0, gap_mm: None });
        assert!(close(c.result.z0_ohm, 126.5, 0.01), "{:?}", c.result);
    }

    #[test]
    fn width_for_50_ohm_inverts_the_forward_model() {
        let g = LineGeometry { width_mm: 0.0, height_mm: 0.2104, thickness_mm: 0.035, er: 4.4, gap_mm: None };
        let w = width_for_impedance(50.0, g, false).unwrap();
        assert!(close(w.result.z0_ohm, 50.0, 0.002), "{:?}", w.result);
        // FR-4 rule of thumb: 50 Ω outer trace ≈ 1.7–1.9 × prepreg height.
        let ratio = w.result.geometry.width_mm / 0.2104;
        assert!((1.5..2.1).contains(&ratio), "w/h {ratio}");
    }

    #[test]
    fn stripline_matches_ipc_2141_reference() {
        // IPC-2141A worked case: b = 0.5 mm, w = 0.15 mm, t = 0.035 mm, εr 4.2:
        // 60/√4.2 · ln(1.9·0.5/(0.12+0.035)) = 53.1 Ω.
        let c = stripline(LineGeometry { width_mm: 0.15, height_mm: 0.5, thickness_mm: 0.035, er: 4.2, gap_mm: None });
        let expect = 60.0 / 4.2f64.sqrt() * (1.9f64 * 0.5 / (0.8 * 0.15 + 0.035)).ln();
        assert!(close(c.result.z0_ohm, expect, 0.001) && close(expect, 53.1, 0.01), "{:?}", c.result);
    }

    #[test]
    fn ipc_2221_width_matches_the_published_chart() {
        // IPC-2221 chart: 1 A, 10 °C rise, 1 oz outer ≈ 0.3 mm (≈ 12 mil);
        // 2 A ≈ 0.76 mm (30 mil). Inner layers need ~2.6× the outer width.
        let one = width_for_current_ipc2221(1.0, 10.0, 1.0, false).result.width_mm;
        let two = width_for_current_ipc2221(2.0, 10.0, 1.0, false).result.width_mm;
        assert!((0.25..0.35).contains(&one), "{one}");
        assert!((0.65..0.85).contains(&two), "{two}");
        let inner = width_for_current_ipc2221(1.0, 10.0, 1.0, true).result.width_mm;
        assert!(close(inner / one, 2f64.powf(1.0 / 0.725), 0.001));
    }

    #[test]
    fn ipc_2152_matches_kicads_reference_values() {
        // KiCad qa/tests/common/test_track_width_calculations.cpp (commit
        // 7f558e2779), checked there to 1e-6 %: 20 mil, 1.378 mil (1 oz),
        // ΔT 10 °C → 1.4164291025677085 A external, 1.513124408404138 A
        // internal 1 oz; 2.756 mil (2 oz) internal → 2.208736063285966 A.
        // Same equation here, so the tolerance is 1e-9 relative.
        assert!(close(ipc2152_current_a(20.0, 10.0, 1.378, false), 1.416_429_102_567_708_5, 1e-9));
        assert!(close(ipc2152_current_a(20.0, 10.0, 1.378, true), 1.513_124_408_404_138, 1e-9));
        assert!(close(ipc2152_current_a(20.0, 10.0, 2.756, true), 2.208_736_063_285_966, 1e-9));
        // Width and current are exact inverses (KiCad's RoundTrip tests).
        for inner in [false, true] {
            let i = ipc2152_current_a(25.0, 30.0, 1.378, inner);
            assert!(close(ipc2152_width_mil(i, 30.0, 1.378, inner), 25.0, 1e-9));
        }
    }

    #[test]
    fn ipc_2152_does_not_derate_inner_layers_and_tracks_the_old_chart_outside() {
        // At equal copper an inner trace is no wider than ~1.1× the outer one
        // (IPC-2221 said 2.6×); with JLC04161H-7628's 0.5 oz inner copper it
        // is wider only for the thinner copper.
        let outer = width_for_current(1.0, 10.0, 1.0, false).result.width_mm;
        let inner_1oz = width_for_current(1.0, 10.0, 1.0, true).result.width_mm;
        assert!(inner_1oz < 1.1 * outer && inner_1oz > 0.8 * outer, "{outer} vs {inner_1oz}");
        // External 1 A / 10 °C / 1 oz: within 15 % of the IPC-2221 chart's
        // 0.3 mm — the two agree for small outer traces.
        assert!(close(outer, width_for_current_ipc2221(1.0, 10.0, 1.0, false).result.width_mm, 0.15), "{outer}");
        let half = width_for_current(1.5, 10.0, 0.5, true).result.width_mm;
        assert!(half > width_for_current(1.5, 10.0, 1.0, false).result.width_mm, "{half}");
    }

    #[test]
    fn copper_resistance_matches_the_sheet_resistance_rule() {
        // 1 oz copper is ≈ 0.495 mΩ/square at 20 °C (1.7241e-8 / 34.79e-6).
        let c = trace_resistance(10.0, 1.0, 1.0, 20.0, Some(2.0));
        assert!(close(c.result.resistance_ohm, 10.0 * 4.956e-4, 0.005), "{:?}", c.result);
        assert!(close(c.result.voltage_drop_v.unwrap(), 2.0 * c.result.resistance_ohm, 1e-4));
        // Copper's resistance rises ≈ 0.39 %/°C near room temperature.
        let hot = trace_resistance(10.0, 1.0, 1.0, 70.0, None).result.resistance_ohm;
        assert!(close(hot / c.result.resistance_ohm, 1.0 + 0.0039 * 50.0, 0.02), "{hot}");
    }

    #[test]
    fn standard_values() {
        let n = nearest_standard(5000.0, "E24", E24).result;
        assert_eq!((n.below, n.above, n.nearest), (4700.0, 5100.0, 5100.0));
        let n = nearest_standard(10_100.0, "E96", E96).result;
        assert_eq!(n.nearest, 10_000.0);
        // VBUS 5 V → 3.3 V-tolerant ADC sense: ratio 0.5 within E24 at ~100 kΩ total.
        let d = divider(0.5, 100_000.0, E24, "E24", Some(5.0)).result;
        assert_eq!(d.error_percent, 0.0);
        assert!(close(d.vout_v.unwrap(), 2.5, 1e-9));
    }
}
