//! Pure logic shared by the kernel and testable on the host: address
//! arithmetic, and (in later milestones) command parsing and the line editor.
//! Nothing in this crate may touch hardware or require an allocator.

#![cfg_attr(not(test), no_std)]

pub mod elf;

/// Aligns `addr` upwards to `align`, which must be a power of two.
pub const fn align_up(addr: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    (addr + align - 1) & !(align - 1)
}

/// Aligns `addr` downwards to `align`, which must be a power of two.
pub const fn align_down(addr: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    addr & !(align - 1)
}

pub const FRAME_SIZE: u64 = 4096;

/// Yields the start addresses of every whole `FRAME_SIZE` frame inside
/// `[start, end)`, clipping partial frames at both edges. Degenerate regions
/// (empty, reversed, or smaller than one aligned frame) yield nothing.
pub fn frame_starts(start: u64, end: u64) -> impl Iterator<Item = u64> {
    let first = align_up(start, FRAME_SIZE);
    let last_end = align_down(end, FRAME_SIZE);
    (first..last_end).step_by(FRAME_SIZE as usize)
}

/// Capacity of one command line, in bytes (ASCII only).
pub const LINE_CAP: usize = 256;

/// What a fed character did to the line buffer. The shell re-renders the whole
/// line from [`LineEditor::line`] and [`LineEditor::cursor`] after every key,
/// so display and buffer cannot disagree; this enum only tells the caller
/// whether the line was submitted and whether anything changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditAction {
    /// A printable character was inserted at the cursor.
    Echoed(char),
    /// A character was removed at or before the cursor.
    Erased,
    /// Enter was pressed; read the line with `line()`, then `clear()`.
    Submitted,
    /// Nothing changed (full buffer, empty backspace, unsupported char).
    Ignored,
}

/// Fixed-capacity ASCII line editor with an insertion cursor: no allocator,
/// host-testable. The cursor is a byte index in `0..=len`; insertion and
/// deletion happen at the cursor, so mid-line editing keeps buffer and cursor
/// consistent.
pub struct LineEditor {
    buf: [u8; LINE_CAP],
    len: usize,
    cursor: usize,
}

impl LineEditor {
    pub const fn new() -> Self {
        Self {
            buf: [0; LINE_CAP],
            len: 0,
            cursor: 0,
        }
    }

    pub fn feed(&mut self, c: char) -> EditAction {
        match c {
            '\n' | '\r' => EditAction::Submitted,
            '\u{8}' | '\u{7f}' => {
                if self.cursor > 0 {
                    // Delete the char before the cursor, closing the gap.
                    self.buf.copy_within(self.cursor..self.len, self.cursor - 1);
                    self.len -= 1;
                    self.cursor -= 1;
                    EditAction::Erased
                } else {
                    EditAction::Ignored
                }
            }
            c if c.is_ascii() && !c.is_ascii_control() => {
                if self.len < LINE_CAP {
                    // Insert at the cursor, shifting the tail right.
                    self.buf.copy_within(self.cursor..self.len, self.cursor + 1);
                    self.buf[self.cursor] = c as u8;
                    self.len += 1;
                    self.cursor += 1;
                    EditAction::Echoed(c)
                } else {
                    EditAction::Ignored
                }
            }
            _ => EditAction::Ignored,
        }
    }

    /// Moves the cursor one cell left; returns whether it moved.
    pub fn move_left(&mut self) -> bool {
        if self.cursor > 0 {
            self.cursor -= 1;
            true
        } else {
            false
        }
    }

