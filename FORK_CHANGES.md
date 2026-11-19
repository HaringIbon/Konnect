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

## Pass 6: design intent, calculators and support-part placement

New toolset **`design_intent`** (6 tools, making 22 toolsets). The calculators exist for the AI's own decisions: results are JSON with the method, assumptions, valid range and warnings behind every number.

- **`engineering_calc`** (`tools/eecalc.rs`):
  - Width for current: IPC-2221 curve fit (outer k 0.048, inner 0.024). This errs wide compared with IPC-2152, which is the safe side for a decision.
  - Trace resistance and voltage drop: ρ(20 °C) = 1.7241e-8 Ω·m (IACS annealed copper). The temperature ratio comes from the NIST copper table (Matula 1979), as tabulated in EELearn's materials data.
  - Microstrip impedance: Hammerstad–Jensen with thickness correction.
  - Stripline and edge-coupled differential impedance: IPC-2141A.
  - Width for a target impedance (bisection), E12/E24/E96 values, and divider pairs.
  - Oracle tests against published values:
    - Pozar Example 3.7 (50 Ω, εe 1.87);
    - Wheeler's air microstrip (126.5 Ω at w/h 1);
    - the IPC-2141A stripline formula;
    - the IPC-2221 chart (1 A ≈ 0.3 mm, 2 A ≈ 0.76 mm);
    - 1 oz sheet resistance ≈ 0.495 mΩ/□ and +0.39 %/°C.
- **`get_design_intent` / `set_design_intent`:** `<project>.intent.json` (schema 1).
  - Contents:
    - per-net role (`signal`, `diff_pair`, `power`, `clock`, `sensitive`, `plane`), priority, pair, impedance, gap, current, temperature rise, maximum drop, source pad, maximum length, skew and clearance;
    - ties (`part`, `to`, `pin`, `max_mm`);
    - stackup, defaulting to JLC04161H-7628.
  - Merge semantics (null removes a net). Validated: roles, reciprocal pairs, positive values and `REF.PAD` sources. Revision-checked writes.
- **`apply_design_intent`:** dry run by default; refused while KiCad holds the project.
  - **Net classes:** one `intent_<net>` per net, cloned from `Default`, with exact-name patterns placed first.
  - **DRC rules:** `intent: …` rules in the `.kicad_dru` (track width minimum, diff-pair gap, skew, length, clearance).
  - Stale intent classes and rules are removed; user classes and rules are left as they were.
  - Warns when a pair's names lack the P/N or +/- suffix KiCad needs to treat them as a pair.
- **`check_design_intent`** (live or saved board), with each check pass, fail or unverified:
  - tie distance, pad edge to pad edge, measured from the part's pad on the pin's net;
  - routed length and pair skew;
  - track width against the current or impedance requirement;
  - voltage drop: Dijkstra over routed-copper resistance from the source pad, conservatively assuming the full current on every path;
  - impedance estimate and plane zones.
- **`plan_tie_placement`:** for each failing tie, a same-side position and rotation that minimises the distance from the part's pad on the pin's net to the pin. Its courtyard must clear the other courtyards and stay inside the real outline (via `fanout::BoardOutline`), and its pads must clear other nets. Later ties see earlier plans. It returns `placements` for `set_component_placements`.
- **Shared code:** `fanout.rs` now exposes `copper_pads` (true rotated pad outlines), `polygon_distance`, `sample_arc` and `BoardOutline`. `verification::named_rule_range` and `upsert_named_rule` are now `pub(crate)`.

**Verification:** 13 new unit tests, plus the oracle tests. Live run on a copy of the autorouted 4-layer board with KiCad closed (`poc/intent_check.py`, 9/9 checks):
- **Calculators through the tool:** 1 A → 0.302 mm; a 90 Ω pair on 7628 → 0.268 mm wide with a 0.15 mm gap; 5 V → 3 V → 20 k/30 k.
- **Rules and DRC:** `apply_design_intent` wrote 3 classes and 5 rules. `kicad-cli pcb drc` accepted them and enforced them: the XTAL2 length and its clearance to neighbouring nets were reported under the `intent:` rule names.
- **`check_design_intent`:** found C1, C2 and C3 at 5.9, 10.6 and 7.9 mm from VCC/AVCC (2 mm required), XTAL2 at 11.1 mm (10 mm allowed), and the +5V drop at 31 mV (100 mV allowed).
- **`plan_tie_placement`:** moved C1 to 1.1 mm and C3 to 0.5 mm. C2 reached 2.5 mm and is reported `meets: false`: VCC pins 4 and 6 share one side, and C1 took the nearest spot. After applying the plan, KiCad's DRC shows 0 courtyard violations, and `check_design_intent` agrees with the planner to 0.001 mm.

**Not done here:**
- `score_placement` and `place_decoupling_caps` keep their proven-rectangle outline model. Declared ties are now measured exactly by `check_design_intent` and placed by `plan_tie_placement` on any outline, so loosening that model, which several placement tools share, was left alone.
- Circuit-level checks with EELearn's nodal solver or ngspice are a separate step.

## Pass 7: AI-control tools and placement on real outlines

**Placement:** `score_placement` and `place_decoupling_caps`, committed separately as `8650a54`.
- **Real outlines:** any readable outline (rounded, notched, L-shaped, arcs) is checked against its real Edge.Cuts geometry through `fanout::BoardOutline::contains_rect`. A rectangle passes only if its corners are inside and no outline edge cuts into it. Before, such outlines skipped the checks with `outline_unproven`.
  - `outline_checks` reports `rectangle`, `real_outline` or `none`.
  - An edge-mount connector (J*) whose courtyard overhangs the edge, with its centre on the board, goes to `edge_overhangs` instead of failing.
- **Decoupling distance:** measured pad edge to pad edge, from the capacitor's pad on a shared supply net to the nearest IC pin on that net. The old courtyard distance and the 8-reference fanout limit remain only as a fallback when there is no pad geometry. This is why the 4-layer PoC's six MCU associations used to come out "unproven".
- **`place_decoupling_caps` is pin-driven:**
  - Each capacitor gets the nearest unclaimed same-net IC pin (pins are shared when they run out) and is placed beside it by the design-intent tie planner.
  - Capacitors already within their limit stay, and are listed in `unplaced` with the reason.
  - Capacitors sharing no supply net with the IC keep the old row on `side`.
  - Under the pin-level score the old row plan *lowered* the fixture's score (85 → 70); the pin-driven plan raises it to 100.
- **Tests:** about 20 tests were updated to the new metric, each with the distance hand-checked from the fixture geometry. Examples:
  - C2's VCC pad is 2.383 mm from U1 pin 8: dx 1.24, dy 2.035;
  - C1 is 7.146 mm from it, beside pins 1–4.

  One test's premise ("a cap within both the IC and the connector limit") cannot hold once the distance is measured to the VCC pin, so it was rewritten around the fixture's C2. The off-board row tests now use capacitors that share no supply net, so they still exercise the row path. The deprecated force-directed refiner's tests use a courtyard overlap, because its net-centroid model cannot improve a pin-level decoupling distance.
- **New tests:** a notch cutting a part whose four corners are all on the board, and connector overhang.
- **Live:** on the rounded 4-layer board, the score went from 50 to 85 (pass). C1, C2 and C3 moved to 1.1, 0.5 and 2.5 mm from their pins, with 0 courtyard violations in KiCad's DRC.

**AI-control tools:**
- **`route_path`** (`pcb_routing`): polylines whose points are `{x, y}` or `{ref, pad}`, plus through, blind or buried vias, in one KiCad undo step.
  - The net is given or inferred from the named pads. A pad on another net, or an SMD pad on another layer, is refused.
  - Widths and via sizes default to the net class. `lock` is optional.
  - Arguments are validated before any IPC call.
- **`render_board_png`** (`pcb_export`): renders the live board, or the saved file, as a PNG.
  - Chosen layers are drawn back to front: B.*, inner, F.Cu, front mask and paste, F.SilkS, outline.
  - `kicad-cli` paints in the order given, so a B.Cu pour used to hide the front copper.
  - A dark background is added.
  - The view is the whole board or a region in board mm, and the response returns `px_per_mm` and the region so pixels map back to board coordinates.
- **`checkpoint_board` / `diff_board` / `restore_checkpoint`** (`pcb_board`, new `board_history.rs`):
  - Checkpoints live under `.konnect/checkpoints/<board>/` with metadata.
  - The semantic diff reports parts added, removed, moved, rotated or flipped (with footprint changes), per-net segment, via and length changes, and zones.
  - A restore needs KiCad closed on the board, is revision-checked, and checkpoints the current file first.
- **`run_drc` `detail: "summary"`:** counts per rule with one example each (1.1 KB on the PoC, against about 33 KB in full). Every response gains `by_rule`.
- **`editor_not_ready` error kind:** any error result mentioning KiCad's `AS_NOT_READY` is normalised at dispatch, with `retry_after_s` and the original kind kept. It used to arrive as `stale_target`, `editor_unavailable`, `handler_error` or plain text, depending on the tool. A contract test now expects this kind for that one status.

**Live check** (`poc/step3_check.py`, 4-layer board):
- During pcbnew start-up the refusal arrived as `editor_unavailable` (no listener yet), then as `editor_not_ready` with `retry_after_s: 15`.
- Checks 7/7:
  - checkpoint from the live board;
  - whole-board render (1200 × 960, 24 px/mm) and a U1 zoom (56 px/mm), both inspected;
  - `route_path` U1.7 → Y1.1 inferred `/XTAL1` (3.302 mm, one undo step), and refused a pad on another net;
  - DRC summary 1,120 bytes;
  - `diff_board` showed the new XTAL1 segment.

**Counts:** `pcb_board` has 16 tools, `pcb_routing` 18 and `pcb_export` 15.

## Pass 8: ordering loop (fab rules, LCSC assignment, JLCPCB package gate)

- **`apply_fab_rules`** (`design_intent`): writes 16 JLCPCB standard-process limits into `.kicad_dru` as `fab: jlcpcb …` rules.
  - Limits: track width and space (outer 0.127 mm, inner 0.09 mm), routed-edge clearance, hole size, NPTH size, annular ring, hole-to-hole, track to PTH and NPTH, track to pad, pad and via to pad and via, silkscreen.
  - Values come from the community labtroll/KiCad-DesignRules file (document database `labtroll-jlcpcb-kicad-dru`). JLCPCB's own page could not be reached to cross-check them.
  - **Ordering:** KiCad lets a later rule override an earlier one for the same constraint. The fab rules are floors, so they are written at the top of the file, and intent and user rules override them.
    - The fable review predicted that appending them would mask intent rules. KiCad's DRC confirmed it on the PoC board: with fab rules last, one of three `intent: clearance /XTAL2` violations disappeared.
  - The intent merge was generalised to `merge_managed_rules(prefix)`, so the two rule families never touch each other.
- **`assign_lcsc_parts`** (`integration`, new `lcsc_assign.rs`) covers every in-BOM, non-DNP symbol in the root sheet and each sub-sheet file placed once.
  - **Existing numbers** are checked: existence, stock against quantity × `boards`, and package against the footprint.
  - **Missing numbers** are searched:
    - Passive values are parsed to SI and respelled the way LCSC descriptions are written: `100n`/`0.1u` → `100nF`, `4k7` → `4.7kΩ`, `2R2`, `1nF`/`1000pF`.
    - Candidates must be the right kind, so a "10kΩ" capacitor array is not a resistor.
    - Ratings written in the value (`16V`, `X7R`, `1%`, `1/10W`) must be stated in the description; an unstated rating fails closed.
    - Other parts match by an `MPN` field or the value as an exact MPN.
  - Ranking reuses `rank_alternatives`: Basic, then price, then stock, with a stock floor of max(100, quantity × boards).
  - **Each line reports:**
    - the search evidence and the ratings it could not check (capacitor voltage and dielectric, resistor power, inductor saturation);
    - the alternatives considered;
    - the number of distinct Extended parts, each of which carries a JLCPCB loading fee.
  - `parts` gives explicit choices, which are checked, not searched.
  - Dry run by default. Writing is one atomic batch edit per sheet file, through `batch_edit_schematic_components`.
