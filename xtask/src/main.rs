//! Build orchestration for Osmium: compiles the kernel for
//! `x86_64-unknown-none`, wraps it into bootable BIOS and UEFI disk images,
//! and drives QEMU both interactively (`run`) and headless for the CI boot
//! proof (`test`).

use anyhow::{Context, Result, bail};
use std::io::{Read, Write as _};
use std::net::TcpStream;
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
/// Measured floors 2026-08-17 (QEMU 11.1): BIOS passes at 21 MB, fails at
/// 20; UEFI passes at 46, fails at 45 — the UEFI premium is OVMF's own
/// footprint, not the kernel's. CI pins slightly above the floors (3 MB
/// BIOS, 2 MB UEFI of recorded headroom) to absorb QEMU-version variance;
/// a regression larger than that headroom fails the gate.
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
    /// Boot this exact image file instead of building one; used by the release
    /// workflow to prove the very bytes it uploads.
    image_override: Option<PathBuf>,
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
        image_override: None,
    };
    for arg in args {
        match arg.as_str() {
            "--uefi" => opts.uefi = true,
            "--bios" => opts.uefi = false,
            "--selftest" => opts.selftest = true,
            "--shipped" => opts.shipped = true,
            other => {
                if let Some(mb) = other.strip_prefix("--mem=") {
                    opts.mem_override = Some(mb.parse().context("--mem=<MB> takes a number")?);
                } else if let Some(path) = other.strip_prefix("--image=") {
                    opts.image_override = Some(PathBuf::from(path));
                } else {
                    bail!("unknown argument: {other}");
                }
            }
        }
    }
    match cmd.as_str() {
        "build" => build_images(opts.selftest).map(|_| ()),
        "run" => run(&opts),
        "test" => test(&opts),
        "privacy" => privacy(),
        _ => bail!(
            "usage: cargo xtask <build|run|test|privacy> [--bios|--uefi] [--selftest] [--shipped] [--mem=MB]"
        ),
    }
}

