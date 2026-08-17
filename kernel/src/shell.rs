//! The interactive shell — Osmium's user surface, running as an async task
//! fed by the keyboard stream.
//!
//! Privacy rule (TDD): everything typed here renders to the local console
//! only. Keystrokes and command output never reach the serial port, which is
//! an output channel an observer could be attached to.

use crate::console::with_console;
use crate::framebuffer::{ACCENT, DANGER, FOREGROUND};
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use core::fmt::Write;
use core::sync::atomic::Ordering;
use futures_util::stream::StreamExt;
use kshared::{EditAction, LineEditor};
use pc_keyboard::layouts::{AnyLayout, Uk105Key, Us104Key};
use pc_keyboard::{DecodedKey, EventDecoder, HandleControl, KeyCode, ScancodeSet, ScancodeSet1};

const HISTORY_CAP: usize = 32;
const PROMPT: &str = "osmium> ";

pub async fn run() {
    let mut stream = crate::task::keyboard::ScancodeStream::new();
    let mut scancodes = ScancodeSet1::new();
    let mut layout_name: &'static str = "us";
    let mut decoder = EventDecoder::new(AnyLayout::Us104Key(Us104Key), HandleControl::Ignore);
    let mut editor = LineEditor::new();
    let mut history: VecDeque<String> = VecDeque::new();
    let mut recall: Option<usize> = None;

    banner();
    prompt();

    while let Some(scancode) = stream.next().await {
        let Ok(Some(event)) = scancodes.advance_state(scancode) else {
            continue;
        };
        let Some(key) = decoder.process_keyevent(event) else {
            continue;
        };
        match key {
            DecodedKey::Unicode(c) => match editor.feed(c) {
                EditAction::Echoed(ch) => {
                    recall = None;
                    with_console(|con| con.write_char(ch));
                }
                EditAction::Erased => {
                    recall = None;
                    with_console(|con| con.erase_last_char());
                }
                EditAction::Submitted => {
                    with_console(|con| con.write_char('\n'));
                    let line = editor.line().to_string();
                    execute(&line, &mut layout_name, &mut decoder);
                    if !line.trim().is_empty()
                        && history.front().map(String::as_str) != Some(line.as_str())
                    {
                        history.push_front(line);
                        history.truncate(HISTORY_CAP);
                    }
                    editor.clear();
                    recall = None;
                    prompt();
                }
                EditAction::Ignored => {}
            },
            DecodedKey::RawKey(KeyCode::ArrowUp) => {
                let target = match recall {
                    None if !history.is_empty() => Some(0),
                    Some(i) if i + 1 < history.len() => Some(i + 1),
                    other => other,
                };
                if let Some(i) = target
                    && recall != Some(i)
                {
                    recall = Some(i);
                    replace_line(&mut editor, &history[i]);
                }
            }
            DecodedKey::RawKey(KeyCode::ArrowDown) => match recall {
                Some(0) => {
                    recall = None;
                    replace_line(&mut editor, "");
                }
                Some(i) => {
                    recall = Some(i - 1);
                    replace_line(&mut editor, &history[i - 1]);
                }
                None => {}
            },
            DecodedKey::RawKey(_) => {}
        }
    }
}

/// Erases the rendered line and re-renders `new` (history recall).
fn replace_line(editor: &mut LineEditor, new: &str) {
    let old_len = editor.len();
    editor.set_line(new);
    with_console(|con| {
        for _ in 0..old_len {
            con.erase_last_char();
        }
        for c in editor.line().chars() {
            con.write_char(c);
        }
    });
}

fn banner() {
    let ms = crate::interrupts::TICKS.load(Ordering::Relaxed) * (1000 / crate::interrupts::TICK_HZ);
    with_console(|con| {
        con.set_color(ACCENT);
        let _ = writeln!(
            con,
            "\nOsmium v{} - a small OS, private by construction",
            env!("CARGO_PKG_VERSION")
        );
        con.set_color(FOREGROUND);
        // The PIT starts at interrupts::init, so earlier boot stages are
        // outside its sight; the label says exactly what is measured.
        let _ = writeln!(
            con,
            "shell ready {ms} ms after interrupts-on. Type 'help' to begin."
        );
    });
}

fn prompt() {
    with_console(|con| {
        con.set_color(ACCENT);
        for c in PROMPT.chars() {
            con.write_char(c);
        }
        con.set_color(FOREGROUND);
    });
}

fn println_con(s: &str) {
    with_console(|con| {
        let _ = writeln!(con, "{s}");
    });
}

fn execute(line: &str, layout_name: &mut &'static str, decoder: &mut EventDecoder<AnyLayout>) {
    let Some((cmd, args)) = kshared::parse_command(line) else {
        return;
    };
    match cmd {
        "help" => help(),
        "echo" => println_con(args),
        "clear" => {
            with_console(|con| con.clear_screen());
        }
        "mem" => mem(),
        "uptime" => uptime(),
        "sysinfo" => sysinfo(layout_name),
        "privacy" => privacy(),
        "keymap" => keymap(args, layout_name, decoder),
        // Fixed message on purpose: the argument would be typed input, and
        // panics are reported on the serial port — which typed input must
        // never reach (privacy rule).
        "panic" => panic!("user-requested panic (the 'panic' command)"),
        "shutdown" => shutdown(),
        "selftest" => runtime_selftest(),
        other => println_con(&format!("unknown command: {other} (try 'help')")),
    }
}

