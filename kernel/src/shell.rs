//! The interactive shell — Osmium's user surface, running as an async task
//! fed by the keyboard stream.
//!
//! Privacy rule (TDD): everything typed here renders to the local console
//! only. Keystrokes and command output never reach the serial port, which is
//! an output channel an observer could be attached to.

use crate::console::with_console;
use crate::framebuffer::{ACCENT, AMBER, DANGER, FOREGROUND, MUTED, OK};
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

/// One editing session, factored out so the E2E self-test can drive the same
/// key handling the interactive loop uses.
pub struct Shell {
    layout_name: &'static str,
    decoder: EventDecoder<AnyLayout>,
    editor: LineEditor,
    history: VecDeque<String>,
    recall: Option<usize>,
}

impl Shell {
    pub fn new() -> Self {
        Self {
            layout_name: "us",
            // MapLettersToUnicode so Ctrl-U/Ctrl-L/Ctrl-C reach the shell.
            decoder: EventDecoder::new(
                AnyLayout::Us104Key(Us104Key),
                HandleControl::MapLettersToUnicode,
            ),
            editor: LineEditor::new(),
            history: VecDeque::new(),
            recall: None,
        }
    }

    fn render(&self) {
        with_console(|con| con.render_input(self.editor.line(), self.editor.cursor()));
    }

    /// Decodes one scancode-set-1 byte and applies it. Public so the battery
    /// can drive a scripted session through the same path as live input.
    pub fn feed_scancode(&mut self, scancodes: &mut ScancodeSet1, byte: u8) {
        if let Ok(Some(event)) = scancodes.advance_state(byte)
            && let Some(key) = self.decoder.process_keyevent(event)
        {
            self.handle_key(key);
        }
    }

    fn handle_key(&mut self, key: DecodedKey) {
        match key {
            // Ctrl-C: abandon the line, fresh prompt.
            DecodedKey::Unicode('\u{3}') => {
                let shown = format!("{}^C", self.editor.line());
                with_console(|con| con.commit_input(&shown));
                self.editor.clear();
                self.recall = None;
                prompt();
                self.render();
            }
            // Ctrl-U: clear the line in place.
            DecodedKey::Unicode('\u{15}') => {
                self.editor.clear();
                self.recall = None;
                self.render();
            }
            // Tab: complete the command verb against kshared::COMMANDS.
            DecodedKey::Unicode('\t') => {
                self.complete_line();
            }
            // Ctrl-A / Ctrl-E: jump to start / end of the line, the way every
            // readline-style shell does. MapLettersToUnicode delivers these.
            DecodedKey::Unicode('\u{1}') => {
                self.editor.move_home();
                self.render();
            }
            DecodedKey::Unicode('\u{5}') => {
                self.editor.move_end();
                self.render();
            }
            // Ctrl-L: clear the screen, keep the in-progress line.
            DecodedKey::Unicode('\u{c}') => {
                with_console(|con| con.clear_screen());
                prompt();
                self.render();
            }
            DecodedKey::Unicode(c) => match self.editor.feed(c) {
                EditAction::Submitted => {
                    let line = self.editor.line().to_string();
                    with_console(|con| con.commit_input(&line));
                    execute(&line, &mut self.layout_name, &mut self.decoder);
                    if !line.trim().is_empty()
                        && self.history.front().map(String::as_str) != Some(line.as_str())
                    {
                        self.history.push_front(line);
                        self.history.truncate(HISTORY_CAP);
                    }
                    self.editor.clear();
                    self.recall = None;
                    prompt();
                    self.render();
                }
                EditAction::Echoed(_) | EditAction::Erased => {
                    self.recall = None;
                    self.render();
                }
                EditAction::Ignored => {}
            },
            DecodedKey::RawKey(KeyCode::ArrowLeft) => {
                self.editor.move_left();
                self.render();
            }
            DecodedKey::RawKey(KeyCode::ArrowRight) => {
                self.editor.move_right();
                self.render();
            }
            DecodedKey::RawKey(KeyCode::Home) => {
                self.editor.move_home();
                self.render();
            }
            DecodedKey::RawKey(KeyCode::End) => {
                self.editor.move_end();
                self.render();
            }
            DecodedKey::RawKey(KeyCode::ArrowUp) => {
                let target = match self.recall {
                    None if !self.history.is_empty() => Some(0),
                    Some(i) if i + 1 < self.history.len() => Some(i + 1),
                    other => other,
                };
                if let Some(i) = target
                    && self.recall != Some(i)
                {
                    self.recall = Some(i);
                    let entry = self.history[i].clone();
                    self.editor.set_line(&entry);
                    self.render();
                }
            }
            DecodedKey::RawKey(KeyCode::ArrowDown) => {
                match self.recall {
                    Some(0) => {
                        self.recall = None;
                        self.editor.set_line("");
                    }
                    Some(i) => {
                        self.recall = Some(i - 1);
                        let entry = self.history[i - 1].clone();
                        self.editor.set_line(&entry);
                    }
                    None => return,
                }
                self.render();
            }
            DecodedKey::RawKey(_) => {}
        }
    }

