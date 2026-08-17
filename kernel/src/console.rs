//! Glyph text console on the framebuffer: monospace rendering with
//! anti-aliased bitmap glyphs, wrapping and scrolling. The console lock is
//! never taken from interrupt context — that rule is what makes locking here
//! deadlock-free.

use crate::framebuffer::{BACKGROUND, Display, FOREGROUND, Rgb};
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

    // The next three are the shell's surface; the selftest build has no shell.
    #[cfg_attr(feature = "selftest", allow(dead_code))]
    pub fn info(&self) -> FrameBufferInfo {
        self.display.info
    }

    /// Clears the whole screen and homes the cursor.
    #[cfg_attr(feature = "selftest", allow(dead_code))]
    pub fn clear_screen(&mut self) {
        self.display.clear(BACKGROUND);
        self.row = 0;
        self.col = 0;
    }

    /// Steps the cursor back one cell (wrapping to the previous row) and
    /// blanks that cell; the shell's backspace.
    #[cfg_attr(feature = "selftest", allow(dead_code))]
    pub fn erase_last_char(&mut self) {
        if self.col == 0 {
            if self.row == 0 {
                return;
            }
            self.row -= 1;
            self.col = self.cols - 1;
        } else {
            self.col -= 1;
        }
        let x0 = BORDER + self.col * self.char_width;
        let y0 = BORDER + self.row * RASTER_HEIGHT.val();
        for dy in 0..RASTER_HEIGHT.val() {
            for dx in 0..self.char_width {
                self.display.set_pixel(x0 + dx, y0 + dy, BACKGROUND);
            }
        }
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
                let raster = get_raster(c, FONT_WEIGHT, RASTER_HEIGHT).unwrap_or_else(|| {
                    get_raster('?', FONT_WEIGHT, RASTER_HEIGHT)
                        .expect("fallback glyph '?' missing from the compiled-in font")
                });
                let x0 = BORDER + self.col * self.char_width;
                let y0 = BORDER + self.row * RASTER_HEIGHT.val();
                for (dy, row_pixels) in raster.raster().iter().enumerate() {
                    for (dx, intensity) in row_pixels.iter().enumerate() {
                        let color = Rgb::mix(self.fg, BACKGROUND, *intensity);
                        self.display.set_pixel(x0 + dx, y0 + dy, color);
                    }
                }
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
