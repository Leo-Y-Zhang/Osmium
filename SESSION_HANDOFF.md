# Session handoff

**State: v0.3.0 — M6 (ring 3) + M7 (ELF loader) + M8 (preemptive
multitasking) + M9 (per-task address spaces), hardened.** M8 and M9 both
landed 2026-08-18. Nothing is in flight and nothing is owed.

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

## What M9 added (same day as M8)

- **`memory::AddressSpace`** — per-task PML4: every kernel subtree shared,
  the entry-0 chain deep-copied so the user window's and stack's 2 MiB PD
  slots are private per task. The low-half contents this rests on were
  MEASURED, not assumed (the bootloader leaves an identity-mapped handover
  region at `0x0..0x200000` and its early-GDT region at `0x1000000..0x1200000`
  — both stay shared; `new_user` asserts the user slots are vacant
  kernel-side). A first draft assumed entry 0 was empty and the assert caught
  it on the first boot — the measure-then-build lesson, again.
- **CR3 switched with RSP0** at every context switch (`sched::load_cr3`),
  restored to the kernel's root when the last task exits. Same-VA programs
  now coexist: the battery runs hello twice at one VA; hello's exit code
  carries the data-segment value it read, so a shared page is caught as
  instance 1 reading `0x46` ('F', the other instance's write) instead of the
  pristine `0x45`. Observed failing under three mutations (TDD test plan).
- **The kernel-table audit is now total**: not one user-accessible entry,
  leaf or intermediate, ever — user mappings exist only in task spaces, and
  teardown is dropping the spaces (nothing to unmap in the kernel table).
- The M8 cross-image overlap refusal was REMOVED (same-VA is the point now);
  `kshared::plans_overlap` and its host tests went with it. The user stack is
  one VA (`0x80_0000`) in every space; `USER_STACK_ADDRS` is gone.

## M8, for context (earlier the same day)

Preemptive round-robin of ring-3 tasks — naked timer entry saving the full
register file per task on its own kernel stack, TSS RSP0 retargeted per
switch, kernel non-preemptible, AC scrubbed at every ring-3-controlled entry.
Proven by exit order, an exact cross-checked checksum, and a hostile-AC
program; hardened same-day by a 15-agent adversarial review (6 findings
fixed, each observed failing first — including every `debug_assert!` being
dead code in this release-only repo, and a ~50%-flaky exit-order assertion
that CI passed once by luck).

## Roadmap (PRD Won't-list, unchanged)

Per-task fault isolation (a crashing program still takes the machine down —
tasks are memory-isolated, not fault-isolated), ramfs, APIC/HPET, SMP.
Networking is on no roadmap. The console scroll cell-grid rework stays
deferred with its measured number in the TDD.
