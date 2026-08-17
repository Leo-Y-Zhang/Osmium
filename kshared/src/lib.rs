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
}
