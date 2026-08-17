//! Physical and virtual memory: the boot-time frame allocator, the offset
//! page table, and the kernel heap. Interrupt handlers never touch anything
//! in this module (TDD locking rule), so the mutexes here cannot deadlock
//! against IRQ context.

pub mod frames;
pub mod heap;

use bootloader_api::info::MemoryRegions;
use core::sync::atomic::{AtomicU64, Ordering};
use frames::BootFrameAllocator;
use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{OffsetPageTable, PageTable};

pub static MAPPER: Mutex<Option<OffsetPageTable<'static>>> = Mutex::new(None);
pub static FRAME_ALLOCATOR: Mutex<Option<BootFrameAllocator>> = Mutex::new(None);
/// The bootloader's physical-memory mapping offset, stored so the page-table
/// audit can walk the live tables without re-deriving it.
static PHYSICAL_MEMORY_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Initialises paging and the frame allocator, then maps the kernel heap.
/// `physical_memory_offset` must be the bootloader's all-of-physical-memory
/// mapping (guaranteed by `BOOTLOADER_CONFIG`); the caller fails closed if
/// the bootloader did not provide one.
pub fn init(physical_memory_offset: u64, memory_regions: &'static MemoryRegions) {
    // Enable the NX bit so user pages can be marked non-executable (W^X).
    use x86_64::registers::model_specific::{Efer, EferFlags};
    // SAFETY: the CPU is in long mode (EFER.LME already set by the bootloader);
    // this only adds NXE, which every x86_64 CPU supports.
    unsafe { Efer::update(|f| f.insert(EferFlags::NO_EXECUTE_ENABLE)) };

    PHYSICAL_MEMORY_OFFSET.store(physical_memory_offset, Ordering::Relaxed);
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

/// Maps one user-accessible page at `virt`. The `USER_ACCESSIBLE` flag must be
/// on the intermediate tables too, not just the leaf — the classic mistake is
/// setting it only on the leaf and getting a fault from ring 3 anyway — so
/// this uses `map_to_with_table_flags`. Returns the mapped page's frame so the
/// caller can later tighten permissions (W^X).
pub fn map_user_page(virt: u64, writable: bool, executable: bool) {
    use x86_64::structures::paging::{FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB};
    let mut mapper = MAPPER.lock();
    let mut allocator = FRAME_ALLOCATOR.lock();
    let mapper = mapper.as_mut().expect("memory not initialised");
    let allocator = allocator.as_mut().expect("memory not initialised");
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt));
    let frame = allocator.allocate_frame().expect("out of physical frames");
    let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    if writable {
        flags |= PageTableFlags::WRITABLE;
    }
    if !executable {
        flags |= PageTableFlags::NO_EXECUTE;
    }
    let parent =
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
    // SAFETY: a fresh, otherwise-unused user virtual page mapped to a frame the
    // allocator just handed out exclusively; parent tables carry USER so ring 3
    // can actually reach it.
    unsafe {
        mapper
            .map_to_with_table_flags(page, frame, flags, parent, allocator)
            .expect("mapping a user page failed")
            .flush();
    }
}

/// Drops the WRITABLE flag on an already-mapped page (W^X for user code, once
/// the kernel has finished copying the program in).
pub fn make_read_only(virt: u64) {
    use x86_64::structures::paging::{Mapper, Page, PageTableFlags, Size4KiB};
    let mut mapper = MAPPER.lock();
    let mapper = mapper.as_mut().expect("memory not initialised");
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt));
    let flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
    // SAFETY: `page` is already mapped by map_user_page; this only narrows its
    // permissions, and the TLB is flushed.
    unsafe {
        mapper
            .update_flags(page, flags)
            .expect("tightening user code flags failed")
            .flush();
    }
}

/// Tears down a user-page mapping. The frame is not reclaimed (bump
/// allocator); only the mapping is cleared, so the address can be reused.
pub fn unmap_user_page(virt: u64) {
    use x86_64::structures::paging::{Mapper, Page, Size4KiB};
    let mut mapper = MAPPER.lock();
    let mapper = mapper.as_mut().expect("memory not initialised");
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt));
    if let Ok((_frame, flush)) = mapper.unmap(page) {
        flush.flush();
    }
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

/// Walks the live page tables and returns true if NO present mapping, at any
/// level, is user-accessible. In v1 there is no ring 3, so the honest
/// invariant is that nothing is reachable from user mode; M6 tightens this to
/// "only the declared user range". The battery asserts it, and mapping the
/// probe page `USER_ACCESSIBLE` makes it fail.
#[cfg(feature = "selftest")]
pub fn no_user_accessible_mappings() -> bool {
    use x86_64::structures::paging::{PageTable, PageTableFlags};

    let offset = PHYSICAL_MEMORY_OFFSET.load(Ordering::Relaxed);
    fn walk(table_phys: u64, offset: u64, level: u8) -> bool {
        let virt = offset + table_phys;
        // SAFETY: `table_phys` is a present page-table frame and `offset` maps
        // all physical memory, so this reference is valid and read-only;
        // recursion is bounded by the four paging levels.
        let table = unsafe { &*(virt as *const PageTable) };
        for entry in table.iter() {
            if entry.is_unused() {
                continue;
            }
            if entry.flags().contains(PageTableFlags::USER_ACCESSIBLE) {
                return false;
            }
            let leaf = level == 1 || entry.flags().contains(PageTableFlags::HUGE_PAGE);
            if !leaf && !walk(entry.addr().as_u64(), offset, level - 1) {
                return false;
            }
        }
        true
    }

    let (l4, _) = Cr3::read();
    walk(l4.start_address().as_u64(), offset, 4)
}

/// Maps one fresh page at a fixed probe address and returns it; the self-test
/// battery uses it to prove mapping works and frames arrive zeroed. The
/// `user_accessible` argument is normally `false`; flipping it to `true` is
/// the mutation that proves [`no_user_accessible_mappings`] catches a
/// user-visible mapping.
#[cfg(feature = "selftest")]
pub fn map_probe_page() -> VirtAddr {
    map_probe_page_inner(false)
}

#[cfg(feature = "selftest")]
fn map_probe_page_inner(user_accessible: bool) -> VirtAddr {
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
    let mut flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
    if user_accessible {
        flags |= PageTableFlags::USER_ACCESSIBLE;
    }
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
