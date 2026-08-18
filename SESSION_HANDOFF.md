# Session handoff

**State: v0.1.0 + M6 (ring 3) + M7 (ELF loader), hardened.** An autonomous
overnight run (17→18 Aug) took Osmium through a full critique → judge →
implement → adversarial-verify pass on the operator's request. Nothing is in
flight and nothing is owed.

- All milestones and the overnight work are merged, pushed and CI-green on
  `main`. The four engineering documents in `docs/` are reconciled to the
  shipped code; treat any code-vs-doc drift as a defect in whichever is wrong.
- Build, run and test commands are in the README (`cargo xtask ...`). On a
  machine without QEMU on PATH, point `OSMIUM_QEMU` at the binary.
- Local gate = `cargo fmt --all --check`, clippy (both feature configs),
  `cargo test -p kshared -p xtask`, and `cargo xtask test --bios|--uefi|--shipped`
  plus `cargo xtask privacy`. Note: `cargo test -p xtask` needs `--release`
  locally (Smart App Control blocks the dev-profile bootloader build script);
  CI on ubuntu is unaffected. Force a fresh clippy (`touch` changed files) —
  cached results can hide a lint CI's fresh run catches.

## What M7 and the overnight run added

- **M7 ELF loader.** `user/hello` is a real linker-scripted Rust ELF parsed by
  host-tested `kshared::elf` (refusal-by-default), mapped per-segment W^X, run
  at CPL 3. `kernel/build.rs` builds and embeds it.
- **SMEP/SMAP/UMIP** (`kernel/src/cpu.rs`), CPUID-gated and CR4-readback-proven;
  SMAP made genuinely active via a physical-alias copy; the syscall gate scrubs
  `EFLAGS.AC` so ring 3 cannot disable SMAP.
- NX heap; an order-independent page-table audit that re-runs after teardown;
  a console pixel-readback selftest; tab completion; a no-persistence image-hash
  CI gate; a keystroke-privacy allowlist; release gating; a hardware-report
  funnel; and a full docs + screenshot refresh.

## Roadmap (PRD Won't-list, unchanged)

ramfs, preemptive scheduling, APIC/HPET, SMP. Networking is on no roadmap. The
console scroll cell-grid rework stays deferred with its measured number in the
TDD.
