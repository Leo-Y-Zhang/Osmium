# Real-hardware reports

**Date:** 2026-08-18 · **README:** [Real hardware](../README.md#real-hardware) · **App Flow:** [APP_FLOW.md](APP_FLOW.md)

Osmium is proven on both firmwares under QEMU 11.1 and has not been validated on
a physical machine. One report from real metal is worth more than any amount of
emulator CI, because every claim below the framebuffer is currently a claim about
one emulator.

File one with the [real-hardware boot report form](https://github.com/Leo-Y-Zhang/Osmium/issues/new?template=hardware-report.yml).
A failed boot is as useful as a successful one; a photo of the screen is enough.

## Known caveats

The same three the README states, because they decide what a report will look
like:

- **Input is PS/2** (port `0x60`, IRQ 1). A USB keyboard needs the firmware's
  legacy PS/2 emulation; a machine in pure-UEFI mode with that disabled will
  likely have a dead keyboard, though the boot and self-tests are unaffected.
- **`shutdown` halts rather than powering off** on hardware without the QEMU
  debug-exit port — it says so and stops, as the App Flow documents.
- Scroll performance is untested on real, uncached framebuffers.

## Results

| Machine | Firmware | RAM | Image | Outcome | Notes |
|---|---|---|---|---|---|

*No reports yet. Yours would be the first.*
