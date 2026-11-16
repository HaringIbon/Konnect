# specctra_native_4layer

A real round trip through KiCad 10.0.8 and Freerouting 2.1.0, used by
`specctra::native` tests.

- `specctra_native_4layer.kicad_pcb`: the proof-of-concept 4-layer ATmega328P
  board (50 × 40 mm, F.Cu signals, In1.Cu GND plane, In2.Cu +3V3 plane, B.Cu
  GND pour) with every track and via removed. Saved by KiCad 10.0.8. It is
  outside the strict exporter's profile: 4 layers, zones, roundrect and oval
  pads, arc outline corners, unplated mounting holes.
- `specctra_native_4layer.dsn`: `pcbnew.ExportSpecctraDSN` of that board, unmodified
  (it still contains the three `(plane …)` forms Konnect strips).
- `specctra_native_4layer.ses`: Freerouting 2.1.0's session for the DSN with the
  planes stripped (`java -jar freerouting-2.1.0.jar -de … -do …`, headless).
  Fully routed in 5 passes: 244 segments and 12 vias on F.Cu/B.Cu. It contains
  Freerouting 2.1's repeated `library_out` padstacks and repeated segments,
  and writes rotations as 270 where KiCad's DSN has -90.

With the planes left in, the same DSN stalls at 35 of 41 connections
unrouted after 999 passes.