- **`export_manufacturing_package` for JLCPCB:**
  - Without `bom_fields`, the BOM uses JLCPCB's template columns (Comment, Designator, Footprint, LCSC Part #), grouped by Value, Footprint and LCSC so different parts never share a row.
  - A row without a valid `C`+digits number makes the package incomplete. `allow_unsourced` lists consigned or hand-fitted parts.
  - The verified Gerber and drill files are also zipped (`gerbers-<board>.zip`).

**Live check** (`poc/step4_check.py`, copy of the 4-layer board, non-zero exit on failure): 13/13.
- **Fab rules:**
  - KiCad parses the merged rules.
  - The same intent-rule violations are reported before and after the fab rules are added.
  - Moving the fab rules last changes them, which demonstrates the ordering bug.
  - The fab rules themselves flag 32 silkscreen texts under 1 mm, 3 pad–silk and 1 track–pad violation on this board.
- **LCSC assignment:**
  - 8 lines assigned, all Basic: 100nF → C14663 (50 V X7R), 10uF → C15850, 22pF → C1653 (C0G), 5.1k/10k/4.7k/680 Ω, AMS1117-3.3 → C6186.
  - 2 existing numbers verified.
  - 6 unresolved with reasons: an LED valued "RED", the USB-C and pin-header connectors, and the button.
  - Both sheet files were written, and a re-run reports everything as ok.
- **Package:**
  - It is refused while those 6 parts are unsourced, and passes the LCSC gate with them in `allow_unsourced`.
  - The BOM header matches JLCPCB's template, and the zip holds 12 Gerber and drill files.
  - The one remaining warning is real: the board's 4 mounting holes predate the pass-2 attribute fix and lack `exclude_from_pos_files`, so they are in the CPL but not the BOM.

**Review:** a fable review of passes 1–7 and the test design ran alongside this pass. Its other findings are listed for the next pass: impedance enforcement and real stackup, inner-layer width check, voltage-drop reachability, SES gate by violation identity, in-pad via obstacles, diff and restore coverage, and calculator input validation. So are its test methods: KiCad-DRC oracles with seeded violations, negative gate tests, proptest geometry and metamorphic tests, published impedance tables, and CI for the live checks.

## Pass 9: review fixes (fable review of passes 1–8) and oracle tests

The fable reviewer critiqued the fix plan before coding. Its refinements are marked *(review)*.

1. **DRC gate by identity.**
   - **Comparison:** the gate used to compare error counts per rule, so one new short could cancel one fixed error. Now `new_drc_errors` takes the multiset difference of violations keyed by rule and sorted item KIIDs.
   - **Fallbacks:** a violation that names no items is keyed by rule and position at 10 µm, never by description, because descriptions embed measured values that change on a refill *(review)*.
   - **Always blocking:** `shorting_items`, `tracks_crossing`, `clearance` and `hole_clearance` block even when a project downgrades them to warnings *(review)*.
   - **Shared helper:** `gated_commit` creates items in one commit, then runs DRC before and after with the project rules and zones refilled on a copy. It refuses any new error, or any rise in unconnected items *(review)*. On refusal it deletes exactly the created KIIDs and proves the board serialises byte-identically.
   - **Coverage:** `route_path` and `fanout_to_plane` use it (`drc_gate`, default on). The SES import uses the identity comparison. Results report `zones_stale` *(review)*.
   - **Not fixed:** the SES path still refuses boards with existing arcs, because the exporter does not yet represent them to the router *(review)*.
2. **Impedance** (new `layer_stack.rs`).
   - **Stackup:** read from the board's `(setup (stackup))`, with sub-layers combined by thickness-weighted harmonic-mean εr. Falls back to the intent plus the board thickness.
   - **Per-layer model:** the reference planes are the copper layers holding plane-role zones. An outer layer over a plane is a microstrip; an inner layer between two planes is a symmetric stripline, or an IPC-2141A asymmetric one (2·Za·Zb/(Za+Zb)). Any other layer is unmodelled, never given a number.
   - **Rules:** per-layer width rules (`intent: impedance <net> <layer>`) with min and opt solved from Z·(1±tol) *(review)*. A width below the fab minimum, or a current width over the impedance maximum, is reported as a `conflict`; the width is no longer silently widened.
   - **Check:** each segment is evaluated on its own layer's model. A plane zone must cover the segment. A pair uses the measured edge-to-edge gap to the partner's parallel overlapping segments *(review)*. The check reports lengths in and out of tolerance, unmodelled, uncoupled and outside the formula's fit.
3. **Current width per layer.**
   - The inner minimum comes from the inner copper weight and the inner IPC-2221 factor. There are separate `(layer outer)` and `(layer inner)` rules.
   - The net-class width is the wider one, so the autorouter's single width is valid on every layer *(review)*.
   - The check compares each segment with its own layer's minimum at the DRC rule's rounding, and reports the undersized length. Up to 1 mm of necking passes.
   - The `eecalc` comment claiming IPC-2221 "errs wide" was corrected *(review)*.
4. **Voltage drop.**
   - Nodes are per layer, and vias join their span through a barrel resistance (0.020 mm plating). A segment is split wherever a track end, via or pad centre lies on it, which handles T-junctions.
   - An SMD pad joins its own layer; a through pad joins every layer. Copper is taken at 25 °C plus the allowed rise *(review)*.
   - A load that routed copper does not reach makes the check `unverified` and is listed by name. It used to be dropped silently, as an existing test showed: C1 sat mid-trace.
   - A failing track-only drop on a net that also has a zone is `unverified`, not `fail`.
5. **Fan-out.**
   - In-pad vias are now checked against other-net copper on every layer.
   - "Reaches the plane" is judged on the zone's `filled_polygon` when the file has one, and requires the whole via circle on copper *(review)*. Each plan reports its `plane_source`.
6. **Board history.**
   - Parts are keyed by footprint UUID, so duplicate references are told apart.
   - Routing is compared by length per layer and width. `other_changes` digests Edge.Cuts, zone outlines and settings (not fills), pad nets, and board setup and stackup.
   - Checkpoints include `.kicad_pro`, `.kicad_dru` and `.intent.json`.
   - A restore checks that the id is a plain name, verifies every stored sha256, parses the board, and refuses while the project is locked. It then writes all the files in one journaled `try_commit_file_transaction`. Older board-only checkpoints still restore *(review)*.
7. **Calculator inputs.** `engineering_calc` refuses inputs that are non-finite, ≤ 0, εr < 1, or a temperature at or below absolute zero *(review)*.

**Oracle tests** (method 1): `drc_oracle` builds seeded boards, judged by `kicad-cli`, and is added to the KiCad e2e workflow.
- **Coverage:** every JLCPCB rule must fire on a violation 20 µm past its limit and stay silent 10 µm inside it. The intent rule wins over the fab floor in the written order, and is masked in the reverse order. The per-layer impedance and inner-width rules fire only on their own layer.
- **Bugs found:** the oracle found two bugs that string tests had passed.
  - `!A.isPlated()` never matches an NPTH pad in KiCad 10; the rule now uses `A.Pad_Type == 'NPTH, mechanical'`.
  - The rule map sorted the fab rules by name, so the general `hole diameter` rule followed and overrode the NPTH one. Fab rules are now written in authored order, general first.
- **Dead-rule note:** "track to PTH hole" only bites where a hole has no copper on the track's layer, such as an inner layer with unused via pads removed.

**Negative gate tests** (method 2):
- **Unit tests:** a swap within one rule, ordering, multiset counts, uuid-less violations, a description re-measured after refill, and a downgraded short.
- **Live** (`poc/gate_check.py`, 4-layer board in pcbnew):
  - a `route_path` shorting XTAL1 into XTAL2 is refused (shorting_items, clearance, mask bridge) and the board diffs as identical;
  - a clean `route_path` passes;
  - a hand-made SES wire shorting two nets is rolled back, and the board is identical afterwards;
  - a clean SES wire is accepted.

  While I wrote the test, the gate also refused two "clean" wires I had chosen badly: one into Y1's GND pad, and one straight across it. Both were real shorts.

## Pass 10: fixes found by building the STM32 demo board (step 5)

The STM32F072 + Feather nRF52840 USB-C board (`~/Projects/kicad/stm32_demo/build.py`) was built end to end through these tools. A fable subagent reviewed the circuit and layout. Every fix below came from a real failure on that board.

**Design intent:**
- **New projects:** `apply_design_intent` failed on a project from `create_project` (no `net_settings`). It now starts from KiCad 10's defaults.
- **Planned planes:** the intent has `plane_layers`, so impedance rules can be made before the pours are drawn. Before this, no layer had a reference plane, and the pair got no rules at all.
- **Plane-only boards:** current widths only use the inner-layer width when an inner layer can carry tracks. A board with both inner layers as planes was getting a 0.6 mm net class for 0.5 A.
- **Net names:** `check_design_intent` fails intent nets that aren't on the board (`net_on_board`) and hints at a missing `/` sheet prefix. The demo's `USB_D_P` was really `/USB_D_P`, so its rules would have matched nothing. `set_design_intent` gained `remove_nets`.
- **Pair coupling:** it now unions every parallel partner segment, not only the best one. Partners further than 3 dielectric heights away count as uncoupled, which is where the coupled-line fit ends.
  - The demo's length-matching bump had read as a 112 Ω out-of-tolerance section.
  - The straight run beside two partner segments had read as 11.6 mm uncoupled.
  - It now measures 90.6 Ω over 17.5 mm.

**Routing:**
- **Atomic pairs:** `route_path` takes `routes`, several nets in one undo step and one DRC gate. Routing half a differential pair failed its own skew rule (KiCad: 22.4 mm vs 0 mm), so the gate rolled each half back.
- **Edge clearance:** the Specctra export now carries the copper-to-edge clearance as keep-out bands, one per outline edge on each signal layer.
  - Freerouting had kept only the 0.2 mm net-class clearance from the edge, and the gate refused its 0.24 mm tracks against JLCPCB's 0.3 mm rule.
  - A closed keep-out path is read as the whole board, which left nothing routed, so the band is drawn as separate edges.

**Checkpoints:**
- The list took the saved `<id>.intent.json` sidecar for a checkpoint, and `diff_board` failed on it.
- A restore failed when any saved file was unchanged, because the journal refuses no-op transitions. Unchanged files are now skipped.
- The restore test missed both bugs, so it now includes an intent file.

**Ordering:**
- **CPL:** the JLCPCB CPL leaves out footprints excluded from the BOM (fiducials). They broke the BOM/CPL designator cross-check.
- **LCSC:** `assign_lcsc_parts` reports an explicit choice that is already written as `ok`.

**Demo result:**
- **Schematic and parts:** ERC 0/0; 40 parts. The MCU changed to STM32F072C8T6 (C80488), because the CBT6 (C81720) shows 0 stock in the 2026-11-16 database.
- **Placement:** score 85 (pass); the one deduction is the Feather socket away from the edge, which is intended. All 14 ties are met.
- **USB pair:**
  - Routed by hand with `route_path`: 0.3/0.2 mm, length-matched to 22.431 mm each, 0 skew, no vias.
  - The channel swap on the USBLC6, suggested by the fable review, removed the crossing.
