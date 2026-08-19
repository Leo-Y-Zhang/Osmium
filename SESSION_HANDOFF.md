# Session handoff

**State: v0.6.0, and the project is deliberately CLOSED here.** M6 (ring 3)
+ M7 (ELF loader) + M8 (preemptive multitasking) + M9 (per-task address
spaces) + M10 (fault isolation) + M11 (frame reclamation) + M12 (a RAM-only
filesystem), all hardened. M8-M11 landed 2026-08-18, M12 on 2026-08-19.
Nothing is in flight and nothing is owed.

## Why it stops here, and what would actually be worth doing

The roadmap's remaining items are **APIC/HPET** and **SMP**. Both were
considered and both were judged poor value for this project: they are large,
mostly plumbing, and — the deciding argument — they add little that a CI job
can *falsify*. This repo's thesis is machine-checked claims, not feature
count, so more surface with no new proof is a cost rather than a gain.
Networking is on no roadmap, ever, and that is a thesis decision.

**The one genuinely valuable next step needs hardware, not code:** every
claim here is proven under QEMU. Writing a release image to a USB stick and
booting a real x86_64 machine would add something no further commit can. The
report form and `docs/HARDWARE.md` are already in place for exactly that.

Accepted, deliberate limits (do not "fix" these without re-reading why):
a faulting *kernel* is still fatal by design; there is one core; the shell is
compiled in; and the filesystem's capacities are small on purpose, chosen so
that every refusal it can return is reachable from the shipped prompt.

- **M10:** every fault handler forks on the faulting CPL — ring 3 goes to
  `sched::kill_current`, which terminates that task alone and resumes the
  next (or returns to the launcher); CPL 0 still panics, and NMI/#MC/#DF
  panic at any CPL. The `crash` command demonstrates it.
- **M11:** frames are reclaimed. The allocator has a free list, scrubbing
  both at free time and again at hand-out, and each `AddressSpace` records
  every frame it pulls and returns exactly that set on drop.
- **M12:** `kshared::ramfs` — a flat, allocator-free namespace over a fixed
  arena; `ls`/`write`/`cat`/`rm`. Deleting scrubs and compacts. The privacy
  gates were extended over it rather than around it.
- ⚠ **Toolchain:** the pin must stay on an LLVM-**22** nightly
  (`nightly-2026-08-05`). LLVM 23 raises a `wcslen` libcall the UEFI
  bootloader cannot link, which breaks CI as well as local builds.

- All milestones are merged, pushed and CI-green on `main`. The four
  engineering documents in `docs/` are reconciled to the shipped code; treat
  any code-vs-doc drift as a defect in whichever is wrong.
- **The published v0.6.0 images were verified independently of CI** on
  2026-08-19: downloaded from the releases page, checksummed against the
  published `SHA256SUMS`, and booted on both firmwares with
  `cargo xtask test --shipped --bios|--uefi --image=<path>`. That flag pair
  matters — `--image=` alone waits for a *selftest* verdict the shipped image
  never emits, and looks like a hang.
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
