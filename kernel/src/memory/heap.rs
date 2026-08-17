//! The kernel heap: 1 MiB mapped at a fixed virtual address, backed by
//! `linked_list_allocator` behind a zero-on-free wrapper.

use core::alloc::{GlobalAlloc, Layout};
use linked_list_allocator::LockedHeap;
use x86_64::VirtAddr;
use x86_64::structures::paging::mapper::MapToError;
use x86_64::structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB};

pub const HEAP_START: u64 = 0x_4444_4444_0000;
pub const HEAP_SIZE: u64 = 1024 * 1024;

/// Zeroes every freed block BEFORE handing it back to the allocator
/// (privacy pillar). The order matters: the allocator writes its free-list
/// node into the head of the block during `dealloc`, so scrubbing afterwards
/// would corrupt the free list.
struct ZeroOnFree(LockedHeap);

// SAFETY: defers entirely to LockedHeap's GlobalAlloc contract; the wrapper
// only adds a scrub of memory its caller has already relinquished.
unsafe impl GlobalAlloc for ZeroOnFree {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: same contract as the caller's; passed straight through.
        unsafe { self.0.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` is a live allocation of `layout.size()` bytes that
        // the caller is giving up; zeroing it and then freeing it is sound.
        unsafe {
            core::ptr::write_bytes(ptr, 0, layout.size());
            self.0.dealloc(ptr, layout);
        }
    }
}

#[global_allocator]
static ALLOCATOR: ZeroOnFree = ZeroOnFree(LockedHeap::empty());

/// (used, free) bytes.
pub fn stats() -> (usize, usize) {
    let heap = ALLOCATOR.0.lock();
    (heap.used(), heap.free())
}

pub fn init(
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), MapToError<Size4KiB>> {
    let start = VirtAddr::new(HEAP_START);
    let end = start + HEAP_SIZE - 1u64;
    let range = Page::range_inclusive(
        Page::containing_address(start),
        Page::containing_address(end),
    );
    for page in range {
        let frame = frame_allocator
            .allocate_frame()
            .ok_or(MapToError::FrameAllocationFailed)?;
        // The heap holds data, never code: NO_EXECUTE closes what would
        // otherwise be a writable-and-executable megabyte (NXE is enabled
        // before this runs, so the bit is honoured).
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
        // SAFETY: pages in a fixed, otherwise-unused virtual range, each
        // mapped to a frame the allocator just handed out exclusively.
        unsafe { mapper.map_to(page, frame, flags, frame_allocator)?.flush() };
    }
    // SAFETY: the range above was just mapped and belongs solely to the heap.
    unsafe {
        ALLOCATOR
            .0
            .lock()
            .init(HEAP_START as *mut u8, HEAP_SIZE as usize);
    }
    Ok(())
}
