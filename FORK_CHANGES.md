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
- `cargo test --workspace --no-fail-fast`: `konnect-core` lib 1914 passed, 0 failed, 23 ignored (1901 before, + 13); `konnect-sexp` lib 216 passed (+ 1); every other target passes except `conformance_test::pin_anchored_labels_match_eeschema_orientation` (needs kicad-demos) and `konnect/tests/doc_tool_counts.rs` (4 tests: stale tool counts in DEV.md and tool-directory.md, the same at `5813f87`, untouched here; fixed in Pass 22). The design-intent tests ran three times in a row to rule out hash-order flakiness: ways down with equal stubs are now ordered by target as well as distance, so the matching gives the same answer every run.

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

## Pass 22: grouped autoroute, per-net import, rip-up (Tier 2, item 1)

**Problem (WORKFLOW §3.1, §4.2):** routing was most of the time spent, and every failure cost a full cycle.
- The SES import was all or nothing: one length or width violation (USB_C 19.6 mm against 15 mm; VBUS at 0.112 mm against 0.15 mm) rolled back an otherwise good autoroute.
- There was no "route these nets again, keep the rest": a retry meant restoring a checkpoint, which needs KiCad closed (about 2 minutes).
- Freerouting ends every job `COMPLETED` with nets still open (Pass 15), so the router's word cannot decide what was routed.

**Sources (doc DB):**
- `freerouting-2-3-0-wiring-java` (`calc_fixed`): DSN `(type fix)` is `SYSTEM_FIXED`; `protect` is `USER_FIXED`. A wire whose net is not in the network is inserted with no net.
- `freerouting-2-3-0-seswriter-java` (`writeNet`): the session skips `SYSTEM_FIXED` items, so fixed copper never comes back in the SES. `USER_FIXED` items come back marked `protect`.
- `freerouting-2-3-0-network-java` (`insert_component`): a pin whose net is not in the network becomes a pin with no net, with the default class clearance; every net keeps clear of it.
- `freerouting-2-3-0-routersettings-java`: `ignore_net_classes` is `transient`, so it cannot be set through the API; no DSN form sets a class's "ignored by the autorouter" flag. Leaving nets out of the DSN is the router-independent way to exclude them.
- `freerouting-2-3-0-batchautorouter-java`: the batch router routes every incomplete connection of every net in the network.
- `kicad-10-0-8-specctra-export`: KiCad writes locked tracks and vias `(type fix)` ("not returned in .ses files") and unlocked ones `(type route)`. An arc is written as the straight chord between its ends.
- `kicad-10-0-8-specctra-import`: KiCad's own SES import deletes every unlocked track and re-creates the whole session, so it is all or nothing too.

**`export_specctra_dsn` with `nets` / `net_classes`: a grouped export** (`specctra/native.rs`, `restrict_to_group`, `fix_wiring`):
- Only the group's nets stay in the `(network …)`.
  - Every other net is removed with the Pass 4 lowering: its class membership goes, and its copper becomes keep-outs of the copper's own size.
  - Its pads stay in the placement as obstacles with no net.
- All existing copper is held fixed, locked or not.
  - The group's own tracks and vias are rewritten `(type fix)`. The router connects to them, never moves them, and does not echo them in the session, so the import cannot duplicate a track.
  - The whole-board "unlock means refuse" rule does not apply. Arcs are still refused, because KiCad exports them as chords.
- Net classes are matched by KiCad name:
  - `Power` matches the DSN class `Power,Default`.
  - `Default` matches only `kicad_default`, the nets with no other class.
- The response gives `group.nets` and `group.not_in_job`, each with its reason: a single pad, left to its plane, or no pad. Unknown nets or classes are refused.
- A grouped export always uses KiCad's exporter.
- The manifest records `routing_group` and `existing_routing_fixed`. The import then counts all existing tracks and vias, not only the locked ones.

