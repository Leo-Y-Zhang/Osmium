//! Builds the `hello` user program (a standalone crate in `user/hello`) and
//! hands its ELF path to the kernel via the `HELLO_ELF` env var, which
//! `usermode.rs` embeds with `include_bytes!`.
//!
//! The nested cargo gets its own CARGO_TARGET_DIR: sharing the workspace
//! target dir with the invoking build would deadlock on cargo's build lock.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let hello_dir = manifest_dir
        .parent()
        .expect("kernel lives one level below the workspace root")
        .join("user")
        .join("hello");
    for tracked in ["src/main.rs", "Cargo.toml", "link.ld"] {
        println!(
            "cargo:rerun-if-changed={}",
            hello_dir.join(tracked).display()
        );
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let target_dir = out_dir.join("hello-target");
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let link_script = hello_dir.join("link.ld");
    let status = Command::new(&cargo)
        .arg("build")
        .arg("--release")
        .args(["--target", "x86_64-unknown-none"])
        .arg("--manifest-path")
        .arg(hello_dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", &target_dir)
        // Static relocation + the fixed-base linker script give an ET_EXEC
        // image whose first segment sits exactly at the user window base.
        .env(
            "RUSTFLAGS",
            format!(
                "-Crelocation-model=static -Clink-arg=-T{}",
                link_script.display()
            ),
        )
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        // A `cargo clippy` of the kernel must BUILD the user program, not
        // lint it: strip clippy's wrapper env or the nested build inherits
        // -D warnings through clippy-driver and fails the kernel's lint run
        // on the user crate's account.
        .env_remove("RUSTC_WRAPPER")
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("CLIPPY_ARGS")
        .status()
        .expect("spawning cargo to build the hello user program");
    assert!(status.success(), "building the hello user program failed");

    let elf = target_dir.join("x86_64-unknown-none/release/hello");
    assert!(elf.is_file(), "hello ELF missing at {}", elf.display());
    println!("cargo:rustc-env=HELLO_ELF={}", elf.display());
}
