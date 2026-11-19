# WIP: design-intent defaults from physics and published guidance

Status, 2026-11-19. Everything below is committed on `wip/intent-defaults`, including the routed tie-path check (§4 item 1, done). `cargo test -p konnect-core --lib`: 1841 passed, 0 failed, 23 ignored (after the review fixes). `cargo build --release` builds. The change log is FORK_CHANGES.md "Pass 16: design-intent defaults"; the API notes are in docs/API_MIGRATIONS.md. This file keeps the research detail and the open items (§4 items 2–5); delete it once those are done or tracked elsewhere.

Files changed:
- `crates/konnect-core/src/tools/design_intent.rs`
- `crates/konnect-core/src/tools/eecalc.rs`
- `crates/konnect-core/src/tools/layer_stack.rs`
- `crates/konnect-core/src/tools/placement.rs`: 3 lines. This adapts the `Tie` literal to `max_mm: Some(..)` and adds `..Default::default()`. Behaviour is unchanged, and main approved the edit.

**Formatting.** None of these files were rustfmt-clean before this change; the repo uses long lines. My edits follow the surrounding style, and I did not reformat anything. `rustfmt --check` reports no hunk at the `placement.rs` edit.

## 1. Research (docdb slugs)

### Current-carrying width

| Source | docdb slug | What it gives |
|---|---|---|
| KiCad commit `7f558e2779` (master, 2026-07-05, "Use IPC-2152 track width formula") | `kicad-ipc2152-track-width` | Brooks & Adam closed-form fit to the IPC-2152 data, ΔT = K·I^a·W^b·Th^c (W and Th in mil, I in A). Reference values in KiCad's unit tests. |
| KiCad 10.0.8 `panel_track_width.cpp` | `kicad-10-ipc2221-track-width` | The installed KiCad 10.0.8 still uses IPC-2221: I = k·ΔT^0.44·A^0.725, k = 0.048 external, 0.024 internal. |

Brooks & Adam coefficients:

| Layer | K | a | b | c |
|---|---|---|---|---|
| External (all copper weights) | 215.3 | 2 | −1.15 | −1.0 |
| Internal 0.5 oz | 120 | 2 | −1.10 | −1.52 |
| Internal 1 oz | 200 | 1.9 | −1.10 | −1.52 |
| Internal 2 oz | 300 | 2 | −1.15 | −1.52 |
| Internal 3 oz | 262.5 | 1.9 | −1.15 | −1.52 |

- The internal fit is chosen by the nearest nominal copper thickness.
- IPC-2152 has no internal derating factor. Its data show internal traces cool about as well as external ones.
- KiCad's calculator defaults to a 10 °C rise.
- Primary paper, not fetched: D. Brooks & J. Adam, "Trace Currents and Temperatures Revisited", 2015 (ultracad.com).
- The IPC-2152 standard itself is paywalled; ipc.org was also unreachable.

### Differential-pair skew

| Source | docdb slug | What it gives |
|---|---|---|
| TI SPRAAR7J (2023), Table A-1 | `ti-spraar7j` | USB2.0: 90 Ω (81–99 Ω); skew within any USB2.0 pair ≤ 50 mil; ≤ 4 vias. |
| TI SNLA387 (June 2021), Ethernet PHY PCB Design Layout Checklist | `ti-snla387-ethernet-phy-layout` | MDI pairs: under 2 in, 100 Ω ±10 %, length-matched within 50 mil (10/100M) or 20 mil (1G). MII: under 6 in, matched within 50 mil. |
| TI SNLA079D, AN-1469 PHYTER guide | `ti-snla079d-phyter-layout` | RMII/MII: under 6 in, matched within 2 in. |
| LAN8742 routing checklist | `microchip-lan8742-routing-checklist` (already in the DB) | 100 Ω MDI pairs; 50 MHz clock runs matched within 0.10 in and under 6.0 in; 68 Ω RMII traces. VDDCR: "heavy, wide trace with multiple vias". Crystal routing: "as small as possible". |
| LAN8742A datasheet | `microchip-lan8742a-ds` | 100BASE-TX rise/fall time TRF 3.0–5.0 ns; rise/fall symmetry ≤ 0.5 ns. |
| H. Johnson, "Common Mode Analysis of Skew", EDN 2004-01-22 | `johnson-edn-2004-cm-analysis-of-skew` | "A 20 % skew creates a 10 % common-mode component." |
| H. Johnson, "Your Layout is Skewed", EDN 2002-04-18 | `johnson-edn-2002-your-layout-is-skewed` | An unspecified driver has skew of at least 10 % of its rise time. For 100BASE-TX, the layout-plus-connector skew budget is about 0.1 in. |
| STM32H563 DS14258 | `st-ds14258-stm32h563` | USB FS rise time 4–20 ns. VCAP: C_EXT 2.2 µF, ESR < 100 mΩ. |

