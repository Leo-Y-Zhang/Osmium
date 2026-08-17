# Session handoff

**State: v0.1.0 complete.** Milestones M0–M5 are merged, pushed and CI-green on
`main`; the adversarial-test, security-review and release audits have all run and
every finding is fixed. Nothing is in flight and nothing is owed.

- Build, run and test commands are in the README (`cargo xtask ...`). On a
  machine where QEMU is not on PATH, point `OSMIUM_QEMU` at the
  `qemu-system-x86_64` binary.
- The four engineering documents in `docs/` are reconciled to the shipped code;
  treat any future code-vs-doc drift as a defect in whichever is wrong.
- The RAM floors (21 MiB BIOS / 46 MiB UEFI) are dated manual measurements on
  QEMU 11.1, recorded in `xtask/src/main.rs`; re-measure before tightening the
  CI gates.
- Next work, if any, comes from the PRD's Won't list — ring 3 + syscalls is the
  natural first item. No partial branches exist.
