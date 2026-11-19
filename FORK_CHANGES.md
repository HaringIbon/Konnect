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
- **Which footprints the position file omits:** KiCad 10.0.8 `PLACE_FILE_EXPORTER::GenPositionData` (doc DB `kicad-10-0-8-place-file-exporter`) skips `exclude_from_pos_files`, and with Konnect's `--exclude-dnp` also `dnp`. Konnect already drops `exclude_from_bom` rows from the JLCPCB CPL; it now drops `board_only` rows too (no symbol, so no BOM line to place from).
- **Change** (`tools/manufacturing.rs`):
  - `not_placed_footprints` lists every footprint carrying one of those four attributes, with the attributes as reasons.
  - `drop_unplaced_bom_designators` takes those designators out of the JLCPCB BOM (whole rows, or one designator of a grouped row, with a Qty column kept in step), so they need no LCSC number and the BOM and CPL match. The file is rewritten through the verified-publish path.
  - A row that carries an LCSC number is left alone: a sourced part whose footprint is excluded from placement is a real inconsistency. The BOM/CPL mismatch warning now names the attribute that hides it.
- **Response:** `not_placed: [{reference, reasons, removed_from_bom}]`.
- **Tests:** `cpl_bom_exclusion_tests` (6): attribute reading, the J3 case end to end (BOM rewrite, LCSC check, designator match, report), grouped rows and quantity, sourced rows still failing with the reason, untouched BOM without exclusions.
- **Live** (copy of the Ethernet board, pcbnew open): without `allow_unsourced`, `complete: true`, no warnings. `not_placed` lists FID1–3 (`exclude_from_bom`), H1–4 and J3 (`exclude_from_pos_files`, `exclude_from_bom`); only J3 was in the schematic BOM and was removed from it. The BOM is otherwise identical to the board's last package apart from parts removed from the design since.

### Bottom-side `add_board_text` is mirrored

- **Problem:** text on B.SilkS, B.Cu, B.Fab and the other back layers was written unmirrored, so it read backwards from the bottom.
- **KiCad's behaviour:** its text tool sets `m_Mirrored = IsBackLayer(layer)` (doc DB `kicad-10-0-8-drawing-tool`), and the file form is `(justify mirror)` inside `(effects …)` (`kicad-10-0-8-eda-text`).
- **Change** (`tools/pcb_board.rs`): `board_text_mirrored` mirrors text on any `B.` layer by default; the IPC path sets `TextAttributes.mirrored`, and the file fallback writes `(justify mirror)`. A new optional `mirror` argument overrides the default. The response reports `mirrored`.
- **Tests:** `bottom_side_board_text_is_mirrored_in_the_file` (8 layer/override cases, parsed back), `bottom_side_board_text_is_mirrored_over_ipc` (the BoardText sent in `CreateItems`, and the file left untouched).
- **Live** (copy of the Ethernet board): over IPC, B.SilkS and B.Fab text saved by pcbnew as `(justify mirror)`, F.SilkS text without; through the file fallback (KiCad closed), B.SilkS and B.Cu text. `pcbnew.LoadBoard` reads `IsMirrored()` true for all four back-layer texts and false for both front ones, and `kicad-cli pcb export svg` loads the board.
