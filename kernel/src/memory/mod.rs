//! Physical and virtual memory: the boot-time frame allocator, the offset
//! page table, and the kernel heap. Interrupt handlers never touch anything
//! in this module (TDD locking rule), so the mutexes here cannot deadlock
//! against IRQ context.

pub mod frames;
pub mod heap;

use bootloader_api::info::MemoryRegions;
use frames::BootFrameAllocator;
use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{OffsetPageTable, PageTable};

pub static MAPPER: Mutex<Option<OffsetPageTable<'static>>> = Mutex::new(None);
pub static FRAME_ALLOCATOR: Mutex<Option<BootFrameAllocator>> = Mutex::new(None);

/// Initialises paging and the frame allocator, then maps the kernel heap.
/// `physical_memory_offset` must be the bootloader's all-of-physical-memory
/// mapping (guaranteed by `BOOTLOADER_CONFIG`); the caller fails closed if
/// the bootloader did not provide one.
pub fn init(physical_memory_offset: u64, memory_regions: &'static MemoryRegions) {
    let offset = VirtAddr::new(physical_memory_offset);
    // SAFETY: the bootloader maps all physical memory at `offset`, and this
    // is the only mapper ever constructed over the active page table.
    let mapper = unsafe { OffsetPageTable::new(active_level_4_table(offset), offset) };
    *MAPPER.lock() = Some(mapper);
    *FRAME_ALLOCATOR.lock() = Some(BootFrameAllocator::new(memory_regions, offset));

    let mut mapper = MAPPER.lock();
    let mut allocator = FRAME_ALLOCATOR.lock();
    heap::init(
        mapper.as_mut().expect("mapper just stored"),
        allocator.as_mut().expect("allocator just stored"),
    )
    .expect("mapping the kernel heap failed");
}

/// (frames handed out, usable frames total).
pub fn frame_stats() -> (usize, usize) {
    FRAME_ALLOCATOR
        .lock()
        .as_ref()
        .map(BootFrameAllocator::stats)
        .unwrap_or((0, 0))
}

unsafe fn active_level_4_table(offset: VirtAddr) -> &'static mut PageTable {
    let (frame, _flags) = Cr3::read();
    let virt = offset + frame.start_address().as_u64();
    // SAFETY: the caller guarantees `offset` maps all physical memory, so
    // this points at the live level-4 table, and only one reference is made.
    unsafe { &mut *virt.as_mut_ptr() }
}

/// Maps one fresh page at a fixed probe address and returns it; the
/// self-test battery uses it to prove mapping works and frames arrive zeroed.
#[cfg(feature = "selftest")]
pub fn map_probe_page() -> VirtAddr {
    use x86_64::structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB};

    const PROBE: u64 = 0x_5555_5555_0000;
    let mut mapper = MAPPER.lock();
    let mut allocator = FRAME_ALLOCATOR.lock();
    let mapper = mapper.as_mut().expect("memory not initialised");
    let allocator = allocator.as_mut().expect("memory not initialised");
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(PROBE));
    // Pre-soil the frame so the zero assertion proves the allocator's scrub.
    allocator.soil_next_frame(0xC3);
    let frame = allocator.allocate_frame().expect("out of physical frames");
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
    // SAFETY: a fixed, otherwise-unused virtual page mapped to a frame the
    // allocator just handed out exclusively.
    unsafe {
        mapper
            .map_to(page, frame, flags, allocator)
            .expect("mapping the probe page failed")
            .flush();
    }
    VirtAddr::new(PROBE)
}
