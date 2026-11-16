# Fork changes

Two passes so far: the safe-write fixes (below), then the simple tool fixes
found by the proof-of-concept boards (see [Pass 2](#pass-2-simple-tool-fixes)).

## Pass 1: safe-write fixes

This fork (`HaringIbon/Konnect`, branch `safe-writes`) is based on the final
upstream `mixelpixx/Konnect` `main` (`676efce`, v0.13.0 + 62 commits). Upstream
was archived on 2026-11-02 and development moved to the closed-source
Konnect-EDA.

**Scope:** only the open bugs where a tool can lose or corrupt a user's
design file, with the smallest change that fixes each one. Where an upstream
PR already existed (CI green, unmerged at archive time), it was merged as is
after review. Two issues had no PR, so the fixes were written here.

## Summary by tool

The served MCP surface did not change in that pass: the same 235 tools (228
plus 7 meta-tools), and every `inputSchema` is byte-identical to upstream
`main`. Only behaviour on the
unsafe paths changed, plus 4 tool descriptions.

| Tool | Issue(s) | What changed | Path affected |
|---|---|---|---|
| `set_board_size` | #845 | File write is revision-checked. A board changed between read and write returns `conflict` and is not overwritten. | File fallback (no live KiCad) |
| `add_layer` | #845 | Same as above | Always file |
| `add_board_outline` | #845 | Same as above | File fallback |
| `delete_graphics` | #845 | Same as above | File fallback |
| `add_board_text` | #845, #844, #849 | Revision-checked write. Unknown layer refused (`invalid_argument`). A layer alias (`F.Silkscreen`, `In01.Cu`, …) is written and reported under KiCad's canonical name (`F.SilkS`, `In1.Cu`). | Validation on both paths; revision check on file fallback |
| `import_svg_logo` | #845, #844, #849 | Same as `add_board_text` | Same as above |
| `add_zone` | #845, #844, #849 | Same as `add_board_text` | Same as above |
| `add_copper_pour` | #845, #844, #849 | Same as `add_board_text` (shares `add_zone_impl`) | Same as above |
| `rename_project` | #851 | Every file is read before any rename. Reference rewrites commit as one journaled transaction that only replaces files still holding what was read. A conflict rolls the renames back and returns `conflict` naming the file. Also refuses up front if KiCad holds any of the files. | Always |
| `set_design_rules` | #804, #964 | Refuses while `~<project>.kicad_pro.lck` exists (Eeschema / project manager / pcbnew). `.kicad_pro` write is revision-checked. **Description changed.** | Always |
| `set_predefined_sizes` | #804, #964 | Same as above. **Description changed.** | Always |
| `create_netclass` | #804, #964 | Same as above. **Description changed.** | Always |
| `assign_net_to_class` | #804, #964 | Same as above. **Description changed.** | Always |

### Error responses callers may now see

| `error.kind` | `reason` / meaning | Tools |
|---|---|---|
| `conflict` | The file changed after Konnect read it, so nothing was written. `paths` names the file. Retry. | the #845 board writers, `rename_project`, the four `.kicad_pro` writers |
| `mutation_outcome_uncertain` | The write happened, but reading it back did not show the edit (another program saved over it). Inspect before retrying. | the #845 board writers, `rename_project`, the four `.kicad_pro` writers |
| `unsafe_file_fallback` | `kicad_project_lock_present` / `kicad_project_lock_unreadable`: a KiCad program holds the project | the four `.kicad_pro` writers |
| `invalid_argument` (`field: layer`) | Not a KiCad board layer | `add_board_text`, `import_svg_logo`, `add_zone`, `add_copper_pour` |

Successful responses are unchanged, except that `layer` now echoes the
canonical name when an alias was passed (#849).

## Commits

| Commit | Source | Issue | Production change |
|---|---|---|---|
| `8f101cd` (merged in `f28a20b`) | upstream PR #852 | #845 | `pcb_board.rs`: new `write_board_edits` / `board_write_outcome` (compare under the writer lock with `transact_atomic`). 7 handlers switch from `read_to_string` + `write_atomic` to `read_consistent` + `write_board_edits`. |
| `c2d0908` (merged in `1f73ac1`) | upstream PR #850 | #844 (prerequisite of #849) | `pcb_board.rs`: new `board_layer_arg`, which validates the layer before either path runs |
| `6c3b191` (merged in `dba4dc9`) | upstream PR #868 | #851 | `project.rs`: `handle_rename_project` restructured (read, then rename, then one transaction, with rollback). `konnect-sexp`: new `try_commit_file_transaction` and `TransactionFailure`, which report whether a journal was written. |
| `4677626` (merged in `38ce521`) | upstream PR #817 | #804 | `writer.rs`: `kicad_project_lock_path`. `live_board.rs`: `project_lock`. `pcb_board.rs`: `refuse_if_project_locked_by_kicad`, called by the 4 project writers. 4 descriptions updated. |
| `44003bf` | **this fork** | #964 | `pcb_board.rs`: new `write_project_if_unchanged`. `verification.rs`: `set_design_rules` and `set_predefined_sizes` use it; `load_sibling_project` also returns the raw text. `pcb_routing.rs`: `save_project_settings` takes the text it read and returns a conflict result; `load_project_settings` also returns the raw text. |
| `74844f1` | **this fork** | #849 | `pcb_board.rs`: `board_layer_arg` returns `layer_name(try_layer_from_name(..))` instead of the caller's string (+11/−4 lines). |

The only merge conflicts were additive entries in `docs/API_MIGRATIONS.md`,
and all of them were kept. Each fix has its own entry there.

## Verification

**Automated:** `cargo clippy --workspace --all-targets -D warnings` is clean.
`cargo test --workspace` passes 2426 tests. The one failure,
`conformance_test::pin_anchored_labels_match_eeschema_orientation`, also fails
on unmodified upstream `main` in this environment, because it sweeps KiCad's
demo projects and `kicad-demos` is not installed. New regression tests:

- #964: `project_file_writer_tests::each_writer_refuses_a_project_file_changed_between_read_and_write`
  runs a served `tools/call` for all 4 writers with a concurrent rewrite injected between read and
  write. If the comparison is neutered, the test fails.
- #849: `board_layer_arg_tests::an_alias_is_returned_as_kicads_canonical_name`
- #845, #851, #804, #844 come with the tests from their upstream PRs.

**Live, against KiCad 10.0.8 on Linux** (proof-of-concept project in
`~/Projects/kicad/poc`):

- #804: with only Eeschema open, all 4 `.kicad_pro` writers refused with
  `kicad_project_lock_present` and left the file byte-identical.
- #849: with KiCad closed, `add_board_text(layer="F.Silkscreen")` used the file
  fallback, wrote `F.SilkS`, and `kicad-cli` loads the board. Control: the same
  board with `F.Silkscreen` written verbatim, which is what the old code did,
  fails with "Failed to load board: One or more items were found on undefined
  layers".
- #964: with KiCad closed, all 4 project writers succeed, and DRC afterwards
  shows 0 errors and 0 unconnected items.
- #851: `rename_project` on a copy produced the same ERC, DRC and
  schematic-parity results as the original, so no symbols were de-annotated.

## Known issues seen in the proof of concept, deliberately not fixed

These are out of scope for the minimal-change goal. None of them loses data.

- #788 / #789: `update_pcb_from_schematic` doesn't copy `Description` (and other
  fields) onto footprints, so DRC reports `footprint_symbol_field_mismatch`
  warnings plus a `lib_footprint_mismatch` warning.
- #465 (upstream PR #919): `export_pdf` with no `layers` fails ("At least one
  layer must be specified"). Workaround: pass `layers`.
- #790: the Specctra/Freerouting export refuses roundrect pads, which KiCad's
  stock 0805 footprints use, so the proof of concept routes with
  `route_pad_to_pad` / `route_trace`.
- #747: schematic tool responses report float noise (`78.74000000000001`).

## Pass 2: simple tool fixes

Found while building the 4-layer proof-of-concept board (`poc/mcu_board`) and
confirmed by an independent review. Each one is small and local. None of them
was on the upstream tracker except #784.

| Tool | What was wrong | What changed | Schema |
|---|---|---|---|
| `estimate_cost` | Every KiCad-saved board measured 0 × 0 mm. The text scan cut each `(gr_line` block at its first `)\n`, which comes before the `(layer "Edge.Cuts")` line. | Uses the parsed tree and the existing `board_outline_bbox` (lines, arcs, rects, polys), rounded to the micron. | unchanged |
| `audit_power_rails` | Any net starting with `VBUS`/`VCC`/`3V3`/… was audited as a rail, so `VBUS_SENSE` failed with "no decoupling". | A name with a signal suffix (`_SENSE`, `_EN`, `_PG`, `_DET`, `_FB`, `_OK`, `_FAULT`, …) is not a rail. | unchanged |
| `create_project` | The new board used pre-KiCad-9 layer ids (B.Cu = 31) under a KiCad 10 header; KiCad renumbered it on first save. | Writes the KiCad 10 ids, in the order KiCad 10 saves them. | unchanged |
| `add_layer` | Picked the first free id, which only matched legacy numbering: on a KiCad 10 board, adding `In2.Cu` alone took In1's id, and ids past In4 landed on technical-layer ids. Adding an existing layer duplicated it. | `In<n>.Cu` gets its canonical id (`2n+2`, or `n` on a legacy board). An existing layer, or an occupied id, is refused. | unchanged |
| `update_footprints_from_library` | Refused `(unlocked yes)` on a property or user text. Stock KiCad 10 libraries put it on Datasheet/Description: it was the refusal reason for 2,787 stock footprints, including SOT-223. | Accepted. On a property, it maps to the `BoardText` lock state (`yes` → `LS_UNLOCKED`, `no` → `LS_LOCKED`). On user text, `yes` is what the rebuild already sends, and `no` is still refused. | unchanged |
| `place_component`, `add_mounting_hole`, `duplicate_component` | Over IPC, the placed footprint got none of the library's `(attr …)` flags, description or 3D model (only the sync applied them, #789). Mounting holes landed in the CPL but not the BOM, so `export_manufacturing_package` reported INCOMPLETE. | Same library readers as the sync (`library_footprint::LibraryExtras`), applied to the built item before it is sent. The file fallback already copied the library text and is unchanged. | unchanged |
| `apply_template` | The description said it wires components; it only places them. Its `connections_to_wire` named template parts (`R1.2`, `D1.A`, `U.VIN`) instead of the references placed (R8, D8) (#784). | Description corrected. `from`/`to`/`via` are rewritten to the placed references. | **description changed** |
| `add_via` | Through vias only (F.Cu → B.Cu hardcoded), although KiCad 10's API takes blind, buried and micro vias. | Optional `start_layer`, `end_layer` and `via_type` (`through`/`blind`/`buried`/`micro`, inferred from the span when omitted). A type that contradicts the span is refused. The default call is unchanged. | **3 optional properties added** |

New internal API: `KiCadIpcClient::place_footprint_item` (place a pre-built
item; `place_footprint` now calls it), `builders::build_via_span`
(`build_via` calls it), and `library_footprint::LibraryExtras`.

### Measurements

- `update_footprints_from_library` parser over all 15,468 footprints in
  KiCad 10.0.8's library: 9,666 accepted before, 12,387 after. The remaining
  3,081 refusals are other clauses: pad `property` (fab properties, 1,069),
  `point` (617), custom pad shapes (459), footprint `solder_mask_margin`
  (252), `zone_connect`, zones, `justify`. Those need real mapping work and
  are not in this pass.
- The review's figure of 22% refused "for the `(unlocked)` clause" was about
  right for the total (37.5% before), but `(unlocked)` was the first refusal
  reason for only 2,787 of the 5,802. Fixing it accepted 2,721 of them; the
  other 66 hit a second unsupported clause.

### Verification

**Automated:**
- `cargo clippy --workspace --all-targets -D warnings` is clean.
- New tests:
  - `manufacturing::board_dimension_tests`
  - `design_review::power_net_name_tests`
  - `project::tests::blank_kicad_pcb_uses_kicad_10_layer_ids`
  - `pcb_board::add_layer_id_tests`
  - `pcb_footprint_update::tests::parser_accepts_the_unlocked_clause_as_the_field_lock_state`
  - `library_footprint::tests::extras_land_on_the_built_item`
  - `pcb_routing::via_span_tests`
  - the `apply_template` remapping assertions in `templates::tests`
- Three upstream tests used `(unlocked yes)` as their example of an unsupported clause. They now use the malformed `(unlocked maybe)`, so they still exercise the refusal path.

**Live, against KiCad 10.0.8** (`poc/simple_fixes_check.py`, on `poc/fixcheck`, a fresh copy of the 4-layer board): 21/21 checks pass.
- KiCad closed:
  - `create_project` + `add_layer` give ids 2/4/6, and pcbnew loads the board as 4 copper layers.
  - `apply_template` returns R8/D8.
  - `estimate_cost` measures 50 × 40 mm.
  - `audit_power_rails` no longer flags `VBUS_SENSE`.
- pcbnew live:
  - `update_footprints_from_library` plans U10 (SOT-223).
  - H1–H4 re-placed with `add_mounting_hole` are saved with `(attr exclude_from_pos_files exclude_from_bom)`, and so are TP90/TP91 from `place_component`/`duplicate_component`.
  - `export_manufacturing_package` (jlcpcb) is now `complete: true`.
  - Blind (F.Cu→In1.Cu), buried (In1.Cu→In2.Cu) and micro vias are saved as `(via blind|buried|micro … (layers …))`. A contradicting `via_type` is refused.
  - DRC accepts the new via types. The only errors were hole clearance to H1, from the test's own placement of the via 1.4 mm from the hole.
