# PRD — Osmium v1: a bootable x86_64 operating system

**Status:** agreed
**Date:** 2026-08-17 · **Repo:** `Leo-Y-Zhang/Osmium` · **Related:** [TDD](TDD.md), [App Flow](APP_FLOW.md), [Design Brief](DESIGN_BRIEF.md)

## Problem

Most from-scratch operating systems are unfalsifiable. The repository contains a
kernel, the README contains a screenshot, and there is no way for a reader — or for
the author six months later — to establish that the thing still boots, on which
firmware, or with how much memory. The build usually needs a Linux host, root, or a
toolchain that has since rotted. Osmium exists to be the opposite: an OS that
proves its own claims on every push, builds on an ordinary non-administrator
Windows machine with no system-wide installs, and states nothing in prose that a
continuous-integration job does not check. Two further properties are requirements
rather than nice-to-haves, because they are the difference between a demo and a
design: it must be **private by construction**, and **light by measurement**.

## Who it is for

Three readers, in order of how often they are inconvenienced.

1. **The author, at a terminal on a non-administrator Windows machine.** No admin
   rights, no MSI installers, no WSL dependency. Two commands must take a clean
   clone to a booted kernel: install the pinned toolchain, then `cargo xtask run`.
   If the loop from edit to booted shell is slower than about a minute, the project
   stops being worked on.
2. **A reader evaluating the repository.** Arrives with no context, clones, and
   wants to know within five minutes whether this is real. They read the CI badge,
   run `cargo xtask test`, and see a kernel boot and self-test on their own machine.
3. **Continuous integration**, which is a user with no eyes. Every claim in the
   README must be reducible to a process exit code and a string in a serial log,
   because CI cannot look at a screen.

## Success looks like

- [x] CI on `main` boots the kernel under QEMU on **both BIOS and UEFI firmware**,
      at the pinned minimal RAM size, and both jobs are green.
- [x] Each boot job asserts **two independent signals**: the QEMU exit code is 33
      (the kernel wrote `0x10` to the `isa-debug-exit` port) **and** the captured
      serial log contains `SELFTEST PASSED`. Neither alone is accepted.
- [x] A separate CI job boots the **shipped** image — the default build, without the
      self-test feature — and asserts it reaches the interactive prompt. The binary
      a reader runs is boot-proven, not merely its test-mode sibling.
- [x] Booting on a real screen gives an **interactive framebuffer shell**: typed
      characters appear, backspace and line editing work, history recalls previous
      lines, and every command in the surface below returns output or a stated error.
- [x] The **in-kernel self-test battery** passes, covering: console rendering;
      breakpoint exception returns to the interrupted instruction; timer ticks
      advance; heap allocation, a 50,000-element vector, and allocator reuse
      after free; a freshly mapped, deliberately pre-soiled page reads as zero;
      **zero-on-free leaves no caller data recoverable**; a spawned async task
      provably runs; and, last, a deliberate kernel stack overflow is caught as a
      double fault on its own interrupt stack rather than tripling to a reset.
- [x] **Zero-on-free is verified by the battery, not asserted in prose**: a
      sentinel pattern written into a heap block is absent from memory handed back
      by the next allocation of that block.
- [x] Both disk images are **under the asserted size budget**, checked on every
      build; exceeding it fails the build rather than printing a warning.
- [x] The minimal RAM figure and the size budget in CI are **measured values**, each
      pinned just above the observed floor, with the measurement recorded in the
      repository.
- [x] **CI greps for networking and storage on every push.** The
      `no-network-no-storage` job scans `kernel/src`, `kshared/src` and
      `kernel/Cargo.toml` for the tokens either would have to bring with it and fails
      on a hit, so neither a crate nor a hand-written driver can enter the tree
      unnoticed.
- [x] `README.md` claims nothing that the above does not check.

## Requirements

**Must** — without these it does not ship.

- Boots on x86_64 under both BIOS and UEFI, from images built by `cargo xtask` with
  no external image tooling (no GRUB, no `xorriso`, no privileged mount).
