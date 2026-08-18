# Session handoff

**State: v0.1.0 + M6 (ring 3) + M7 (ELF loader) + M8 (preemptive multitasking),
hardened.** M8 landed 2026-08-18: the PIT tick now drives a round-robin context
switch between ring-3 tasks. Nothing is in flight and nothing is owed.

- All milestones are merged, pushed and CI-green on `main`. The four
  engineering documents in `docs/` are reconciled to the shipped code; treat
  any code-vs-doc drift as a defect in whichever is wrong.
- Build, run and test commands are in the README (`cargo xtask ...`). On a
  machine without QEMU on PATH, point `OSMIUM_QEMU` at the binary.
- Local gate = `cargo fmt --all --check`, clippy (both feature configs),
  `cargo test -p kshared -p xtask`, and `cargo xtask test --bios|--uefi|--shipped`
  plus `cargo xtask privacy`. Note: `cargo test -p xtask` needs `--release`
  locally (Smart App Control blocks the dev-profile bootloader build script);
  CI on ubuntu is unaffected. Force a fresh clippy (`touch` changed files) —
  cached results can hide a lint CI's fresh run catches.

## What M8 added

- **`kernel/src/sched.rs`** — TCBs with per-task heap-allocated kernel stacks,
  a naked timer entry (vector 32, installed by address) that saves the full
  register file and scrubs `EFLAGS.AC`, a round-robin `timer_tick` that only
  ever preempts CPL-3 contexts (the kernel stays non-preemptible), `SYS_EXIT`
  routed through the scheduler, and TSS RSP0 retargeted at every switch
  (`gdt::set_privilege_stack`; the TSS now lives in an `UnsafeCell`).
- **`user/counter`** — an unyielding 30M-iteration checksum program linked at
  the upper half of the user window; holds AC set for its whole run. The
  battery launches it first, `hello` second, and asserts hello exits FIRST
  (the preemption proof), the checksum is bit-exact across every switch
  (register-integrity proof, recomputed independently kernel-side — the two
  loops must stay in step), and the timer AC scrub held. All three named
  mutations were observed failing (TDD test plan records them).
- **`run_programs`** — the multi-program loader; refuses cross-image page
  overlap before mapping anything. `user` is now the one-task degenerate case.
- The `sched` shell command demonstrates it live (dots + hello's byte
  interleave); `cargo xtask privacy` types it, so the serial-silence allowlist
  covers the scheduler path; CI's input-path grep gate covers `sched.rs`.

## Same-day adversarial review (15-agent fleet), 6 findings fixed

Three lenses (security / concurrency / test-strength) + a skeptic per finding;
6 of 12 findings survived and all are fixed, each fix observed failing first:
every M8 `debug_assert!` was dead code (the kernel only ever builds
`--release`; all seven are real `assert!`s now — repo rule: a `debug_assert`
here proves nothing); the battery could not see a rotate-once scheduler (new
sustained-rotation proof: the same counter linked at two bases scheduled
against itself, exit order + ≥4-switch floor); `SAVED_CS_OFFSET` was asserted
by comment (new always-on CS-selector tripwire on every tick); the cross-image
overlap check was only ever shown identical images (predicate moved to
`kshared::elf::plans_overlap` with partial-overlap host tests). The CPL-3
gate remains deliberately untestable-as-false (documented in the TDD — while
the scheduler is active no kernel path runs with IF=1).

## Roadmap (PRD Won't-list, unchanged)

Per-task address spaces (tasks are isolated from the kernel, not yet from each
other), ramfs, APIC/HPET, SMP. Networking is on no roadmap. The console scroll
cell-grid rework stays deferred with its measured number in the TDD.
