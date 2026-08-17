//! Frame allocation from the bootloader's memory map.

use bootloader_api::info::{MemoryRegionKind, MemoryRegions};
use x86_64::structures::paging::{FrameAllocator, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

/// Boot-memory-map frame allocator. Every frame is zeroed as it is handed
/// out (privacy pillar: no previous owner's bytes ever reach a new mapping).
/// It never frees — reclamation is a roadmap item — and `stats` keeps that
/// honest rather than hidden.
pub struct BootFrameAllocator {
    regions: &'static MemoryRegions,
    physical_memory_offset: VirtAddr,
    next: usize,
}

impl BootFrameAllocator {
    pub fn new(regions: &'static MemoryRegions, physical_memory_offset: VirtAddr) -> Self {
        Self {
            regions,
            physical_memory_offset,
            next: 0,
        }
    }

    fn usable_frames(&self) -> impl Iterator<Item = PhysFrame> + '_ {
        self.regions
            .iter()
            .filter(|region| region.kind == MemoryRegionKind::Usable)
            .flat_map(|region| kshared::frame_starts(region.start, region.end))
            .map(|addr| PhysFrame::containing_address(PhysAddr::new(addr)))
    }

    /// (frames handed out, usable frames total).
    pub fn stats(&self) -> (usize, usize) {
        (self.next, self.usable_frames().count())
    }

    /// Deliberately dirties the frame the next `allocate_frame` will return.
    /// QEMU hands out pre-zeroed RAM, so without this the zero-on-hand-out
    /// selftest would pass even with the scrub deleted — it must prove OUR
    /// zeroing, not the emulator's.
    #[cfg(feature = "selftest")]
    pub fn soil_next_frame(&self, pattern: u8) {
        if let Some(frame) = self.usable_frames().nth(self.next) {
            let virt = self.physical_memory_offset + frame.start_address().as_u64();
            // SAFETY: the frame is usable RAM mapped at the physical-memory
            // offset and has not been handed to anyone yet.
            unsafe { core::ptr::write_bytes(virt.as_mut_ptr::<u8>(), pattern, 4096) };
        }
    }
}

// SAFETY: every frame is returned at most once — `next` only ever grows and
// indexes a deterministic iterator over the fixed boot memory map.
unsafe impl FrameAllocator<Size4KiB> for BootFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        // O(n) walk per allocation; acceptable at boot-time allocation
        // volumes, and it keeps the allocator borrow-free and simple.
        let frame = self.usable_frames().nth(self.next)?;
        self.next += 1;
        let virt = self.physical_memory_offset + frame.start_address().as_u64();
        // SAFETY: usable RAM, mapped at the physical-memory offset, one
        // frame long, and not yet handed to anyone else.
        unsafe { core::ptr::write_bytes(virt.as_mut_ptr::<u8>(), 0, 4096) };
        Some(frame)
    }
}