/// Behavioural proof of the keystroke-privacy claim: boot the shipped image,
/// type a sentinel through QEMU's monitor, and drive `shutdown` from the
/// keyboard. The clean exit is the positive control — it proves the keystrokes
/// were decoded and the shell ran them — and the serial log containing no
/// sentinel is the negative one: typed input never reached the serial port.
fn privacy() -> Result<()> {
    const SENTINEL: &str = "osmiumsecretzqxj";
    const MONITOR_PORT: u16 = 55532;
    let images = build_images(false)?;
    let mut cmd = Command::new(qemu_program());
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", images.bios.display()))
        .args(["-serial", "stdio"])
        .args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"])
        .arg("-no-reboot")
        .args(["-m", &format!("{BIOS_TEST_MEM_MB}M")])
        .args(["-display", "none"])
        .arg("-monitor")
        .arg(format!("tcp:127.0.0.1:{MONITOR_PORT},server,nowait"))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .stdin(Stdio::null());
    let start = Instant::now();
    let mut child = cmd.spawn().context(QEMU_MISSING_HINT)?;
    let mut stdout = child.stdout.take().expect("stdout was piped");
    let captured = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&captured);
    let reader = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdout.read(&mut buf) {
            if n == 0 {
                break;
            }
            let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
            print!("{chunk}");
            io::stdout().flush().ok();
            sink.lock().unwrap().push_str(&chunk);
        }
    });

    // Wait for the shell to come up.
    while !captured.lock().unwrap().contains(SHIPPED_MARKER) {
        if child.try_wait()?.is_some() {
            reader.join().ok();
            bail!("QEMU exited before the shell was ready");
        }
        if start.elapsed() > BOOT_TIMEOUT {
            child.kill().ok();
            reader.join().ok();
            bail!("timed out waiting for the shell");
        }
        thread::sleep(Duration::from_millis(50));
    }

    // Drive the keyboard through the monitor: echo the sentinel, then quit.
    let mut monitor = TcpStream::connect(("127.0.0.1", MONITOR_PORT))
        .context("connecting to the QEMU monitor")?;
    send_line_as_keys(&mut monitor, &format!("echo {SENTINEL}"))?;
    send_line_as_keys(&mut monitor, "shutdown")?;

    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() > BOOT_TIMEOUT {
            child.kill().ok();
            reader.join().ok();
            bail!("the shell did not act on the typed 'shutdown' — were the keystrokes delivered?");
        }
        thread::sleep(Duration::from_millis(50));
    };
    reader.join().ok();
    let log = captured.lock().unwrap().clone();

    if status.code() != Some(QEMU_EXIT_SUCCESS) {
        bail!(
            "typed 'shutdown' did not exit cleanly (code {:?}); the positive control failed",
            status.code()
        );
    }
    // The strongest form of the claim: once the boot-complete line ends, a
    // healthy shipped kernel emits NOTHING further on serial — keystrokes,
    // echoes and command output all render to the framebuffer only. So the
    // gate is an allowlist (post-boot serial must be EMPTY), not a sentinel
    // blocklist: a leak of any shape fails it — a per-scancode hex dump in
    // the IRQ handler defeated the sentinel version (observed by mutation:
    // hex bytes never spell the sentinel) and fails this one.
    let after_marker = log.rsplit(SHIPPED_MARKER).next().unwrap_or(&log);
    // The marker is a prefix of its line ("...shell ready (NN ms...)"), so
    // skip the rest of that line before demanding silence.
    let post_boot = match after_marker.find('\n') {
        Some(i) => &after_marker[i + 1..],
        None => "",
    };
    if !post_boot.trim().is_empty() {
        let compacted: String = post_boot.chars().filter(|c| !c.is_whitespace()).collect();
        if compacted.contains(SENTINEL) {
            bail!("the typed sentinel reached the serial log — keystroke privacy is BROKEN");
        }
        bail!(
            "serial was not silent after boot — something on the input or output path \
             writes to the serial port. Post-boot serial bytes: {:?}",
            post_boot.trim()
        );
    }
    println!(
        "\nkeystroke-privacy OK: the shell executed typed input (clean exit) and the \
         serial port stayed silent after boot (sentinel \"{SENTINEL}\" typed)"
    );
    Ok(())
}

