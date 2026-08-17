# Osmium

A small operating system for x86_64, written from scratch in Rust — **private by
construction, light by measurement**.

[![CI](https://github.com/Leo-Y-Zhang/Osmium/actions/workflows/ci.yml/badge.svg)](https://github.com/Leo-Y-Zhang/Osmium/actions/workflows/ci.yml)

**Status: v1 in development.** Every push to `main` must boot in QEMU under CI and pass
an in-kernel self-test battery — `main` is never un-bootable.

## Design pillars

- **Privacy by construction.** No network stack exists, so nothing can phone home. No
  telemetry. No persistence: Osmium is a RAM-only live system — cold boot is a clean
  slate. Freed memory is zeroed so no stale data survives reallocation.
- **Light by measurement.** CI boots the OS at a pinned minimal RAM size and asserts the
  disk image stays under a size budget. Both BIOS and UEFI images are built and
  boot-tested, so old hardware stays viable.
- **Honest engineering.** Self-tests run inside the booted kernel; the CI gate asserts
  the QEMU exit code and the serial log. What the README claims, CI proves.

## Building

Requires Rust (the pinned toolchain in `rust-toolchain.toml` installs automatically)
and QEMU (`qemu-system-x86_64` on PATH) to run.

```
cargo xtask build            # build BIOS + UEFI disk images into target/img/
cargo xtask run              # boot the BIOS image in QEMU
cargo xtask run --uefi       # boot the UEFI image (OVMF fetched automatically)
cargo xtask test             # headless boot + self-test battery (the CI gate)
```

## License

MIT — see [LICENSE](LICENSE).
