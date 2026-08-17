//! Build orchestration for Osmium: compiles the kernel for
//! `x86_64-unknown-none`, wraps it into bootable BIOS and UEFI disk images,
//! and drives QEMU both interactively (`run`) and headless for the CI boot
//! proof (`test`).

use anyhow::{Context, Result, bail};
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{env, fs, io, thread};

/// isa-debug-exit makes QEMU exit with `(value << 1) | 1`.
const QEMU_EXIT_SUCCESS: i32 = 33; // kernel wrote 0x10
const QEMU_EXIT_FAILURE: i32 = 35; // kernel wrote 0x11
const BOOT_TIMEOUT: Duration = Duration::from_secs(120);
/// Lightness gates: boot + selftest must fit in this much RAM.
/// Measured 2026-08-17 (QEMU 11.1): BIOS passes at 24 MB and fails at 20;
/// UEFI passes at 48 MB and fails at 40 — the difference is OVMF's own
/// footprint, not the kernel's. Pinned at the measured floors so RAM-hunger
/// regressions fail the gate.
const BIOS_TEST_MEM_MB: u32 = 24;
const UEFI_TEST_MEM_MB: u32 = 48;
/// Lightness gate: disk images must stay under this ceiling; growth is a
/// deliberate decision, never drift. Both images currently sit around
/// 2.1-2.5 MiB; 4 MiB leaves headroom without hiding a regression.
const IMAGE_BUDGET_BYTES: u64 = 4 * 1024 * 1024;

struct Opts {
    uefi: bool,
    selftest: bool,
    shipped: bool,
    mem_override: Option<u32>,
}

impl Opts {
    fn mem_mb(&self) -> u32 {
        self.mem_override.unwrap_or(if self.uefi {
            UEFI_TEST_MEM_MB
        } else {
            BIOS_TEST_MEM_MB
        })
    }
}

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    let mut opts = Opts {
        uefi: false,
        selftest: false,
        shipped: false,
        mem_override: None,
    };
    for arg in args {
        match arg.as_str() {
            "--uefi" => opts.uefi = true,
            "--bios" => opts.uefi = false,
            "--selftest" => opts.selftest = true,
            "--shipped" => opts.shipped = true,
            other => match other.strip_prefix("--mem=") {
                Some(mb) => {
                    opts.mem_override = Some(mb.parse().context("--mem=<MB> takes a number")?);
                }
                None => bail!("unknown argument: {other}"),
            },
        }
    }
    match cmd.as_str() {
        "build" => build_images(opts.selftest).map(|_| ()),
        "run" => run(&opts),
        "test" => test(&opts),
        _ => bail!(
            "usage: cargo xtask <build|run|test> [--bios|--uefi] [--selftest] [--shipped] [--mem=MB]"
        ),
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the workspace root")
        .to_path_buf()
}

fn build_kernel(selftest: bool) -> Result<PathBuf> {
    let root = workspace_root();
    let cargo = env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.current_dir(&root).args([
        "build",
        "--package",
        "kernel",
        "--target",
        "x86_64-unknown-none",
        "--release",
    ]);
    if selftest {
        cmd.args(["--features", "selftest"]);
    }
    let status = cmd.status().context("running cargo build for the kernel")?;
    if !status.success() {
        bail!("kernel build failed");
    }
    Ok(root.join("target/x86_64-unknown-none/release/kernel"))
}

struct Images {
    bios: PathBuf,
    uefi: PathBuf,
}

fn build_images(selftest: bool) -> Result<Images> {
    let kernel = build_kernel(selftest)?;
    let root = workspace_root();
    let img_dir = root.join("target/img");
    fs::create_dir_all(&img_dir).context("creating target/img")?;
    let images = Images {
        bios: img_dir.join("osmium-bios.img"),
        uefi: img_dir.join("osmium-uefi.img"),
    };
    let builder = bootloader::DiskImageBuilder::new(kernel);
    builder
        .create_bios_image(&images.bios)
        .context("building the BIOS image")?;
    builder
        .create_uefi_image(&images.uefi)
        .context("building the UEFI image")?;
    for (name, path) in [("BIOS", &images.bios), ("UEFI", &images.uefi)] {
        let size = fs::metadata(path)?.len();
        println!(
            "{name} image: {} ({:.2} MiB)",
            path.display(),
            size as f64 / (1024.0 * 1024.0)
        );
        if size > IMAGE_BUDGET_BYTES {
            bail!(
                "{name} image is {size} bytes, over the {IMAGE_BUDGET_BYTES}-byte budget; \
                 image growth must be a deliberate decision"
            );
        }
    }
    Ok(images)
}

/// Pinned OVMF firmware build for UEFI boots, fetched on demand.
const OVMF_RELEASE: &str = "edk2-stable202411-r1";

fn fetch_ovmf() -> Result<(PathBuf, PathBuf)> {
    let root = workspace_root();
    let dir = root.join("target/ovmf");
    let base = dir.join(format!("{OVMF_RELEASE}-bin")).join("x64");
    let code = base.join("code.fd");
    let vars = base.join("vars.fd");
    if !(code.is_file() && vars.is_file()) {
        fs::create_dir_all(&dir).context("creating target/ovmf")?;
        let url = format!(
            "https://github.com/rust-osdev/ovmf-prebuilt/releases/download/{OVMF_RELEASE}/{OVMF_RELEASE}-bin.tar.xz"
        );
        let archive = dir.join("ovmf.tar.xz");
        let status = Command::new("curl")
            .args(["-fsSL", "-o"])
            .arg(&archive)
            .arg(&url)
            .status()
            .context("running curl to fetch OVMF (is curl on PATH?)")?;
        if !status.success() {
            bail!("OVMF download failed: {url}");
        }
        let status = Command::new("tar")
            .arg("-xf")
            .arg(&archive)
            .arg("-C")
            .arg(&dir)
            .status()
            .context("running tar to extract OVMF")?;
        if !status.success() {
            bail!("OVMF extraction failed");
        }
        if !(code.is_file() && vars.is_file()) {
            bail!("OVMF archive did not contain the expected x64/code.fd and x64/vars.fd");
        }
    }
    Ok((code, vars))
}

