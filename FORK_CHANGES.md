# Fork changes

Three passes so far:
1. The safe-write fixes (below).
2. The simple tool fixes found by the proof-of-concept boards ([Pass 2](#pass-2-simple-tool-fixes)).
3. Freerouting export through KiCad's own exporter, including 4-layer boards with inner planes ([Pass 3](#pass-3-freerouting-export-through-kicads-exporter)).

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

The served MCP surface did not change in that pass: no tool was added or
removed, and every `inputSchema` is byte-identical to upstream `main`. Only behaviour on the
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

## Pass 3: Freerouting export through KiCad's exporter

**Problem:**
- `export_specctra_dsn` refused almost every real board: anything with
  - more or fewer than 2 copper layers (#449),
  - zones,
  - roundrect, oval or NPTH pads (#790),
  - arc outline corners,
  - repeated pad numbers,
  - back-side parts,
  - or a `.kicad_dru` (#670/#685).
- The SES importer mirrored the same 2-layer, no-zone profile.
- The optional native path (ActionPlugin bridge) correlated KiCad's DSN against the strict exporter's output, so it could only ever accept boards the strict exporter already accepted. Its Topola syntax check also rejects KiCad's own `(outline (polygon …))` image outlines.

**Change:** with the new default `exporter: "auto"`, a board the strict exporter refuses (or one with a `.kicad_dru`) is exported by KiCad's own `pcbnew.ExportSpecctraDSN`.
- It runs on the exact IPC snapshot, written to a private temp dir with the project file, so the live board file is never touched. The board must be unchanged afterwards, or the export returns `conflict`.
- Python is `KONNECT_KICAD_PYTHON`, else the interpreter beside `kicad-cli`, else `python3`.
- The DSN is post-processed:
  - **Zones:** every `(plane …)` form is removed and reported, because Freerouting 2.1 treats Specctra planes as obstacles to every other net. Measured on the 4-layer PoC board: 35 of 41 connections unrouted after 999 passes with the planes in; fully routed in 5 passes without them.
  - **Power layers:** layers KiCad types `power` stay in the DSN, and Freerouting does not route on them.
  - **Zone nets:** routed as ordinary nets on the signal layers.
- The reverse manifest is built from that DSN (**schema 2**): layers, components (bound to board KIIDs by reference), pins, nets and classes. Via padstacks get their size and drill from KiCad's `Via[a-b]_D:d_um` name. Rotations are normalised to [0, 360).
- Existing routing must still be locked, as on the strict path.

**SES importer:**
- Accepts schema 2: any copper layer count, zones, custom rules, and front or back parts. The manifest, revision-hash, placement, net, layer and via checks all still apply.
- Refuses copper on a plane layer.
- Compares rotations modulo 360.
- Two changes for Freerouting 2.1's real output, on both schemas:
  - A via padstack repeated in `library_out` is accepted (each copy is still checked).
  - An exact repeat of a wire (same net, layer, width and endpoints) is dropped and counted in `duplicate_wires_dropped`, instead of refusing the session.
- `apply_specctra_ses` runs its DRC with zone refill when the board has zones, and its response says to refill the live zones.

| Tool | Schema |
|---|---|
| `export_specctra_dsn` | **new optional `exporter`** (`auto`/`strict`/`kicad`); description changed. `strict` is the old behaviour. The response gains `strict_exporter_refusal`, `copper_layers`, `power_layers_not_routed`, `removed_planes` and `warnings` when KiCad's exporter was used (`method: kicad_native_pcbnew`). |
| `plan_specctra_ses_import`, `apply_specctra_ses` | unchanged inputs; `apply_specctra_ses` adds `drc.zones_refilled` and `next_step` |

**Verification:**
- **Fixture:** `tests/fixtures/specctra_native_4layer.*` holds the real KiCad 10.0.8 DSN and Freerouting 2.1.0 SES of the 4-layer board (see its README). `specctra::native::round_trip_tests` lowers it, plans the import (244 segments, 12 vias, all on F.Cu/B.Cu), and checks that a session routing on In1.Cu and one for another revision are both refused.
- **Live** (`poc/freerouting_check.py`, on `poc/frcheck`: the 4-layer board with all routing removed, zones and `.kicad_dru` kept):
  - `export_specctra_dsn` used KiCad's exporter (4 layers, In1/In2 not routed, 3 planes stripped, `.kicad_dru` warning).
  - Freerouting 2.1.0 routed it completely in 3 passes.
  - `plan_specctra_ses_import`, then `apply_specctra_ses`, created 243 tracks and 6 vias in one undoable commit, with an exact IPC read-back.
  - After `refill_zones`, DRC shows **0 errors and 0 unconnected items**. The warnings are the board's existing ones, plus 2 dangling track ends from Freerouting.
- **With Freerouting 2.3.0 (Java 25), entirely through Konnect tools:**
  - `export_specctra_dsn`, then `route_specctra_dsn` (25 s, local, `cloud_used: false`).
  - `plan_specctra_ses_import`, then `apply_specctra_ses`: 228 tracks and 7 vias.
  - After `refill_zones` and save, DRC shows **0 errors and 0 unconnected items**, with the same 13 rule warnings as the hand-routed board.
  - With the planes left in the DSN, 2.3.0 still stops at 25 unrouted and 12 violations, so stripping them is still required.

**Fixes needed for Freerouting 2.3.0 (build 2026-08-07):**
- `route_specctra_dsn` failed with MCP "Internal error" on `create_session`. 2.3.0 refuses a plain-HTTP API endpoint unless `api_server.http_allowed` is set, and its MCP tools call that API. Konnect now passes `--api_server.http_allowed=true` and `--mcp_server.http_allowed=true`; both endpoints are loopback, as before.
- 2.3.0 also reports usage to `api.freerouting.app` by default, which contradicts the tool's "never sends board data to a cloud service" promise. Konnect now passes `--usage_and_diagnostic_data.disable_analytics=true`.
- 2.3.0 writes KiCad's variant images (`Lib:Name::1`) back under the base name. Schema-2 imports accept a base name for a `::N` variant; placement is still checked by position, side and rotation.

**Still open:**
- Unlocked existing routing is refused, and no Konnect tool locks tracks.
- A Konnect-owned fanout-to-plane step would let plane nets drop straight to their planes instead of being routed as traces.

## Pass 4: lock_routing and fanout_to_plane

Two new tools in `pcb_routing`, both live-only (KiCad IPC), each one undoable commit:

- **`lock_routing`**: locks or unlocks existing tracks, arcs and vias, optionally only on named nets, then reads the board back to verify. It fills the gap left by the Specctra export's "existing routing must be locked" rule: route critical nets (USB pair, power, crystal) by hand, lock them, then autoroute the rest.
- **`fanout_to_plane`**: gives every surface-mount pad on a plane net a short stub and a through via, so the pad drops straight to its plane.
  - Plane nets default to nets with a zone on an inner layer.
  - Via sites are chosen clear of every other net's pads, tracks and vias, of keep-out zones and of the board edge. These are box-model checks, so DRC follows.
  - Directions pointing away from the part are preferred.
  - Through-hole pads, and pads already joined to a track or via, are skipped.
  - Fan-out copper is locked by default. `dry_run` returns the plan without creating anything.
  - The planner (`tools/fanout.rs`) is pure geometry with unit tests.

**Export change (`specctra::native`):** a plane net whose every SMD pad is already fanned out is left out of the routing job. Its `(net …)` and class membership are removed from the DSN. Its fan-out copper goes back into the structure as keep-outs at the copper's own size, so the router routes around it.
- Widening the keep-outs by the clearance walled in USB-C pins: 1 connection was left unrouted.
- Leaving the copper out entirely let the router short +5V through GND vias: 4 shorts and 2 clearance errors.
- The response lists such nets in `left_to_planes`.

**Live verification** (`poc/fanout_check.py`, on the 4-layer PoC board with all routing removed): 8/8 checks pass.
- `fanout_to_plane` planned and placed 41 SMD pads (82 items). The 9 through-hole pads were skipped.
- `lock_routing` unlocked and then relocked exactly the 27 GND vias and 27 GND stubs, with read-back.
- The export left GND and +3V3 to the planes.
- `route_specctra_dsn` (Freerouting 2.3.0) routed only the signal nets: 100 tracks and 2 vias.
- After import and zone refill, DRC shows **0 errors and 0 unconnected items**, with the board's usual 13 warnings.

**Counts:** the tool count is now 230 (237 with meta-tools). The registry, `tool-directory.md`, README, DEV, TROUBLESHOOTING and the plugin/packaging metadata are updated.

## Pass 5: fixes from the independent review

An independent review (fable model) of passes 1–4 reproduced three high-severity and several medium defects with probe tests. All are fixed here.

**Autorouting, high severity:**
- **Signal traces could be routed through a plane.** The plane guard keyed only on KiCad's layer *type*, and `add_layer` creates `signal` layers. Now any inner layer carrying a copper zone is a plane: it is retyped `power` in the DSN (Freerouting does not route on it), and it is listed in the manifest's `plane_layers`, where the importer refuses copper on it.
- **`apply_specctra_ses` committed routes that shorted nets and still reported success.**
  - It now runs KiCad's DRC on the board both before and after, with the project's own `.kicad_pro`/`.kicad_dru`. The candidate was previously checked as a bare file against KiCad's defaults.
  - Any DRC error the import adds rolls the whole import back.
  - The report separates `errors`, `errors_before_import`, `warnings` and `unconnected_items`. `clean` now means no errors and nothing unconnected; before, it counted warnings and so was always false.
- **A net could be left to its plane without reaching it.** "Already connected" accepted any same-net track ending on a pad. Now a pad counts only when it reaches, through same-net tracks, a via that lies inside the net's inner-layer zone. Every planned via must also land inside that zone, so split and partial planes are handled.

**Fan-out geometry, medium:**
- **Outline:** inside/outside and edge distance come from the real Edge.Cuts lines, arcs, rectangles, circles and polygons. Before, the bounding box let a via land outside an L-shaped board.
- **Obstacles:**
  - Routed arcs are obstacles.
  - Keep-out rule areas, board-level and footprint-embedded, are respected for the whole via circle (`vias not_allowed`) and the stub (`tracks not_allowed`).
  - Pads are their true rotated rectangles, not axis-aligned boxes. This unblocked 45° QFN pins.
  - Copper-less pads (exposed-pad paste windows) are not obstacles.
- **Large pads:** a pad whose smaller side is at least 2 mm (an exposed pad or a regulator tab) gets a grid of up to 3 × 3 vias inside the pad, laid out in the pad's own rotated frame.
- **Fine pitch:**
  - A pin with no clear via site can tie with a pin-width stub to a same-net large pad of the same part that has vias (QFN ground pins to the exposed pad).
  - Clearance comparisons allow 1e-6 mm, so 0.5 mm-pitch stubs at exactly the clearance pass.
- **Sizes:** stub width and via size and drill come from each net's net class in the `.kicad_pro` (`netclass_patterns` with `*`/`?`, falling back to `Default`). Explicit arguments still override.
- **`fanout_to_plane` response:** each fan-out reports `in_pad`, `via` (false for a tie) and its sizes.

**Other:**
- **DSN net names:** names with spaces or brackets (`"GND A"`, `"GND(A)"`) are tokenised with their quotes when nets are dropped. They used to leave the `(net …)` behind.
- **Freerouting ports:** the API/MCP HTTP servers get a free loopback port per run, so concurrent jobs no longer collide on fixed ports and no other process can sit on a well-known one.
- **Tool directory:** corrected the stale `apply_template` (it does not wire) and `move_connected` (not implemented) entries.
- **Test flakiness:** the review saw 3 parallel-run failures in a scratch copy of the repo. Three full runs here passed cleanly, so it is most likely the shared build directory documented in the fork notes.

**Second regression board:** `tests/fixtures/fanout_regress2.*`, built by `poc/regress2/build_regress2.py`. It has:
- an L-shaped outline with an arc corner;
- inner planes typed `signal`;
- an LQFP-48 at 0.5 mm pitch;
- a QFN-32 at 45° with an exposed pad;
- back-side 0603 capacitors;
- an ESP-12E with a footprint antenna keep-out;
- a board-level keep-out.

Its tests check every planned via independently: on the board, on its plane, out of keep-outs, and clear of every other net's pads and vias. On that board, all 37 SMD power pads plan (9 in-pad, 2 ties, 0 skipped).

**Live verification** (`poc/autoroute_check.py`: fan-out → export → `route_specctra_dsn` with Freerouting 2.3.0 → plan → apply → refill → DRC, all through tool calls):

| Board | Fan-out | Import | Final DRC |
|---|---|---|---|
| 4-layer PoC (`frcheck`) | 43 pads (3 in-pad on the SOT-223 tab), 0 skipped, no new DRC errors | `clean: true`, 0 errors, 13 warnings | **0 errors, 0 unconnected**; 8/8 checks |
| Regression board 2 | 37 pads (9 in-pad on the 45° QFN exposed pad, 2 ties), 0 skipped, no new DRC errors | 0 errors (gate passed), 1 unconnected | 0 errors, **1 unconnected** (SIG6, LQFP → rotated QFN) |

**Open:**
- On the regression board Freerouting left one signal net unrouted with `max_passes` 20.
- `route_specctra_dsn` still reports `COMPLETED` in that case, because Freerouting's job details give no unrouted count.
- The import does report `unconnected_items: 1` and `clean: false`, so the gap is visible, but the router tool itself should surface it.