    /// Tab completion of the command verb. Completes only a single-token line
    /// (no arguments yet); a unique match fills the verb in, a shared prefix
    /// extends as far as it can, and an exhausted prefix lists the candidates
    /// on one line and keeps the input.
    fn complete_line(&mut self) {
        let line = self.editor.line().to_string();
        if line.is_empty() || line.contains(char::is_whitespace) {
            return;
        }
        match kshared::complete(&line) {
            kshared::Completion::None => {}
            kshared::Completion::Unique(name) => {
                self.editor.set_line(name);
                self.recall = None;
                self.render();
            }
            kshared::Completion::Ambiguous(common) if common.len() > line.len() => {
                self.editor.set_line(common);
                self.recall = None;
                self.render();
            }
            kshared::Completion::Ambiguous(common) => {
                with_console(|con| con.commit_input(&line));
                let mut list = String::new();
                for c in kshared::COMMANDS {
                    if c.name.starts_with(common) {
                        if !list.is_empty() {
                            list.push(' ');
                        }
                        list.push_str(c.name);
                    }
                }
                with_console(|con| {
                    con.set_color(MUTED);
                    let _ = writeln!(con, "{list}");
                    con.set_color(FOREGROUND);
                });
                prompt();
                self.render();
            }
        }
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn run() {
    let mut stream = crate::task::keyboard::ScancodeStream::new();
    let mut scancodes = ScancodeSet1::new();
    let mut shell = Shell::new();

    banner();
    prompt();
    shell.render();

    while let Some(scancode) = stream.next().await {
        shell.feed_scancode(&mut scancodes, scancode);
    }
}

fn banner() {
    let ms = kshared::ticks_to_ms(
        crate::interrupts::TICKS.load(Ordering::Relaxed),
        crate::interrupts::TICK_HZ,
    );
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
        con.begin_input();
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
        "user" => user_command(),
        "keymap" => keymap(args, layout_name, decoder),
        // Fixed message on purpose: the argument would be typed input, and
        // panics are reported on the serial port — which typed input must
        // never reach (privacy rule).
        "panic" => panic!("user-requested panic (the 'panic' command)"),
        "shutdown" => shutdown(),
        "selftest" => runtime_selftest(),
        other => error(&format!("unknown command: {other} (try 'help')")),
    }
}

fn help() {
    println_con("commands:");
    // The list is data-driven from kshared::COMMANDS, the same source tab
    // completion uses, so the two can never disagree.
    for c in kshared::COMMANDS {
        println_con(c.help);
    }
    keys_help();
}

/// The editing chords and history keys — implemented since the first shell,
/// and until now discoverable nowhere on screen.
fn keys_help() {
    with_console(|con| {
        con.set_color(MUTED);
        let _ = writeln!(con, "keys:");
        let _ = writeln!(con, "  tab           complete a command name");
        let _ = writeln!(con, "  up/down       recall history");
        let _ = writeln!(con, "  left/right    move within the line");
        let _ = writeln!(con, "  home/end      jump to start/end of line");
        let _ = writeln!(con, "  ctrl-a/ctrl-e start/end of line");
        let _ = writeln!(con, "  ctrl-u        clear the line");
        let _ = writeln!(con, "  ctrl-l        clear the screen");
        let _ = writeln!(con, "  ctrl-c        abandon the line");
        con.set_color(FOREGROUND);
    });
}

fn mem() {
    let (heap_used, heap_free) = crate::memory::heap::stats();
    let (frames_used, frames_total) = crate::memory::frame_stats();
    field(
        "heap:   ",
        &format!(
            "{heap_used} B used, {heap_free} B free of {} KiB",
            crate::memory::heap::HEAP_SIZE / 1024
        ),
    );
    field(
        "frames: ",
        &format!(
            "{frames_used} handed out of {frames_total} usable ({} MiB usable RAM)",
            frames_total as u64 * kshared::FRAME_SIZE / (1024 * 1024)
        ),
    );
}

fn uptime() {
    let ticks = crate::interrupts::TICKS.load(Ordering::Relaxed);
    let hz = crate::interrupts::TICK_HZ;
    println_con(&format!("{}", kshared::Uptime { ticks, hz }));
}

fn sysinfo(layout_name: &str) {
    field(
        "system:  ",
        &format!("Osmium v{} (x86_64)", env!("CARGO_PKG_VERSION")),
    );
    if let Some(info) = with_console(|con| con.info()) {
        field(
            "display: ",
            &format!(
                "{}x{} px, {:?}, {} B/px",
                info.width, info.height, info.pixel_format, info.bytes_per_pixel
            ),
        );
    }
    let (_, frames_total) = crate::memory::frame_stats();
    field(
        "memory:  ",
        &format!(
            "{} MiB usable RAM, {} KiB kernel heap",
            frames_total as u64 * kshared::FRAME_SIZE / (1024 * 1024),
            crate::memory::heap::HEAP_SIZE / 1024
        ),
    );
    field(
        "cpu:     ",
        &format!("ring-3 hardening: {}", crate::cpu::summary()),
    );
    field("keymap:  ", layout_name);
    uptime();
}

fn privacy() {
    println_con("privacy by construction:");
    println_con("  network:     no network stack exists - nothing can phone home");
    println_con("  persistence: none - RAM-only, a cold boot is a clean slate");
    println_con("  memory:      freed heap blocks and handed-out frames are zeroed");
    println_con("  keystrokes:  rendered on this screen only, never on the serial port");
    println_con(&format!(
        "  ring 3:      user code runs at CPL 3 under {}",
        crate::cpu::summary()
    ));
    println_con("memory claims are self-tested at every boot; network and persistence");
    println_con("are CI-gated; keystroke privacy holds by construction, not by policy");
}

fn keymap(args: &str, layout_name: &mut &'static str, decoder: &mut EventDecoder<AnyLayout>) {
    match args {
        "us" => {
            *decoder = EventDecoder::new(
                AnyLayout::Us104Key(Us104Key),
                HandleControl::MapLettersToUnicode,
            );
            *layout_name = "us";
            println_con("keymap: us");
        }
        "uk" => {
            *decoder = EventDecoder::new(
                AnyLayout::Uk105Key(Uk105Key),
                HandleControl::MapLettersToUnicode,
            );
            *layout_name = "uk";
            println_con("keymap: uk");
        }
        "" => println_con(&format!("keymap: {layout_name} (available: us, uk)")),
        _ => error("usage: keymap [us|uk]"),
    }
}

fn user_command() {
    match crate::usermode::run_hello() {
        Ok(exit) => field(
            "user:    ",
            &format!("hello ELF exited with CS={exit:#x} (CPL {})", exit & 3),
        ),
        Err(e) => println_con(&format!("user: the embedded ELF was refused: {e:?}")),
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
    skip(
        "paging probe",
        "maps a fixed address that cannot be mapped twice",
    );
    skip("stack-overflow guard", "cannot return; boot battery only");
}

/// Names a boot-only battery check this runtime subset deliberately omits, in
/// the warning colour the design brief assigns to a skipped phase — so the
/// output states what it does NOT cover rather than quietly leaving it out.
fn skip(name: &str, why: &str) {
    with_console(|con| {
        con.set_color(AMBER);
        let _ = write!(con, "  [skip] ");
        con.set_color(MUTED);
        let _ = writeln!(con, "{name} ({why})");
        con.set_color(FOREGROUND);
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
        // folds the check away (see the battery's twin of this test). The
        // scan skips the allocator's 16-byte free-list node at the block
        // head for the reason the twin documents.
        const HOLE_META: usize = 16;
        let clean =
            p1 == p2 && (HOLE_META..layout.size()).all(|i| p2.add(i).read_volatile() != 0xA5);
        dealloc(p2, layout);
        clean
    }
}

fn report(name: &str, ok: bool) {
    with_console(|con| {
        con.set_color(if ok { OK } else { DANGER });
        let _ = write!(con, "{}", if ok { "  [ ok ] " } else { "  [FAIL] " });
        con.set_color(FOREGROUND);
        let _ = writeln!(con, "{name}");
    });
}

/// Prints a `label: value` line with the label dimmed, so the eye lands on the
/// value. Colour is decoration only — the label text still carries the meaning.
fn field(label: &str, value: &str) {
    with_console(|con| {
        con.set_color(MUTED);
        let _ = write!(con, "{label}");
        con.set_color(FOREGROUND);
        let _ = writeln!(con, "{value}");
    });
}

/// A shell error line: a danger-coloured `error:` marker (the Design Brief's
/// Danger role, which was otherwise unused in the shell) followed by the
/// message in the ordinary colour, so the words carry the meaning and colour
/// is only reinforcement.
fn error(msg: &str) {
    with_console(|con| {
        con.set_color(DANGER);
        let _ = write!(con, "error: ");
        con.set_color(FOREGROUND);
        let _ = writeln!(con, "{msg}");
    });
}