- Framebuffer text console with scrolling, plus a serial console carrying kernel
  logs.
- A panic path that always renders: a panic screen carrying the message, its
  location and the register state, mirrored to serial. A silent hang is a defect,
  not a panic.
- GDT, TSS and IDT, with the double-fault handler on a dedicated interrupt stack so
  that a kernel stack overflow is survivable and observable rather than a reset.
- Physical frame allocator over the boot memory map, page-table access through the
  bootloader's physical-memory mapping, and a kernel heap.
- Keyboard input delivered from the interrupt handler to the shell without the
  interrupt handler ever taking a lock the shell can hold.
- Interactive shell with the command surface listed in the [App Flow](APP_FLOW.md).
- Self-test battery, feature-gated, ending in a QEMU exit code that encodes the
  verdict.
- **Privacy by construction** (see below). The three claims must be structural,
  not configurable.
- **Lightness by measurement**: the RAM floor and the image size are CI gates with
  numbers behind them.

**Should** — real value, cuttable under pressure.

- Boot-to-shell time measured from a timestamp counter calibrated against the timer,
  displayed in the banner.
- Command history and cursor movement within the line, not just backspace.
- ANSI colour handling in the console writer.
- Keyboard layout switching between US and UK.
- A screenshot in the README captured from a real boot rather than a mock-up.

**Won't (this time)** — written down so it is not silently rebuilt later. Each of
these is a coherent next project; none of them is in v1, and none of them should
appear in a v1 pull request.

- ~~**Ring 3 and system calls.**~~ **Delivered as M6 (2026-08-17).** The kernel now
  drops to ring 3 to run a user program on a user-only page, which returns to the
  kernel through a software-interrupt (`int 0x80`) system-call path; the battery
  proves the program executed in CPL 3 and that the kernel's mappings are not
  user-accessible.
- ~~**ELF loading.**~~ **Delivered as M7 (2026-08-18).** The user program is a
  real, linker-scripted Rust ELF64 (`user/hello`) parsed by a host-tested
  loader in `kshared` that refuses W+X segments, out-of-window addresses and
  malformed images; segments are mapped with per-segment W^X permissions. The
  shell is still compiled into the kernel.
- **Filesystems: no ramfs, no FAT32.** The bootloader's ramdisk facility is not
  used. Nothing is mounted.
- **Preemptive scheduling.** The v1 executor is cooperative: tasks yield, and the
  idle path halts until the next interrupt. There is no timer-driven context switch
  and no task priority.
- **APIC and HPET.** The legacy 8259 PIC and the 8254 timer are used instead. The
  APIC requires ACPI table parsing for no demonstrable v1 benefit; that cost is
  deferred until something needs it.
- **SMP.** One core is brought up. Application processors stay parked.
- **Networking.** No driver, no stack, no socket layer, not behind a flag, not in
  the dependency tree. If networking is ever added it ships **off by default**, is
  announced in the banner when on, and does not weaken any claim made here without
  that claim being rewritten first.

## Explicitly out of scope

- **Real hardware validation.** v1 is proven under QEMU with two firmwares. Booting
  a physical machine may work and is untested; the README will say untested rather
  than implying otherwise.
- **Hardware breadth.** One timer, one keyboard controller, one framebuffer format
  family negotiated at boot. No USB, no PCI enumeration, no storage controller, no
  sound, no ACPI power management beyond a best-effort shutdown attempt.
- **Cryptography.** Nothing is encrypted, because nothing is stored or transmitted.
  Adding a cipher would be machinery with no data to protect.
- **A GUI, windowing, or mouse input.** The framebuffer is a text console.
- **Multi-user anything.** There are no accounts, no sessions, no permissions.
  Whoever is at the keyboard is the operator of the machine.
- **Stability guarantees between milestones.** Internal interfaces change freely
  until v1 is tagged.

## Safety and privacy

