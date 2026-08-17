//! Glyph text console on the framebuffer: monospace rendering with
//! anti-aliased bitmap glyphs, wrapping and scrolling. The console lock is
//! never taken from interrupt context — that rule is what makes locking here
//! deadlock-free.

use crate::framebuffer::{ACCENT, BACKGROUND, Display, FOREGROUND, Rgb};
use bootloader_api::info::FrameBufferInfo;
use core::fmt;
use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster, get_raster_width};
use spin::Mutex;

const FONT_WEIGHT: FontWeight = FontWeight::Regular;
const RASTER_HEIGHT: RasterHeight = RasterHeight::Size16;
/// Pixel margin around the text area; scrolling leaves it intact.
const BORDER: usize = 4;

pub struct Console {
    display: Display,
    char_width: usize,
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
    fg: Rgb,
    /// Where the current input line begins, if the shell is editing one.
    input_start: Option<(usize, usize)>,
}

impl Console {
    pub fn new(mut display: Display) -> Self {
        display.clear(BACKGROUND);
        let char_width = get_raster_width(FONT_WEIGHT, RASTER_HEIGHT);
        let cols = display.info.width.saturating_sub(2 * BORDER) / char_width;
        let rows = display.info.height.saturating_sub(2 * BORDER) / RASTER_HEIGHT.val();
        Self {
            display,
            char_width,
            cols,
            rows,
            col: 0,
            row: 0,
            fg: FOREGROUND,
            input_start: None,
        }
    }

    pub fn set_color(&mut self, fg: Rgb) {
        self.fg = fg;
    }

    /// (row, column) of the cursor; used by the self-test battery.
    #[cfg_attr(not(feature = "selftest"), allow(dead_code))]
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn info(&self) -> FrameBufferInfo {
        self.display.info
    }

    /// Renders one character cell with explicit foreground and background,
    /// filling the whole cell. A space glyph is all-background, so this is
    /// also how a cell is cleared or a caret's inverse block is drawn.
    fn draw_cell(&mut self, row: usize, col: usize, c: char, fg: Rgb, bg: Rgb) {
        let raster = get_raster(c, FONT_WEIGHT, RASTER_HEIGHT).unwrap_or_else(|| {
            get_raster('?', FONT_WEIGHT, RASTER_HEIGHT)
                .expect("fallback glyph '?' missing from the compiled-in font")
        });
        let x0 = BORDER + col * self.char_width;
        let y0 = BORDER + row * RASTER_HEIGHT.val();
        for (dy, row_pixels) in raster.raster().iter().enumerate() {
            for (dx, intensity) in row_pixels.iter().enumerate() {
                self.display
                    .set_pixel(x0 + dx, y0 + dy, Rgb::mix(fg, bg, *intensity));
            }
        }
    }

    /// Clears the whole screen and homes the cursor.
    pub fn clear_screen(&mut self) {
        self.display.clear(BACKGROUND);
        self.row = 0;
        self.col = 0;
        self.input_start = None;
    }

    /// Marks where the current input line starts, so [`render_input`] knows
    /// which cells it owns. Call right after drawing the prompt.
    pub fn begin_input(&mut self) {
        self.input_start = Some((self.row, self.col));
    }

    /// Redraws the input line and the caret. Single-row and clipped to the
    /// prompt row: the buffer may hold more than fits, but interactive lines
    /// are far shorter than a row, so this trades an unreachable wrap case for
    /// caret positioning that can never desync from the editor.
    pub fn render_input(&mut self, text: &str, cursor: usize) {
        let Some((row, start_col)) = self.input_start else {
            return;
        };
        let avail = self.cols.saturating_sub(start_col);
        if avail == 0 {
            return;
        }
        for i in 0..avail {
            self.draw_cell(row, start_col + i, ' ', FOREGROUND, BACKGROUND);
        }
        let visible = avail - 1; // leave a cell for a caret at end-of-line
        for (i, c) in text.chars().take(visible).enumerate() {
            self.draw_cell(row, start_col + i, c, self.fg, BACKGROUND);
        }
        let caret = cursor.min(visible);
        let under = text.chars().nth(cursor).unwrap_or(' ');
        self.draw_cell(row, start_col + caret, under, BACKGROUND, ACCENT);
        self.row = row;
        self.col = (start_col + text.chars().count().min(visible)).min(self.cols);
    }

    /// Redraws the committed line without a caret and moves to the next line.
    /// Called when the input is submitted.
    pub fn commit_input(&mut self, text: &str) {
        if let Some((row, start_col)) = self.input_start {
            let avail = self.cols.saturating_sub(start_col);
            for i in 0..avail {
                self.draw_cell(row, start_col + i, ' ', FOREGROUND, BACKGROUND);
            }
            for (i, c) in text.chars().take(avail).enumerate() {
                self.draw_cell(row, start_col + i, c, self.fg, BACKGROUND);
            }
            self.row = row;
            self.col = (start_col + text.chars().count().min(avail)).min(self.cols);
        }
        self.input_start = None;
        self.newline();
    }

    pub fn write_char(&mut self, c: char) {
        match c {
            '\n' => self.newline(),
            '\r' => self.col = 0,
            '\t' => {
                for _ in 0..4 {
                    self.write_char(' ');
                }
            }
            c => {
                if self.col >= self.cols {
                    self.newline();
                }
                self.draw_cell(self.row, self.col, c, self.fg, BACKGROUND);
                self.col += 1;
            }
        }
    }

    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 >= self.rows {
            let line = RASTER_HEIGHT.val();
            self.display
                .scroll_region_up(BORDER, BORDER + self.rows * line, line, BACKGROUND);
        } else {
            self.row += 1;
        }
    }
}

impl fmt::Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for c in s.chars() {
            self.write_char(c);
        }
        Ok(())
    }
}

pub static CONSOLE: Mutex<Option<Console>> = Mutex::new(None);

pub fn init(display: Display) {
    *CONSOLE.lock() = Some(Console::new(display));
}

#[cfg_attr(not(feature = "selftest"), allow(dead_code))]
pub fn is_initialised() -> bool {
    CONSOLE.lock().is_some()
}

/// Reclaims the console lock on the panic path. The caller must have disabled
/// interrupts; the previous holder never resumes, so this cannot race.
pub unsafe fn force_unlock_for_panic() {
    if CONSOLE.is_locked() {
        // SAFETY: guaranteed by the caller as documented above.
        unsafe { CONSOLE.force_unlock() };
    }
}

/// Runs `f` with the console if it is initialised. Never called from
/// interrupt context; do not log inside `f` (the logger takes this lock).
pub fn with_console<R>(f: impl FnOnce(&mut Console) -> R) -> Option<R> {
    CONSOLE.lock().as_mut().map(f)
}