**`apply_specctra_ses` `accept: "per_net"`** (`tools/route_verdict.rs`, pure, plus the IPC flow in `pcb_routing.rs`):
1. Import everything in one commit. DRC the board (project rules, zones refilled) against the baseline DRC.
2. Blame each new error, found by identity, on the nets of the created items it names. If it names no created item, blame the importing nets its items are on. Failing that, blame the nets its description names as whole names (a pair's skew rule names no track).
3. Count KiCad's unconnected items per net, by item KIID with `[NET]` in the description as fallback.
4. Judge each net. A net is rejected for DRC errors, for more unconnected items than before, for "joins nothing new" (still `n > 0`), or, with `require_complete`, for being incomplete.
5. If any net is rejected: delete exactly the created KIIDs, prove the board serialises as before, and re-import the survivors in one commit (`Import Freerouting SES (k of n nets)`). At most 4 rounds.
6. Roll everything back if a new error cannot be blamed on any net, or if a net outside the import loses connections after the refill. Both would mean a plane cut that no net owns.
- **Response, both modes:**
  - `nets[]`: status `imported`, `rejected` or `no_routes`, with the reason, tracks, vias, session and board length, unconnected before and after, `complete`, and up to 3 `new_drc_errors`.
  - `unconnected_by_net` for the whole board, and a `summary`.
  - `all` mode keeps its behaviour. Its rollback message now gives the errors by net.
- `nets` imports a subset of a session. `plan_specctra_ses_import` gives the same per-net summary.
- An empty session (the router routed nothing) is now a result, not a parse error.
- **Fix:** `<candidate>.drc.json` was always written empty, because `run_drc` deletes KiCad's JSON after parsing it. It now holds the parsed report.

**New tool `rip_up_nets`** (`pcb_board`, handler in `pcb_routing.rs`):
- Deletes the tracks, arcs and vias of `nets` and/or of every net on the pads of `footprints` ("move a part and re-route its nets").
- `region` limits it: an item goes whole if any end point or its centre is inside.
- Locked copper is kept unless `include_locked`, and listed in `locked_kept`.
- Dry run by default. One undo step. Items are verified absent afterwards.

**Docs:**
- `tool-directory.md` now lists `delete_vias`, `set_footprint_text` and `add_keepout_area`, which earlier passes registered but never documented.
- The tool counts were stale since Pass 14. The `doc_tool_counts` guards were failing on `safe-writes`; they now pass.
- 248 registered tools, 255 with the meta-tools.

**Tests:** 21 new; `konnect-core --lib` 1922 passed (was 1901). Whole workspace: 2624 passed, 1 failed (the environmental `conformance_test` pin-label test).
- `route_verdict` (7): item→net map and copper per net; KIID and description fallback; unconnected counts (absent is not zero); error blame including two imported nets, refills and rule-level errors by name (`/PA1` is not in `/PA10`); the net judge; other nets made worse.
- `specctra::native::group_tests` (7), on the real 4-layer fixture:
  - only the group is in the job and the manifest;
  - class selection (`Power,Default`, `Default`);
  - unknown nets refused, single-pad nets reported;
  - unlocked copper: fixed for the group, keep-outs for others, refused on a whole-board export;
  - a grouped session imports over unlocked copper, and a session with a stranger net is refused;
  - `retain_nets`;
  - `fix_wiring`.
- `rip_up_tests` (5): selection by net, region and lock; IPC records; argument refusals.
- One test each for `routing_group_argument` and the empty session.

**Live validation** (KiCad 10.0.8 headless, Freerouting 2.3.0, on copies in the session scratchpad; the originals were not touched):

*`stm32_nrf_module` (the benchmark):*

| Step | Time | Unconnected after |
|---|---|---|
| Saved board as built | | 5 |
| `rip_up_nets` on every net (327 unlocked items; 140 locked hand-route and fan-out items kept) | 0.2 s | 38 |
| Whole-board export, 40 passes, `per_net` | route 155 s, apply 5.6 s | 8 |
| Group retry, 4 open nets | export 0.4 s, route 21 s, apply 3.4 s | 8 |
| Region rip-up (190 items, 14 nets) and re-route of a 17-net group | route 69 s, apply 4.0 s | 8 |
| Group retry, 3 nets, 60 passes | route 12 s, apply 1.7 s | 8 |
| `accept: "all"` on VBUS | route 9 s, apply 3.4 s | rolled back |
| Move D3 0.5 mm, `rip_up_nets(footprints: [D3])`, re-route its nets | rip-up 0.1 s, route 9 s, apply 4.2 s | |

- **Whole-board run:** Freerouting reported 45 → 7 unrouted with 52 router-side violations.
  - The per-net import kept 22 nets (21 complete) in 2 rounds and rejected VBUS: 3 `track_width` errors, intent 0.15 mm against Freerouting's 0.112 mm.
  - The old all-or-nothing import would have rolled back all 254 tracks.
  - Still open: VBUS 3, GND 2 (the J2/J7 pad-5 fan-out gap seen in Pass 17), and /BOOT_SW, /NRF_TX and /SWDIO 1 each.
- **Group retry (4 open nets):** VBUS was rejected again (`track_width`). The router routed nothing for the 3 signals, and they are reported `no_routes`.
- **Region rip-up and 17-net group:**
  - 14 nets imported, all complete; /BOOT_SW was fixed.
  - /LDO_EN, /NRF_TX and /SWDIO got no routes, so the total stayed at 8.
  - 83 partial wires of the group were held fixed (`(type fix)`).
- **3-net retry at 60 passes:** an empty session, reported `no_routes` per net. This is the placement-limited case in WORKFLOW §4.2.
- **`accept: "all"` on VBUS:** the rollback message names "VBUS: 6", and the board is restored exactly.
- **Move-and-re-route:** D3's net /STATUS_K was imported complete. GND was reported `not_in_job` (left to its plane).

*`stm32_demo`:*

| Step | Time | Unconnected after |
|---|---|---|
| Saved board | | 0 |
| `rip_up_nets` on every net (586 unlocked items) | 0.2 s | 63 |
| Group: net classes `intent_*` (power and USB) | route 18 s | rolled back |
| Group: `net_classes: ["Default"]`, 46 nets, 50 passes | route 108 s, apply 3.6 s | 9 |
| Power group again (after the description-blame fix) | route 26 s, apply 3.9 s | 1 |
| `rip_up_nets(VBUS)` and VBUS alone | | 3 |

- **First power group:** it was rolled back as "involve no imported net". A pair-skew error names its nets only in the rule text. That led to the description fallback in the error blame, which is unit-tested.
- **Default group:** all 45 routed nets imported complete. /SWDIO was already connected by its fixed hand route (`no_routes`). The router said 1 unrouted; the board said 9, all of them in the power classes, which were not in this group.
- **Power group again:** +5V and /VBUS_F were imported complete, and VBUS was partial (3 → 1, kept).
- **VBUS alone, after ripping up its partial copper:** the router routed nothing, so VBUS went back to 3 open. Partial progress is worth keeping, and VBUS is a trunk to hand-route (§4.2).

**Limits and things not done:**
- Freerouting sees the other nets' pads with the default class clearance and does not see `.kicad_dru` rules (intent widths, lengths, skew). The DRC import catches violations per net, but the router does not avoid them. VBUS failed on width for this reason; give such nets a net class with the width, or hand-route them.
- Plane cuts cannot be blamed on a net. If a refill leaves another net with more unconnected items, the whole import is rolled back.
- Arcs on the board still refuse an export.
- Rejected nets' partial copper is not kept. Accepted partial nets are kept unless `require_complete`.
- A net that was already complete and receives tiny stubs (Freerouting's 0.001 mm pad snaps on the locked USB pair) is accepted, because it adds no error.
- The undo history holds one entry per round. The last one ("Import Freerouting SES (k of n nets)") removes the import.
- No orchestrating tool: export → route → apply is still three calls (`route_specctra_dsn` sits in another toolset).
- There is no `undo` tool, so a rip-up that made things worse (VBUS on the demo) needs the earlier copper re-imported or a checkpoint.

### Review fixes (independent Opus review of e118255)

Rebased by merging `safe-writes` 78bb158 (Pass 21); this pass was renumbered from 20 to 22. Some behaviour described above changed, as follows.

| # | Finding | Fix | Test (fails on e118255) |
|---|---|---|---|
| 1 | `rip_up_nets(footprints)` took every net on the part's pads board-wide, GND and +3V3 included, against the plane-pad-via rule | A walk from the part's pads over its own copper (`walk_from_footprint`). It stops at other parts' pads. On a plane net (a zone, or intent role `plane`) it also stops at a via and where the copper enters the net's fill. Named `nets` still go whole. `footprint_walk` reports each net's walk and its stops | `a_footprint_takes_only_the_copper_connected_to_its_pads` |
| 2 | Candidate and `.drc.json` were written with bare `?` after the import was accepted | `write_import_outputs`: both are created atomically and last. A failure removes the candidate and rolls the import back | `a_failed_report_write_leaves_no_candidate` |
| 3 | Half of a pair could be kept | Units kept or dropped whole, from the intent `pair`s, the `.kicad_dru` rules with `skew` / `diff_pair_gap` / `diff_pair_uncoupled` constraints, and `_P`/`_N`, `+`/`-`, `_DP`/`_DN` names. A new `groups` argument adds more. `nets[].decided_by` is `group` for a dropped partner | `a_pair_is_kept_or_dropped_as_a_unit`, `groups_come_from_names_rules_and_intent` |
| 4 | One unblamable error or one other-net regression rolled every net back | Delta-debugging bisection (below). Budget: 2·units + 8 trials, capped at 64 | `bisection_isolates_…`, `a_fault_that_needs_two_nets_…`, `running_out_of_bisection_trials_or_rounds_fails_the_import` |
| 5 | Undo history: per-net rounds left import/rollback pairs, and `undo_description` was wrong | Trials now run offline, so there are at most 3 entries (trial import, rollback, re-import). `undo` reports `entries_added`, `last_entry` and a warning, repeated in `next_step`. `undo_description` is the real last label | `the_undo_history_is_described_as_it_is` |
| 6 | A trial DRC without `unconnected_items` read as all complete | Trial and final DRCs now fail the import instead | (IPC path; the trial bails in `LiveTrial::run`) |
| 7 | Created KIIDs were paired with nets by position | `created_route_items` decodes each returned item's own net | `created_items_are_read_with_their_own_nets` |
| 8 | The board-unchanged check came before a multi-second DRC | `import` re-checks `save == before` right before each commit | (IPC path) |
| 9 | Region selection used only track end points | `Region::touches`: a track or arc by any chord (Liang–Barsky), a via by its centre | `a_track_crossing_the_region_with_both_ends_outside_is_taken` |
| 10 | A test assertion passed via "revision"; the round loop was not testable | Assertion fixed (same board text, one track unlocked → "not locked"). The loop is now pure (`route_verdict::decide` over a `Trial` trait) and tested with a fake DRC: rounds, the round-limit exit, groups, bisection and its budget. Trials replace live rollbacks between rounds | `a_clean_import_…`, `a_failing_net_is_dropped_…`, `require_complete_drops_partial_nets` |
| 11 | `net_classes` whose nets were all left to planes was refused as unknown | Classes are read from the DSN before the plane nets leave it. They are reported in `not_in_job`. When nothing at all is routable, the refusal names every net with its reason, as it now does for `nets` (it gave a bare count) | `a_class_left_entirely_to_planes_names_its_nets_and_why` |

**How `per_net` works now:**
1. The whole session is imported in one commit (the board-unchanged check runs right before it).
2. Every trial is an offline DRC of that board's text, with the other nets' created items cut out by KIID (`without_items`, which refuses unless it finds every KIID). Trials never touch the live board or its undo history.
3. `decide` runs rounds of: one trial of all remaining units. On a fault no net can be blamed for, it bisects and drops the units that bring the fault. Otherwise it judges each net and drops failing units whole.
4. Bisection trials each half with a context. When neither half shows the fault, the fault needs both (two routes that together cut a pour into an island). The second half is then searched again with the first half added to the context, so only its part of the pair is dropped.
5. If anything was rejected: one compensating deletion (proved byte-identical to before), one re-import of the kept nets, and a DRC check that the re-import has no more errors than its trial.

**Live** (KiCad 10.0.8, Freerouting 2.3.0, a fresh copy of `stm32_nrf_module`):
- **Fix 1, dry runs comparing a footprint walk against ripping its nets whole.** The walks stopped at plane vias and other pads, as listed below.

  | Footprint | Walk | Nets whole | GND | +3V3 |
  |---|---|---|---|---|
  | U1 | 175 items | 334 | 8 against 74 (4 plane vias) | 35 against 125 (7 plane vias, 1 other pad) |
  | U4 | 81 | 266 | 10 against 74 | 7 against 125 |
  | C8 (decoupling cap) | 9 | 199 | | |

- **Fix 4, the reviewer's case.**
  - Setup: every unlocked track ripped up (327 items), then full-board GND pours added on F.Cu and B.Cu, which Freerouting does not see.
  - Whole-board autoroute: route 136 s, 23 nets with routes.
  - On e118255's logic every net would have been rolled back. The first run here, with the old 12-trial cap, ran out and rolled back. That is why the budget is now 2·n + 8.
  - With the budget alone, the first bisection version dropped a whole half (14 nets rejected) for an island that only two routes make together. That is why the delta-debugging context was added.
  - Final run, apply 45 s:
    - 25 trial DRCs, 3 rounds; 19 nets kept (18 complete).
    - 4 rejected by bisection, each with its starved-thermal error: /LDO_EN, /NRF_IRQ, /NRF_RESET, and VBUS ("together with other kept nets").
    - Undo: 3 entries, last "Import Freerouting SES (19 of 23 nets)", with the warning.
  - Between the runs, `rip_up_nets` on the 9 nets imported in the second run restored the board exactly: the manifest's revision hash matched again.

**Tests:**
- `konnect-core --lib`: 1954 passed, 0 failed (1935 after the merge).
- Workspace: 2657 passed, 1 failed (the environmental `conformance_test` pin-label test).
- `doc_tool_counts`: 6/6.

**Not done:**
- No unit test for fixes 6 and 8: they are in the IPC closure, and no IPC fake exists.
- The offline trials assume KiCad's DRC on the text equals the live board. The re-import's own DRC is the check on that.

### Final follow-up (second review of 112e980)

| # | Finding | Fix | Test (fails on 112e980) |
|---|---|---|---|
| A | The footprint walk ripped shared supply trunks: it followed tees and joints to the next pad in every direction. The tool text claimed otherwise | On a supply net the walk also stops at the first branch: a tap onto another track's body, or a node where 3 or more tracks meet (vias are joints, not branches). A supply net is a plane net, intent role `power`, a ground-like name (`GND`, `VSS…`, `0V`) or a net with more than 3 pads. `footprint_walk` gains `supply_net` and the stop reason `branch`. The tool text and tool-directory now say exactly that, and also that a segment goes whole: a part's own stretch of a trunk up to the first branch goes with it | `a_supply_trunk_and_other_parts_taps_stay` |
| B | A pair half was kept when its partner had no routes, because groups were built from the routed nets only | Groups are built from the job's nets. A unit whose group has a member with no routes that was still unconnected before the import is rejected before any trial, with `decided_by: "group"` and the partner named. A partner that was already complete does not hold it back | `a_pair_half_without_its_partner_is_dropped_before_any_trial` |
| C | A project with `unconnected_items` set to `ignore` made every net read complete | The project copy used for every Konnect DRC (`drc_with_project_rules`: per-net trials, the import gate, `route_path` and `fanout_to_plane` gates) sets `unconnected_items` back to `error` when the project ignores it. Other severities and the user's project are untouched | `the_drc_copy_of_the_project_always_reports_unconnected_items` |
| D | API_MIGRATIONS said "at most 12 extra trial DRCs" | Corrected to 2·units + 8, capped at 64 | (doc) |

**Optional and low-priority items:**
- **Teardrops.** Trials now also leave out the teardrop zones the import created (zones with `(attr (teardrop …))` that the board did not have before) for the nets they cut. This is not checked live: no board here has teardrops enabled, and whether IPC-created tracks get teardrops is not established. Tests: `only_teardrops_the_import_made_are_tracked`, plus a teardrop case in `trial_boards_leave_out_exactly_the_named_items`.
- **Re-import gate.** The re-import of the kept nets now also fails, and is rolled back, when a kept net has more unconnected items than in its trial, or when another net has more than before the import (`reimport_regression`, tested by `a_reimport_must_not_open_more_than_its_trial`).
- **Undo warning on rollback.** A rolled-back import's error now ends with the undo warning: the history holds 2 to 4 entries from the call.

**Tests:**
- `konnect-core --lib`: 1959 passed, 0 failed.
- Workspace: 2662 passed, 1 failed (the environmental `conformance_test` pin-label test).
- `doc_tool_counts`: 6/6.

## Pass 23: routability pre-check (Tier 2, item 2)

**Problem (WORKFLOW §3.2, §5 item 1):** a doomed placement cost a 10-minute autoroute to discover. Ratsnest crossings, pin-escape capacity and channel capacity were worked out by hand (the module board's 23 → 5 unrouted came from reasoning about pins round the package), and nothing said where the board was short or which part to turn.

**New tool `check_routability`** (`placement` toolset, read-only, `tools/routability/`). It returns `doomed | tight | fine` in 1.2–2.1 s on the benchmark boards, with every reason naming its threshold as `sourced` (doc-DB slug) or `judgement`, the measured value, and the coordinates, parts and nets behind it.
- **Airwires.** KiCad's own ratsnest when possible: an offline `kicad-cli pcb drc` with zones refilled, on a copy with the project's rules (`drc_with_project_rules`, made `pub(crate)`). This counts partial routing and pours. Each `unconnected_items` entry becomes an airwire between the nearest anchors of its two items, found by UUID: a pad's centre, a track's nearer end, a via, or a zone's fill vertices. Without kicad-cli, or with `airwires: "mst"`: KiCad's construction, re-done. Per net, copper that touches is joined (weight 0), then Kruskal between clusters (`kicad-10-0-8-ratsnest-data`, `RN_NET::kruskalMST`). `airwires.source` says which was used.
- **Demand.** Nets with a zone on a `power` layer, a pour on a `mixed` layer, or an outer pour over at least half the board (judgement) are plane nets. Their airwires are reported as fan-out work (`plane_net_airwires`), not demand. The tool does not rebuild Pass 17's "kept for mixed planes" split; mixed-layer pours count as blocked area on that layer instead. `nets` / `net_classes` restrict demand to one routing group, as `export_specctra_dsn` does.
- **Rules.** Per net, the net class's track width and clearance, matched by `netclass_patterns` (`glob_matches`, made `pub(crate)`). Also the project's `min_track_width` / `min_clearance` (the "fab minimum") and `min_copper_edge_clearance`. Signal layers are the stack-up's `signal` and `mixed` copper layers, or `signal_layers`.

**Metrics** (one module each, reusable by the Tier 3 pin-swap optimiser):
1. **Crossings** (`crossings.rs`):
   - Proper crossings between airwires of different nets; per layer, airwires crossing other nets' existing copper; the worst net pairs with a location.
   - Layers: DSatur colouring of the conflict graph. Vias: colour classes mapped to layers to minimise end vias (one per single-layer end on another layer) plus 2 per crossing left on one layer. This generalises "2 per SMD–SMD airwire off its layer".
   - No planarity, thickness or crossing-number bound: the airwire graph is a forest (`wikipedia-graph-thickness`, `wikipedia-crossing-number`). Crossings as a placement signal: `arxiv-2210-14259-ns-place`.
   - Can only give `tight`: more than layers + 1 colours, K/E ≥ 0.3, or more than 1 via per airwire (judgement, calibrated below).
   - `rotation_hints`: for the three parts in most crossings, the crossings and airwire length with the part turned 90/180/270°, plus whether any turn `improves`. The airwires are kept, not rebuilt.
2. **Pin escape** (`escape.rs`), for footprints with at least 8 SMD pads:
   - Wang's `C = ⌊(p − d − s)/(w + s)⌋` per gap and `D = ⌊(√2·p − d − s)/(w + s)⌋` (`wang-iccad06-escape-layer-minimization`). The text extraction reads "2p"; the PDF in `docs/raw` (page 2, rendered and checked) and both worked examples confirm √2·p. With 2p, 225/100/25/25 would give D = 6, not 3.
   - AN114's 3-trace table row is inconsistent (`intel-an114-bga-board-design`), so the general formula is used.
   - **Perimeter parts** (QFN/QFP/SOIC): pads on the pad-centre bounding box form the sides; the rest are interior. Per side:
     - pitch, pad width, C, ΣC;
     - interior nets assigned to the side facing their destination (overflow goes to `interior_needs_via`);
     - ring pads wired to something inside the ring when C = 0 (`via_outside_ring`: a part underneath, the centre pad);
     - nets whose far end is behind the part (`behind`);
     - whether neighbouring pads that both need a trace are a track pitch apart. This uses each pad's own net width: half of each width plus the larger clearance.
   - **Grid arrays** (≥ 9 round pads on a ≥ 3×3 lattice): signal rows, C, D, the 4(n − 1) boundary channels, UG1099 Eq. 2-1, and `layers = 1 + ⌈max(0, R − (C+1))/C⌉` from the outermost-row rule (`xilinx-ug1099-bga-design-rules`). C = 0 with more than one signal row cannot escape.
   - `doomed` (sourced) when the escape needs more signal layers than the board has. `tight` (judgement) when interior nets use more than 80 % of a side's channels, or neighbouring trace pads are closer than their traces need.
3. **Capacity** (`capacity.rs`):
   - **Full-board section cuts** every 0.5 mm, horizontal and vertical (the section-crossing method named in `holden-tt42-density-equations`). Capacity per layer is Σ ⌊(g − s)/(w + s)⌋ over the free intervals. The free intervals are the line inside the outline minus the union of pads, tracks, vias, track keep-outs, mixed-layer pours and the edge clearance (`kim-iccad11-simplr`: count partial obstacles, union first).
   - **Demand on a cut** is the airwires crossing it, minus those crossing on their own pad. The **lower bound** is the nets with copper on both sides and none of their own across the line: each must cross at least once.
   - A full-board straight line cannot be gone round. So lower bound > lanes (at the narrowest crossing net's pitch), or Σ(w + s) > Σ(g − s_min), is a proof that no router can finish (`doomed`, sourced). The cut also reports the lanes at the fab minimum: "fits only if necked down".
   - Airwire utilisation over 0.8 on a cut is `tight` (judgement).
   - **Channel cuts** run between two parts facing each other across ≤ 3 mm with no third part between, one of them with at least 8 pads, sharing at least 2 mm. Each is a segment from one part's centre to the other's at 1/4, 1/2 and 3/4 of the shared span. A route can go round a channel, so these gave only `tight` (utilisation > 1 with ≥ 3 airwires; judgement). *Since the follow-up (N2) they are informational, measured between pad copper.*
   - **Congestion grid** (`cell_mm`): supply per cell is Σ_layers free area / (w + s), from a raster built from the same shape–line intersections as the cuts (`lou-ispd2001-probabilistic-congestion-slides`: tracks per pitch; PCB layers are not directional). Demand is RUDY per airwire, its box clamped to one track pitch and weighted by the net's pitch (`circuitnet-routability-features`, `arxiv-2206-03032-xie-thesis-rudy`). Overflow uses CircuitNet's definition.
   - Cells with no free track at all (pin fields) are counted apart as `blocked_cells_with_demand`, not as overflow. Otherwise every fine-pitch pad cell overflowed.
   - More than 5 % of cells overflowing is `tight` (judgement). On the benchmark boards it never exceeded 0.9 %.
4. **Density** (`holden-tt26-density-equation`): Wd = Σ Manhattan airwire length (pitch-weighted); Wc = Σ_layers free area / (w + s) · E; U = Wd / Wc. E is calibrated below. *Changed by the review (H2):* density only ever gives `tight` (judgement). U > 1 is `density.beyond_calibration`, "denser than any board routed so far"; U > 1/1.2 is `density.u_tight`.
- **Pin density** (informational, no threshold): pins per cm² and per in² of board area (`cadence-burkhert-pcb-density-layer-count`).
- **Verdict:** `doomed` only if the pin escape or a full-board cut proves it (after the review, density no longer dooms); `tight` if any metric says so; else `fine`.
- **`recommendation`:**
  - `doomed`: the failing necessary condition, and what to change first.
  - `tight`: the brief's tiebreaker, citing `kim-iccad11-simplr` (estimators miss what a router sees). Route the named worst nets first: the nets in the most crossings, on the worst cut and in the worst cells. Use a grouped `export_specctra_dsn`, about 10–20 passes and `per_net` import.

**Calibration on the benchmark boards.** The setup:
- Copies in the session scratchpad; the board files were not touched.
- "rip" = unlocked routing removed (as Pass 22). "base" and the perturbations = all routing removed.
- Freerouting 2.3.0 headless, 40 passes, on pcbnew's own DSN. The routes were imported with `ImportSpecctraSES`, then counted with `kicad-cli pcb drc --refill-zones`.
- "FR open" is KiCad's unconnected count after the route; in brackets, signal nets only. GND/+3V3 openings are plane fan-out work, which the tool does not count as demand.
- Freerouting is deterministic here: 4 repeated runs, identical results.
- The PoC `mcu_board` and `led_board` were added as boards Freerouting completes. `stm32_nrf_module_wall` is the base module with a 1 mm track/via keep-out across the board at y = 118–119, leaving a 2 mm gap at the right edge.

| board | verdict | reasons | airwires | K | K/E | k | vias | worst cut util | U (E=0.13) | pins/cm² | time s | FR open (signal) |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| led_board_base | fine | | 3 | 0 | 0.00 | 1 | 0 | 0.02 | 0.09 | 1.3 | 1.2 | 0 (0) |
| mcu_board_base | fine | | 29 | 8 | 0.28 | 3 | 8 | 0.05 | 0.24 | 6.2 | 1.4 | 7 (0) |
| stm32_nrf_module_rip | tight | crossings | 33 | 29 | 0.88 | 3 | 35 | 0.12 | 0.63 | 24.2 | 1.7 | 6 (2) |
| stm32_nrf_module_base | tight | crossings | 39 | 30 | 0.77 | 3 | 43 | 0.11 | 0.66 | 24.2 | 1.4 | 19 (3) |
| stm32_nrf_module_u1r90 | tight | crossings | 39 | 40 | 1.03 | 5 | 56 | 0.11 | 0.70 | 24.2 | 1.4 | 20 (2) |
| stm32_nrf_module_u1r180 | tight | crossings | 39 | 47 | 1.21 | 5 | 55 | 0.16 | 0.74 | 24.2 | 1.4 | 20 (3) |
| stm32_nrf_module_swapJ2J7 | tight | crossings | 39 | 32 | 0.82 | 4 | 45 | 0.11 | 0.69 | 24.2 | 1.4 | 19 (3) |
| stm32_nrf_module_wall | **doomed** | cuts (sourced), crossings | 39 | 30 | 0.77 | 3 | 43 | **1.08** | 0.67 | 24.2 | 1.4 | 26 (10) |
| stm32_demo_rip | tight | crossings, density | 63 | 228 | 3.62 | 11 | 190 | 0.19 | 0.90 | 8.5 | 1.8 | 1 (1) |
| stm32_demo_base | tight | crossings, density | 66 | 244 | 3.70 | 11 | 195 | 0.19 | 0.93 | 8.5 | 1.6 | 20 (2) |
| stm32_demo_u1r90 | tight | crossings, density | 66 | 196 | 2.97 | 13 | 174 | 0.19 | 0.94 | 8.5 | 1.6 | 27 (5) |
| stm32_demo_u1r180 | tight | crossings, density | 66 | 273 | 4.14 | 12 | 206 | 0.19 | 0.96 | 8.5 | 1.5 | 21 (6) |
| stm32_demo_swapJ5J6 | tight | crossings, density | 66 | 281 | 4.26 | 10 | 246 | 0.20 | 0.93 | 8.5 | 1.5 | 19 (2) |
| eth_board_rip | tight | crossings | 58 | 25 | 0.43 | 3 | 56 | 0.06 | 0.26 | 15.8 | 2.1 | 9 (5) |
| eth_board_base | tight | crossings | 136 | 95 | 0.70 | 5 | 163 | 0.09 | 0.52 | 15.8 | 1.6 | 45 (5) |
| eth_board_u4r180 | tight | crossings | 136 | 198 | 1.46 | 8 | 259 | 0.16 | 0.59 | 15.8 | 1.7 | 50 (10) |

- **Verdict class against Freerouting: 16 of 16 consistent.**
  - The two boards it completes are `fine`.
  - Every board it leaves 1–10 signal nets open on is `tight`.
  - The wall board is `doomed`. The failing cut names 7 nets (/NRF_IRQ, /NRF_RESET, /NRF_RX, /NRF_TX, /NRST, /SWCLK, …) against 6 lanes (8 at the fab minimum). Freerouting left 10 signal connections open, mostly those nets.
- **The verdict is coarse.** `tight` spans 1 to 10 open nets, and no metric ranks the boards well. Spearman rank correlation with signal nets open over the 16 runs:
  - K/E 0.13, K 0.24, vias per airwire 0.19, U 0.16, worst cut utilisation 0.37, overflow fraction 0.06;
  - without the wall board: K/E 0.21, K 0.33.
  - Absolute crossing counts do not transfer between designs: the demo, at K/E 3.7, routes better than the Ethernet board at 0.7.
- **Perturbations (base → perturbed) against Freerouting:**
  - Agree: Ethernet U4 180° (K 95 → 198; open 5 → 10); demo U1 180° (K 244 → 273, U 0.93 → 0.96; open 2 → 6); module wall (open 3 → 10).
  - Metrics worse, Freerouting unchanged: demo J5/J6 swap; module U1 180° and J2/J7 swap.
  - Disagree: demo U1 90°: K 244 → 196 and `rotation_hints` proposes exactly this turn (195 predicted), but Freerouting went 2 → 5. Module U1 90°: K 30 → 40, open 3 → 2.
  - So the rotation what-if is a hint, not a predictor. The `tight` recommendation (a short grouped Freerouting pass) is the real tiebreaker, as `kim-iccad11-simplr` warns.
- **Thresholds calibrated on these runs (judgement):**
  - `crossings.layers` is now colours > layers + 1: the completed `mcu_board` needs 3 colours on 2 layers.
  - K/E ≥ 0.3 separates `mcu_board` (0.28, complete) from every failing board (≥ 0.43).
  - Vias per airwire > 1 does not fire on the complete boards.
  - The cut, channel and congestion thresholds never fired on a real board except the wall. The worst real cut was at 0.20 and overflow at 0.9 % of cells; failures on these boards are local, not capacity.
- **Fitted E (layout efficiency).** Holden's E is the share of capacity a router can deliver; `holden-tt42-density-equations` says to fit it on your own boards. Measured as routed signal track on F/B (plane nets excluded) over raw capacity (free area / pitch, ripped board):

  | board | routed signal track (mm) | raw capacity (mm) | ratio | state |
  |---|---|---|---|---|
  | stm32_demo as saved | 1396 | 11102 | 0.126 | complete |
  | stm32_demo Freerouting (rip) | 1402 | 11001 | 0.127 | 1 signal open |
  | stm32_nrf_module as saved | 401 | 3694 | 0.109 | 5 open |
  | eth_board as saved | 1098 | 15186 | 0.072 | 2 open |
  | mcu_board Freerouting | 237 | 9045 | 0.026 | complete |
  | led_board Freerouting | 21 | 2708 | 0.008 | complete |

  - **E = 0.13**: the densest completed board, rounded up. This replaces the unsourced 0.5 placeholder. At 0.5 every board read U ≤ 0.25, so the metric could never fire.
  - U > 1 means "denser than any board routed so far", not a physical proof. E rests on one dense completed board, so since the review it can only make a board `tight`.
  - `holden-tt42-density-equations` recommends a regression on your own boards to normalise the wiring-demand model. Here the same idea is applied to E, which has no other source.
  - Routed length is close to the Manhattan airwire length (demo 1396 against Wd 1341), so Wd needs no detour factor here.
- **Pin density** (informational): module 24.2, Ethernet 15.8, demo 8.5, mcu 6.2, led 1.3 pins/cm². It ranks the boards' hand-routing effort, not Freerouting's result: the module is densest but has fewer open nets than the Ethernet board.

**Tests:** 31 new unit tests in `tools/routability/tests.rs`, plus 1 ignored real-board report (`ROUTABILITY_BOARD`).
- Formulas: Wang C and D on the paper's worked examples, with a 2p-versus-√2·p check; the UG1099 row rule and Eq. 2-1.
- Geometry: line intervals per shape, interval algebra, raster area.
- Crossings: proper crossings only; DSatur; via and layer plan, including through-hole ends; the crossing bundle is `tight` and never `doomed`; the rotation what-if.
- Escape:
  - a QFN with C = 0 and a signal centre pad needs a via, so it is `doomed` on one layer;
  - a part underneath needs a via outside the ring;
  - a net behind the part is flagged;
  - BGA rows and layers;
  - fan-out pitch with per-net widths; the fab-minimum reason.
- Cuts:
  - a keep-out wall with a 1 mm gap proves doom; four nets still fit it;
  - a wall that can be gone round does not doom;
  - a partly routed net across the cut owes no crossing, and its track takes the lane;
  - channel cuts name the parts and never doom.
- Airwires: the spanning tree joins existing copper at zero weight; DRC items become airwires between the nearest anchors (by UUID, a track's nearer end).
- Plane nets, mixed-layer pours, groups, net classes from the project, the density verdict path, RUDY overflow against pin-field cells, threshold labelling (only sourced thresholds can doom), the `signal_layers` check, and the handler on a saved board.
- `konnect-core --lib`: 1990 passed, 0 failed (was 1959). `doc_tool_counts`: 6/6. 249 registered tools, 256 with the meta-tools.

**Known limits:**
- **Cuts** are axis-parallel lines only. A diagonal wall, or a slot that is not straight, cannot prove doom. Channel cuts are only examined next to parts with at least 8 pads.
- **Airwires are straight lines between KiCad's anchors.** Crossings depend on the ratsnest topology, which a router does not follow. Differential pairs are not treated as one unit.
- **Plane nets:** outer-layer pours are not counted as blocked, and mixed-layer pours are blocked in full. A net with a small pour on a mixed layer is treated as carried by it.
- **Widths come from net classes only.** Per-net `.kicad_dru` width rules are not read; the boards here carry intent widths as classes.
- **Escape classification** uses the pad-centre bounding box. Modules with castellations, and connectors, get a rough side analysis. Escape demand counts only pads with an unrouted airwire, so a channel already used by existing tracks is not subtracted.
- **Calibration:** E and the judgement thresholds come from 16 runs of 5 designs, all 2-signal-layer boards with inner planes, and one router (Freerouting 2.3.0, 40 passes). No 1-layer, 6-layer or BGA board was available, so the grid-escape and layers-needed paths are unit-tested only.
- **Not validated live in KiCad's GUI.** The IPC (open board) source path is shared with `score_placement` but not exercised here. The MCP path was run against the release binary on saved files.

### Review fixes (independent Opus review of f1dd99e)

| # | Finding | Fix | Regression test |
|---|---|---|---|
| H1 | A BGA was `doomed` (sourced) when C = 0 at the class width, though UG1099 allows neck-down in the breakout. The same report gave `ug1099_layers` 2 | Escape doom is judged at the fab minimum (`min_track_width` / `min_clearance`). The class-rule result is `layers_needed_at_class_rules`, with `needs_neck_down`; it gives `tight` (`escape.neck_down`, judgement) when it exceeds the layers. A grid adds `c_at_fab_minimum` | `review_h1_bga_escape_is_judged_at_the_fab_minimum` |
| H2 | Density `doomed` was labelled sourced, but E is a one-board fit; U > 1 is only 7.6 % denser than a completed board. TT26 also gives "layout performance 1.05 to 1.15" (U ≈ 0.87–0.95), which conflicts with a sourced 0.833 | Density can only give `tight`, both thresholds `judgement`, and the conflict is stated in the threshold text. "No router can finish" now appears only for cut and escape proofs. The tt42 citation now says what tt42 says (normalise the demand model on your own boards) | `holden_density_verdicts_follow_u` (rewritten) |
| M3 | Inner layers typed `signal` (KiCad's default) with a full GND/+3V3 pour were routing layers. On the demo: 47 plane airwires became demand, 4 layers, U 0.93 → 0.54 | The pour-area test runs on every copper layer. An inner layer poured over at least half the board is a plane layer (`rules.plane_layers`): its net is a plane net and the layer is not routed on by default. If `signal_layers` names it, its pour is blocked | `review_m3_a_signal_typed_inner_layer_poured_over_the_board_is_a_plane` |
| M4 | `nets: []` and `nets: ["GND"]` gave `fine`; names were not trimmed; `netclass_assignments` was ignored | Group arguments are read as `routing_group_argument` reads them: an empty array or a blank name is refused, names are trimmed. A group with no demand is refused, each net with its reason (left to its plane, single pad, already connected). `netclass_assignments` (string or array form) is applied before `netclass_patterns` | `review_m4_groups_are_read_as_export_specctra_dsn_reads_them`, `review_m4_netclass_assignments_come_before_patterns` |
| M5 | A QFN pin wired to its own exposed pad (non-plane net) was counted as needing vias: `doomed` on one layer | A connection to another pad of the same part on a shared layer is a direct stub inside the part, not escape demand | `review_m5_a_pin_wired_to_its_own_exposed_pad_is_a_stub_not_escape` |
| L6 | `crossings_by_part` counted a crossing twice when both airwires touch the part | Distinct crossings per part | `review_l6_a_crossing_counts_once_per_part` |
| L7 | Channel cuts ran centre to centre, counting space inside a part's pad ring | The segment spans only the gap, courtyard to courtyard | `review_l7_channel_cuts_span_only_the_gap` |
| L8 | `F&B.Cu` / `*.Cu` zone layers were not recognised; pads in tstamp-era files had no identity | Zone layer wildcards are expanded. A pad's `tstamp` is used when it has no `uuid`, and DRC items still fall back to the reported position | `review_l8_zone_layer_wildcards_are_expanded` |
| L9 | Edge clearance was charged twice: the band, then `s` again in the lane formula | A free interval that ends on the edge band (or the outline) gets that end's `s` back | `review_l9_the_edge_band_is_not_charged_a_clearance_twice`; the partly-routed-cut test now expects `[2, 1]` lanes (one track fits beside the edge) |
| L10 | The doom text did not say existing copper is kept (unlocked tracks count as fixed) | Cut and recommendation texts say "with existing copper kept" | (text) |
| L11 | `-0.0` printed | `round3` returns `+0.0` | (all reports) |

**After the review:**
- The reviewer's repro boards now give: `bga` `tight` (`escape.neck_down`, was `doomed`); `qfn` `tight` (not doomed); `demo_sig` the same as the power-typed demo (2 layers, 47 plane airwires, U 0.93); `fb` with GND recognised on `F&B.Cu`.
- The calibration table was re-run on all 16 boards: no verdict changed (none had been doomed by density or escape, and no channel cut had fired). With the edge fix (L9), the wall board's cut has 6 lanes (10 at the fab minimum, was 8) and width utilisation 0.93. It is still `doomed` on the lane count: 7 nets against 6 lanes.
- **Tests:** 9 new or rewritten; `konnect-core --lib` 1999 passed, 0 failed.

**Revised limits:**
- Doom now comes only from proofs: a full-board cut, or an escape that does not fit even at the fab minimum.
- Density is advisory.
- A partial inner pour (less than half the board) on a signal-typed layer is still routed on, unblocked.

### Follow-up (re-review of 457f664)

**Principle:** doom proofs assume the most favourable reading of the board; the advisory (`tight`) metrics assume the less favourable one; both are disclosed.

| # | Finding | Fix | Regression test |
|---|---|---|---|
| N1 | M3 turned a signal-typed inner layer with a flood of at least half the board into a plane for everything. A flooded routing layer (common; the pour yields to tracks) could then give a false sourced `doomed`. Repro: an 8×8 BGA with In2 signal + GND flood was `doomed`, "needs 3, routes on 2" | Such layers are now `rules.ambiguous_plane_layers`, with a note: "set the layer type to power if it is a plane, or remove/limit the pour if it is for routing". **Proofs** (`escape.layers`, `cut.lower_bound`) use `rules.proof_layers`: only `power`-typed inner layers are left out, ambiguous pours are unblocked, and full-board cuts are run on that set. **Advisory** metrics (crossings, cut utilisation, congestion, density) keep the M3 reading (a plane, pour blocked if named), so `demo_sig` still reads like the power-typed demo. The net is a plane net either way. An escape that fits only with the ambiguous layers is `tight` (`escape.ambiguous_layers`, judgement). Only `power`-typed inner layers are now in `plane_layers` | `followup_n1_a_flooded_signal_typed_inner_layer_never_proves_doom`, `followup_n1_the_cut_proof_reads_the_flood_as_routable`; `review_m3_…` updated |
| N2 | Channel cuts ran courtyard to courtyard: close parts got 0 lanes (Ethernet U5–C25, 0.52 mm) and a false `tight` | Channels are measured between the parts' pad copper; the lane formula keeps the clearance at both ends. They are now **informational** (`channels_note`), never a verdict reason. A route can go round a channel, and when the gap is the only way through, the full-board cut along the same line counts it. The `channel.utilization` threshold is removed | `channel_cuts_name_the_parts_and_never_doom` (rewritten: 15 airwires, utilisation > 1, no reason), `review_l7_…` (copper extents) |
| N3 | A group whose nets are all already connected was refused, but `export_specctra_dsn` accepts any net with 2 or more pins | Such a group returns `fine`, with `group.note` "nothing left to route: … already connected". Empty or blank names and groups of only plane or single-pad nets are still refused | `followup_n3_a_fully_routed_group_is_fine_with_a_note` |
| N4 | Perimeter escape mixed rule sets: the via lists were at the class rules, `layers_needed` at the fab minimum | `interior_needs_via` and `via_outside_ring` now follow `layers_needed` (fab minimum). What only the class widths add is listed in `interior_needs_via_at_class_rules_only` / `via_outside_ring_at_class_rules_only`. The `sides[]` channel numbers stay at the class rules | `followup_n4_via_lists_follow_layers_needed_at_the_fab_minimum` |
| N5 | M5 dropped every same-part connection on a shared layer, including pin to pin across the package past a different-net centre pad, and through-hole links | A direct stub only when both pads are SMD on one layer, one of them is an interior pad (off the pad-centre box), and the straight stub crosses no pad of another net. Everything else stays demand | `followup_n5_pin_to_pin_across_the_part_stays_demand`; `review_m5_…` still passes |

**After the follow-up:**
- The reviewer's `bga8` (In2 flooded): `tight`, with `escape.ambiguous_layers` naming In2.Cu (was `doomed`). `bga8nofill`: `tight` (`escape.neck_down`).
- Tests: 5 new; `konnect-core --lib` 2004 passed, 0 failed.
- The 16 calibration rows were re-run: no verdict changed.

**Revised limits:**
- An ambiguous layer is read both ways, so a board whose routability hinges on it is reported `tight`, never `doomed` or `fine`. Setting the layer's type resolves it.
- Channel cuts no longer affect the verdict.
  A crowded gap between two parts therefore no longer raises `tight` by itself; this is accepted because the calibration never saw it fire, and a gap that is the only way through is still counted by the full-board cut on the same line (third review, L-b).
- When the caller names an ambiguous layer in `signal_layers`, the advisory metrics (cut utilisation, density) still block its pour, so they understate that layer's capacity. It can only add a `tight` reason, never `doomed` (third review, L-a; not fixed).

## Pass 24: low-inductance fan-out (Tier 2, item 3)

Code: `tools/fanout.rs` (planner), `pcb_routing.rs` (`handle_fanout_to_plane`), `design_intent.rs` (`plane_verdicts`, `intent_for_board`, `regime_note`).

**Problem (WORKFLOW §5 item 4, §8 R4):** `fanout_to_plane` skipped any pad whose track reached a plane via, however long the track or whoever's pad it ran through; put vias wherever the nearest clear site was (often at the end of a capacitor, or in a small pad's neighbour); never shared a via even where the physics favours it; and put a capacitor's via inside another capacitor's same-net pad (the Pass 18 daisy chains on the module and demo boards). The AI then had to run `check_design_intent`, read the failures and hand-place vias.

**What it does now (`mode: "low_inductance"`, the new default; `legacy` keeps the old planner):**
- **The judge decides what needs a via.** Before planning, `plane_connection` runs on the board exactly as `check_design_intent` runs it (intent beside the board, its limits and exceptions). Pads it passes are `kept`; pads covered by an intent exception are `exception` with the exception's kind and source; pads it fails (long stub, daisy chain, forbidden shared via, no via) or leaves unconnected get a via. An `unverified` result a via cannot settle (an unfilled zone) is `not_planned` with the judge's reason. Every pad on the nets, through-hole included, gets exactly one decision in `pads[]`: nothing is skipped silently.
- **Own via against the pad, not in it.** Sites are searched on rings round the pad rectangle, starting where the drill edge is `via_hole_gap` (0.25 mm) from the pad edge and stepping out 0.05 mm, up to the judge's connecting-trace limit for the net (1.5 mm on JLC04161H-7628) and the old `max_stub` (centre to centre). A small pad takes the via inside it only with `via_in_pad: "filled_capped"`, and then both pads of a two-pad part do.
- **Side of the pad.** Among legal sites (rings up to 0.3 mm past the first legal one) the planner minimises `2 × connecting trace + distance to the partner point + 0.3 × distance to the served IC pin` (weights: Konnect judgement). The partner point is the other pad's planned via, else its centre, so both vias of a capacitor land beside the pads, close together, on the side facing the IC pin the part serves (nearest device pad on the part's nets, same side, ≤ 15 mm). Pads of other parts (ICs) keep the old outward preference. Sites under the part's own body (between its pads) are not used. The class is reported (`side`, `end`, `corner`, `outward`, `inner`).
- **Symmetry.** Both stubs of a two-pad part get the narrower of the two nets' track widths; `pads[].symmetry` says how the other pad was treated (`symmetric: …` or `check: …`, or that it is not on a plane net).
- **Shared via only where physics allows (`pwr.decoupling_via_sharing_with_device`).** With `share_with_ic: "auto"` (default) a capacitor pad takes a stub to an IC pin's via, instead of a via of its own, only when: the IC pad is on the same side; the net's regime from the stack-up is `widely_spaced` (planes > 0.5 mm apart); the via is within `share_max_gap` (0.5 mm in c80dea3; since the review fixes the default is butted against the pad, see L8 below) and keeps the 0.25 mm drill gap; the stub clears other copper and the IC's pad; and no other capacitor already uses that via (from the judge, or earlier in the plan). Capacitors never share with each other; opposite sides, `closely_spaced` and `unknown` regimes never share, and the reason says which (`share_note`). `never` gives every pad its own via.
- **Never in another part's pad, body or keep-out.** A pad of another part is an obstacle even on the same net, for the via and for the stub (the Pass 18 bug; fixed in the shared clearance test, so `legacy` gets it too). With `avoid_courtyards` (default) a via may not reach another same-side part's body, taken as its courtyard outline (real `F.CrtYd`/`B.CrtYd` lines, polygons, rects, arcs and circles, not the bounding box, so a QFP's cut corners stay usable) less the courtyard margin (0.25 mm in c80dea3; per part since the review fixes, see L6 below). Keep-outs, the edge, plane landing and copper on every layer (THT pads, back-side pads and tracks) are checked as before.
- **`no_site` is decision-ready.** The reason lists the refused-site counts by cause (`under U1's body`, `track of /PA0 on F.Cu`, `C5's same-net pad`, `off the plane fill`, …), so the AI knows what to move.
- **Self-check.** The response's `self_check` runs `plane_connection` on the result: after a dry run on the board text with the plan's items added (zones not refilled; new same-net vias sit inside the fill by construction), after apply on the board read back from KiCad. It gives `before` and `summary` counts, `failing`, and per pad `status`, `connection`, `stub_mm`, `via`, `via_shared_with`, `warnings`, `exception`, `detail`. Apply still goes through the DRC gate (`gated_commit`).
- **Dry run without KiCad.** `dry_run` plans from the saved file when KiCad is unreachable or does not have the board open (`board_source: "saved_file"`); board access is now `ApplyModeDependent`.

**Other fixes found on the way:**
- A tie into an exposed pad (`via: false`) created a via at its end on apply anyway; now no via is created, and the same holds for a shared stub.
- A tie's end (no via) is checked against copper on the pad's layer only; a via site still against every layer.
- `reaches_plane_with` refused sites within a via radius of the fill's fracture slits (the zero-width cuts KiCad uses to join holes to the outline), as "off the plane". Slits (an edge walked once each way) are no longer edges. Plane fills are indexed (y bands for even-odd, a 1 mm grid of solid edges), so a 140-pad board plans in about 1.5 s in a debug build.
- On the `fanout_regress2` fixture the QFN's exposed-pad grid drops from 9 to 5 vias: 4 touched C6.2, a GND pad of a back-side capacitor under the QFN. The test now says so.

**Sources (doc DB):**
- `xilinx-ug483-7series-pcb`: p.24 "Vias should be butted directly against the pads … more optimally located at the sides of the pads … Via placement at the sides of the pads decreases the mounting's overall parasitic inductance by increasing the mutual inductive coupling of one via to the other" (side placement); p.30 no connecting trace, via-in-pad "an option for reducing parasitic inductance" (`via_in_pad`), sharing vias among capacitors "should not be used under any circumstances" (no cap–cap sharing).
- `learnemc-decoupling-widely-spaced-planes` (planes > 0.5 mm): sharing the device's via "is optimal" if near enough; "never share a via with an active device when they are located on opposite sides"; "Locate the two capacitor vias as close together as possible" (partner point); "Never use traces … Locate the via adjacent to the mounting pad" (`share_max_gap`).
- `learnemc-decoupling-closely-spaced-planes` (≤ 0.5 mm): "Do not let high-frequency decoupling capacitors share a via with an active device"; "If there is no room for the via adjacent to the pad, then move the whole capacitor" (why `no_site` names the blockers rather than allowing a long trace).
- `eurocircuits-tombstoning`: "a minimum of 0.25mm from the edge of the component pad to the edge of the actual hole"; via filling helps but moving the hole "at least 0.25mm" away "is a more reliable solution" (`via_hole_gap`, in-pad only when declared); "make the track width to each pad of the component the same width" (symmetry). The guide measures to the *hole* edge, so with a 0.6/0.3 mm via the copper sits 0.1 mm from the pad: butted against it, as UG483 asks.
- Exceptions (hot loop, crystal, split ground, `vendor_reference_ground`, `no_plane`, `datasheet`) are the Pass 18 kinds, read through the judge; their sources are echoed per pad.

**Tests (`fanout::low_inductance_tests`, 15; lib 2004 → 2019 passed):** own via per pad beside the pads on one side, drill gap ≥ 0.25 mm and judged `pass`; side facing the IC (above and below); symmetric widths and `via_in_pad` for both pads, never in pad without it; no via near another part's same-net pad, no `daisy_chain` after (fails with the obstacle fix reverted); share allowed (widely spaced, same side) and judged `shared_via_ok`, refused with `never`; refused with close planes, unknown spacing, and across sides, each with its reason; capacitor–capacitor never shared; a `datasheet` exception reported with its source and no via; keep-out, THT and back-side pads never hit; via kept from under another part's body, allowed with `avoid_courtyards: false`, a back-side courtyard not blocking; every plane pad gets a decision; shared stubs write no via; legacy mode equals the old planner; fracture slits; courtyard outline (cut QFP corner) vs body. The ignored `fanout::board_validation::pass24_board_validation` (env `PASS24_BOARD`) reruns the board validation below.

**Validation on copies of the three boards (offline: kicad-cli DRC with project rules and zones refilled, the same comparison as the DRC gate; judge = `plane_connection` with each board's intent; the saved boards were never touched).** The saved boards have moved since Pass 18: they now fail 2/49 (demo), 3/60 (module) and 23/140 (Ethernet).

| Board | Saved board fails | A. in place: vias added → fails after (before = previous column) | B. GND/+3V3 ripped, then fan-out: vias / shared / no_site → pass, unverified, fail | Legacy on B | C. all routing ripped: vias / shared / no_site → pass, unverified, fail | Legacy on C | New DRC errors |
|---|---|---|---|---|---|---|---|
| `stm32_demo` (49 plane pads) | 2 | 1 → 1 | 44 / 1 / 4 → 45, 4, 0 | 44 vias → 43, 5, 1 | 47 / 0 / 2 → 47, 2, 0 | 47 → 47, 2, 0 | 0 in every run |
| `stm32_nrf_module` (60) | 3 | 1 → 2 | 45 / 1 / 14 → 48, 12, 0 | 49 → 48, 11, 1 | 53 / 1 / 6 → 55, 5, 0 | 54 → 52, 6, 2 | 0 (7 pre-existing) |
| `eth_board` (140) | 23 | 15 (9 pads + an exposed-pad grid) → 14 | 137 / 0 / 19 → 121, 19, 0 | 129 → 111, 27, 2 | 152 / 0 / 4 → 136, 4, 0 | 146 → 129, 10, 1 | 0 |

- Unverified pads after a fan-out are the `no_site` pads, left unconnected (KiCad's unconnected count drops, e.g. Ethernet C: 277 → 144 against 150 for legacy). The low-inductance planner never leaves a fail; legacy leaves 1–2 (shared or chained vias) in every scenario.
- **In place** fixes only what fails and keeps everything else: the remaining fails are pads with no legal site within 1.5 mm (Ethernet U1.28: tracks of /ETH_NRST and /ETH_TX_EN and the fill edge; demo C9.2: track of /USB_D_P and C10's body), which the response names. The old long traces stay (harmless; not removed).
- **Own-via connecting trace** (C): mean 0.13 / 0.19 / 0.19 mm, max 0.42 / 0.91 / 1.46 mm. Placements on Ethernet C: 66 side, 12 end, 5 corner, 50 outward (IC pins), 3 grids.
- **Routability (`check_routability` internals, KiCad airwires):** plane nets are not routing demand, so the fan-out changes only obstacles. On the bare boards (C) the verdict stays `tight` with the same crossings (244 / 30 / 95); density utilisation rises 0.929 → 0.935, 0.656 → 0.671, 0.521 → 0.529 (legacy the same); RUDY overflow 0.0010 → 0.0020 on Ethernet, unchanged elsewhere. On the routed boards (A, B) there is almost no demand left and nothing moves.
- Planning time (debug build, judge included): 0.3 s, 0.9 s and 1.5 s.

**Known limits:**
- Part kinds (capacitor, device) come from the reference prefix, as in the judge. A shared via must already exist (judged `via`/`via_in_pad` for a device pin) or be planned in this run; the planner does not move an IC pin's via towards its capacitor to make sharing possible, so sharing happens rarely (1 pad on demo and module, 0 on Ethernet).
- The cost weights, the 0.5 mm share gap, the 15 mm IC search and the courtyard-margin body are Konnect judgements. "Body" assumes the KLC 0.25 mm margin; a footprint with a larger margin (connectors, 0.5 mm) is treated as a slightly larger body.
- Sites are judged on pad rectangles (rounded pads are conservative) and on the file's fill; a dry run does not refill zones, so a new via can still cut another net's thin neck (the DRC gate's unconnected check catches that on apply).
- No live KiCad session was used: apply over IPC, the read-back self-check and the `saved_file` fallback over MCP were exercised only as far as noted in the final report; the offline harness replays the gate with kicad-cli.
- The stub runs from the pad centre to the via centre. Old long traces of in-place pads are left on the board.

### Review fixes (independent Opus review of c80dea3)

The review reproduced the board numbers and found one high, two medium and eight low items. All are fixed in "fix(fanout): Pass 24 review fixes"; each H/M item has a regression test that fails with the fix reverted.

| # | Finding | Fix | Test |
|---|---|---|---|
| H1 | Verdicts were keyed by `REF.PAD`, so same-numbered pads collapsed: a SOT-223's pin 2 and tab 2 (Ethernet U9), the switches' pads 1, 1, 2, 2. One pad's verdict decided both, depending on file order. | The judge's `measured` gains `pad_uuid` and `at`; the planner keys verdicts by `pad_identity` (UUID, else name and centre). `pads[]` and `self_check.pads[]` carry `pad_uuid` and `at`. On the Ethernet board it is the tab that fails (7.15 mm) and pin 2 that passes; pin 2 no longer gets a via it does not need (in place: 15 → 14 vias). | `same_numbered_pads_get_their_own_verdicts_and_decisions` (both file orders) |
| M1 | A via could land in another small pad of the same part on the same net (two GND pins at 0.65 mm pitch; the judge then called the neighbour `via_in_pad`). | Pad obstacles carry their centre and size; only the pad itself and its part's exposed pad (short side ≥ 2 mm) are free. Applies to `legacy` too. | `a_via_never_lands_in_a_neighbouring_pin_of_the_same_part` |
| M2 | No test went through `handle_fanout_to_plane`. | Response assembly is `fanout_response`, the IPC items `fanout_ipc_items`. Handler tests with KiCad unreachable: argument refusals (`mode`, `share_with_ic`, `via_in_pad`, `via_hole_gap`), the saved-file dry run with `self_check`/`before`, apply refused without writing; the items (own via: via + stub; shared/tie: stub only; in pad: via only; unknown net is an error). | `fanout_handler_tests` (5) |
| L1 | A failed read-back after a committed fan-out turned success into an error. | The read-back is optional; `self_check` then carries a note. | `the_response_survives_a_failed_read_back_and_names_an_empty_apply` |
| L2 | A non-dry run with nothing to place said "planned (dry run…)". | Stage `nothing to place: the board as it is`; `locked` only when items were created. | same |
| L3 | `legacy_mode_is_the_old_planner` compared `plan()` with itself. | Replaced by `legacy_mode_keeps_the_old_placement`, which pins the old result (via straight out along the axis, 0.05 mm off the pad). The obstacle fixes do change legacy plans: `fanout_regress2`'s exposed-pad grid 9 → 5 (documented above). | — |
| L4 | Two tests were weak. | The keep-out/THT test requires both pads planned and passing; the same-net-neighbour test has a tight box, requires C6.2 `no_site` naming C5's same-net pad, C5.1 planned, and no legacy via for C6. | — |
| L5 | `F.CrtYd` and `B.CrtYd` were merged into one even-odd outline (a footprint drawing both looked hollow). | Only the footprint's own side's courtyard. | `small_chips_have_a_smaller_courtyard_margin` |
| L6 | "KLC F5.3, 0.25 mm" had no doc-DB source. | klc.kicad.org and gitlab.com are blocked here (proxy 403). The KiCad footprint generator's own parameter files are in the DB instead: `kicad-fpgen-ipc7351b-two-terminal` (IPC-7351B courtyard excess, nominal: 0.15 mm below 0603, 0.25 mm at and above), `kicad-fpgen-ipc-definitions` (gull-wing/J-lead: 0.25 mm nominal), `kicad-fpgen-config-klcv3` (`courtyard_offset` default 0.25, connector 0.5, BGA 1). The margin is now per part (`courtyard_margin`). | same |
| L7 | New-mode `skipped[]` reasons are no longer `ALREADY_REACHES_PLANE`. | Documented in API_MIGRATIONS. | — |
| L8 | Sharing over a stub up to 0.5 mm conflicts with LearnEMC's "under no circumstances should a trace be used between the decoupling capacitor mounting pads and the vias". | `share_max_gap` defaults to "butted": no further than an own via would sit (drill edge `via_hole_gap` from the pad, plus one 0.05 mm step; 0.15 mm copper gap for a 0.6/0.3 mm via). A number is an explicit opt-in, a Konnect judgement. The tension remains: LearnEMC calls sharing "optimal" if the cap is "near enough", and Konnect reads "near enough" as butted. | `a_capacitor_shares_an_adjacent_ic_via_…` (0.32 mm: own via by default, shared with `share_max_gap: 0.5`) |

Also: three MCP dispatch tests used fixed names in the shared temp dir and collided across worktrees; they use `tempfile::tempdir()` (separate commit). Lib tests: 2019 → 2027 passed.

**Validation after the review fixes** (same harness and copies). Columns: fails before → vias added → fails after; for B and C: vias / shared / no_site → pass, unverified, fail.

| Board | A. in place | B. GND/+3V3 ripped | Legacy on B | C. all routing ripped | Legacy on C | New DRC errors |
|---|---|---|---|---|---|---|
| `stm32_demo` (49) | 2 → 1 → 1 | 43 / 0 / 6 → 44, 5, 0 | 43, 5, 1 | 47 / 0 / 2 → 47, 2, 0 | 47, 2, 0 | 0 |
| `stm32_nrf_module` (60) | 3 → 1 → 2 | 46 / 0 / 14 → 48, 12, 0 | 48, 11, 1 | 53 / 0 / 7 → 54, 6, 0 | 52, 6, 2 | 0 (7 pre-existing) |
| `eth_board` (140) | 23 → 14 → 14 | 134 / 0 / 22 → 118, 22, 0 | 109, 29, 2 | 150 / 0 / 6 → 134, 6, 0 | 128, 11, 1 | 0 |

- With the butted default no via is shared on these boards (the two earlier shares were 0.27 and 0.30 mm away). The stricter body margin for small chips (0.15 mm) and the same-part pin obstacles cost a few sites: the low-inductance planner now places 0–3 fewer vias than before the review, and still never leaves a failing pad where legacy leaves 1–2.
- Routability (C) is unchanged by the fixes: verdict `tight`, same crossings, density utilisation +0.006 to +0.015 with either planner.

**Revised known limits:**
- Sharing needs an IC via already butted against the capacitor pad (existing, or planned earlier in the run); the planner does not steer an IC pin's via towards its capacitor, so with the default it shares rarely (0 pads on these boards).
- The cost weights, the 15 mm IC search and the "smaller than 0603" test (two pads, courtyard ≤ 2.4 mm) are Konnect judgements; the courtyard excess values are IPC-7351B nominal as encoded by KiCad's footprint generator, not read from IPC-7351B itself or from KLC F5.3 (both unreachable here).
- Pad identity uses the file's pad UUID; a board without pad UUIDs falls back to name and centre (3 decimals).
- Apply over live IPC is still not exercised against KiCad; the handler tests cover argument handling, the saved-file dry run, response assembly and the IPC item list.
