# Session handoff

## AUTONOMOUS OVERNIGHT MODE — 2026-08-17 night → 2026-08-18 12:00

An overnight improvement run is in progress on the operator's direct request
("critique it, improve it, refine it, tune it, secure it, optimise it,
beautify it, perfect it"). Deadline: everything committed, pushed, CI-green
and reported by 12:00 on 18 Aug.

**Resume protocol for any fresh session:** read this banner, `git log --oneline -10`,
and the task list; continue from "Next action" below. Full local gate =
`cargo fmt --check` + clippy (both feature configs) + `cargo test -p kshared` +
`cargo xtask test --bios`, `--uefi`, `--shipped`. Every green increment is
committed and pushed immediately; CI is the gate, not local success.

- **State:** Ranks 1-11, 16-subset, 19, 21 all landed, pushed, CI-green.
  Wave 1 (CI gate hardening, privacy allowlist, sentinel skip, page-permission
  cluster). Wave 2: M7 ELF loader (`user/hello`, host-tested `kshared::elf`).
  Wave 3 so far: rank 6 SMEP/SMAP/UMIP, rank 7 pixel-readback probe, rank 8
  no-persistence hash gate, rank 9 docs sweep + top-up, rank 11 shell
  discrimination + chords, rank 19 xtask host tests, rank 21 hardware funnel,
  rank 16 cleanups. All three worktrees merged and removed. Every mutation
  observed red then reverted; every gate run with pipefail.
  ⚠ Local `cargo test -p xtask` needs `--release` (Smart App Control blocks
  the dev-profile bootloader stage-4 build script); CI (ubuntu) is unaffected.
- **Next action:** remaining Wave-3 as time allows, in value order: rank 20
  (image budget 3 MiB), rank 14 (boot-perf fill_rows + mix early-outs, uses
  rank 7's accessor), rank 12 (shell output consistency + format_uptime host
  test), rank 10 (release/CI hardening), rank 13 (screenshot refresh — LAST,
  after help text is final), then optional 15/17/18. Then Phase 3 adversarial
  verification, then closure from 11:00.
- **Standing constraints:** networking on NO roadmap ever; console scroll rework
  stays DEFERRED (measured ~87 ms/100 scrolls under TCG, needs real-hardware or
  KVM data first); the four privacy properties are inviolable; exactly one
  unstable feature (`abi_x86_interrupt`); everything must build on a
  non-administrator Windows box via `cargo xtask` alone.

## Baseline (pre-overnight state)

**v0.1.0 + M6 complete.** Milestones M0–M6 merged, pushed, CI-green on `main`;
adversarial-test, security-review and release audits all run and their findings
fixed. The four engineering documents in `docs/` were reconciled to the shipped
code as of M6; treat code-vs-doc drift as a defect in whichever is wrong.

- Build, run and test commands are in the README (`cargo xtask ...`). On a
  machine where QEMU is not on PATH, point `OSMIUM_QEMU` at the
  `qemu-system-x86_64` binary.
- The RAM floors (21 MiB BIOS / 46 MiB UEFI) are dated manual measurements on
  QEMU 11.1, recorded in `xtask/src/main.rs`; re-measure before tightening the
  CI gates.
