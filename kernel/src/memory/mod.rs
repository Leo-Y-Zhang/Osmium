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
    // Enable the NX bit so user pages can be marked non-executable (W^X).
    use x86_64::registers::model_specific::{Efer, EferFlags};
    // SAFETY: the CPU is in long mode (EFER.LME already set by the bootloader);
    // this only adds NXE, which every x86_64 CPU supports.
    unsafe { Efer::update(|f| f.insert(EferFlags::NO_EXECUTE_ENABLE)) };

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
/// this uses `map_to_with_table_flags`. Permissions are adjusted later with
/// [`update_user_page`] (W^X once a program has been copied in).
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

/// Rewrites the WRITABLE and NO_EXECUTE bits of an already-mapped user page,
/// preserving every other flag. The ELF loader maps segments writable+NX for
/// the copy, then locks each page to its final W^X permissions here; the
/// flags are adjusted bit by bit, never rewritten wholesale (an earlier
/// version of the tightening helper did that and silently cleared NX).
pub fn update_user_page(virt: u64, writable: bool, executable: bool) {
    use x86_64::structures::paging::mapper::TranslateResult;
    use x86_64::structures::paging::{Mapper, Page, PageTableFlags, Size4KiB, Translate};
    let mut mapper = MAPPER.lock();
    let mapper = mapper.as_mut().expect("memory not initialised");
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt));
    let TranslateResult::Mapped { flags, .. } = mapper.translate(page.start_address()) else {
        panic!("update_user_page on an unmapped page");
    };
    let mut new = flags & !(PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE);
    if writable {
        new |= PageTableFlags::WRITABLE;
    }
    if !executable {
        new |= PageTableFlags::NO_EXECUTE;
    }
    // SAFETY: `page` is already mapped; only W and NX change, and the TLB is
    // flushed.
    unsafe {
        mapper
            .update_flags(page, new)
            .expect("updating user page flags failed")
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

/// Copies `bytes` into an already-mapped user page at `virt`, writing through
/// the kernel's physical-memory alias rather than the user virtual address.
/// This is what makes the ELF loader's copy SMAP-safe: the destination is
/// reached through a supervisor mapping, so a stray write to a genuine user
/// pointer still faults, while the loader's own copy does not need `stac`.
/// `virt..virt+bytes.len()` must lie within one mapped 4 KiB page.
pub fn copy_into_user_page(virt: u64, bytes: &[u8]) {
    use x86_64::structures::paging::Translate;
    use x86_64::structures::paging::mapper::TranslateResult;
    assert!(
        (virt & 0xfff) as usize + bytes.len() <= 4096,
        "copy_into_user_page crosses a page boundary"
    );
    let mapper = MAPPER.lock();
    let mapper = mapper.as_ref().expect("memory not initialised");
    let (phys, offset) = match mapper.translate(VirtAddr::new(virt)) {
        TranslateResult::Mapped { frame, offset, .. } => (frame.start_address().as_u64(), offset),
        _ => panic!("copy_into_user_page on an unmapped page"),
    };
    let phys_offset = mapper.phys_offset().as_u64();
    let dst = (phys_offset + phys + offset) as *mut u8;
    // SAFETY: `dst` is the kernel's supervisor alias of the just-translated
    // user frame, valid for `bytes.len()` bytes (checked not to cross the
    // page), and nothing else writes it during the copy.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
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

/// Audits the live page tables for user-accessible entries. True iff NO leaf
/// mapping anywhere is user-accessible AND every user-accessible intermediate
/// entry covers the declared user window (the two fixed pages ring 3 runs
/// on). Order-independent by construction: it holds before ring 3 has ever
/// run (no USER bit exists at all) and after any `run_user` teardown — the
/// leaf unmap clears the leaves, and parent tables legitimately keep USER
/// only for the window they exist to reach. A stray user leaf ANYWHERE, at
/// any point the battery looks, fails it; so does a user-accessible
/// intermediate reaching outside the window.
///
/// The walk holds the MAPPER lock for its whole duration and reaches the
/// root through the mapper (a shared reborrow, not a second CR3 read), so it
/// cannot race a mapping operation or alias the mapper's exclusive borrow.
/// Virtual addresses are tracked without canonical sign-extension: high-half
/// entries get raw bases >= 2^47, which can never intersect the low user
/// window — the comparison stays correct.
#[cfg(feature = "selftest")]
pub fn no_stray_user_mappings() -> bool {
    use x86_64::structures::paging::{PageTable, PageTableFlags};

    fn covers_user_window(base: u64, span: u64) -> bool {
        // The whole ELF image window is one 2 MiB page-table region, so an
        // intermediate reaches it iff its range contains the window base;
        // the stack page sits in its own region.
        let contains = |addr: u64| addr >= base && addr < base.saturating_add(span);
        contains(kshared::elf::USER_IMAGE_BASE) || contains(crate::usermode::USER_STACK_ADDR)
    }

    fn walk(table: &PageTable, offset: u64, level: u8, base: u64) -> bool {
        // VA span per entry: L4 512 GiB, L3 1 GiB, L2 2 MiB, L1 4 KiB.
        let span = 1u64 << (12 + 9 * (u64::from(level) - 1));
        for (i, entry) in table.iter().enumerate() {
            if entry.is_unused() {
                continue;
            }
            let entry_base = base + i as u64 * span;
            let user = entry.flags().contains(PageTableFlags::USER_ACCESSIBLE);
            let leaf = level == 1 || entry.flags().contains(PageTableFlags::HUGE_PAGE);
            if leaf {
                if user {
                    return false;
                }
            } else {
                if user && !covers_user_window(entry_base, span) {
                    return false;
                }
                let virt = offset + entry.addr().as_u64();
                // SAFETY: the entry is present and points at a page-table
                // frame; `offset` maps all physical memory, the reference is
                // read-only, and recursion is bounded by the four levels.
                let child = unsafe { &*(virt as *const PageTable) };
                if !walk(child, offset, level - 1, entry_base) {
                    return false;
                }
            }
        }
        true
    }

    let mapper = MAPPER.lock();
    let mapper = mapper.as_ref().expect("memory not initialised");
    let offset = mapper.phys_offset().as_u64();
    walk(mapper.level_4_table(), offset, 4, 0)
}

/// The lowest-level page-table entry flags for `virt`, if mapped. Selftest
/// plumbing for the NX assertions.
#[cfg(feature = "selftest")]
pub fn translate_flags(virt: u64) -> Option<x86_64::structures::paging::PageTableFlags> {
    use x86_64::structures::paging::Translate;
    use x86_64::structures::paging::mapper::TranslateResult;
    let mapper = MAPPER.lock();
    let mapper = mapper.as_ref().expect("memory not initialised");
    match mapper.translate(VirtAddr::new(virt)) {
        TranslateResult::Mapped { flags, .. } => Some(flags),
        _ => None,
    }
}

/// Maps one fresh page at a fixed probe address and returns it; the self-test
/// battery uses it to prove mapping works and frames arrive zeroed. The
/// `user_accessible` argument is normally `false`; flipping it to `true` is
/// the mutation that proves [`no_stray_user_mappings`] catches a
/// user-visible leaf outside the declared window.
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
