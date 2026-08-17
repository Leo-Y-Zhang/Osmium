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

- **State:** Wave 1 + Wave 2 landed and pushed, all CI-green:
  ranks 1-4 (CI grep-gate hardening; keystroke-privacy allowlist; sentinel
  16-byte skip; page-permission cluster incl. heap NX + order-independent
  user-mapping audit) and rank 5 (M7 ELF loader: `user/hello` real Rust ELF,
  host-tested `kshared::elf` parser, per-segment W^X). Every mutation observed
  red then reverted.
- **In flight:** three Wave-3 agents in worktrees off `5d7074f`:
  wt-xtask (rank 8 no-persistence hash gate + rank 19 xtask host tests),
  wt-docs (rank 9 docs reconciliation sweep), wt-funnel (rank 21 hardware
  report funnel). Main clone: doing rank 6 (SMEP/SMAP/UMIP) directly.
- **Next action:** finish rank 6; merge the three worktree branches in order
  (funnel, xtask, docs — docs last as it is the widest), gate each merge,
  push. Then remaining Wave-3 ranks 7/11/12/14 as time allows. Closure from
  11:00: full gates, CI green, gh api notifications, memory update.
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