**What personal data does this touch?** None, at any point. Osmium has no accounts,
no profile, no configuration file, no storage driver and no clock-synchronised
identity. The only data it ever holds is what someone types at its own keyboard
during a single power-on, and that data exists only in RAM.

The three privacy claims, stated as properties of the build rather than settings:

1. **No network stack exists.** Not disabled: absent. There is no driver, no
   protocol code, and no networking crate in the dependency tree. CI checks that on
   every push: the `no-network-no-storage` job greps the kernel and `kshared` sources
   and the kernel's manifest for the tokens a network or storage driver would have to
   carry, and a hit fails the build. It reads the source as well as the manifest, so
   the claim covers code written here, not just crates pulled in. Osmium cannot phone
   home for the same reason a hammer cannot: it has no mechanism.
2. **No persistence.** There is no filesystem and no storage driver, so the kernel
   cannot write to the medium it booted from. Osmium is a live system: a cold boot
   is a clean slate, every time, with no state to clear because none was kept.
3. **Freed memory is zeroed.** Every heap block is overwritten before it is returned
   to the allocator, and every physical frame is zeroed before it is first handed
   out. Data that has been freed cannot be observed by the next allocation. This is
   verified by the self-test battery, not claimed.

Two honest qualifications, stated here so the claims stay exactly true:

- **The serial port is an output channel.** Kernel logs are written to it by design;
  under QEMU they reach the host, and on real hardware they reach whatever is
  attached to the port. It is therefore a rule of this design that **keyboard input
  and shell input never reach the serial log**. The input path has no route to the
  serial writer, and the rule needs no exception clause: the one command that could
  have carried typed text there, `panic`, takes no message argument and panics with a
  string fixed in the source. Kernel events are logged; what a person types is not.
- **Firmware is outside our control.** UEFI implementations write their own NVRAM
  variables during boot. Osmium writes nothing; the firmware's own behaviour is not
  a claim this project can make.

**Who can see it, and who must not?** Whoever is physically at the machine. There is
no remote surface to defend, because there is no remote anything.

**What happens when someone's access is revoked?** Not applicable, and the reason is
worth stating rather than leaving blank: there are no accounts to revoke. The
analogue of revocation here is switching the machine off, and because there is no
persistence and freed memory is zeroed, power-off is total. Nothing survives it.

**What is the worst outcome if this is wrong?** A CI job fails, or a virtual machine
hangs and has to be closed. Nobody's data is exposed, because Osmium is never given
anybody's data. This project's failure mode is embarrassment, not harm, and that is
a deliberate consequence of the scope above; it is the reason the "won't" list is
as long as it is.

## Open questions

None that change this document. One technical question is open in the
[TDD](TDD.md) — whether the kernel keeps a strict zero-nightly-features rule once
interrupt handlers arrive — but it changes a build gate, not a requirement here,
and it blocks no work before milestone M2.

## Not doing / rejected alternatives

- **Limine or GRUB as the bootloader.** Both need `xorriso` or equivalent image
  tooling, which is hostile on a non-administrator Windows machine. The `bootloader`
  crate builds both BIOS and UEFI images from pure Cargo, and its own CI runs on
  Windows. Chosen for the environment it has to work in, not for its feature list.
- **`custom_test_frameworks` for in-kernel tests.** A nightly-only feature that
  would put a nightly dependency into the kernel source purely to run tests. The
  self-test battery, gated behind a Cargo feature and reporting its verdict through
  the QEMU exit code, gives the same coverage with a mechanism CI already trusts.
- **A microkernel or a rewrite in a different language.** Neither serves any stated
  requirement. The interesting properties of this project are the privacy claims and
  the CI proof; architecture novelty for its own sake would trade those for nothing.
- **Targeting `aarch64` or RISC-V as well.** Doubling the boot-proof matrix before
  a single firmware target is finished. x86_64 first; the second architecture is not
  a v1 question.
- **Shipping a disabled network stack behind a feature flag.** A flag can be turned
  on; absent code cannot. The stronger claim is worth more than the option.
