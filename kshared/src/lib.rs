//! Pure logic shared by the kernel and testable on the host: address
//! arithmetic, and (in later milestones) command parsing and the line editor.
//! Nothing in this crate may touch hardware or require an allocator.

#![cfg_attr(not(test), no_std)]

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

/// What a fed character did to the line buffer — the caller renders exactly
/// this, so display and buffer can never disagree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditAction {
    /// The char was appended; echo it.
    Echoed(char),
    /// The last char was removed; erase one display cell.
    Erased,
    /// Enter was pressed; read the line with `line()`, then `clear()`.
    Submitted,
    /// Nothing changed (full buffer, empty backspace, unsupported char).
    Ignored,
}

/// Fixed-capacity ASCII line editor: no allocator, host-testable.
pub struct LineEditor {
    buf: [u8; LINE_CAP],
    len: usize,
}

impl LineEditor {
    pub const fn new() -> Self {
        Self {
            buf: [0; LINE_CAP],
            len: 0,
        }
    }

    pub fn feed(&mut self, c: char) -> EditAction {
        match c {
            '\n' | '\r' => EditAction::Submitted,
            '\u{8}' | '\u{7f}' => {
                if self.len > 0 {
                    self.len -= 1;
                    EditAction::Erased
                } else {
                    EditAction::Ignored
                }
            }
            c if c.is_ascii() && !c.is_ascii_control() => {
                if self.len < LINE_CAP {
                    self.buf[self.len] = c as u8;
                    self.len += 1;
                    EditAction::Echoed(c)
                } else {
                    EditAction::Ignored
                }
            }
            _ => EditAction::Ignored,
        }
    }

    pub fn line(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Replaces the contents (history recall). Truncates at capacity;
    /// control and non-ASCII characters are skipped rather than interpreted
    /// (a backspace in `s` must not delete characters already set).
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