fn help() {
    println_con("commands:");
    println_con("  help          this list");
    println_con("  echo <text>   print text");
    println_con("  clear         clear the screen");
    println_con("  mem           heap and physical-frame statistics");
    println_con("  uptime        time since boot");
    println_con("  sysinfo       hardware and kernel summary");
    println_con("  privacy       what this OS can and cannot leak");
    println_con("  keymap [us|uk] show or switch keyboard layout");
    println_con("  selftest      run the runtime test subset");
    println_con("  panic         demonstrate the panic screen");
    println_con("  shutdown      power off (QEMU) or halt");
}

fn mem() {
    let (heap_used, heap_free) = crate::memory::heap::stats();
    let (frames_used, frames_total) = crate::memory::frame_stats();
    println_con(&format!(
        "heap:   {heap_used} B used, {heap_free} B free of {} KiB",
        crate::memory::heap::HEAP_SIZE / 1024
    ));
    println_con(&format!(
        "frames: {frames_used} handed out of {frames_total} usable ({} MiB usable RAM)",
        frames_total as u64 * kshared::FRAME_SIZE / (1024 * 1024)
    ));
}

fn uptime() {
    let ticks = crate::interrupts::TICKS.load(Ordering::Relaxed);
    let hz = crate::interrupts::TICK_HZ;
    println_con(&format!(
        "up {}.{:02} s ({ticks} ticks @ {hz} Hz)",
        ticks / hz,
        ticks % hz
    ));
}

fn sysinfo(layout_name: &str) {
    println_con(&format!("Osmium v{} (x86_64)", env!("CARGO_PKG_VERSION")));
    if let Some(info) = with_console(|con| con.info()) {
        println_con(&format!(
            "display: {}x{} px, {:?}, {} B/px",
            info.width, info.height, info.pixel_format, info.bytes_per_pixel
        ));
    }
    let (_, frames_total) = crate::memory::frame_stats();
    println_con(&format!(
        "memory:  {} MiB usable RAM, {} KiB kernel heap",
        frames_total as u64 * kshared::FRAME_SIZE / (1024 * 1024),
        crate::memory::heap::HEAP_SIZE / 1024
    ));
    println_con(&format!("keymap:  {layout_name}"));
    uptime();
}

fn privacy() {
    println_con("privacy by construction:");
    println_con("  network:     no network stack exists - nothing can phone home");
    println_con("  persistence: none - RAM-only, a cold boot is a clean slate");
    println_con("  memory:      freed heap blocks and handed-out frames are zeroed");
    println_con("  keystrokes:  rendered on this screen only, never on the serial port");
    println_con("each claim is enforced by the CI self-test battery, not by policy");
}

fn keymap(args: &str, layout_name: &mut &'static str, decoder: &mut EventDecoder<AnyLayout>) {
    match args {
        "us" => {
            *decoder = EventDecoder::new(AnyLayout::Us104Key(Us104Key), HandleControl::Ignore);
            *layout_name = "us";
            println_con("keymap: us");
        }
        "uk" => {
            *decoder = EventDecoder::new(AnyLayout::Uk105Key(Uk105Key), HandleControl::Ignore);
            *layout_name = "uk";
            println_con("keymap: uk");
        }
        "" => println_con(&format!("keymap: {layout_name} (available: us, uk)")),
        _ => println_con("usage: keymap [us|uk]"),
    }
}

fn shutdown() -> ! {
    println_con("shutting down");
    // Under QEMU this exits the VM; on hardware the port write is a no-op
    // and exit() falls through to a halt loop.
    crate::qemu::exit(crate::qemu::ExitCode::Success)
}

/// The subset of the boot battery that is safe to re-run at any time.
/// (The paging and stack-overflow tests are boot-only by design.)
fn runtime_selftest() {
    report("heap alloc", {
        let v: alloc::vec::Vec<u64> = (0..1000).collect();
        v.iter().sum::<u64>() == 499_500
    });
    report("freed memory zeroed", freed_memory_is_zeroed());
    report("int3 handled", {
        let before = crate::interrupts::BREAKPOINT_HITS.load(Ordering::Relaxed);
        x86_64::instructions::interrupts::int3();
        crate::interrupts::BREAKPOINT_HITS.load(Ordering::Relaxed) > before
    });
    report("timer ticking", {
        let start = crate::interrupts::TICKS.load(Ordering::Relaxed);
        let mut ok = false;
        for _ in 0..1_000 {
            if crate::interrupts::TICKS.load(Ordering::Relaxed) > start {
                ok = true;
                break;
            }
            x86_64::instructions::hlt();
        }
        ok
    });
}

fn freed_memory_is_zeroed() -> bool {
    use alloc::alloc::{alloc, dealloc};
    use core::alloc::Layout;
    let layout = Layout::from_size_align(256, 8).unwrap();
    // SAFETY: matched alloc/dealloc pairs; contents only read while owned.
    unsafe {
        let p1 = alloc(layout);
        if p1.is_null() {
            return false;
        }
        // Volatile for the same reason as the battery's twin: a fill before
        // an immediate free is a dead store that LLVM otherwise deletes.
        for i in 0..layout.size() {
            p1.add(i).write_volatile(0xA5);
        }
        dealloc(p1, layout);
        let p2 = alloc(layout);
        if p2.is_null() {
            return false;
        }
        // Volatile: a plain read of fresh-alloc memory is undef and LLVM
        // folds the check away (see the battery's twin of this test).
        let clean = p1 == p2 && (0..layout.size()).all(|i| p2.add(i).read_volatile() != 0xA5);
        dealloc(p2, layout);
        clean
    }
}

fn report(name: &str, ok: bool) {
    with_console(|con| {
        if ok {
            let _ = write!(con, "  [ ok ] ");
        } else {
            con.set_color(DANGER);
            let _ = write!(con, "  [FAIL] ");
            con.set_color(FOREGROUND);
        }
        let _ = writeln!(con, "{name}");
    });
}
