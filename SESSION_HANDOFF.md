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
- **Progress:** ranks 1-14, 16-subset, 17, 19-21 and 13 (screenshot) all
  landed, pushed, CI-green (screenshot run pending — it carries all of rank
  17 too). Only rank 15 (dedup zero-on-free into checks.rs) and rank 18
  (history into kshared) remain from the plan — both optional internal
  quality. ⚠ Lesson: local clippy uses cached results and MISSED a real
  question_mark error that CI caught (commit cf9a5.. → 695c1.. green); the
  "exit 101" I dismissed as build-lock races were real first-run clippy
  failures. Force a fresh clippy (touch changed files) and never dismiss a
  clippy 101.
- **In flight:** Phase 3 — a tester agent (adversarial: break the ELF parser,
  page audit, SMAP copy, completion) and a security-reviewer agent (ring-3 /
  ELF / syscall / SMAP surface) are analyzing the night's diff read-only. Hold
  code edits on selftest.rs/shell.rs/memory until they report; fix real
  findings, then optionally do 15/18.
- **Next action:** consume the two Phase-3 reports, fix any real finding
  (test-first, mutation-observed), then closure from ~11:00: full gates, CI
  green on final HEAD, gh api notifications (UNKNOWN if it errors, never 0),
  update project_osmium.md + MEMORY.md, final report.
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
