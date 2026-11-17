# fanout_regress2

The second fan-out / native-export regression board. It was built offline with
pcbnew's Python API by `poc/regress2/build_regress2.py` (KiCad 10.0.8), and its
DSN is KiCad's own `ExportSpecctraDSN`, unmodified.

It covers what the first PoC board did not:
- an L-shaped outline with a rounded (arc) inner corner;
- inner GND/+3V3 planes on layers typed `signal`;
- an LQFP-48 at 0.5 mm pitch (STM32F072 power pins);
- a QFN-32 rotated 45° with a 3.45 mm exposed pad;
- 0603 decoupling capacitors on the back side, rotated 45°/135°;
- an ESP-12E module whose footprint carries an antenna keep-out;
- a board-level keep-out rule area.

Zones are filled. `kicad-cli pcb drc` reports only the expected unconnected
items and 2 isolated-copper warnings.