- **Rest of the board:** 49 pads fanned out to the planes; Freerouting did the rest through the identity gate.
- **DRC:** 0 errors, 0 unconnected, pours refilled.
- **Intent checks:** 27/27 pass:
  - Impedance 90.6 Ω.
  - +5V drop 86 mV against a 100 mV limit.
  - Widths and lengths.
- **JLCPCB package:**
  - Complete: 24 BOM lines and 37 placements.
  - The five hand-fitted connectors are listed in `allow_unsourced`.
  - Five Extended parts: J1, U1, U2, U3, F1.
- **Iterations:** reaching that took three routing passes. Twice Freerouting reported COMPLETED with 3 nets unrouted around the MCU. Moving C8 out of PF0/PF1/NRST's escape, C6 past C5, and the SWD header beside the MCU cleared it.

**Not fixed:**
- `route_specctra_dsn` still says COMPLETED when nets are unrouted.
- `update_pcb_from_schematic` doesn't copy custom fields (LCSC, MPN) to footprints: 37 DRC parity warnings. The BOM is built from the schematic, so the order is unaffected.
- There is no keep-out area tool. The antenna keep-out was done by stopping the plane pours.
- Footprint reference text is 0.12 mm thick, under JLCPCB's 0.15 mm guideline (47 DRC warnings).

## Pass 11: `delete_zones`

- **New tool, `delete_zones`** (`pcb_routing`): it deletes copper zones and rule-area keep-outs from the live board in one undo step.
  - Zones are selected by net, layer and/or name; dry run by default.
  - KiCad 10's zone protobuf has no net, so zones are matched in the live board's saved text, checked against KiCad's own zone list, and deleted by UUID.
  - Shrinking the demo board needed it: the old plane pours had to go, and only `delete_graphics` existed, which doesn't cover zones.
- **Demo board compacted:** from 76.5 × 58 mm to 55 × 46 mm (−43%).
  - Low-profile parts sit under the Feather.
  - The MCU turned back to 0°, so its GPIO side faces the headers and the USB pair runs straight with no length-matching bump.
  - SWDIO is hand-routed before the fan-out.
  - Results: DRC 0 errors / 0 unconnected; 27/27 intent checks (90.6 Ω pair, +5V drop 33 mV); JLCPCB package complete.
- **Not fixed:** the DRC gate compares unconnected items by count. One import lowered the total from 64 to 8 while stranding two fan-out vias, so per-item comparison would be stricter. KiCad doesn't give unconnected items stable identities across refills, so this is noted, not changed.

## Pass 12: rotated pads in KiCad's Specctra export

- **Bug:** `export_specctra_dsn` failed with "DSN is missing pin name" on any board where a pad is rotated within its footprint. Two-sided boards hit this all the time; the Ethernet board's agent reported it too.
  - KiCad writes such a pin as `(pin <padstack> (rotate <deg>) <id> <x> <y>)`.
  - The KiCad-exporter lowering in `specctra/native.rs` read the id at a fixed index 2, which is the `(rotate …)` list.
- **Fix:** the id is now the first atom after the padstack.
- **Test:** `rotated_pins_keep_their_names` inserts a `(rotate 90)` into the 4-layer fixture. The manifest must come out identical to the unrotated one.
- **Found while building:** `boards/stm32_nrf_module`, the two-sided castellated-module board.
- **Noted, not changed:**
  - `update_pcb_from_schematic` refuses every pad-net change on a board that has zones. KiCad 10's zone protobuf hides the net, so the check fails closed. Workaround: delete the pours first, sync, then pour again.
  - `add_board_outline` appends a second outline rather than replacing the old one; its description says so. Workaround: clear Edge.Cuts with `delete_graphics` first.

## Pass 13: keep-out areas, layer retyping, `mixed` routing layers

- **New tool, `add_keepout_area`** (`pcb_routing`, so it now has 20 tools; 245 registered and 252 with the meta-tools):
  - It writes a KiCad rule area to the live board in one undo step: a polygon on chosen copper layers (default: all of them).
  - The area can forbid tracks, vias and copper pour, and optionally pads and footprints.
  - KiCad's DRC enforces it, and KiCad's Specctra export passes it to Freerouting as a `(keepout …)`. Remove it with `delete_zones(names=[…])`.
  - Builder: `konnect_ipc::builders::build_keepout`, with a unit test.
  - **Why:** `update_pcb_from_schematic` and the footprint pipeline drop zones embedded in a footprint. As a result the MDBT50Q's antenna keep-out and the TC2030 pad-field keep-outs were missing from the module board. The Fable review found that the planes and tracks ran under the antenna. Restoring embedded zones in the sync is the larger fix and has not been done.
- **`add_layer` retypes:**
  - Naming an existing inner copper layer with a different explicit `layer_type` (for example `power` → `mixed`) changes its type in place.
  - Without a type, or with the same type, it still refuses as before.
  - Test: `an_existing_inner_layer_is_retyped_on_an_explicit_type`.
- **Specctra lowering keeps `mixed` layers routable:**
  - An inner layer the board types `mixed` keeps its pour out of the DSN, but is no longer forced to `power`, so Freerouting can route on it.
  - Test: `a_mixed_inner_layer_stays_routable`.
  - **Limit, found on the module board:** a pour net that every pad already reaches through fan-out is still dropped from the routing job. Routes on the mixed layer then cut the pour into islands that nothing rejoins (five +3V3 islands). That board went back to a solid `power` plane. A fix would keep such nets in the job when their plane is on a `mixed` layer.

## Pass 14: `delete_vias` and `set_footprint_text`

Both tools are registered in `pcb_board`, which now has 18 tools. `pcb_routing` and `pcb_components` are at the 20-tool cap. The code lives in `pcb_routing.rs`. Totals are 247 registered and 254 with the meta-tools.