    /// Moves the cursor one cell right; returns whether it moved.
    pub fn move_right(&mut self) -> bool {
        if self.cursor < self.len {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    pub fn move_home(&mut self) {
        self.cursor = 0;
    }

    pub fn move_end(&mut self) {
        self.cursor = self.len;
    }

    pub fn line(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.cursor = 0;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Replaces the contents (history recall), cursor left at the end.
    /// Truncates at capacity; control and non-ASCII characters are skipped
    /// rather than interpreted (a backspace in `s` must not delete characters
    /// already set).
    pub fn set_line(&mut self, s: &str) {
        self.clear();
        for c in s.chars() {
            if c.is_ascii() && !c.is_ascii_control() {
                let _ = self.feed(c);
            }
        }
    }
}

impl Default for LineEditor {
    fn default() -> Self {
        Self::new()
    }
}

/// Milliseconds represented by `ticks` at `hz`, computed multiply-first so it
/// is exact at any tick rate. The tempting `ticks * (1000 / hz)` divides
/// first and loses precision at every rate that does not divide 1000 evenly
/// (e.g. 60 Hz), which is the kind of latent bug a "it works at 100 Hz"
/// constant hides. Callers: the boot-latency line and the shell banner.
pub const fn ticks_to_ms(ticks: u64, hz: u64) -> u64 {
    ticks * 1000 / hz
}

/// A boot/uptime duration that renders itself, so the one formatting rule
/// lives here (host-tested) instead of being re-derived at each call site.
/// Under a minute: `S.CC s`; past a minute: `Mm SS s`; past an hour:
/// `Hh MM SS s`. The raw tick count and rate follow in parentheses.
pub struct Uptime {
    pub ticks: u64,
    pub hz: u64,
}

impl core::fmt::Display for Uptime {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let total_s = self.ticks / self.hz;
        let centis = (self.ticks % self.hz) * 100 / self.hz;
        let (h, m, s) = (total_s / 3600, (total_s % 3600) / 60, total_s % 60);
        if h > 0 {
            write!(f, "up {h}h {m:02}m {s:02} s")?;
        } else if m > 0 {
            write!(f, "up {m}m {s:02} s")?;
        } else {
            write!(f, "up {s}.{centis:02} s")?;
        }
        write!(f, " ({} ticks @ {} Hz)", self.ticks, self.hz)
    }
}

/// Splits a command line into (command, argument string), both trimmed.
/// Returns None for blank lines.
pub fn parse_command(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.split_once(char::is_whitespace) {
        Some((cmd, rest)) => Some((cmd, rest.trim())),
        None => Some((trimmed, "")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_up_rounds_to_next_boundary() {
        assert_eq!(align_up(0, 4096), 0);
        assert_eq!(align_up(1, 4096), 4096);
        assert_eq!(align_up(4096, 4096), 4096);
        assert_eq!(align_up(4097, 4096), 8192);
    }

    #[test]
    fn align_down_rounds_to_previous_boundary() {
        assert_eq!(align_down(0, 4096), 0);
        assert_eq!(align_down(1, 4096), 0);
        assert_eq!(align_down(4096, 4096), 4096);
        assert_eq!(align_down(8191, 4096), 4096);
    }

    #[test]
    fn frame_starts_clips_partial_frames_at_both_edges() {
        let frames: Vec<u64> = frame_starts(100, 3 * 4096 + 50).collect();
        assert_eq!(frames, [4096, 2 * 4096]);
    }

    #[test]
    fn frame_starts_keeps_exactly_aligned_regions_whole() {
        let frames: Vec<u64> = frame_starts(8192, 8192 + 4 * 4096).collect();
        assert_eq!(frames, [8192, 12288, 16384, 20480]);
    }

    #[test]
    fn frame_starts_yields_nothing_for_degenerate_regions() {
        assert_eq!(frame_starts(0, 0).count(), 0);
        assert_eq!(frame_starts(500, 400).count(), 0);
        assert_eq!(frame_starts(1, 4095).count(), 0); // no whole frame fits
    }

    #[test]
    fn editor_echoes_and_accumulates() {
        let mut ed = LineEditor::new();
        assert_eq!(ed.feed('h'), EditAction::Echoed('h'));
        assert_eq!(ed.feed('i'), EditAction::Echoed('i'));
        assert_eq!(ed.line(), "hi");
    }

    #[test]
    fn editor_backspace_removes_and_bottoms_out() {
        let mut ed = LineEditor::new();
        ed.feed('a');
        assert_eq!(ed.feed('\u{8}'), EditAction::Erased);
        assert_eq!(ed.line(), "");
        assert_eq!(ed.feed('\u{8}'), EditAction::Ignored);
    }

    #[test]
    fn editor_submits_without_storing_newline() {
        let mut ed = LineEditor::new();
        ed.feed('o');
        ed.feed('k');
        assert_eq!(ed.feed('\n'), EditAction::Submitted);
        assert_eq!(ed.line(), "ok");
    }

    #[test]
    fn editor_ignores_input_past_capacity() {
        let mut ed = LineEditor::new();
        for _ in 0..LINE_CAP {
            assert!(matches!(ed.feed('x'), EditAction::Echoed('x')));
        }
        assert_eq!(ed.feed('y'), EditAction::Ignored);
        assert_eq!(ed.len(), LINE_CAP);
    }

    #[test]
    fn editor_ignores_control_and_non_ascii() {
        let mut ed = LineEditor::new();
        assert_eq!(ed.feed('\u{1b}'), EditAction::Ignored); // escape
        assert_eq!(ed.feed('é'), EditAction::Ignored);
        assert_eq!(ed.line(), "");
    }

    #[test]
    fn set_line_replaces_and_truncates() {
        let mut ed = LineEditor::new();
        ed.feed('z');
        ed.set_line("recalled");
        assert_eq!(ed.line(), "recalled");
        let long: String = "a".repeat(LINE_CAP + 50);
        ed.set_line(&long);
        assert_eq!(ed.len(), LINE_CAP);
    }

    #[test]
    fn set_line_skips_control_chars_instead_of_interpreting_them() {
        let mut ed = LineEditor::new();
        ed.set_line("ab\u{8}c");
        assert_eq!(ed.line(), "abc"); // the backspace must not delete 'b'
        ed.set_line("a\u{7f}b\nc");
        assert_eq!(ed.line(), "abc");
    }

    #[test]
    fn ticks_to_ms_is_exact_multiply_first() {
        assert_eq!(ticks_to_ms(2, 100), 20);
        assert_eq!(ticks_to_ms(0, 100), 0);
        // The case a divide-first `ticks * (1000 / hz)` gets wrong: at 60 Hz
        // it would compute 3 * 16 = 48, not 50.
        assert_eq!(ticks_to_ms(3, 60), 50);
        assert_eq!(ticks_to_ms(1_000, 1_000), 1_000);
    }

    #[test]
    fn uptime_formats_and_rolls_over() {
        let fmt = |ticks, hz| format!("{}", Uptime { ticks, hz });
        assert_eq!(fmt(105, 100), "up 1.05 s (105 ticks @ 100 Hz)");
        // 59 s stays in seconds; 60 s rolls to a minute.
        assert_eq!(fmt(5_900, 100), "up 59.00 s (5900 ticks @ 100 Hz)");
        assert_eq!(fmt(6_000, 100), "up 1m 00 s (6000 ticks @ 100 Hz)");
        // 3600 s rolls to an hour.
        assert_eq!(fmt(360_000, 100), "up 1h 00m 00 s (360000 ticks @ 100 Hz)");
    }

    #[test]
    fn parse_command_splits_and_trims() {
        assert_eq!(parse_command("help"), Some(("help", "")));
        assert_eq!(
            parse_command("echo hello world"),
            Some(("echo", "hello world"))
        );
        assert_eq!(parse_command("  mem   "), Some(("mem", "")));
        assert_eq!(
            parse_command("echo   spaced   args "),
            Some(("echo", "spaced   args"))
        );
        assert_eq!(parse_command(""), None);
        assert_eq!(parse_command("   "), None);
    }

    #[test]
    fn cursor_inserts_and_deletes_mid_line() {
        let mut ed = LineEditor::new();
        ed.set_line("helo"); // cursor at end (4)
        assert_eq!(ed.cursor(), 4);
        assert!(ed.move_left()); // between l and o -> cursor 3
        ed.feed('l'); // insert -> "hello", cursor 4
        assert_eq!(ed.line(), "hello");
        assert_eq!(ed.cursor(), 4);
    }

    #[test]
    fn backspace_deletes_the_char_before_the_cursor_not_the_last() {
        let mut ed = LineEditor::new();
        ed.set_line("axbc");
        ed.move_home();
        assert!(ed.move_right()); // cursor after 'a' (1)
        assert!(ed.move_right()); // cursor after 'x' (2)
        assert_eq!(ed.feed('\u{8}'), EditAction::Erased); // deletes 'x'
        assert_eq!(ed.line(), "abc");
        assert_eq!(ed.cursor(), 1);
    }

    #[test]
    fn cursor_movement_saturates_at_both_ends() {
        let mut ed = LineEditor::new();
        ed.set_line("hi");
        assert!(!ed.move_right()); // already at end
        ed.move_home();
        assert!(!ed.move_left()); // already at start
        assert!(ed.move_right());
        ed.move_end();
        assert_eq!(ed.cursor(), 2);
    }

    #[test]
    fn insert_at_home_prepends() {
        let mut ed = LineEditor::new();
        ed.set_line("bc");
        ed.move_home();
        ed.feed('a');
        assert_eq!(ed.line(), "abc");
        assert_eq!(ed.cursor(), 1);
    }

    /// The `EditAction` contract: an action tells the caller exactly what
    /// happened to the buffer, so a renderer that draws one cell per
    /// `Echoed` and rubs out one per `Erased` can never drift from
    /// `line()`. Here the buffer is *reconstructed from the actions alone*
    /// and must equal the real one after every single keystroke.
    #[test]
    fn edit_actions_account_for_every_buffered_char() {
        fn step(ed: &mut LineEditor, model: &mut Vec<char>, c: char) {
            let before = model.len();
            match ed.feed(c) {
                EditAction::Echoed(got) => {
                    assert_eq!(got, c, "Echoed {got:?} but {c:?} was fed");
                    model.push(got);
                }
                EditAction::Erased => {
                    assert!(
                        before > 0,
                        "Erased on an empty line: the renderer would rub out the prompt"
                    );
                    model.pop();
                }
                // Both leave the buffer untouched; the caller clears it.
                EditAction::Submitted | EditAction::Ignored => {}
            }
            let rebuilt: String = model.iter().collect();
            assert_eq!(ed.len(), model.len(), "length drifted after feeding {c:?}");
            assert_eq!(ed.line(), rebuilt, "contents drifted after feeding {c:?}");
        }

        let mut ed = LineEditor::new();
        let mut model: Vec<char> = Vec::new();

        // A typed-and-corrected line, ending in a submit (which must leave
        // the buffer alone for the caller to read).
        for c in "echo hi\u{8}\u{8}there\n".chars() {
            step(&mut ed, &mut model, c);
        }
        assert_eq!(ed.line(), "echo there");
        ed.clear();
        model.clear();

        // Backspacing past empty, then over the capacity ceiling and back.
        for c in "\u{8}\u{8}\u{8}".chars() {
            step(&mut ed, &mut model, c);
        }
        for _ in 0..LINE_CAP + 8 {
            step(&mut ed, &mut model, 'x');
        }
        for _ in 0..12 {
            step(&mut ed, &mut model, '\u{8}');
        }
        for c in "back-under-the-cap".chars() {
            step(&mut ed, &mut model, c);
        }

        // Adversarial storm: printable, control, wide, and edit keys mixed.
        let menu: Vec<char> = "ab z9~\u{8}\u{7f}\n\r\t\u{1b}\u{0}é€"
            .chars()
            .chain('\u{80}'..='\u{85}')
            .collect();
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        for _ in 0..20_000 {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            step(
                &mut ed,
                &mut model,
                menu[(seed >> 33) as usize % menu.len()],
            );
        }
        assert!(ed.len() <= LINE_CAP);
    }
}
