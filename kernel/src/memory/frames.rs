//! Frame allocation from the bootloader's memory map.

use alloc::vec::Vec;
use bootloader_api::info::{MemoryRegionKind, MemoryRegions};
use x86_64::structures::paging::{FrameAllocator, PhysFrame, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

/// Boot-memory-map frame allocator. Every frame is zeroed as it is handed
/// out (privacy pillar: no previous owner's bytes ever reach a new mapping),
/// and — since M11 — zeroed again the moment it is reclaimed, so a dead
/// task's data does not linger in RAM waiting for reuse.
///
/// Reclamation is a free list fed by [`Self::deallocate_frame`]; allocation
/// prefers it over the bump cursor, so a warm ring-3 run allocates no new
/// physical memory at all (the battery asserts exactly that). The list holds
/// only frames this allocator handed out — a double free, or a free of a
/// frame that was never allocated, is a real `assert!` (a `debug_assert`
/// is dead code in this release-only kernel).
pub struct BootFrameAllocator {
    regions: &'static MemoryRegions,
    physical_memory_offset: VirtAddr,
    next: usize,
    /// Reclaimed frames, each already scrubbed, ready for reuse. Starts
    /// empty and stays empty until the first teardown, so the `Vec` never
    /// allocates before the heap exists.
    free_list: Vec<PhysFrame>,
    /// Monotone count of every reclamation ever — the `mem` command reports
    /// it, which is how reuse stays observable rather than assumed.
    reclaimed_total: usize,
}

impl BootFrameAllocator {
    pub fn new(regions: &'static MemoryRegions, physical_memory_offset: VirtAddr) -> Self {
        Self {
            regions,
            physical_memory_offset,
            next: 0,
            free_list: Vec::new(),
            reclaimed_total: 0,
        }
    }

    fn usable_frames(&self) -> impl Iterator<Item = PhysFrame> + '_ {
        self.regions
            .iter()
            .filter(|region| region.kind == MemoryRegionKind::Usable)
            .flat_map(|region| kshared::frame_starts(region.start, region.end))
            .map(|addr| PhysFrame::containing_address(PhysAddr::new(addr)))
    }

    fn alias(&self, frame: PhysFrame) -> *mut u8 {
        (self.physical_memory_offset + frame.start_address().as_u64()).as_mut_ptr::<u8>()
    }

    /// (frames in use, usable frames total). "In use" is gross hand-outs
    /// minus the frames currently sitting reclaimed — the number the battery
    /// asserts returns exactly to its baseline after every ring-3 run.
    pub fn stats(&self) -> (usize, usize) {
        (
            self.next - self.free_list.len(),
            self.usable_frames().count(),
        )
    }

    /// (gross bump-cursor hand-outs ever, reclamations ever). The battery
    /// asserts the FIRST number stops moving once the free list can serve a
    /// warm run — that is the observable form of "reuse actually happens".
    pub fn gross_stats(&self) -> (usize, usize) {
        (self.next, self.reclaimed_total)
    }

    /// Returns `frame` to the free list, scrubbing it first. The scrub
    /// happens at free time, not reuse time, so freed contents are gone the
    /// moment the owner lets go (the same rule the heap follows).
    ///
    /// The asserts are the primitive defending itself: only frames this
    /// allocator handed out may come back, and only once. What no assert
    /// here can catch is freeing a frame that is still MAPPED somewhere —
    /// that is prevented by construction instead: the only caller is
    /// `AddressSpace::drop`, which frees exactly the frames that space
    /// recorded allocating, and a space records only what it exclusively
    /// owns.
    pub fn deallocate_frame(&mut self, frame: PhysFrame) {
        assert!(
            !self.free_list.contains(&frame),
            "double free of physical frame {:#x}",
            frame.start_address().as_u64()
        );
        assert!(
            self.usable_frames().take(self.next).any(|f| f == frame),
            "freeing physical frame {:#x}, which this allocator never handed out",
            frame.start_address().as_u64()
        );
        // SAFETY: the frame was handed out by this allocator and its owner
        // is returning it, so nothing else references it; the physical
        // alias maps all RAM.
        unsafe { core::ptr::write_bytes(self.alias(frame), 0, 4096) };
        self.free_list.push(frame);
        self.reclaimed_total += 1;
    }

    /// Deliberately dirties the frame the next `allocate_frame` will return
    /// — the head of the free list if one is queued, else the bump cursor's
    /// next frame. QEMU hands out pre-zeroed RAM, so without this the
    /// zero-on-hand-out selftest would pass even with the scrub deleted — it
    /// must prove OUR zeroing, not the emulator's.
    #[cfg(feature = "selftest")]
    pub fn soil_next_frame(&self, pattern: u8) {
        let target = self
            .free_list
            .last()
            .copied()
            .or_else(|| self.usable_frames().nth(self.next));
        if let Some(frame) = target {
            // SAFETY: the frame is RAM this allocator controls (queued for
            // reuse or not yet handed out) mapped at the physical-memory
            // offset.
            unsafe { core::ptr::write_bytes(self.alias(frame), pattern, 4096) };
        }
    }
}

// SAFETY: every frame is handed out at most once at a time — `next` only
// ever grows and indexes a deterministic iterator over the fixed boot memory
// map, and a frame enters the free list only through `deallocate_frame`,
// whose asserts refuse duplicates and foreign frames.
unsafe impl FrameAllocator<Size4KiB> for BootFrameAllocator {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        if let Some(frame) = self.free_list.pop() {
            // Already scrubbed at free time; scrub again anyway so the
            // zero-on-hand-out guarantee never rests on the free-time scrub
            // still being there.
            // SAFETY: reclaimed RAM this allocator exclusively holds.
            unsafe { core::ptr::write_bytes(self.alias(frame), 0, 4096) };
            return Some(frame);
        }
        // O(n) walk per allocation; acceptable at boot-time allocation
        // volumes, and it keeps the allocator borrow-free and simple.
        let frame = self.usable_frames().nth(self.next)?;
        self.next += 1;
        // SAFETY: usable RAM, mapped at the physical-memory offset, one
        // frame long, and not yet handed to anyone else.
        unsafe { core::ptr::write_bytes(self.alias(frame), 0, 4096) };
        Some(frame)
    }
}
