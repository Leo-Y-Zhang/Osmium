# Osmium

A small operating system for x86_64, written from scratch in Rust — **private by
construction, light by measurement**.

[![CI](https://github.com/Leo-Y-Zhang/Osmium/actions/workflows/ci.yml/badge.svg)](https://github.com/Leo-Y-Zhang/Osmium/actions/workflows/ci.yml)

![The Osmium shell answering `help` and `privacy`](docs/screenshot.png)

Osmium boots from BIOS or UEFI, renders its own glyph console on the framebuffer,
takes keyboard input through an async executor, and drops you at a shell — inside
24 MiB of RAM. Every push to `main` must boot in QEMU under CI and pass an
in-kernel self-test battery; `main` is never un-bootable.

## Privacy by construction

Claims the code cannot violate, each with a named enforcement mechanism rather
than a policy:

- **No network stack exists.** Nothing can phone home — the capability is
  absent, not disabled. Enforced by CI's `no-network-no-storage` gate, which
  greps the kernel sources, manifests and the resolved dependency graph on
  every push, so neither hand-written code nor a transitive crate can creep in.
- **No persistence.** RAM-only live system: no disk writes, no filesystem; a
  cold boot is a clean slate. The same CI gate covers storage drivers.
- **Freed memory is zeroed.** Heap blocks are scrubbed before they re-enter the
  free list, and physical frames are scrubbed as they are handed out — verified
  by two boot-time sentinel self-tests that were each observed failing when the
  scrubbing was deliberately removed.
- **Keystrokes stay on-screen.** Input renders to the local console only; it
  never reaches the serial port. This one holds by construction — the input
  path contains no serial writes, and the `panic` command's message is fixed in
  the source precisely so typed text has no route there.

The `privacy` command reports these live.

## Light by measurement

- Boots and passes the full battery with a measured floor of **21 MiB** of RAM
  on BIOS and **46 MiB** on UEFI (the difference is OVMF's own footprint, not
  the kernel's). CI boots at 24 and 48 MiB — floor plus a small, recorded
  headroom for QEMU-version variance — so a real RAM-hunger regression fails
  the build. The boot test also cross-checks the kernel's own memory-map
  report against the configured size, so the figure is measured, not quoted.
- Disk images are **2.1–2.5 MiB**, asserted under a 4 MiB budget on every build.
- The shell is up about **20 ms after interrupts enable** — CI parses that
  figure from the boot log and fails above a 200 ms ceiling (PIT-measured, shown
  in the banner — the timer cannot see the boot stages before it starts, so
  that is the honest span).

## What's inside

- BIOS + UEFI boot via the [`bootloader`](https://github.com/rust-osdev/bootloader)
  crate — both images built by `cargo xtask`, no external imaging tools
- A framebuffer glyph console (Noto Sans Mono bitmap) that honours the
  negotiated pixel format; CI boots both firmwares, which negotiate different
  layouts, so format assumptions cannot survive
- GDT/TSS with a dedicated IST stack: a kernel stack overflow lands in the
  double-fault handler and says so instead of triple-faulting — and a self-test
  proves it on every push
- IDT, remapped PIC, 100 Hz PIT, PS/2 keyboard IRQ feeding a lock-free scancode
  queue (interrupt handlers never take a lock or allocate)
- Physical frame allocator over the boot memory map, offset page table, and a
  1 MiB kernel heap
- A cooperative async executor (waker-based); the keyboard stream and the shell
  are async tasks, and an idle Osmium sits in `hlt`
- The shell: `help`, `echo`, `clear`, `mem`, `uptime`, `sysinfo`, `privacy`,
  `keymap` (us/uk), `selftest`, `panic`, `shutdown` — with line editing and
  arrow-key history
- Panic screens that report the failure on both the console and the serial
  port; never a silent hang

## Testing

Three layers, all run by CI on every push:

- **In-QEMU battery** (`cargo xtask test`): boot, console rendering, `int3`
  handling, PIT ticks, heap allocation, the zero-on-free sentinel, the
  fresh-page scrub (the frame is deliberately soiled first, so the test proves
  the kernel's zeroing rather than the emulator's), the executor waker path,
  and stack-overflow-to-double-fault — on BIOS *and* UEFI at the pinned
  minimal RAM sizes.
- **Shipped-image boot** (`cargo xtask test --shipped`): the exact image a user
  would boot must reach the shell.
- **Host tests** (`cargo test -p kshared`): the line editor, command parser and
  frame-range arithmetic are pure logic and are tested natively.

Every battery test has been observed failing at least once via deliberate
mutation — delete the heap scrub, delete the frame scrub, break the waker,
remove the IST stack, gut the `int3` handler — because a test never seen
failing is decoration. That discipline caught a real bug here: LLVM optimised
the original sentinel test to nothing (dead-store elimination plus undef-read
folding), so it kept passing with the scrubbing deleted. The checks are now
volatile on both sides.

## Building and running

You need Rust via rustup (the pinned toolchain in `rust-toolchain.toml`
installs itself on first build) and QEMU on PATH — or point `OSMIUM_QEMU` at
the binary.

```
cargo xtask build            # BIOS + UEFI disk images into target/img/
cargo xtask run              # boot the BIOS image in a QEMU window
cargo xtask run --uefi       # boot via UEFI (OVMF firmware fetched automatically)
cargo xtask test             # the CI gate: headless boot + self-test battery
cargo xtask test --shipped   # boot the real image to the shell
```

The kernel uses exactly one unstable feature (`abi_x86_interrupt`, required for
IDT handlers); a CI gate fails the build if any other nightly feature creeps
in. The image-build tooling itself needs the pinned nightly because the
bootloader's build system uses `-Zbuild-std`.

## Real hardware

Osmium is proven on both firmwares under QEMU 11.1; it has **not** yet been
validated on a physical machine, and the honest position is worth stating
rather than implying otherwise. To try it on metal, write a raw image to a USB
stick (`dd if=target/img/osmium-bios.img of=/dev/sdX bs=4M` — pick the right
device) and boot it in legacy/CSM mode. Known caveats before anyone does:

- **Input is PS/2** (port `0x60`, IRQ 1). A USB keyboard needs the firmware's
  legacy PS/2 emulation; a machine in pure-UEFI mode with that disabled will
  likely have a dead keyboard, though the boot and self-tests are unaffected.
- **`shutdown` halts rather than powering off** on hardware without the QEMU
  debug-exit port — it says so and stops, as the App Flow documents.
- Scroll performance is untested on real, uncached framebuffers.

If you boot Osmium on a real machine, record the make, firmware and what
happened — that observation is worth more than any amount of emulator CI.

## Roadmap (deliberately not in v1)

Ring 3 + syscalls, ELF loading, a RAM-disk filesystem, preemptive scheduling,
APIC/HPET, SMP. Networking is on no roadmap; if it ever lands, it ships off by
default.

## Try it without building

Tagged releases carry prebuilt BIOS and UEFI images that were **boot-proven in
CI as the exact uploaded bytes**, with `SHA256SUMS` to verify them. Download
`osmium-bios.img` from the [releases page](https://github.com/Leo-Y-Zhang/Osmium/releases)
and:

```
qemu-system-x86_64 -drive format=raw,file=osmium-bios.img -m 24M
```

## Documents

- [ARCHITECTURE.md](docs/ARCHITECTURE.md) — a code-anchored walk from firmware
  to the shell prompt, with the keystroke dataflow diagram.
- [PRD.md](docs/PRD.md) — scope, success criteria, and the deliberate non-goals.
- [TDD.md](docs/TDD.md) — the memory map, global-state table, and the inventory
  of every `unsafe` block with the invariant that makes it sound.
- [APP_FLOW.md](docs/APP_FLOW.md) — the command surface and every screen state.
- [DESIGN_BRIEF.md](docs/DESIGN_BRIEF.md) — the visual intent and colour roles.

Third-party licenses (including the embedded Noto font's OFL notice) are in
[THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md).

## License

MIT — see [LICENSE](LICENSE).