- USB HS rise time ≥ 500 ps comes from USB 2.0 spec §7.1.2.2, quoted from memory. usb.org is blocked from this host.
- Johnson's 0.1 in budget is consistent with TI's 50 mil. One thousandth of an 8 ns rise time is 8 ps, which is about 1.3 mm on FR-4.

### Crystal, decoupling and core-regulator capacitor

| Source | docdb slug | What it gives |
|---|---|---|
| Microchip/Atmel AVR186 (8128B, 2016) | `microchip-avr186-oscillator-layout` | Typical PCB parasitics: XTALIN–GND 1 pF, XTALOUT–GND 2 pF, XIN–XOUT 0.5 pF. |
| TI AM67 datasheet §8.3.1 (web viewer, not saved) | none | Crystal on the outer layer over a continuous ground plane, traces kept short. Qualitative only. |
| ST AN5711 | `st-an5711-stm32h5-hw` | §3.1: crystal "as close as possible". §7.4/§9.3: decoupling "as close as possible", with "wider, short trace/via" and fewer vias. No numbers. |

ST AN2867 (oscillator guide) was not obtained. www.st.com returns 502 through the proxy, and I did not use third-party mirrors.

### Blocked domains

- **Unreachable:** st.com (502), usb.org, saturnpcb.com, ultracad.com, ipc.org, nxp.com, silabs.com, intel.com and signalintegrityjournal.com (403 or no connection).
- **Allowlisted mid-task by the user:** sigcon.com and colorado.edu.
- **Saturn PCB Toolkit:** its behaviour is not verified because its site is blocked.

## 2. Chosen formulas and defaults

### Width from current
- **Formula:** IPC-2152 via Brooks & Adam, in `eecalc::width_for_current`, with the same coefficients as KiCad.
  - `ipc2152_width_mil` and `ipc2152_current_a` are the raw functions.
  - `width_for_current_ipc2221` keeps the old chart. `engineering_calc` exposes it with `method: "ipc2221"`.
- **Defaults:** a 10 °C rise when `current_a` is given; 1 oz outer and 0.5 oz inner copper (JLC04161H-7628).
- **Effect:** outer widths come out about 10 % narrower than IPC-2221 at 1 A. Inner widths are no longer 2.6× wider; only the thinner copper makes them about 1.5× wider.
- **Power net without `current_a`:** a warning says no width is derived. The suggested basis is the fuse or PTC trip current.

### Pair and interface standards (`standard`)

| Standard | Zdiff | Tolerance | Skew | Max length | Rise time | Source |
|---|---|---|---|---|---|---|
| `usb2_hs` | 90 Ω | ±10 % | 1.27 mm | none | 0.5 ns | SPRAAR7J; USB 2.0 spec |
| `usb2_fs` | 90 Ω | ±10 % | 1.27 mm | none | 4 ns | SPRAAR7J ("any USB2.0 pair"); DS14258 |
| `ethernet_100base_tx` | 100 Ω | ±10 % | 1.27 mm | 50.8 mm | 3 ns | SNLA387; LAN8742A DS |
| `ethernet_1000base_t` | 100 Ω | ±10 % | 0.508 mm | 50.8 mm | none | SNLA387 |
| `rmii` (single-ended) | none (TI says 50 Ω, Microchip 68 Ω) | none | none | 152.4 mm | none | SNLA079D, SNLA387, LAN8742 checklist |

**Edge-rate rule.** Layout skew may be at most `EDGE_SKEW_FRACTION` = 0.1 of the rise time: half of Johnson's 20 % budget, because the driver already uses at least 10 %. That time becomes a length through the slowest routable signal layer's propagation delay τ = √εeff / c0 (`LineModel::per_mm`); a stripline is slower than the outer microstrip, so this is always the conservative choice (changed after review; it used the outer layer).
- If no signal layer has a line model, the delay uses c/√εr of the slowest dielectric, which no real line is slower than.
- The default skew is the tighter of the published value and the edge-rate bound. Both appear in the source string.
- With a standard, the tighter of the published value and the edge-rate bound wins (both appear in the source string). For the current standards the published value is the tighter one: USB HS's edge-rate bound is about 8 mm, against 1.27 mm published.

**Gap.** The gap default stays 0.15 mm. No standard specifies a gap; it falls out of the impedance solve. The source string says so.

### Tie kinds (`kind`)

The limits come from the default stack-up (L′ and C′ from `LayerStack`).

