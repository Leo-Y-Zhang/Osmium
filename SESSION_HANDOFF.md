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

- **State:** baseline `cc0da17` (M6 complete, CI green, tree clean).
- **In flight:** Phase 1 — a nine-lens critique fleet + adversarial judge is
  producing the ranked overnight work program (workflow `wf_bdc72a21-b6a`).
- **Next action:** read the judge's ranked plan, then execute Wave 1
  (correctness/security/CI fixes), Wave 2 (major milestone(s)), Wave 3
  (hardening/polish/docs) in order, each item test-first with its killing
  mutation observed, full local gates, commit+push, CI verify.
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
