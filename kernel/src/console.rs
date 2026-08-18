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

    /// Proves the rendering path actually touches the framebuffer, not just
    /// the cursor bookkeeping (which advances whether or not a pixel is
    /// written). Returns true iff: a dense glyph drawn at a cell changes some
    /// pixel inside that cell; a pixel three cells away stays background (a
    /// stride/column error would spill there); and an ACCENT pixel's first
    /// framebuffer byte matches the negotiated format's channel order (a
    /// swapped Rgb/Bgr arm writes the wrong byte). ACCENT has distinct R and
    /// B, so the last check discriminates the swap that a grey glyph cannot.
    #[cfg(feature = "selftest")]
    pub fn pixel_probe(&mut self) -> bool {
        use crate::framebuffer::{ACCENT, BACKGROUND, FOREGROUND};
        use bootloader_api::info::PixelFormat;
        self.display.clear(BACKGROUND);
        let mut bg = [0u8; 8];
        let bg_len = match self.display.pixel_bytes(BORDER, BORDER) {
            Some(b) => {
                bg[..b.len()].copy_from_slice(b);
                b.len()
            }
            None => return false,
        };
        let bg = &bg[..bg_len];

        // A dense glyph in cell (0,0): some pixel inside must differ from bg.
        self.draw_cell(0, 0, '#', FOREGROUND, BACKGROUND);
        let mut inside_differs = false;
        for dy in 0..RASTER_HEIGHT.val() {
            for dx in 0..self.char_width {
                if self.display.pixel_bytes(BORDER + dx, BORDER + dy) != Some(bg) {
                    inside_differs = true;
                }
            }
        }
        // Three cells to the right, nothing was drawn: still background.
        let outside_bg = self
            .display
            .pixel_bytes(BORDER + self.char_width * 3, BORDER)
            == Some(bg);

        // Colour fidelity: an ACCENT pixel's first byte is R under Rgb, B
        // under Bgr — a swapped arm fails exactly here.
        self.display.set_pixel(BORDER, BORDER, ACCENT);
        let first = self
            .display
            .pixel_bytes(BORDER, BORDER)
            .and_then(|b| b.first().copied());
        let colour_ok = match self.display.info.pixel_format {
            PixelFormat::Rgb => first == Some(ACCENT.r),
            PixelFormat::Bgr => first == Some(ACCENT.b),
            // U8/Unknown/other layouts have no simple invariant here; the
            // presence + placement checks above still apply, so pass this leg.
            _ => true,
        };
        inside_differs && outside_bg && colour_ok
    }

    /// Proves `clear` fills every row including the last: soils the four
    /// screen corners with a non-background colour, clears, and asserts all
    /// four (and the centre) read back as background. A `fill_rows` off-by-one
    /// that leaves the last row uncopied leaves the soiled bottom corners set,
    /// failing here. Returns the cycles the clear took, for the perf line.
    #[cfg(feature = "selftest")]
    pub fn clear_probe(&mut self) -> Option<(bool, u64)> {
        use crate::framebuffer::{ACCENT, BACKGROUND};
        let w = self.display.info.width;
        let h = self.display.info.height;
        if w == 0 || h == 0 {
            return None;
        }
        let corners = [
            (0, 0),
            (w - 1, 0),
            (0, h - 1),
            (w - 1, h - 1),
            (w / 2, h / 2),
        ];
        for &(x, y) in &corners {
            self.display.set_pixel(x, y, ACCENT);
        }
        let start = crate::time::rdtsc();
        self.display.clear(BACKGROUND);
        let cycles = crate::time::rdtsc().wrapping_sub(start);

        let mut bg = [0u8; 8];
        let n = match self.display.pixel_bytes(0, 0) {
            Some(b) => {
                bg[..b.len()].copy_from_slice(b);
                b.len()
            }
            None => return None,
        };
        let bg = &bg[..n];
        let all_bg = corners
            .iter()
            .all(|&(x, y)| self.display.pixel_bytes(x, y) == Some(bg));
        Some((all_bg, cycles))
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