| Kind | Default path limit | How it is derived | Max vias | Default placement distance |
|---|---|---|---|---|
| `crystal` | ≈ 5.5 mm | 0.5 pF trace budget ÷ C′ (0.2 mm outer trace, ≈ 0.09 pF/mm) | 0 | = path limit |
| `core_regulator_cap` | ≈ 3 mm | 1.0 nH ÷ L′ (0.3 mm trace, ≈ 0.33 nH/mm) | 1 | = path limit |
| `decoupling` | ≈ 6 mm | 2.0 nH ÷ L′ | 1 | = path limit |

- The crystal budget is AVR186's 1 pF XTALIN figure, less about 0.5 pF for the pads.
- The 1 nH and 2 nH inductance budgets are a Konnect choice, roughly one MLCC's own mounted ESL. ST and Microchip give no number, and the output labels them that way.
- If the outer layer has no line model, the fallbacks are 6, 3 and 6 mm (WORKFLOW.md §5 item 6).

### Override precedence and provenance
- An explicit user value always wins.
- Then come the standard's published values. For skew, the edge-rate rule is also considered.
- Then come the physics defaults: 10 °C rise, gap 0.15 mm, and the tie-kind limits.
- `NetRequirement.sources` maps each field to `"user"` or to the citation string.
- `tie_requirements[]` gives each tie's resolved limits with `sources`. It appears in the get, set and apply outputs, and in the tie check's `required.sources`.
- Checks use the resolved values (`NetRequirement.effective`).

## 3. Tests (all pass)

`eecalc`:
- `ipc_2152_matches_kicads_reference_values`: compares against KiCad's test values to 1e-9 relative, and round-trips width and current.
- `ipc_2152_does_not_derate_inner_layers_and_tracks_the_old_chart_outside`: inner/outer ratio below 1.1 at equal copper; within 15 % of the IPC-2221 chart's 0.3 mm at 1 A.
- `ipc_2221_width_matches_the_published_chart`: kept, now calling the renamed function.

`layer_stack`:
- `per_length_constants_match_published_values`
  - Air: 3.3356 ps/mm, exact.
  - Pozar Ex. 3.7: τ, C′ and L′ within 1–1.5 %.
  - FR-4 stripline: 6.997 ps/mm (177.7 ps/in).
  - Identity L′·C′ = τ².

`design_intent::default_tests`:
- `a_standard_fills_impedance_skew_and_length_with_cited_sources`
- `explicit_user_values_override_every_default`
- `a_pair_without_a_standard_gets_skew_from_its_edge_rate_and_the_stackup_velocity`
- `standards_are_validated_against_the_role`
- `power_rise_defaults_to_10c_and_a_power_net_without_current_is_flagged`: 1.5 A gives ≈ 0.56 mm.
- `tie_kinds_derive_path_limits_from_the_stackup_and_user_values_win`
- `a_crystal_tie_fails_placement_beyond_its_derived_limit`

Existing tests changed:
- `rules_are_per_layer_and_power_has_an_inner_minimum`: inner is now 1.3–2× outer, where it was over 3×.
- Tie literals now use `max_mm: Some(..)`.

## 4. What remains

1. **Routed path-length check for ties: done.** `net_graph` (factored out of `voltage_drop`), `NetGraph::shortest`, `tie_path`; the `tie_path` check reports path_mm, vias, layers and the L/C estimates. See FORK_CHANGES.md Pass 16. Open: via barrels and pads are not in the L/C estimate (no sourced via-inductance formula in the doc DB yet).
2. **DRC rules for crystal and pair defaults.** `apply_design_intent` already writes length and skew rules from the resolved values. A crystal tie's path limit could become a `length` rule on the crystal net, but only when that net runs between the crystal and the pin alone; the load-capacitor branch complicates this.
3. **Group matching for RMII (TX group, RX group).** Not modelled; the intent has pair skew only.
4. **AN2867.** Re-read it if st.com becomes reachable, and replace the AVR186-based crystal budget if ST gives a number.
5. **Board migrations.**
   - `eth_board/build.py` sets `max_skew_mm=ETH_SKEW` (12 mm) and explicit crystal lengths. The user values win, so the reviewer's findings still pass until those are removed and `standard: "ethernet_100base_tx"` and tie `kind`s are set.
   - The eth board's 4.5 mm and 7.7 mm skews will then fail against 1.27 mm. KiCad's skew counts the whole net, including the termination stub.
   - The eth board passes `current_a: 1.5` for +5V/VBUS. IPC-2152 needs 0.564 mm outer where IPC-2221 gave 0.53 mm, so with this branch +5V and VBUS (routed at 0.53 mm) **fail** `track_width`, while /RP_1V1 goes fail → pass. IPC-2152 is narrower than IPC-2221 below about 1 A and wider above it. Checked read-only on the saved board.

## 5–6. Drafts

Merged into FORK_CHANGES.md (Pass 16) and docs/API_MIGRATIONS.md ("design-intent defaults and IPC-2152").