- **`delete_vias`:**
  - Deletes vias selected by UUID, or by position within a tolerance, optionally filtered by net.
  - One undo step, dry run by default, and the vias are verified absent afterwards.
  - `delete_trace` refuses vias, so before this a dangling via (the Ethernet board's ETH_TX_EN) or a via from a re-route could not be removed.
- **`set_footprint_text`:**
  - Restyles the reference or value field of placed footprints over IPC (`FootprintInstance.reference_field` / `value_field`): stroke thickness, character size and visibility.
  - Footprints are selected by reference and/or side. One undo step.
  - `update_pcb_from_schematic` writes reference text at KiCad's automatic 0.125 mm, under JLCPCB's 0.15 mm minimum (107 warnings on the Ethernet board), and no tool could change it.
- **Tests:** `via_and_text_tool_tests` (via selection; field restyling).

## Pass 15: Freerouting's own result in `route_specctra_dsn`

- **Problem:**
  - Freerouting 2.3.0 ends every job in state `COMPLETED` even with connections still open. That was the "COMPLETED but unrouted" trap on every board.
  - Konnect also cut Freerouting's stderr at 64 KB. Under MCP that stream carries JSON-RPC traffic, so the router's final score lines were lost.
- **Evidence:**
  - The 2.3.0 jar logs its own counts: `BatchAutorouter` "Auto-routing pass #N … with the score of S (U unrouted and V violations)"; `RoutingJobSchedulerActionThread` "Auto-routing stage … started with N unrouted …, final score: …"; `BatchOptimizer` "Optimization stage completed … final score: …".
  - A real run on the module board's DSN was captured as `tests/fixtures/freerouting_2_3_0_run.log`: 64 unrouted at the start, 12 at the end, and 52 router-side violations.
- **Change:**
  - The whole log is kept (cap raised to 16 MB).
  - `parse_routing_log` reads the initial unrouted count, every pass's score, unrouted and violations, and the final figures (optimizer stage first, then auto-routing stage, then last pass).
  - `route_specctra_dsn` now returns `routing: {complete, initial_unrouted, unrouted, violations, score, autoroute_passes}`, and a `warning` when the router reports anything unrouted or any violation, or when the log holds no score.
  - `final_state` still reports the job state.
- **Tests:** `routing_log_tests` (4 tests, on the real fixture).
- **End-to-end check:** routing the module board through MCP (23 passes) returned `complete: false, initial_unrouted: 64, unrouted: 7, violations: 52`, with the warning.

## Pass 16: design-intent defaults

Code: `design_intent.rs`, `eecalc.rs`, `layer_stack.rs` (and a 3-line `Tie` literal update in `placement.rs`). Sources are cited by document-database slug.

- **Width from current is IPC-2152:**
  - The Brooks & Adam closed-form fit to the IPC-2152 data, ΔT = K·I^a·W^b·Th^c, with the same equation and coefficients as KiCad master commit `7f558e2779` [`kicad-ipc2152-track-width`]. The internal fit is chosen by the nearest nominal copper weight.
  - No internal-layer derating: IPC-2152's data show inner traces cool about as well as outer ones. Inner widths are now about 1.5× outer (0.5 oz inner, 1 oz outer), where IPC-2221 gave about 2.6×.
  - The old chart [`kicad-10-ipc2221-track-width`] stays available as `engineering_calc` width_for_current `method: "ipc2221"`.
  - Default rise is 10 °C (KiCad's calculator default). A power net without `current_a` gets a warning that no width is derived (suggested basis: the fuse or PTC trip current).
- **Interface standards on nets (`standard`, `rise_time_ns`):**
  - `usb2_fs`, `usb2_hs`: 90 Ω ±10 %, skew ≤ 1.27 mm (50 mil) [`ti-spraar7j`]; FS rise time 4 ns [`st-ds14258-stm32h563`], HS 0.5 ns (USB 2.0 spec §7.1.2.2, quoted from memory: usb.org is blocked from this host).
  - `ethernet_100base_tx`: 100 Ω ±10 %, skew 1.27 mm, length ≤ 50.8 mm [`ti-snla387-ethernet-phy-layout`], rise 3 ns [`microchip-lan8742a-ds`]. `ethernet_1000base_t`: skew 0.508 mm.
  - `rmii` (single-ended): length ≤ 152.4 mm [`ti-snla079d-phyter-layout`, `ti-snla387-ethernet-phy-layout`, `microchip-lan8742-routing-checklist`]. No impedance default, because TI says 50 Ω and Microchip 68 Ω.
  - **Edge-rate rule:** a pair without a published skew gets 10 % of its rise time as length, through the slowest routable signal layer's propagation delay √εeff/c0 (a stripline where one is modelled; c/√εr of the slowest dielectric when none is), so the bound is always conservative. Johnson gives a 20 % skew budget, and an unspecified driver already uses at least 10 % [`johnson-edn-2004-cm-analysis-of-skew`, `johnson-edn-2002-your-layout-is-skewed`]. With a standard, the tighter of the published value and the edge-rate bound wins.
  - The gap default stays 0.15 mm: no standard gives a gap.
  - **Pairs share pair-level fields:** `standard`, `rise_time_ns`, impedance, tolerance, gap and skew set on one net apply to both halves (source "user, set on pair partner …"), so both resolve to the same limits and `dru_rules` emits the pair's impedance and skew rules whichever net sorts first. Different values on the two nets are refused by validation.
- **Tie kinds (`kind`, `max_path_mm`, `max_vias`):**
  - `crystal`: path ≤ 0.5 pF ÷ C′ of a 0.2 mm outer trace, ≈ 5.5 mm on JLC04161H-7628; 0 vias. The 0.5 pF budget is AVR186's 1 pF XTALIN parasitic less the pads [`microchip-avr186-oscillator-layout`].
  - `core_regulator_cap`: path ≤ 1 nH ÷ L′ of a 0.3 mm outer trace, ≈ 3 mm; 1 via. `decoupling`: 2 nH ÷ L′, ≈ 6 mm; 1 via. The nH budgets are a Konnect choice, about one MLCC's own mounted ESL; ST and Microchip give no number [`st-an5711-stm32h5-hw`, `microchip-lan8742-routing-checklist`]. The output says so.
  - `max_mm` is optional with a kind (it defaults to the path limit). Without a line model for the outer layer the fallbacks are 6, 3 and 6 mm.
- **Stack-up basis:** `get_design_intent` and `set_design_intent` resolve against the saved `.kicad_pcb` when it exists, like `apply_design_intent`, and report `stackup_basis`; without a board the basis reads "assumed default stack-up".
- **Width flips on existing boards:** IPC-2152 is narrower than IPC-2221 below about 1 A and wider above. On the Ethernet board +5V and VBUS (1.5 A, routed 0.53 mm) now fail against 0.564 mm, and /RP_1V1 goes fail → pass.
- **Provenance:** an explicit user value always wins. Every resolved value reports where it came from: `requirements[].sources` (field → `"user"` or the citation) and `tie_requirements[]` in the get, set and apply outputs, and `required.sources` on tie checks.
- **`LineModel::per_mm`:** gives εeff, delay, C′ and L′ per mm for a microstrip or stripline.
- **Routed tie path (`tie_path` check):**
  - `voltage_drop`'s graph builder is factored into `net_graph`: per-layer nodes, tracks split at T-junctions, via barrels between consecutive spanned layers, and pads joining every node on them. Each edge is typed (`Track`, `Barrel`, `Pad`) so callers choose the weight. `NetGraph::shortest` is a multi-source Dijkstra with predecessors. `voltage_drop` now weighs that graph by resistance; its results are unchanged and its tests pass as before.
  - `tie_path` runs Dijkstra on length (barrels at their dielectric height) from the tied part's pads on each net it shares with the pin, to the pin pad, and keeps the shortest over the shared nets.
  - It reports `from_pad`, `to_pad`, `net`, `path_mm` (track length), `vias` (distinct vias), `layers` (in path order, including a layer a via lands on straight into a pad, but not inner layers a via only passes through), `est_inductance_nh` = Σ L′·len and `est_capacitance_pf` = Σ C′·len per segment at its own width and layer, and `unmodelled_mm` (track on layers with no reference plane, left out of the estimates).
  - It fails above `max_path_mm` or `max_vias`. It is `unverified`, with the reason, when the part shares no net with the pin, when no routed track path joins them (`not routed`), or when that net has a zone that may carry the connection. A failing path on a net that also has a zone is `unverified`, because a zone could only shorten the path. A passing path stands.
  - **Limits:** the L/C estimates leave out via barrels and pads. Via inductance is not estimated: no sourced formula is in the document database yet. The via limit stands in for it.
- **Tests:**
  - `eecalc`: `ipc_2152_matches_kicads_reference_values`; `ipc_2152_does_not_derate_inner_layers_and_tracks_the_old_chart_outside`.
  - `layer_stack`: `per_length_constants_match_published_values` (air, Pozar Ex. 3.7, FR-4 stripline, L′·C′ = τ²).
  - `design_intent::default_tests` (7 tests).
  - `design_intent::tie_path_tests`:
    - `a_direct_trace_passes_with_its_length_and_capacitance`: L/C equal L′·len and C′·len.
    - `a_detour_fails_on_length_and_the_shortest_branch_counts`.
    - `a_path_through_a_via_to_the_bottom_counts_the_via`: a crystal fails, decoupling passes, a bottom pad on the via counts B.Cu, and no via means no path.
    - `an_unrouted_or_zone_carried_tie_is_unverified_with_the_reason`.
  - `design_intent::review_tests` (7, after an independent review): a standard, rise time or skew on one half of a pair; conflicting pair values refused; the edge-rate skew at the slowest layer (stripline); `get_design_intent` on a saved 2-layer board against the assumed stack-up; a tie without a pin over two shared nets; a through-hole pin joined by a bottom track; `voltage_drop` pinned to the exact least-resistance path on a fixed graph.
  - `cargo test -p konnect-core --lib`: 1841 passed, 0 failed, 23 ignored.
- **Sanity check on the saved Ethernet board** (read-only, with every tie given a 1000 mm path limit in memory; no file changed, no KiCad):
  - 41 ties measured in about 0.25 s (debug build). 37 pass. 4 are unverified: +3V3 caps joined only by the 3V3 pour (C7, C11, C25, C26).
  - The figures are plausible: Y2 crystal 5.5 mm, 0 vias, 0.41 pF. C15 VCAP 2.4 mm, 1 via, 0.79 nH. C10 VBAT 10.8 mm, 3 vias. R1 CC1 27.2 mm, 3 vias.
- **Not done:** DRC rules from crystal and pair defaults; RMII group matching; re-reading ST AN2867 (st.com unreachable); migrating the board scripts (`eth_board/build.py` still sets explicit skews and tie lengths, so user values win there).

## Pass 17: Tier 1 fixes (mixed-layer plane nets, CPL pads-only, mirrored bottom text)

### Mixed layers keep the plane net in the routing job

- **Problem (Pass 13 limit):** a plane net whose every pad already reaches its pour through fan-out was dropped from the Freerouting job even when the pour sat on a `mixed` layer. Freerouting then routed other nets across that layer, cut the pour into islands, and nothing rejoined them (five +3V3 islands on the module board).
- **Freerouting's side** (2.3.0 source, doc DB `freerouting-2-3-0-structure-java`, `-layerstructure-java`, `-network-java`, `-net-java`):
  - `(type power)` sets `is_signal = false`, and the router never routes on such a layer. `signal` and `jumper` layers stay routable. KiCad writes a `mixed` layer as `(type signal)`.
  - A net counts as a plane net (`contains_plane`, which only buys cheaper vias) only through a non-signal layer's `use_net` or a `(plane …)`. Konnect strips every plane, so a net kept in the job is an ordinary net and is routed as traces.
  - A net left out of `(network …)` is invisible to the router, so nothing it routes connects that net's islands.
- **Change** (`specctra/native.rs`, `split_by_plane_layer`): a fully fanned plane net leaves the job only when at least one of its inner pours is on a layer that stays a solid plane (not `mixed`). Nets whose inner pours are all on `mixed` layers stay in the job and are reported.
  - A net that also has a pour on a `power` layer still leaves the job: the solid plane joins every fan-out via.
  - On a board without `mixed` layers nothing changes.
- **Response:** `export_specctra_dsn` gains `kept_for_mixed_planes` and a warning naming those nets.
- **Tests:** `a_fully_fanned_net_on_a_power_plane_leaves_the_job`, `a_fully_fanned_net_on_a_mixed_layer_stays_in_the_job` (the real 4-layer fixture with +3V3 pads made fully fanned: all pins stay in the network and the manifest), `a_net_with_a_pour_on_a_power_layer_too_still_leaves_the_job`.
- **Live** (copy of the module board, signal routing stripped, GND/+3V3 fan-out locked):
  - In2 `power`: `left_to_planes: ["+3V3", "GND"]`, In2 `(type power)`, neither net in the DSN network (unchanged behaviour).
  - In2 retyped `mixed` with `add_layer`: `left_to_planes: ["GND"]`, `kept_for_mixed_planes: ["+3V3"]`, In2 `(type signal)`, +3V3 in the network.
  - Freerouting 2.3.0 on the mixed DSN (25 passes): 28 → 4 unrouted, 46 router-side violations; 112 tracks on In2, 3 of them +3V3. `apply_specctra_ses` rolled the import back for reasons outside this fix (the board's intent rules for the stripped USB pair and VBUS width, then Freerouting's 0.112 mm tracks against the 0.127 mm board minimum).
  - The same SES imported offline with `pcbnew.ImportSpecctraSES`, zones refilled, then `kicad-cli pcb drc --refill-zones`: the In2 pour is cut into 8 pieces, yet **+3V3 has 0 unconnected items**, because its own traces join the pads. The 4 unconnected items left are 2 GND (J2/J7 pad 5 to their vias), 1 VBUS and 1 USB_D_P, all outside this fix.
  - Not measured: the same route with the old behaviour, for a side-by-side count. The Pass 13 record (five +3V3 islands left unjoined) is the baseline.

### CPL completeness ignores footprints that are not placements

- **Problem:** the schematic BOM lists a pads-only footprint (the Tag-Connect J3), but KiCad's position file omits it (`exclude_from_pos_files`), so `export_manufacturing_package` for JLCPCB reported "missing from CPL [J3]" plus an unsourced BOM row, and the package read INCOMPLETE. `allow_unsourced` only silenced the second warning.
- **Which footprints the position file omits:** KiCad 10.0.8 `PLACE_FILE_EXPORTER::GenPositionData` (doc DB `kicad-10-0-8-place-file-exporter`) skips `exclude_from_pos_files`, and with Konnect's `--exclude-dnp` also `dnp`. Konnect already drops `exclude_from_bom` rows from the JLCPCB CPL. KiCad never skips `board_only`, so `board_only` counts only alongside another exclusion attribute, or on a footprint with no pads (a logo, reported as `board_only` + `no_pads` and dropped from the CPL). A real part placed without a symbol (`board_only` with pads) is still a placement and still reported "missing from BOM".
- **Change** (`tools/manufacturing.rs`):
  - `not_placed_footprints` lists every footprint carrying one of those four attributes, with the attributes as reasons.
  - `drop_unplaced_bom_designators` takes those designators out of the JLCPCB BOM (whole rows, or one designator of a grouped row, with a Qty column kept in step), so they need no LCSC number and the BOM and CPL match. The file is rewritten through the verified-publish path.
  - A row that carries an LCSC number is left alone: a sourced part whose footprint is excluded from placement is a real inconsistency. The BOM/CPL mismatch warning now names the attribute that hides it.
- **Response:** `not_placed: [{reference, reasons, removed_from_bom}]`, and a non-blocking `notes` array with one line per designator removed from the BOM (and its attributes). The summary names the removed designators. Neither affects `complete`.
- **Tests:** `cpl_bom_exclusion_tests` (8): attribute reading, the J3 case end to end (BOM rewrite, LCSC check, designator match, notes, report), grouped rows and quantity, sourced rows still failing with the reason, a `board_only` part with pads staying a placement ("missing from BOM [C99]"), pad-less and excluded `board_only` footprints, untouched BOM without exclusions.
- **Review fix:** the first version dropped every `board_only` footprint from the CPL, which would have marked a package complete while a symbol-less real part was never placed.
- **Live** (copy of the Ethernet board, pcbnew open): without `allow_unsourced`, `complete: true`, no warnings. `not_placed` lists FID1–3 (`exclude_from_bom`), H1–4 and J3 (`exclude_from_pos_files`, `exclude_from_bom`); only J3 was in the schematic BOM and was removed from it. The BOM is otherwise identical to the board's last package apart from parts removed from the design since.

### Bottom-side `add_board_text` is mirrored

- **Problem:** text on B.SilkS, B.Cu, B.Fab and the other back layers was written unmirrored, so it read backwards from the bottom.
- **KiCad's behaviour:** its text tool sets `m_Mirrored = IsBackLayer(layer)` (doc DB `kicad-10-0-8-drawing-tool`), and the file form is `(justify mirror)` inside `(effects …)` (`kicad-10-0-8-eda-text`).
- **Change** (`tools/pcb_board.rs`): `board_text_mirrored` mirrors text on any `B.` layer by default; the IPC path sets `TextAttributes.mirrored`, and the file fallback writes `(justify mirror)`. A new optional `mirror` argument overrides the default. The response reports `mirrored`.
- **Tests:** `bottom_side_board_text_is_mirrored_in_the_file` (8 layer/override cases, parsed back), `bottom_side_board_text_is_mirrored_over_ipc` (the BoardText sent in `CreateItems`, and the file left untouched).
- **Live** (copy of the Ethernet board): over IPC, B.SilkS and B.Fab text saved by pcbnew as `(justify mirror)`, F.SilkS text without; through the file fallback (KiCad closed), B.SilkS and B.Cu text. `pcbnew.LoadBoard` reads `IsMirrored()` true for all four back-layer texts and false for both front ones, and `kicad-cli pcb export svg` loads the board.

## Pass 18: plane-connection check

Code: `design_intent.rs`, plus doc updates. Pass 17 is on another branch, so the number is left free.

- **The rule** (user, 2026-11-19): an SMD pad with a ground connection must drop straight to the ground plane through a via, with no long trace, unless documentation says otherwise (an analog/digital ground split, for example). Konnect applies this to every plane net, GND first.
- **Why a check was needed:** `fanout_to_plane` skips pads that already have a track or via. A GND pad that the autorouter, or a hand route, reached first through a long trace therefore passed silently.
- **Check `plane_connection`:** `check_design_intent` runs it for every SMD pad on a plane net. A plane net is one with an inner-layer zone (the `fanout_to_plane` default) or an intent `plane` net that has a zone.
  - **Path search:**
    - It reuses `net_graph` and its Dijkstra. `shortest_from` is a variant whose weight also sees the step's nodes, so a path can be blocked through a node.
    - It searches from the pad to the nearest via (or through-hole pad) that lands on a zone layer of the net other than the pad's own. "Lands" means the zone's fill reaches the via (the zone outline when the file has no fill).
  - **Reported per pad:**
    - `stub_mm`: the connecting trace, meaning track outside the pad and outside the target via's annulus. A via in the pad is 0, a via butted against the pad is about 0.
    - `vias`, the target via and the layers it lands on, `est_inductance_nh` (Σ L′·len over the connecting trace, through the `LcSum` factored out of `tie_path`), `connection`, and `through_pads`.
  - **Statuses:**
    - **Pass:** `stub_mm` ≤ limit.
    - **Fail `long_stub`:** the stub is over the limit.
    - **Fail `no_via`:** the track graph finds no path to a plane via. The detail says "the track graph finds no path", not "unconnected".
    - **Fail `daisy_chain`:** the pad reaches the plane only through another part's pad copper. The search runs first with foreign-pad copper blocked; a through-hole pad that is itself a way down is not blocked. UG483 calls for one via per land.
    - **Unverified `none`:** nothing touches the pad yet (not routed).
  - **Same-layer pour:**
    - A pad joined to a filled pour of its net on its own layer passes, with `connection: pour`, when a via landing on another zone layer sits in the same pour island within the limit. The distance is a straight line from the pad edge to the via edge, and the detail gives the reason: a pour is far wider than a trace, so this is a real plane connection.
    - It also passes when the pour's layer is the net's only zone layer, because then the pour is the plane.
    - It fails when the nearest such via is beyond the limit.
    - It is `unverified` when the zone has no fill in the file.
  - **Shared vias** follow the plane regime (see *Review fixes* below): capacitor–capacitor fails, capacitor–device fails on opposite sides or with closely spaced planes and passes as optimal with widely spaced planes, and an unknown regime warns.
  - **Via-in-pad warning:** a via whose annulus overlaps the pad of a 2-pad part with a shorter side under 1 mm (0402/0603) still counts 0 mm electrically, but adds a `warnings` entry. An open via wicks solder and can tombstone the part, and the board file does not record fill/cap. Source: Eurocircuits PCB assembly guidelines, tombstoning (https://www.eurocircuits.com/pcb-assembly-guidelines-tombstoning/), not in the doc DB (blocked); the thresholds are a Konnect judgement.
  - **Summary:** `check_design_intent` returns `plane_connection` with a per-net count by status and the 5 worst failing pads. `summary` counts `excepted_pass`, `excepted_fail` and `excepted_unverified`.
- **Limit:**
  - Default: `max_stub_mm` = 0.5 nH ÷ L′ of a 0.3 mm outer trace, floored to 0.1 mm. That is **1.5 mm** on JLC04161H-7628 (L′ = 0.330 nH/mm). The fallback without an outer line model is 1.0 mm.
  - **No source gives a length; the default is a Konnect judgement:**
    - AMD/Xilinx UG483 v1.14 [`xilinx-ug483-7series-pcb`]: p.24 "Vias should be butted directly against the pads"; p.30 "When possible, a connecting trace should not be used and the via should butt up against the land"; never share vias between capacitors and "maintain a one-to-one ratio of lands to vias". Table 2-5 gives ≤ 0.5 nH ESL for 0603/0805 MLCCs, which is the source of the budget: the connecting trace adds no more than one small capacitor's own ESL.
    - TI SLUA457 [`ti-slua457-decoupling`]: "place a via near the VSS pin to a ground plane layer" (no number).
    - LearnEMC, "Power Bus Decoupling Guidelines … Closely Spaced Power Distribution Planes" [`learnemc-decoupling-closely-spaced-planes`]: "Never use traces! Locate the vias adjacent to the mounting pads"; "If there is no room for the via adjacent to the pad, then move the whole capacitor"; "Do not let high-frequency decoupling capacitors share a via with an active device". No number.
  - A web search found only blog rules of thumb, so they are not cited. Intel's mounting-inductance page (683073) returned a 404 redirect. ADI MT-101 and ADI's exposed-pad design note returned 502 even after analog.com was allowed.
  - **Overrides:** `max_plane_stub_mm` per net, then intent-wide, then the default. Each check's `required.source` says which one applied. A limit that is not a positive finite number (a hand-edited file) gives `unverified`, `invalid_limit`, never infinity.
  - **`fanout_to_plane` stays at 2.5 mm:**
    - Its `max_stub` is measured pad centre to via centre, and the planner already takes the nearest clear site.
    - On the three saved boards every fan-out stub is within the new limit: the longest passing connecting trace is 1.40 mm, with a median of 0.1 mm.
    - In theory, 2.5 mm centre to centre on an 0402 pad is about 1.9 mm of connecting trace, so a fan-out made at the very end of its range would fail this check. That is left visible rather than made more restrictive without evidence.
- **Exceptions are structured specs, not waivers** (user request, 2026-11-19):
  - **Form:** `plane_exceptions` is a list of `PlaneException`, a serde-tagged enum (`kind`, `deny_unknown_fields`). Every kind has a mandatory `source` and an optional `reason`. A new kind is one variant plus one arm in `evaluate_plane_exceptions`.
  - **Every kind is checked:**
    - Each covered pad gets `excepted_pass`, `excepted_fail` or `excepted_unverified` under the kind's alternative rule, with that rule's measurements in `measured.exception`. The detail also says what the plain plane rule would have given.
    - Each exception gets a `plane_exception` check.
    - Covered pads on nets that are not plane nets (`no_plane`) are reported too.
  - **`switching_hot_loop`** `{regulator, input_caps, pgnd_pins, max_path_mm?, min_vias?}`:
    - Each input cap's PGND-net pad must join a PGND pin with 0 vias within `max_path_mm`, either by the routed path (`tie_path`) or through a shared filled pour (pad-edge distance).
    - The default path is 1 nH ÷ L′ (0.3 mm outer), about 3.0 mm: a Konnect judgement reusing the core-regulator budget.
    - The PGND node needs `min_vias` (default 2, Konnect judgement) plane vias within the plane-stub limit of the PGND pins or the regulator's PGND pads (thermal pad).
    - Source: TI LMR33630 §10.1 [`ti-lmr33630-layout`]: "the most critical PCB feature is the loop formed by the input capacitor … and power ground"; caps "as close as possible to the VIN and GND terminals"; "an array of heat-sinking vias" to the thermal pad.
  - **`crystal_local_ground`** `{crystal, load_caps, vss_pin, max_path_mm?}`:
    - The load caps' and the crystal's pads on the VSS net must join `vss_pin` within the crystal tie's `max_path_mm` (6 mm since Pass 19). Vias are reported, not failed.
    - Source: ST AN2867, cited by title and flagged as not retrieved (st.com unreachable). It is opt-in, because guidance is mixed (guard ring versus plane).
  - **`split_ground`** `{analog_net, digital_net, tie: "REF.PAD|REF.PAD" | REF, converter?, max_tie_mm?, allowed_bridges?}`:
    - There must be exactly one tie between the nets. Every part with pads on both nets counts as a tie (net ties, 0 Ω, beads, solder bridges, jumpers, connectors), besides the named tie and converter. Capacitors and parts listed in `allowed_bridges` are not counted; they are listed in `other_parts_on_both_nets`.
    - A tie given as a part needs `converter`, and is refused without one.
    - A tie part must be within `max_tie_mm` (default 5 mm, Konnect judgement) of `converter`. Without `converter` the position is `unverified`.
    - `analog_net` pads are plane-checked against that net's own zones.
    - Sources: Johnson [`johnson-edn-adc-grounding`]: one connection "underneath the ADC, and nowhere else". The note also cites ADI MT-031 by title (not retrieved: analog.com returns 502) for its current preference for one solid plane with partitioned placement.
  - **`no_plane`** `{net, star_point?, max_path_mm?}`:
    - Each SMD pad must reach `star_point` by its own trace, a routed path that does not pass through another part's pad copper.
    - Without `star_point` the pads are `excepted_unverified` ("the plane rule does not apply").
  - **`vendor_reference_ground`** `{pad, via_pin, max_path_mm?}`: the pad joins the named chip ground pin on its own layer with 0 vias (the hot-loop 0-via search; default 1 nH ÷ L′, about 3.0 mm) instead of having its own plane via. Source: [`nordic-nrf52840-ops-v0-5`] §56.8, "It is not directly connected to the ground plane, but grounded via VSS_PA pin F23. This is done to create additional filtering of harmonic components." Test: `vendor_reference_ground_joins_the_named_chip_pin_on_its_own_layer` (pass; fail through vias; fail when not joined).
  - **`datasheet`** `{pad, net?, rule}` (`pad` required, so a net-wide waiver is refused): the matched pads (plane nets, or the named net) are `excepted_unverified`, and the rule is echoed back.
  - **Validation:**
    - Schema-level: a source is required; pins must be `REF.PAD` and parts `REF`; lists must not be empty; limits must be positive.
    - Against the board: parts, pins and nets that the saved board lacks are refused by `set_design_intent`, and reported as a failing `plane_exception` check by `check_design_intent`.
  - Existing intent files deserialize unchanged; a test covers this.
- **`net_graph` connectivity:** prompted by the independent Pass 16 review, three joins were added. They also affect `tie_path` and `voltage_drop`, and all their existing tests pass unchanged.
  - **Track end in a via's annulus:** a track node within the via radius plus its half width joins the via. On the Ethernet board, C49.1's track ended 0.0125 mm off a via centre, and the 0.01 mm snap had made that a false `no_via`.
  - **Overlapping track nodes:** superseded by Pass 19's grid join (every overlapping node joined at its real length). Track ends and vias still split a crossing segment within w/2 plus their own reach (end half-width, via radius).
  - **Track crossing a pad:** a track that crosses a pad without ending in it gets a node inside the pad. Tracks that end in a pad keep their centre convention, so `tie_path` lengths are unchanged.
- **Tests (first version):** `design_intent::plane_connection_tests` (19):
  - default limit and its citation;
  - a via beside the pad (0.3 mm stub and its L estimate);
  - via in pad (0 mm, with the tombstoning warning);
  - a long trace (9.4 mm, fail), per-net and intent-wide overrides;
  - no via, a via outside the plane zone, and an untouched pad (`unverified`);
  - daisy chain through C2.2, fixed by the pad's own via;
  - a via shared with another part fails, while the same part's pads may share;
  - through-hole and non-plane pads ignored, a through-hole GND pad as a way down;
  - same-layer pour with a near via, a far via, and no fill;
  - off-centre via, off-centreline stub, end-to-end gap, and a track crossing the pad;
  - an invalid limit and unknown references;
  - one pass/fail test per exception kind (hot loop: path, via count, a detour through another layer; crystal: routed and unrouted; split ground: converter pins, a 0 Ω second tie, a capacitor not counted, a net tie near and far; no plane: own trace, through another pad, no star point; datasheet: rule echoed, source and rule required);
  - the worked JSON examples parse, while unknown fields, a missing source and an unknown kind are refused;
  - `set_design_intent` round trip, refusing a part the saved board lacks and a missing source;
  - old intent files load.
  - `cargo test -p konnect-core --lib` at 78af833: 1860 passed, 0 failed, 23 ignored (1841 before).
- **Offline sanity run** (saved boards, read-only, through a temporary ignored test that was not committed; existing intents, none with exceptions; default 1.5 mm limit; all three boards are widely spaced, In1–In2 1.075 mm):

  | Board | Plane pads | First version (78af833) | After the review fixes and the merge with Pass 19 |
  |---|---|---|---|
  | `eth_board` | 140 | 42 fail (24 `shared_via`, 12 `long_stub`, 6 `daisy_chain`) | 21 fail (13 `long_stub`, 5 `daisy_chain`, 3 `shared_via`) |
  | `stm32_nrf_module` | 60 | 18 fail (17 `shared_via`, 1 `daisy_chain`) | 1 fail (`daisy_chain`, C6.1 through C5.1, two capacitors) |
  | `stm32_demo` | 49 | 1 fail (`daisy_chain`) | 1 fail (`daisy_chain`, C9.1 through C10.1, two capacitors) |

  - **The 17 module `shared_via` results were false positives:** Dijkstra had given each pad its nearest via, even when a via of its own was free. With per-pad assignment every one of those pads has its own via.
  - **Ethernet board, worst pads:**
    - U1.99 (GND) reaches the plane through crystal Y4.2, over 17.8 mm.
    - U1.28 (+3V3): 9.3 mm. U9.2 (+3V3): 7.2 mm.
    - C5.1 (+3V3) through FB1.1: 4.6 mm. R20.2 through C36.2: 4.1 mm.
    - C49.1 (+3V3): 3.8 mm. C33.2: 3.6 mm. U4.1: 3.5 mm. U1.27: 3.5 mm.
  - **Daisy chains on the module and demo boards:** `fanout_to_plane` put a capacitor's via inside another capacitor's same-net pad. That is a fan-out bug for a later pass.
  - **Via barrels:** they come out at 0.085 nH (F.Cu to In1) and 1.0 nH (F.Cu to In2, for +3V3 pads). For +3V3 pads the barrel is larger than a 1.5 mm stub (about 0.5 nH).
- **Review fixes** (independent review; policy source-checked by the coordinator):
  - **Shared vias depend on the plane regime:**
    - **Spacing:** from the stack-up, the net's plane to the nearest plane of another net. It is reported as `required.plane_spacing_mm` and `required.regime` (`closely_spaced` ≤ 0.5 mm, `widely_spaced`, or `unknown`). The regime is `unknown` when there is no other plane, or when the stack-up is assumed (neither the board nor the intent declares one).
    - **Capacitor–capacitor:** always fails (UG483 p.29 [`xilinx-ug483-7series-pcb`]).
    - **Capacitor–device, same side:**
      - closely spaced: fails ("Do not let high-frequency decoupling capacitors share a via with an active device" [`learnemc-decoupling-closely-spaced-planes`]);
      - widely spaced: passes, `shared_via_ok`, noted as optimal ("If the decoupling capacitor can be located near enough to the active device to share the same via, this is optimal" [`learnemc-decoupling-widely-spaced-planes`]).
    - **Opposite sides:** fails in both regimes.
    - **Unknown regime:** passes with a warning.
    - **Other pairs:** pass with a warning; no source covers them.
    - **Part kinds:** read from the reference: C is a capacitor; U, IC and Q are devices.
    - **Power-net daisy chain:** a supply pin reaching the plane through its own capacitor's pad, within the stub limit, is judged the same way (`shared_pad_ok`). The stub limit still applies to the capacitor's own pads ("under no circumstances should a trace be used between the decoupling capacitor mounting pads and the vias").
  - **Per-pad via assignment (HIGH):** every in-limit way down is collected per pad, and vias are assigned greedily, pads with the fewest choices first. Pads of one part may share. Sharing is judged only for a pad that has no via of its own left.
  - **`split_ground` ties:** every non-capacitor part on both nets counts as a tie (solder bridges, connectors, jumpers, 0 Ω), unless it is listed in `allowed_bridges`.
  - **`datasheet` needs `pad`:** a net-only exception would waive a whole plane net, so it is refused.
  - **Real pad shapes:**
    - Circle, oval and roundrect pads are sampled into a convex outline (`CopperPad.outline`, `fanout.rs`).
    - That outline is used for pad joins, crossing splits, stub clipping and pour contact.
    - Tie distances and placement still use the rectangle.
  - **Plane layers:**
    - A zone layer is a plane of its net when its copper covers ≥ 25 % of the board's Edge.Cuts box (Konnect judgement), or when the intent names it in the new net field `plane_layers`.
    - A `plane` net with no covering layer uses all its zone layers.
    - Only vias landing on plane layers count. A small inner pour (+5V under a regulator) no longer makes a plane net, and a via onto a B.Cu island is no way down.
  - **Hot loop:** the 0-via search runs with via barrels at infinite weight (`tie_path_with`), so a shorter via path cannot hide a valid same-layer path.
  - **Exposed pad:** a path through an IC's large pad (short side ≥ 2 mm) and its own vias passes with a warning, not as a daisy chain.
  - **Via inductance (information only):** `est_via_nh`, the barrel from the pad's layer to the nearest plane layer it lands on, from Pass 19's `barrel_nh` (Johnson [7.9], the cap value with no return via). The pass/fail limit is still on the connecting trace alone. Via inductance is not counted against it, even though the barrel down to In2 can outweigh the trace for +3V3 pads.
  - **Performance:** path reconstruction stops 5 mm past the limit. The Ethernet board takes about 0.6 s in a debug build.
- **Merge:** `tier1-review-fixes` (Passes 17 and 19) is merged into this branch. Its `net_graph` grid join is kept, with the pad-outline fix on top. `layer_distance` (Pass 19) is kept; this pass's version is renamed `plane_distance` (`None` for the same layer).
- **Tests after the fixes:** `plane_connection_tests` has 27:
  - two capacitors on one via fail, but only the one left without a via;
  - a pad with a via of its own passes even when a neighbour routes to that via;
  - a capacitor sharing a device's via under the widely spaced, closely spaced and unknown regimes, and on opposite sides;
  - a supply pin through its capacitor's pad (widely spaced passes, closely spaced fails), and an exposed pad as a way down;
  - the tombstoning warning: an overlapping annulus warns, a QFN pin does not;
  - plane coverage, an island via, and an intent `plane_layers`;
  - a round pad: a corner end and a grazing track do not join, an end inside does;
  - the hot-loop 0-via path;
  - `split_ground` with SB, J and W parts and `allowed_bridges`;
  - a net-only `datasheet` exception refused.
  - `cargo test -p konnect-core --lib` after the merge with Pass 19: 1897 passed, 0 failed, 23 ignored.
- **Limits and what is not verified:**
  - Via inductance is reported (`est_via_nh`, information only) but not counted against the limit; `est_inductance_nh` covers only the connecting trace.
  - Part kinds (capacitor or device) are guessed from the reference prefix, and the greedy via assignment is not an optimal matching.
  - The regime uses the nearest plane of another net. A board with a single plane net gives `unknown`.
  - Pour paths are measured as straight lines, not through the fill geometry.
  - Thermal-relief spokes count as connections when the fill polygon touches the pad.
  - `zone_copper` expands `*.Cu` and `F&B.Cu` but no other wildcards.
  - Nothing has been checked against a live KiCad session or against KiCad's own connectivity; the board results above have not been verified by hand, apart from C9/C10 and C49.

- **Final-review fixes:**
  - **M1:** a tie's plane leg (`plane_leg`) now reads the zone's filled copper (`zone_copper`), as `plane_connection` does. A zone with no fill in the file falls back to its outline; the leg is then a lower bound and the tie is `unverified` ("has no fill in the file").
  - **M2:** a crystal (pF budget) leg with track that has no reference plane is never a pass. It fails above the length guide, and is `unverified` below it, as the inductive branch already was.
  - **M3:** a through-hole pad used as the way down is owned like a via. Two capacitors reaching the plane through one connector pin are judged as sharing (capacitor–capacitor fails). The pad's own part may always use it.
  - **M4:** a capacitor sharing with a connector pin (J, CN, P, X) fails (UG483's one via per land). With a part Konnect cannot classify, the result is `unverified` (`shared_via`, or `shared_pad` for a chained path). Sharing between non-capacitors still passes with a warning.
  - **L1:** the Specctra partial-pour guard (`solid_plane_reaches_every_drop`) reads `filled_polygon`s. An unfilled pour, or a fill that misses a via or through-hole pad, keeps the net in the routing job.
  - **L2:** `via_shared_with` is listed on every pad using a way down, the first owner included.
  - **L4:** with no plane nets, `plane_connection` in the response says so ("no plane nets found").
  - **L5:** a `split_ground` whose tie is a part needs `converter`; it is refused on load without one.
  - **L6:** docs corrected (crystal 6 mm, split-ground tie list, pad shapes).
  - Tests: `a_crystal_leg_without_a_reference_plane_is_not_a_pass`, the unfilled-outline case in `a_leg_through_the_plane_is_measured_stub_via_plane_via_stub`, `a_through_hole_pad_is_owned_like_a_via` (covers L2 too), `a_capacitor_sharing_with_an_unclassified_part_is_unverified_and_with_a_connector_fails`, `a_split_ground_tie_part_needs_a_converter_and_an_empty_board_says_so`, and the unfilled and voided cases in `a_partial_pour_on_a_solid_plane_does_not_take_the_net_out_of_the_job`.

## Pass 19: Tier 1 review fixes (second independent review)

Code: `design_intent.rs`, `eecalc.rs`, `specctra/native.rs`, `pcb_export.rs`. Built on Pass 18's branch (its `net_graph` joins and `NetGraph::pos` are used, not replaced). New sources in the document database: `johnson-sigcon-via-inductance`, `johnson-sigcon-via-inductance-2` (Howard Johnson's two "Via Inductance" newsletters; the formulas on those pages are images and are transcribed, with the check against his table, at the top of the stored text).

Principle: a `pass` is only given for what was measured. A leg that cannot be measured, a limit that does not resolve, a pour that is not filled: `unverified`, with the reason.

### `tie_path` judges every leg, against the budget itself

- **Every leg counts (review B1).** The check used to keep the shortest path over the nets a part shares with its target, so a crystal with a 4 mm XI and a 44 mm XO passed. Now:
  - `crystal`, and ties with only `max_path_mm` / `max_vias`: every shared net is a leg and each must meet the limits. A plane net of a tie without `pin` is `not_judged` (its connection is each pad's via, Pass 18's `plane_connection`).
  - `decoupling`, `core_regulator_cap`: the loop. Every net of the part is a leg to the target: the pin's net to the pin, the other net (the return) to any pad of the target on it.
- **Budget, not a length proxy (review D1).**
  - Crystal: Σ C′·len of the leg (+ 0.4 pF per via, Johnson's measured figure for one geometry) against 0.5 pF, and the leg's length against a flat 6 mm guide. The guide is no longer the budget turned into millimetres, which on a board with a distant plane came to 12 mm (review D2).
  - Decoupling kinds: the legs' inductances add up to `loop_inductance_nh`, compared with the budget. No default path limit; `max_path_mm` and `max_vias` still apply when set (`max_vias` defaults to 1 on a leg routed as tracks).
- **Via barrels and the plane are in the estimate.**
  - Barrel: (μ/2π)·h·ln(s/r), h the barrel length the path uses, r the drill radius, s the distance to the nearest via of another leg of the same tie, at most 2e·h. With no other via this is Johnson's formula [7.9], 5.08·h·[ln(4h/d) + 1] nH (inches), which he calls a gross approximation; with one it is half of his via-pair loop (μ/2π)·h·2·ln(s/r), which his second newsletter checks against measurement.
  - Plane between two vias: (μ/2π)·h·ln(span/r) per leg, h the distance to the nearest other plane. This is the spreading inductance of Xilinx UG483 Table 2-10 (μ0·h per square).
- **A leg through the plane is measured (review D4, tie side).** Where a net has a pour on an inner layer, a leg may be: pad → tracks → via into the pour, the plane, via → tracks → pin. A via counts only inside the zone's outline. The leg used is the lower-inductance of this and the routed track path. The per-pad rule (every pad its own via within a stub limit) stays Pass 18's `plane_connection`.
- **Budgets are Konnect judgements, and say so.** Loop budgets: `decoupling` 6 nH, `core_regulator_cap` 5 nH. No cited source gives a whole-loop number (UG483's 0.3–4 nH covers the capacitor's own lands, traces and vias only). The figures are the 2 nH / 1 nH of supply track the check allowed before, plus the vias and plane of a loop on a 1.6 mm four-layer board with a 1 mm core, which come to 3–4 nH however close the parts sit. A result near the budget should be read with its per-leg breakdown.
- **Verdict.**
  - `fail`: the measured part already exceeds a limit, and no unmeasured copper could shorten it.
  - `unverified`: a leg has no measurable path; a leg runs where no reference plane gives a line model (its inductance is not modelled; over the 6 / 3 mm guide it fails); a failing leg is on a net with a zone the check did not measure (an outer-layer pour, or a through-hole pad sitting in the plane).
  - `pass`: every leg measured and modelled, within every limit.
- **Output.** `measured.legs[]`: `net`, `status`, `detail`, `carried_by` (`tracks` / `plane`), `from_pad`, `to_pad`, `path_mm`, `vias`, `via_barrel_mm`, `layers`, `plane_span_mm`, `track_inductance_nh`, `via_inductance_nh`, `plane_inductance_nh`, `est_inductance_nh`, `est_capacitance_pf`, `unmodelled_mm`. The top-level fields describe the governing leg (the failing one, else the longest). `loop_inductance_nh` for the decoupling kinds. `required`: `budget_pf` / `budget_nh`, `max_path_mm`, `max_vias`, `unmodelled_path_mm`, `sources`.

### Limits and defaults

- **No limit is never a pass (review B2).**
  - `load_intent` validates the file as `set_design_intent` does, so `get_`, `apply_`, `check_design_intent` and `plan_tie_placement` refuse a hand-edited file with a tie that has no limit, an unknown `kind` or `standard`, or a non-positive number. Before Pass 16 serde refused a tie without `max_mm`; Pass 16 made the field optional and the file then resolved to an infinite limit that always passed.
  - `TieLimits.max_mm` is `Option<f64>`: with nothing to resolve the `tie` check is `unverified` and the placement plan skips the tie.
- **Placement defaults per kind (review D3):** `crystal` 6 mm (its length guide), `decoupling` and `core_regulator_cap` 2.5 mm pad edge to pad edge. Konnect judgement, the same figure `place_decoupling` uses for a 100 nF capacitor; no cited source gives a number. A bulk capacitor needs an explicit `max_mm`. The default used to be the path limit, about 5.8 mm for `decoupling`.
- **Pair length (review B5):** `max_length_mm` set on one net of a pair applies to its partner too.
- **Full-speed USB (review D6):** `usb2_fs` no longer takes TI's 50 mil skew, a high-speed layout number; its skew comes from the edge-rate rule (10 % of 4 ns, tens of millimetres). `usb2_hs` is unchanged.
- **IPC-2152 (review D6):** "0.5 oz inner copper: the half-ounce internal fit applies" moved from `warnings` to `assumptions`.

### `net_graph` connectivity (review B3)

Pass 18 had already made the graph join copper that overlaps without sharing coordinates (a track end in a via's annulus, ends a few hundredths apart, a stub off a trunk's centreline, a track crossing a pad). Reviewed here and extended in one place:

- **Found:** the rule that joined a dangling track end to its *nearest* overlapping node failed when the end's last piece was shorter than the gap to the other track: the nearest node was then the end's own neighbour, and the two tracks stayed apart. A track corner (not an end) touching another track was not joined at all.
- **Change:** every track node is joined to every track node of its layer whose copper overlaps it ((w1 + w2) / 2 apart or less) and that it has no edge to, by a `Track` edge as long as their distance (a grid keeps this near-linear). Because the join has its real length, joining the close nodes of one finely segmented track cannot shorten a path below the straight line through copper that is there. Via-annulus and pad joins stay Pass 18's zero-length `Pad` edges.
- Pads: in Pass 19 they were still rectangles; Pass 18's review fixes (merged after it) join copper by the real circle, oval or roundrect outline.
- The overlap rule is copper overlap, a little wider than KiCad's "end inside the other item".

### Specctra export

- **Partial pours (review B4).** A fully fanned plane net now leaves the routing job only when its pour on a solid plane layer lies under every via and through-hole pad of the net (`solid_plane_reaches_every_drop`, by the zone outline). A net with a small pour on a `power` layer and its main pour on a `mixed` layer used to leave the job and get its main pour cut. Response: `kept_for_partial_planes` and a warning.
- **Renamed layers (review B6).** The review suspected a board with renamed inner layers was routed with its planes silently ignored. It is not: `lower` already refused it ("names layer 'X', which is not a copper layer of the board"). The message now says what to do (restore the standard layer names) and lists the board's copper layers; a test pins the refusal.
- **Kept plane nets (review D5).** The export warning now says what to check after the import. Not done: routing kept plane nets at a power width (the net class decides; give the plane net `current_a` and apply the intent).

### `plane` check (review D5)

- Counts the pour's filled pieces per layer from the board file (`filled_polygon`): `measured.filled_pieces`, with `track_length_mm` and `narrowest_track_mm` of the net's own tracks.
- `unverified` when the zone is not filled in the file (it joins nothing yet), and when an inner layer holds more than one piece (the pieces are joined only through vias or tracks; whether they are joined is DRC's unconnected-items check, and the current through those links is not computed). `pass` only for a zone filled in one piece on every inner layer it is on. Outer-layer pieces are reported, not judged.
- A plane net with `current_a` now gets the `track_width` check on its tracks (it used to skip every other check).
- Needs a refilled, saved board: nothing here refills zones.

### Tests

- `design_intent::second_review_tests` (15): pinless crystal with unequal legs; an invalid intent file through `load_intent` and `get_design_intent`; a tie with no limit; the six connectivity cases with their lengths and two that must not join; `net_graph` reaching a pad a track only crosses; segment clipping; Johnson's published via values and formula [7.9], and UG483's pH/square; a crystal judged on pF (over budget inside the guide, under budget, over the guide); the decoupling loop (two track legs, pinless, return leg missing, proven failure, a wide 7 mm pair that passes); via barrels in the loop; a leg through the plane with every term checked against its formula, a missing via, a via outside the pour; a loop with no reference plane; pair length; full-speed USB skew; filled pieces of a plane.
- `specctra::native`: `a_partial_pour_on_a_solid_plane_does_not_take_the_net_out_of_the_job` (unit cases and the real fixture with the +3V3 outline shrunk), `a_renamed_layer_is_refused_with_the_reason`.
- Changed: `tie_kinds_give_budgets_and_placement_defaults_and_user_values_win` (was `…derive_path_limits…`), `a_tie_without_a_pin_must_meet_the_limit_on_every_shared_net` (was `…takes_the_shortest…`, which pinned the B1 behaviour), the plane assertion in `checks_measure_ties_lengths_widths_and_drop` (an unfilled zone is `unverified`).
- `cargo test -p konnect-core --lib`: 1889 passed, 0 failed, 23 ignored.

### Offline check on the three saved boards (read-only, no KiCad)

- **Intent files as saved** (no tie `kind`): every Pass 16/17 check kind gives the same counts as before. Ethernet 85 pass / 4 fail / 6 unverified, STM32 demo 27 / 0 / 0, module 28 / 0 / 2. Both plane nets on all three boards are filled in one piece.
- **With kinds set in memory** (capacitors `decoupling` or `core_regulator_cap`, crystals `crystal`, no `max_mm`), `tie_path` before → after:

| Board | Before (pass / fail / unverified) | After |
|---|---|---|
| Ethernet (35 ties) | 23 / 4 / 8 | 26 / 9 / 0 |
| STM32 demo (11) | 2 / 0 / 9 | 11 / 0 / 0 |
| Module (14) | 12 / 1 / 1 | 13 / 1 / 0 |

  - The unverified results were capacitors on plane nets; all are now measured through the plane.
  - Ethernet failures: C5 (11.5 nH), C25 (7.2), C12 (6.7), C21 (6.5), C20 (6.3), C14 (6.2), C17 (6.1), C10 (6.01), and C16 VCAP2 (5.1 against 5). Four of them failed before on length; C5, C10 and C25 were unverified; C16 and C17 passed. C10, C16 and C17 are within 3 % of a budget that is a judgement.
  - Loops of capacitors that pass measure 2.3–5.6 nH.
  - `tie` placement with the 2.5 mm default: Ethernet 37 pass / 4 fail (was 40 / 1 against the path limit).
- Time: Ethernet 270 checks in 0.26 s (debug build), with one graph per net shared by all ties.

### Not done, or not verified

- Nothing was run against KiCad: no live export, no refill, no DRC comparison of the connectivity joins.
- The inductance model is an estimate from closed-form terms. It was checked against the published numbers it comes from, not against a field solver or a measurement of these boards.
- A leg whose pad is a through-hole pad sitting in the plane, or that is carried by an outer-layer pour, is not measured (`unverified`).
- The plane leg reads zones by outline: clearances, thermal reliefs and cuts inside the outline are not seen.
- Pads in the graph: superseded by Pass 18's real pad outlines (merged after this pass).
- Kept plane nets are not routed at a power width, and pour pieces are counted, not traced for which pads each piece serves.


## Pass 21: Tier 1 third-review fixes

Code: `design_intent.rs`, `specctra/native.rs` (`solid_plane_reaches_every_drop` only), `freerouting_mcp.rs`, `tools/integration.rs` (two local additions to the `route_specctra_dsn` response), `konnect-sexp/src/board.rs` (`NetTable`). An independent read-only review of Passes 14–19 (`e7a8027..5813f87`) listed 12 findings. Each was checked against the code before it was fixed; all 12 held. Every fix has a test that fails without it.

### High

- **#1 Same-layer joins check the layer.** `local_join`'s pour fallback compared 2-D outlines only. A `switching_hot_loop` input cap on B.Cu under a regulator whose PGND pin is on F.Cu, with a filled GND pour on B.Cu, came out `excepted_pass` with `max_vias = 0`; `vendor_reference_ground` and `crystal_local_ground` had the same hole. Now the pour joins only targets with copper on the pad's layer (`t.layer == from.layer || !t.smd`), and a 0-via rule with the pad on the other side from every target fails at once ("no same-layer join without a via").
- **#2 The crystal budget is the whole node.** AVR186's "XTALIN to ground: 1pF" is the pin's parasitic capacitance [docdb microchip-avr186-oscillator-layout], so the 0.5 pF trace share is compared with Σ C′·len over every track of the net plus 0.4 pF per via of the net, not with the Dijkstra path alone. A 3 mm path with a 15 mm stub left beside it passed at about 0.3 pF; it now fails at about 1.5 pF. The 6 mm path guide stays. Leg output adds `net_capacitance_pf`, `net_track_mm`, `net_vias`, `net_unmodelled_mm`; `est_capacitance_pf` stays the path's share.

### Medium

- **#3 Pads join overlapping copper.** A pad joined a graph node only within 0.02 mm of its outline, and via annuli were joined only to track nodes. A via butted against the land with no trace, which UG483 p.30 asks for ("the via should butt up against the land" [docdb xilinx-ug483-7series-pcb]), came out `unverified` "not connected", or a false `long_stub` / `daisy_chain`. Now a pad joins every node on its layer within the node's reach of its outline: a via's radius at its centre, or half the widest track through the node.
- **#4 A way down must land on the fill, on its main piece.** `via_lands_on` read an unfilled zone by its outline, so a via into a plane with no fill in the file passed. It now returns how each layer is reached: `Main` (the largest filled piece), `Island` (only a smaller filled piece) or `Unfilled`. A pad's chosen way down (its via, a through-hole pad, a chained pad's via, or the pour's nearest via) that lands only on an island or an unfilled zone turns a `pass` into `unverified` with the reason (Pass 19's principle). The `switching_hot_loop` PGND vias count only on a main fill; with enough vias only counting the others, the result is `unverified`. Test fixtures that relied on unfilled planes now carry fills.
- **#5 Ways down are matched.** Each pad got a via by a greedy pass (fewest choices first). Now a bipartite matching by augmenting paths (Kuhn), with the existing ownership rules (pads of one part share freely; a part's own through-hole pad is always free), runs before any pad falls back to sharing. The review's counter-example A [V1, V2], B [V2, V3], C [V1, V2] now gives each its own via.
- **#6 Outer pours are judged when they are the plane.** The `plane` check judges outer-layer pieces when the net has no inner pour (a two-layer board): a B.Cu pour in 5 pieces was `pass`, it is now `unverified` with the pieces listed; `measured.judged_layers` is new. In `plane_connection`, a pad whose pour is its net's only plane layer must touch the main piece, or a piece a via or through-hole pad of the net reaches; a pad on an isolated island fails with the new connection `pour_island`. Another part's pad on the same island does not count: it joins nothing more.
- **#7 Plane legs try every plane layer.** `plane_leg` searched the far stub only on the near stub's plane layer. It now tries each plane layer the drops reach, both stubs on that layer, and keeps the lowest-inductance leg. An inner pour that a via of the net drops into, where `plane_leg` found no way through it from both ends, now counts as an unmeasured zone (`unverified`, not `fail`); an inner pour no via or through-hole pad of the net reaches carries nothing and does not. To keep that from hiding proven failures, a failure is now proven per net: a limit exceeded on a net whose zone was not measured is `unverified`, but the loop total still fails when the legs of the other nets alone exceed it, and a node over its pF budget fails whatever a zone does (more copper only adds capacitance).
- **#8 A plane split in pieces keeps its net in the routing job.** `solid_plane_reaches_every_drop` only asked that each drop touch some fill piece. Now pieces are joined when they touch on one layer or when one drop reaches both (a via only on the layers it spans; a through-hole pad on every layer), and every drop must reach the same joined piece. Otherwise the net stays in the job (`kept_for_partial_planes`), because nothing would route between the halves of a net dropped from the network [docdb freerouting-2-3-0-network-java].

### Low

- **#9 KiCad ≤ 9 nets.** `zone_copper`, `solid_plane_reaches_every_drop`, the arcs in `board_copper` and `pour_pieces` resolve `(net 3)` through the board's net table and `(net 3) (net_name "GND")` on zones: new `konnect_sexp::board::NetTable` (`net_table(tree).item_net(node)`).
- **#10 Freerouting's estimated score.** When the log has no stage-completed or optimizer line, `final_score` falls back to the last pass and `RoutingSummary.final_score_estimated` is now `true`. `route_specctra_dsn` reports it as `routing.final_score_estimated` and adds a warning sentence.
- **#11 Summary.** `plane_connection_summary.worst_failing` includes `excepted_fail` pads; each entry has a `status`.
- **#12 Tie distance by the true pad shape.** The `tie` check and `plan_tie_placement` measured pad edge to pad edge with the bounding rectangles (`corners`); they now use the real outline (two 1 mm round pads on a diagonal: 0.28 mm by rectangles, 0.70 mm by copper).

### Tests

- `design_intent::third_review_tests` (11), `specctra::native::…::a_split_plane_or_a_via_that_stops_short_keeps_the_net_in_the_job`, `freerouting_mcp::routing_log_tests::a_score_from_the_last_pass_is_marked_as_an_estimate`, `konnect_sexp::board::…::the_net_table_resolves_arcs_and_zones_in_every_format`. The 12 design-intent and Specctra tests were run against the unfixed `5813f87` code and each fails there for the reason it names; the Freerouting and `NetTable` tests exercise a new field and a new API.
- Changed: the `plane_connection_tests` fixtures (`checks_with`, `all_checks`, `regime_checks`) carry `filled_polygon`s, since an unfilled plane is no longer a pass (#4); `round_pads_join_by_their_real_shape` moves its corner track 0.03 mm out, because a 0.3 mm track 0.14 mm off the circle overlaps it (#3); `overlapping_copper_is_joined_where_coordinates_do_not_coincide` (a) measures 4.0 mm, its 0.3 mm jog lying on the pad's copper (#3); `a_leg_through_the_plane_is_measured_stub_via_plane_via_stub` moves its vias 0.8 mm off the pads, since at 0.5 mm their annuli overlapped the pads (#3); `a_crystal_leg_is_judged_on_its_capacitance` (message), `a_detour_fails_on_length_and_the_shortest_branch_counts` (the detour beside the tee now fails on the node's capacitance, #2).
- `cargo test --workspace --no-fail-fast`: `konnect-core` lib 1914 passed, 0 failed, 23 ignored (1901 before, + 13); `konnect-sexp` lib 216 passed (+ 1); every other target passes except `conformance_test::pin_anchored_labels_match_eeschema_orientation` (needs kicad-demos) and `konnect/tests/doc_tool_counts.rs` (4 tests: DEV.md and tool-directory.md say 244 tools, the registry has 247; the same at `5813f87`, untouched here). The design-intent tests ran three times in a row to rule out hash-order flakiness: ways down with equal stubs are now ordered by target as well as distance, so the matching gives the same answer every run.

### Follow-up: two false fails from #2 and #6 (review of this pass)

- **A. A crystal's ground is not an oscillator node.** With #2, a pinless `crystal` tie on a 4-pin can made every shared net a leg, and on a board where GND has no inner pour and no `plane` role (two layers, or GND poured only outside) the whole GND net went into the pF budget: XI 4 mm and GND 4 mm plus 25 mm of unrelated GND track failed with "GND: 2.964 pF > 0.5 pF on the whole node". AVR186's figures are for XTALIN and XTALOUT [docdb microchip-avr186-oscillator-layout]. A net of a pinless `crystal` tie is now `not_judged`, with the reason, when it has a zone, its role is `plane` or `power`, or its name (after the sheet path) contains `GND`, starts with `VSS` or is `0V`. The budget and the 6 mm guide apply to the remaining nets, the oscillator nodes.
- **B. A pour piece joined by a track is on the plane.** With #6, a pad on a small piece of its net's only plane layer failed `pour_island` unless the piece reached the main piece, a via or a through-hole pad directly; a track of the net from the piece to the main pour was ignored, though KiCad connects it. Now the net graph is followed (breadth-first) from the pad and from every track node on the piece; reaching a node on the main piece, a via, or a through-hole pad joins the piece. A track that ends nowhere still leaves it `pour_island`.
- Tests: `third_review_tests::a_crystal_ground_net_is_not_judged_as_an_oscillator_node`, `…::a_pour_island_joined_by_a_track_to_the_main_pour_is_on_the_plane`; both fail on `29f5940` (run against it) for the reported reason. `cargo test -p konnect-core --lib`: 1916 passed, 0 failed, 23 ignored.
- Not changed (noted by the reviewer): another part can take a part's own through-hole pad as its way down (C1 takes J1's through-hole GND pad), and the capacitor-and-connector sharing rule is then not applied to that pair. Fixing it changes who owns a through-hole pad and how sharing with a pad's own part is recorded, which is not a local change.

### Second follow-up: a false pass in B, a gap in A (review of the follow-up)

- **B. The island walk must end on the main piece.** The first follow-up stopped at the first via or through-hole pad of the net, and the older check from #6 counted any via or through-hole pad sitting on the island. On a net whose only plane layer is the pad's own, neither is a way to the plane: a track from an island to a via joined to nothing passed as `pour` (the reviewer's `zz_island_via_nowhere`). Now one breadth-first walk over the net graph starts from the pad and from every track node, via and through-hole pad on the piece, goes on through vias and tracks on any layer, and succeeds only at copper on the pad's layer that touches the main piece: a track node within its half width, a via's annulus, or a through-hole pad's outline. A via or through-hole pad on the island counts only when the walk goes on from it to the main piece. The per-node reach and pad index are computed once per net, not per pad. The #6 test that took a lone via on the island as joined now expects `pour_island`, and passes once that via is routed into the main piece.
- **A. Only a real ground is exempt.** "Contains GND" exempted oscillator nets such as `XTAL_GND_SENSE`. A ground name is now `GND`, a name ending in `GND` (AGND, DGND), starting with `GND`, starting with `VSS`, or `0V`. By name, a net is exempt only when it also reaches pads of a part other than the crystal, its target, and the load capacitors (capacitors with a pad on another net of the crystal). A ground that only closes the oscillator circuit is judged like the nodes. A zone or a `plane` / `power` role still exempts on its own. A pinless tie whose every shared net is exempt is `unverified` with each net's reason ("no net the part shares with the target is judged (GND: …)"), not the "plane net" wording.
- Tests: `third_review_tests::an_island_is_joined_only_by_copper_that_reaches_the_main_piece` (via nowhere, lone via, through-hole pad on the island only, a via routed on F.Cu down into the main piece, a via whose annulus touches the main piece) and `…::only_a_real_ground_net_is_exempt_from_the_crystal_budget` (`XTAL_GND_SENSE` judged and failing, `AGND` closing only the oscillator judged, `AGND` going on to another part exempt, the all-exempt reason). Both were run against `bfcefc4` and fail there for the reported reason. `cargo test -p konnect-core --lib`: 1918 passed, 0 failed, 23 ignored (one run had `schematic_placement_tests::native_placement_readback_requires_requested_values` fail once under load; it passed alone three times and in the full rerun).

### Not done, or not verified

- Nothing was run against KiCad: no refill, no DRC comparison of the new pad joins or of the island rule.
- The main piece of a plane layer is its largest filled polygon. A plane cut into two large halves joined through another layer gets `unverified` on the smaller half, not a pass.
- The pad-to-node join uses the node's reach; a track that passes beside a pad without a node near it (a long segment grazing the pad) is still not joined.
