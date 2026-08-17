//! Raw pixel access to the bootloader-provided framebuffer. Every write goes
//! through [`Display::set_pixel`], which honours the *negotiated* pixel
//! format, stride and depth — UEFI GOP and BIOS VBE hand out different
//! layouts, and CI boots both.

use bootloader_api::info::{FrameBuffer, FrameBufferInfo, PixelFormat};

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

pub const BACKGROUND: Rgb = Rgb {
    r: 0x10,
    g: 0x10,
    b: 0x14,
};
pub const FOREGROUND: Rgb = Rgb {
    r: 0xd8,
    g: 0xd8,
    b: 0xd8,
};
pub const ACCENT: Rgb = Rgb {
    r: 0x4f,
    g: 0xc3,
    b: 0xe8,
};
pub const AMBER: Rgb = Rgb {
    r: 0xe8,
    g: 0xb4,
    b: 0x4f,
};
pub const DANGER: Rgb = Rgb {
    r: 0xe8,
    g: 0x4f,
    b: 0x4f,
};
/// Dimmed text for labels and units — present against BACKGROUND above the
/// 4.5:1 floor, but clearly secondary to FOREGROUND.
pub const MUTED: Rgb = Rgb {
    r: 0x8a,
    g: 0x8a,
    b: 0x95,
};
/// Success green for the shell's `[ ok ]` markers; distinct from ACCENT so a
/// pass never reads as ordinary output.
pub const OK: Rgb = Rgb {
    r: 0x7b,
    g: 0xc4,
    b: 0x7f,
};

impl Rgb {
    pub fn luminance(self) -> u8 {
        ((u16::from(self.r) * 30 + u16::from(self.g) * 59 + u16::from(self.b) * 11) / 100) as u8
    }

    /// Linear blend of `fg` over `bg` by `intensity` (0 = bg, 255 = fg).
    pub fn mix(fg: Rgb, bg: Rgb, intensity: u8) -> Rgb {
        let mix = |f: u8, b: u8| {
            ((u16::from(f) * u16::from(intensity) + u16::from(b) * (255 - u16::from(intensity)))
                / 255) as u8
        };
        Rgb {
            r: mix(fg.r, bg.r),
            g: mix(fg.g, bg.g),
            b: mix(fg.b, bg.b),
        }
    }
}

pub struct Display {
    buffer: &'static mut [u8],
    pub info: FrameBufferInfo,
}

impl Display {
    pub fn new(framebuffer: &'static mut FrameBuffer) -> Self {
        let info = framebuffer.info();
        Self {
            buffer: framebuffer.buffer_mut(),
            info,
        }
    }

    pub fn clear(&mut self, color: Rgb) {
        for y in 0..self.info.height {
            for x in 0..self.info.width {
                self.set_pixel(x, y, color);
            }
        }
    }

    /// The raw framebuffer bytes of the pixel at `(x, y)`, in the negotiated
    /// layout — selftest plumbing so the battery can prove a glyph actually
    /// reached the framebuffer (cursor bookkeeping alone updates whether or
    /// not any pixel was written). Returns `None` out of bounds.
    #[cfg(feature = "selftest")]
    pub fn pixel_bytes(&self, x: usize, y: usize) -> Option<&[u8]> {
        if x >= self.info.width || y >= self.info.height {
            return None;
        }
        let bpp = self.info.bytes_per_pixel;
        let offset = (y * self.info.stride + x) * bpp;
        self.buffer.get(offset..offset + bpp)
    }

    pub fn set_pixel(&mut self, x: usize, y: usize, color: Rgb) {
        if x >= self.info.width || y >= self.info.height {
            return;
        }
        let bpp = self.info.bytes_per_pixel;
        let offset = (y * self.info.stride + x) * bpp;
        let Some(pixel) = self.buffer.get_mut(offset..offset + bpp) else {
            return;
        };
        match self.info.pixel_format {
            PixelFormat::Rgb => {
                pixel[0] = color.r;
                pixel[1] = color.g;
                pixel[2] = color.b;
            }
            PixelFormat::Bgr => {
                pixel[0] = color.b;
                pixel[1] = color.g;
                pixel[2] = color.r;
            }
            PixelFormat::U8 => pixel[0] = color.luminance(),
            PixelFormat::Unknown {
                red_position,
                green_position,
                blue_position,
            } => {
                pixel.fill(0);
                for (position, value) in [
                    (red_position, color.r),
                    (green_position, color.g),
                    (blue_position, color.b),
                ] {
                    if let Some(byte) = pixel.get_mut(position as usize) {
                        *byte = value;
                    }
                }
            }
            // PixelFormat is non-exhaustive; degrade to grayscale rather
            // than write garbage in a layout we do not understand.
            _ => pixel[0] = color.luminance(),
        }
    }

    /// Shifts the pixel rows `y_start..y_end` up by `lines_px` and fills the
    /// newly exposed band with `fill`. Rows outside the range are untouched.
    pub fn scroll_region_up(&mut self, y_start: usize, y_end: usize, lines_px: usize, fill: Rgb) {
        let y_end = y_end.min(self.info.height);
        if lines_px == 0 || y_start + lines_px >= y_end {
            return;
        }
        let row_bytes = self.info.stride * self.info.bytes_per_pixel;
        self.buffer.copy_within(
            (y_start + lines_px) * row_bytes..y_end * row_bytes,
            y_start * row_bytes,
        );
        for y in (y_end - lines_px)..y_end {
            for x in 0..self.info.width {
                self.set_pixel(x, y, fill);
            }
        }
    }
}