/// Resolves the QEMU binary: `OSMIUM_QEMU` env var first, then PATH.
fn qemu_program() -> PathBuf {
    if let Ok(q) = env::var("OSMIUM_QEMU") {
        return q.into();
    }
    let name = if cfg!(windows) {
        "qemu-system-x86_64.exe"
    } else {
        "qemu-system-x86_64"
    };
    if let Some(paths) = env::var_os("PATH") {
        for dir in env::split_paths(&paths) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    // Not found; hand the bare name to spawn so the error is still helpful.
    PathBuf::from(name)
}

fn qemu_command(images: &Images, opts: &Opts, headless: bool) -> Result<Command> {
    let image = if opts.uefi {
        &images.uefi
    } else {
        &images.bios
    };
    let mut cmd = Command::new(qemu_program());
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image.display()))
        .args(["-serial", "stdio"])
        .args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"])
        .arg("-no-reboot")
        .args(["-m", &format!("{}M", opts.mem_mb())]);
    if headless {
        cmd.args(["-display", "none"]);
    }
    if opts.uefi {
        let root = workspace_root();
        let (code, vars) = fetch_ovmf()?;
        // OVMF writes to its vars file, so QEMU gets a throwaway copy.
        let vars_copy = root.join("target/ovmf/vars-writable.fd");
        fs::copy(&vars, &vars_copy).context("copying OVMF vars")?;
        cmd.arg("-drive").arg(format!(
            "if=pflash,format=raw,readonly=on,file={}",
            code.display()
        ));
        cmd.arg("-drive")
            .arg(format!("if=pflash,format=raw,file={}", vars_copy.display()));
    }
    Ok(cmd)
}

const QEMU_MISSING_HINT: &str = "starting qemu-system-x86_64 — put QEMU on PATH or point OSMIUM_QEMU at the binary \
     (Debian/Ubuntu: `apt install qemu-system-x86`)";

fn run(opts: &Opts) -> Result<()> {
    let images = build_images(opts.selftest)?;
    let mut cmd = qemu_command(&images, opts, false)?;
    println!(
        "starting QEMU ({}, {} MiB RAM)...",
        if opts.uefi { "UEFI" } else { "BIOS" },
        opts.mem_mb()
    );
    let status = cmd.status().context(QEMU_MISSING_HINT)?;
    println!("QEMU exited: {status}");
    Ok(())
}

/// The serial line the shipped (non-selftest) image logs once it reaches the
/// shell; `test --shipped` waits for it instead of an exit code.
const SHIPPED_MARKER: &str = "boot complete; shell ready";

/// Headless boot proof, run by CI on every push. Default mode boots the
/// selftest build and asserts the QEMU exit code AND the serial log;
/// `--shipped` boots the real image and asserts it reaches the shell.
fn test(opts: &Opts) -> Result<()> {
    let images = build_images(!opts.shipped)?;
    let mut cmd = qemu_command(&images, opts, true)?;
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .stdin(Stdio::null());
    let start = Instant::now();
    let mut child = cmd.spawn().context(QEMU_MISSING_HINT)?;
    let mut stdout = child.stdout.take().expect("stdout was piped");

    // Stream serial output through while also capturing it for assertions.
    let captured = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&captured);
    let reader = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match stdout.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
                    print!("{chunk}");
                    io::stdout().flush().ok();
                    sink.lock().unwrap().push_str(&chunk);
                }
            }
        }
    });

    let status = loop {
        if opts.shipped && captured.lock().unwrap().contains(SHIPPED_MARKER) {
            child.kill().ok();
            child.wait().ok();
            reader.join().ok();
            println!(
                "\nshipped-image boot OK ({}, {} MiB RAM, {:.1}s): reached \"{SHIPPED_MARKER}\"",
                if opts.uefi { "UEFI" } else { "BIOS" },
                opts.mem_mb(),
                start.elapsed().as_secs_f32()
            );
            return Ok(());
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() > BOOT_TIMEOUT {
            child.kill().ok();
            child.wait().ok();
            reader.join().ok();
            bail!(
                "boot test timed out after {}s — the kernel hung",
                BOOT_TIMEOUT.as_secs()
            );
        }
        thread::sleep(Duration::from_millis(100));
    };
    reader.join().ok();
    let serial_log = captured.lock().unwrap().clone();

    if opts.shipped {
        bail!("shipped image exited ({status}) before logging \"{SHIPPED_MARKER}\"");
    }
    match status.code() {
        Some(QEMU_EXIT_SUCCESS) => {}
        Some(QEMU_EXIT_FAILURE) => bail!("selftest battery FAILED (qemu exit {QEMU_EXIT_FAILURE})"),
        other => bail!("unexpected qemu exit {other:?} (success is {QEMU_EXIT_SUCCESS})"),
    }
    if !serial_log.contains("SELFTEST PASSED") {
        bail!("qemu exited with the success code but the serial log lacks 'SELFTEST PASSED'");
    }
    println!(
        "\nboot test OK ({}, {} MiB RAM, {:.1}s)",
        if opts.uefi { "UEFI" } else { "BIOS" },
        opts.mem_mb(),
        start.elapsed().as_secs_f32()
    );
    Ok(())
}
