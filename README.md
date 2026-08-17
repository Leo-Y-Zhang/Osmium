# Osmium

A small operating system for x86_64, written from scratch in Rust — **private by
construction, light by measurement**.

[![CI](https://github.com/Leo-Y-Zhang/Osmium/actions/workflows/ci.yml/badge.svg)](https://github.com/Leo-Y-Zhang/Osmium/actions/workflows/ci.yml)

![The Osmium shell answering `help` and `privacy`](docs/screenshot.png)

Osmium boots from BIOS or UEFI, renders its own glyph console on the framebuffer,
takes keyboard input through an async executor, and drops you at a shell — in
about 20 ms, inside 24 MiB of RAM. Every push to `main` must boot in QEMU under
CI and pass an in-kernel self-test battery; `main` is never un-bootable.

## Privacy by construction

Claims the code cannot violate, each enforced by a boot-time self-test rather
than by policy:

- **No network stack exists.** Nothing can phone home — the capability is
  absent, not disabled.
- **No persistence.** RAM-only live system: no disk writes, no filesystem; a
  cold boot is a clean slate.
- **Freed memory is zeroed.** Heap blocks are scrubbed before they re-enter the
  free list, and physical frames are scrubbed as they are handed out — verified
  by sentinel tests that were each observed failing when the scrubbing was
  deliberately removed.
- **Keystrokes stay on-screen.** Input renders to the local console only; it
  never reaches the serial port.

The `privacy` command reports these live.

## Light by measurement

- Boots and passes the full battery in **24 MiB** of RAM (BIOS) or **48 MiB**
  (UEFI — the difference is OVMF's own footprint, not the kernel's). CI runs at
  exactly these values, so a RAM-hunger regression fails the build.
- Disk images are ~**2.5 MiB**, asserted under a 4 MiB budget on every build.
- Boot-to-shell in roughly **20 ms** under QEMU (PIT-measured, shown in the
  banner).

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

## Roadmap (deliberately not in v1)

Ring 3 + syscalls, ELF loading, a RAM-disk filesystem, preemptive scheduling,
APIC/HPET, SMP. Networking is on no roadmap; if it ever lands, it ships off by
default.

## Documents

The PRD, TDD, App Flow and Design Brief live in [docs/](docs/) — including the
TDD's inventory of every `unsafe` block and the invariant that makes it sound.

## License

MIT — see [LICENSE](LICENSE).
