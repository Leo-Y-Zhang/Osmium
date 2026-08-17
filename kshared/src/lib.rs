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
}
