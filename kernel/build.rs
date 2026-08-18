//! Builds the user programs (standalone crates in `user/`) and hands their
//! ELF paths to the kernel via env vars (`HELLO_ELF`, `COUNTER_ELF`), which
//! `usermode.rs` embeds with `include_bytes!`.
//!
//! Each nested cargo gets its own CARGO_TARGET_DIR: sharing the workspace
//! target dir with the invoking build would deadlock on cargo's build lock.

use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    build_user_program("hello", "link.ld", "HELLO_ELF");
    build_user_program("counter", "link.ld", "COUNTER_ELF");
    build_user_program("crasher", "link.ld", "CRASHER_ELF");
    // The same counter source at a second base (see link_alt.ld): the battery
    // schedules two unyielding programs against each other, and linking one
    // source twice is how it gets two of them. Built unconditionally (a few
    // KiB); only the selftest build embeds it.
    build_user_program("counter", "link_alt.ld", "COUNTER_ALT_ELF");
}

fn build_user_program(name: &str, link_script_name: &str, env_var: &str) {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let crate_dir = manifest_dir
        .parent()
        .expect("kernel lives one level below the workspace root")
        .join("user")
        .join(name);
    for tracked in ["src/main.rs", "Cargo.toml", link_script_name] {
        println!(
            "cargo:rerun-if-changed={}",
            crate_dir.join(tracked).display()
        );
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    // The target dir is keyed on the env var, not just the crate: the same
    // crate built under two linker scripts must not share build caches.
    let target_dir = out_dir.join(format!("{}-target", env_var.to_lowercase()));
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let link_script = crate_dir.join(link_script_name);
    let status = Command::new(&cargo)
        .arg("build")
        .arg("--release")
        .args(["--target", "x86_64-unknown-none"])
        .arg("--manifest-path")
        .arg(crate_dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", &target_dir)
        // Static relocation + the fixed-base linker script give an ET_EXEC
        // image whose segments sit at that program's slot in the user window.
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
        .unwrap_or_else(|e| panic!("spawning cargo to build the {name} user program: {e}"));
    assert!(status.success(), "building the {name} user program failed");

    let elf = target_dir.join(format!("x86_64-unknown-none/release/{name}"));
    assert!(elf.is_file(), "{name} ELF missing at {}", elf.display());
    println!("cargo:rustc-env={env_var}={}", elf.display());
}