/// Maps a lowercase/digit/space line to QEMU `sendkey` tokens and presses it,
/// leaving time between keys for the keyboard IRQ and the async shell to drain.
fn send_line_as_keys(monitor: &mut TcpStream, line: &str) -> Result<()> {
    for c in line.chars() {
        let token = match c {
            'a'..='z' | '0'..='9' => c.to_string(),
            ' ' => "spc".to_string(),
            other => bail!("sendkey helper only handles lowercase/digit/space, got {other:?}"),
        };
        writeln!(monitor, "sendkey {token}")?;
        thread::sleep(Duration::from_millis(25));
    }
    writeln!(monitor, "sendkey ret")?;
    thread::sleep(Duration::from_millis(100));
    Ok(())
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
/// sha256 of the release tarball above; a re-pointed release fails loudly
/// instead of silently substituting guest firmware. Verified at download
/// time only — an already-extracted tree (e.g. restored from the CI cache)
/// is trusted without re-hashing.
const OVMF_SHA256: &str = "963fc6cef6a0560cec97381ed22a7d5c76f440c8212529a034cb465466cd57cc";

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
        let digest = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(fs::read(&archive).context("reading the OVMF archive")?);
            format!("{:x}", hasher.finalize())
        };
        if digest != OVMF_SHA256 {
            bail!("OVMF checksum mismatch: got {digest}, expected {OVMF_SHA256}");
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
    if opts.shipped && opts.selftest {
        bail!("--shipped and --selftest are contradictory: shipped boots the non-selftest image");
    }
    if opts.mem_override == Some(0) {
        bail!("--mem=0 is not a machine; QEMU would silently fall back to its default");
    }
    // --image boots an exact prebuilt file (release provenance); otherwise
    // build fresh. Both fields of Images point at the same file since --image
    // supplies one firmware's image and `--bios`/`--uefi` selects it.
    let images = match &opts.image_override {
        Some(path) => Images {
            bios: path.clone(),
            uefi: path.clone(),
        },
        None => build_images(!opts.shipped)?,
    };
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
            // The marker alone could mask a crash moments later; give the
            // shell a beat and make sure no panic followed it.
            thread::sleep(Duration::from_millis(500));
            let log = captured.lock().unwrap().clone();
            child.kill().ok();
            child.wait().ok();
            reader.join().ok();
            if log.contains("KERNEL PANIC") {
                bail!("shipped image reached the shell and then panicked");
            }
            verify_ram_claim(&log, opts.mem_mb())?;
            let boot_ms = verify_boot_latency(&log)?;
            println!(
                "\nshipped-image boot OK ({}, {} MiB RAM, {boot_ms} ms to shell, {:.1}s): reached \"{SHIPPED_MARKER}\"",
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
            let log = captured.lock().unwrap().clone();
            if log.contains("FrameAllocationFailed") || log.contains("ERROR: panicked") {
                bail!(
                    "boot timed out after {}s: the BOOTLOADER panicked before the kernel ran \
                     (usually below the RAM floor) — see the log above",
                    BOOT_TIMEOUT.as_secs()
                );
            }
            bail!(
                "boot test timed out after {}s — no verdict and no bootloader panic; the kernel hung",
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
    verify_ram_claim(&serial_log, opts.mem_mb())?;
    println!(
        "\nboot test OK ({}, {} MiB RAM, {:.1}s)",
        if opts.uefi { "UEFI" } else { "BIOS" },
        opts.mem_mb(),
        start.elapsed().as_secs_f32()
    );
    Ok(())
}

/// The success line quotes the requested RAM size, so that figure must match
/// what the machine really had: the kernel logs `<used>/<total> frames`, and
/// total*4096 has to sit plausibly under the request (firmware reservations
/// eat some, but never more than ~40%). Catches a wrong `--mem` turning the
/// lightness claim into fiction — e.g. a value QEMU rejects and silently
/// replaces with its 128 MiB default.
fn verify_ram_claim(serial_log: &str, mem_mb: u32) -> Result<()> {
    let total_frames = serial_log
        .lines()
        .filter(|line| line.contains(" frames"))
        .filter_map(|line| {
            line.split_whitespace()
                .find(|token| token.contains('/'))
                .and_then(|token| token.split('/').nth(1))
                .and_then(|n| n.parse::<u64>().ok())
        })
        .next_back();
    let Some(total_frames) = total_frames else {
        bail!("the serial log never reported usable frames; cannot verify the RAM figure");
    };
    let usable_mb = total_frames * 4096 / (1024 * 1024);
    let claimed_mb = u64::from(mem_mb);
    if usable_mb > claimed_mb || usable_mb * 10 < claimed_mb * 6 {
        bail!(
            "the kernel saw {usable_mb} MiB usable but this test claims {claimed_mb} MiB; \
             the lightness figure must be measured, not quoted"
        );
    }
    Ok(())
}

/// The kernel logs `shell ready (<n> ms after interrupts-on)`; parse it and
/// fail if boot-to-shell exceeds a generous ceiling (10x the observed ~20 ms),
/// so the README's latency claim is CI-gated the same way the RAM figure is.
const BOOT_LATENCY_CEILING_MS: u64 = 200;

fn verify_boot_latency(serial_log: &str) -> Result<u64> {
    let ms = serial_log
        .lines()
        .find_map(|line| {
            let after = line.split("shell ready (").nth(1)?;
            after.split(" ms").next()?.trim().parse::<u64>().ok()
        })
        .context("the serial log never reported a boot-to-shell time")?;
    if ms > BOOT_LATENCY_CEILING_MS {
        bail!("boot-to-shell was {ms} ms, over the {BOOT_LATENCY_CEILING_MS} ms ceiling");
    }
    Ok(ms)
}
