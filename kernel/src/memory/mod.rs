//! Physical and virtual memory: the boot-time frame allocator, the offset
//! page table, the kernel heap, and — since M9 — per-task address spaces.
//! Interrupt handlers never touch anything in this module (TDD locking rule)
//! except the raw CR3 write in the scheduler's switch path, which takes no
//! lock; the mutexes here cannot deadlock against IRQ context.
//!
//! The M9 shape: the KERNEL's page table never carries a user-accessible
//! entry — not a leaf, not an intermediate. Every user program is mapped into
//! its own [`AddressSpace`], a per-task PML4 that shares every kernel subtree
//! and deep-copies only the entry-0 chain, so the user window's and stack's
//! 2 MiB slots are private per task while the bootloader's measured low
//! mappings stay reachable (see [`AddressSpace::new_user`]). Context switches
//! load the task's CR3; kernel code keeps running across the switch because
//! every kernel mapping exists identically in every space.

pub mod frames;
pub mod heap;

use alloc::vec::Vec;
use bootloader_api::info::MemoryRegions;
use core::sync::atomic::{AtomicU64, Ordering};
use frames::BootFrameAllocator;
use spin::Mutex;
use x86_64::registers::control::Cr3;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

pub static MAPPER: Mutex<Option<OffsetPageTable<'static>>> = Mutex::new(None);
pub static FRAME_ALLOCATOR: Mutex<Option<BootFrameAllocator>> = Mutex::new(None);

/// The kernel's own CR3, captured at init: what the scheduler restores when
/// the last task exits, and the root the audit walks.
static KERNEL_CR3: AtomicU64 = AtomicU64::new(0);
/// The bootloader's physical-memory offset, kept for building per-space
/// mappers without re-locking `MAPPER`.
static PHYS_OFFSET: AtomicU64 = AtomicU64::new(0);

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

    KERNEL_CR3.store(Cr3::read().0.start_address().as_u64(), Ordering::Relaxed);
    PHYS_OFFSET.store(physical_memory_offset, Ordering::Relaxed);

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

/// The kernel's page-table root, for the scheduler to restore when a run ends.
pub fn kernel_cr3() -> PhysFrame {
    PhysFrame::containing_address(PhysAddr::new(KERNEL_CR3.load(Ordering::Relaxed)))
}

fn phys_offset() -> u64 {
    PHYS_OFFSET.load(Ordering::Relaxed)
}

/// A per-task address space (M9): a private PML4 whose kernel half is shared
/// with — and byte-identical to — the kernel's table, and whose user region
/// (the ELF image window and the user stack page, both in the first GiB) is
/// private to this space. Two spaces can therefore map the SAME virtual
/// addresses to different frames, which is what makes tasks isolated from
/// each other and not merely from the kernel.
///
/// Every frame this space pulls — the constructor's three table clones, each
/// leaf frame `map_user_page` allocates, and the intermediate page-table
/// frames the mapping machinery creates on the way down — is recorded in
/// `owned_frames`, and `Drop` returns exactly that list to the allocator
/// (M11). Recording at the single allocation choke point is what makes the
/// reclaim provably safe: a space can only ever free what it exclusively
/// pulled, never a frame shared with the kernel's table.
pub struct AddressSpace {
    pml4: PhysFrame,
    owned_frames: Vec<PhysFrame>,
}

/// The allocation choke point for a space: hands out frames from the real
/// allocator and records each one against the space that pulled it, so
/// teardown frees exactly what was allocated — including the page-table
/// frames `map_to_with_table_flags` conjures internally, which no call site
/// ever sees.
struct RecordingAllocator<'a> {
    inner: &'a mut BootFrameAllocator,
    log: &'a mut Vec<PhysFrame>,
}

// SAFETY: delegates to `BootFrameAllocator`, which upholds the contract;
// recording a frame does not change its ownership.
unsafe impl FrameAllocator<Size4KiB> for RecordingAllocator<'_> {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        let frame = self.inner.allocate_frame()?;
        self.log.push(frame);
        Some(frame)
    }
}

impl Drop for AddressSpace {
    /// Returns every frame this space pulled to the allocator's free list
    /// (M11), each scrubbed on the way back. Must not run while this space
    /// is the active CR3 — the assert pins it, and the only call sites drop
    /// spaces after the scheduler has restored the kernel's own root (which
    /// the battery separately proves with a CR3 read-back after every run).
    fn drop(&mut self) {
        assert!(
            Cr3::read().0 != self.pml4,
            "dropping the ACTIVE address space"
        );
        let mut guard = FRAME_ALLOCATOR.lock();
        let allocator = guard.as_mut().expect("memory not initialised");
        for frame in self.owned_frames.drain(..) {
            allocator.deallocate_frame(frame);
        }
    }
}

/// The physical-address bits of a page-table entry (4-level paging).
const ENTRY_ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

impl AddressSpace {
    /// Builds this space's table: the kernel's PML4 cloned (entries other
    /// than 0 point at the kernel's own subtrees, shared), with the entry-0
    /// chain — PML4[0] → PDPT → the first GiB's PD — deep-copied so the user
    /// slots in that PD are private while everything else under it is shared.
    ///
    /// The deep copy exists because entry 0 is not empty, which was MEASURED
    /// rather than assumed (2026-08-18, QEMU BIOS+UEFI): the bootloader
    /// leaves two supervisor mappings there — its identity-mapped handover
    /// region at `0x0..0x200000` (PD slot 0) and its early-GDT region at
    /// `0x1000000..0x1200000` (PD slot 8). Both must stay reachable in every
    /// space; only the user window's and stack's 2 MiB slots become private.
    /// The assert below pins the measurement: if a future bootloader ever
    /// claims one of the USER slots, space creation fails loudly instead of
    /// silently sharing user memory with the kernel's low mappings.
    ///
    /// A second invariant is documented rather than asserted (nothing can
    /// observe it here), and its scope is wider than "no new PML4 entries"
    /// (an adversarial review caught the first, narrower wording): **the
    /// kernel must not create any new mapping that a clone would fail to
    /// share while task spaces exist** — that means no new PML4-level
    /// entries anywhere, AND no new mappings at any level under PML4 entry 0,
    /// whose PDPT and first-GiB PD are deep-COPIES here, not shared frames.
    /// A kernel mapping added under entry 0 mid-run (say, a future LAPIC
    /// MMIO window) would exist in the kernel's table but not in live
    /// spaces, and fault only when touched with a task CR3 active. Every
    /// kernel mapping today is created at boot (heap, the bootloader's
    /// mappings) or between runs (the battery's probe page), so this holds
    /// by construction — any milestone that maps at runtime must revisit it.
    pub fn new_user() -> Self {
        let mut allocator = FRAME_ALLOCATOR.lock();
        let allocator = allocator.as_mut().expect("memory not initialised");
        let offset = phys_offset();

        /// Reads entry `idx` of the page table at physical `table`.
        /// SAFETY (of the contained deref): callers pass a valid table frame's
        /// physical address; the alias is read-only.
        fn read_entry(offset: u64, table: u64, idx: usize) -> u64 {
            // SAFETY: per the function contract above.
            unsafe { *((offset + table + (idx as u64) * 8) as *const u64) }
        }
        fn write_entry(offset: u64, table: u64, idx: usize, val: u64) {
            // SAFETY: callers only write tables this space just allocated.
            unsafe { *((offset + table + (idx as u64) * 8) as *mut u64) = val }
        }
        /// Copies one whole page-table frame into a fresh one.
        fn clone_table(allocator: &mut BootFrameAllocator, offset: u64, src_phys: u64) -> u64 {
            let frame = allocator.allocate_frame().expect("out of physical frames");
            let dst_phys = frame.start_address().as_u64();
            // SAFETY: both are physical aliases of whole page-table frames —
            // the source read-only, the destination a zeroed frame the
            // allocator just handed out exclusively. 512 qword entries.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (offset + src_phys) as *const u64,
                    (offset + dst_phys) as *mut u64,
                    512,
                );
            }
            dst_phys
        }
        /// Replaces an entry's target frame, keeping its flag bits.
        fn retarget(entry: u64, new_phys: u64) -> u64 {
            (entry & !ENTRY_ADDR_MASK) | new_phys
        }

        let present = PageTableFlags::PRESENT.bits();
        let huge = PageTableFlags::HUGE_PAGE.bits();
        let kernel_pml4 = KERNEL_CR3.load(Ordering::Relaxed);
        // Every clone below is a frame this space owns and Drop must return.
        let mut owned_frames = Vec::new();
        let mut clone_owned = |allocator: &mut BootFrameAllocator, src_phys: u64| {
            let dst = clone_table(allocator, offset, src_phys);
            owned_frames.push(PhysFrame::containing_address(PhysAddr::new(dst)));
            dst
        };
        let pml4 = clone_owned(allocator, kernel_pml4);

        let e0 = read_entry(offset, kernel_pml4, 0);
        if e0 & present != 0 {
            let kernel_pdpt = e0 & ENTRY_ADDR_MASK;
            let pdpt = clone_owned(allocator, kernel_pdpt);
            let p0 = read_entry(offset, kernel_pdpt, 0);
            if p0 & present != 0 {
                assert!(
                    p0 & huge == 0,
                    "the first GiB is a huge page; the per-slot privacy model cannot subdivide it"
                );
                let kernel_pd = p0 & ENTRY_ADDR_MASK;
                let pd = clone_owned(allocator, kernel_pd);
                // Pin the measurement that the kernel side leaves the user
                // slots vacant, then zero them in the copy. The zeroing is
                // deliberately redundant today (a vacant kernel slot clones
                // as vacant): it defends the one case the assert's PRESENT
                // check waves through — a non-present-but-nonzero kernel
                // entry, which must not survive into a table that map_to
                // will later walk. The deep copy above is what actually
                // privatises the slots between spaces.
                let window_slots = (kshared::elf::USER_IMAGE_BASE >> 21)
                    ..=((kshared::elf::USER_IMAGE_END - 1) >> 21);
                let stack_slot = crate::usermode::USER_STACK_ADDR >> 21;
                for slot in window_slots.chain(core::iter::once(stack_slot)) {
                    assert!(
                        read_entry(offset, kernel_pd, slot as usize) & present == 0,
                        "the kernel's low PD claims user slot {slot}; \
                         the measured vacancy this design rests on no longer holds"
                    );
                    write_entry(offset, pd, slot as usize, 0);
                }
                write_entry(offset, pdpt, 0, retarget(p0, pd));
            }
            write_entry(offset, pml4, 0, retarget(e0, pdpt));
        }

        AddressSpace {
            pml4: PhysFrame::containing_address(PhysAddr::new(pml4)),
            owned_frames,
        }
    }

    /// The page-table root the scheduler loads into CR3 while a task of this
    /// space runs.
    pub fn cr3(&self) -> PhysFrame {
        self.pml4
    }

    /// Runs `f` with a mapper over THIS space's table plus the recording
    /// allocator, so every frame `f` pulls — directly or through the mapping
    /// machinery — lands in this space's ownership log for Drop to reclaim.
    /// The `&mut self` receiver is what makes the exclusive borrow of the
    /// table sound.
    fn with_mapper<R>(
        &mut self,
        f: impl FnOnce(&mut OffsetPageTable, &mut RecordingAllocator) -> R,
    ) -> R {
        let offset = VirtAddr::new(phys_offset());
        let virt = offset + self.pml4.start_address().as_u64();
        // SAFETY: `virt` is the physical alias of this space's PML4, owned
        // exclusively through `&mut self`; `offset` maps all physical memory.
        let table = unsafe { &mut *virt.as_mut_ptr::<PageTable>() };
        // SAFETY: same offset contract as the kernel mapper's construction.
        let mut mapper = unsafe { OffsetPageTable::new(table, offset) };
        let mut guard = FRAME_ALLOCATOR.lock();
        let inner = guard.as_mut().expect("memory not initialised");
        let mut allocator = RecordingAllocator {
            inner,
            log: &mut self.owned_frames,
        };
        f(&mut mapper, &mut allocator)
    }

    /// True iff `virt..virt+len` lies wholly inside the ELF image window or
    /// the user stack page — the only regions a user mapping may occupy. The
    /// arithmetic is overflow-checked so the refusal is profile-independent
    /// (the release kernel wraps silently; a wrapped `end` of 0 would have
    /// waved a top-of-address-space VA through the window comparison — an
    /// adversarial review demonstrated exactly that bypass).
    fn in_user_region(virt: u64, len: u64) -> bool {
        let Some(end) = virt.checked_add(len) else {
            return false;
        };
        let in_image = virt >= kshared::elf::USER_IMAGE_BASE && end <= kshared::elf::USER_IMAGE_END;
        let in_stack = virt >= crate::usermode::USER_STACK_ADDR
            && end <= crate::usermode::USER_STACK_ADDR + 4096;
        in_image || in_stack
    }

    /// Maps one user-accessible page at `virt` in this space. The
    /// `USER_ACCESSIBLE` flag must be on the intermediate tables too, not just
    /// the leaf — the classic mistake is setting it only on the leaf and
    /// getting a fault from ring 3 anyway — so this uses
    /// `map_to_with_table_flags`. Permissions are adjusted later with
    /// [`Self::update_user_page`] (W^X once a program has been copied in).
    ///
    /// The window assert is the primitive defending itself, not decoration:
    /// only the entry-0 chain is private to this space — every other subtree
    /// is SHARED with the kernel's table, and the x86_64 crate ORs the USER
    /// parent flag into already-present entries on the way down. A wild VA
    /// here would therefore stamp USER into the kernel's own page-table
    /// frames for every space at once. The parser bounds every real caller,
    /// but that validation lives in another crate; this makes the one
    /// primitive that grants ring-3 reachability safe independent of it.
    pub fn map_user_page(&mut self, virt: u64, writable: bool, executable: bool) {
        assert!(
            Self::in_user_region(virt, 4096),
            "map_user_page target {virt:#x} is outside the user window"
        );
        self.with_mapper(|mapper, allocator| {
            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt));
            let frame = allocator.allocate_frame().expect("out of physical frames");
            let mut flags = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
            if writable {
                flags |= PageTableFlags::WRITABLE;
            }
            if !executable {
                flags |= PageTableFlags::NO_EXECUTE;
            }
            let parent = PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::USER_ACCESSIBLE;
            // SAFETY: a fresh, otherwise-unused user virtual page in this
            // space, mapped to a frame the allocator just handed out
            // exclusively; parent tables carry USER so ring 3 can reach it.
            // No flush: this space's CR3 is not loaded while the loader runs.
            unsafe {
                mapper
                    .map_to_with_table_flags(page, frame, flags, parent, allocator)
                    .expect("mapping a user page failed")
                    .ignore();
            }
        });
    }

    /// Rewrites the WRITABLE and NO_EXECUTE bits of an already-mapped user
    /// page in this space, preserving every other flag. The ELF loader maps
    /// segments writable+NX for the copy, then locks each page to its final
    /// W^X permissions here; the flags are adjusted bit by bit, never
    /// rewritten wholesale (an earlier version of the tightening helper did
    /// that and silently cleared NX).
    pub fn update_user_page(&mut self, virt: u64, writable: bool, executable: bool) {
        self.with_mapper(|mapper, _| {
            use x86_64::structures::paging::Translate;
            use x86_64::structures::paging::mapper::TranslateResult;
            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(virt));
            let TranslateResult::Mapped { flags, .. } = mapper.translate(page.start_address())
            else {
                panic!("update_user_page on an unmapped page");
            };
            let mut new = flags & !(PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE);
            if writable {
                new |= PageTableFlags::WRITABLE;
            }
            if !executable {
                new |= PageTableFlags::NO_EXECUTE;
            }
            // SAFETY: `page` is already mapped in this space; only W and NX
            // change. No flush needed: the space is not the active CR3 while
            // the loader adjusts it, and loading CR3 flushes non-global TLB
            // entries anyway.
            unsafe {
                mapper
                    .update_flags(page, new)
                    .expect("updating user page flags failed")
                    .ignore();
            }
        });
    }

    /// Copies `bytes` into an already-mapped user page at `virt` in this
    /// space, writing through the kernel's physical-memory alias rather than
    /// the user virtual address. This is what makes the ELF loader's copy
    /// SMAP-safe — and with per-task spaces it is also what makes it possible
    /// at all: the destination VA is not even mapped in the kernel's table.
    /// `virt..virt+bytes.len()` must lie within one mapped 4 KiB page.
    pub fn copy_into_user_page(&mut self, virt: u64, bytes: &[u8]) {
        assert!(
            (virt & 0xfff) as usize + bytes.len() <= 4096,
            "copy_into_user_page crosses a page boundary"
        );
        // Defense in depth: the destination must be inside the user window or
        // the user stack page (overflow-checked — see `in_user_region`). The
        // sole caller passes parser-validated addresses, but that validation
        // lives in another crate; this makes the primitive safe independent
        // of its caller, so a future loader bug cannot turn it into a write
        // over kernel memory.
        assert!(
            Self::in_user_region(virt, bytes.len() as u64),
            "copy_into_user_page target {virt:#x} is outside the user window"
        );
        self.with_mapper(|mapper, _| {
            use x86_64::structures::paging::Translate;
            use x86_64::structures::paging::mapper::TranslateResult;
            let (phys, offset) = match mapper.translate(VirtAddr::new(virt)) {
                TranslateResult::Mapped { frame, offset, .. } => {
                    (frame.start_address().as_u64(), offset)
                }
                _ => panic!("copy_into_user_page on an unmapped page"),
            };
            let dst = (phys_offset() + phys + offset) as *mut u8;
            // SAFETY: `dst` is the kernel's supervisor alias of the
            // just-translated user frame, valid for `bytes.len()` bytes
            // (checked not to cross the page), and nothing else writes it
            // during the copy.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
            }
        });
    }

    /// Audits THIS space's tables: true iff no user-accessible leaf is both
    /// writable and executable. The scratch-space W^X probe proves the
    /// `update_user_page` primitive narrows flags correctly; this proves the
    /// REAL loaded spaces obeyed W^X end to end — without it, a loader that
    /// mapped the stack or a data segment executable passed the battery,
    /// because no program executes from those pages (adversarial-review
    /// finding, observed as exactly that mutation).
    #[cfg(feature = "selftest")]
    pub fn user_mappings_are_wx_clean(&mut self) -> bool {
        fn walk(table: &PageTable, offset: u64, level: u8) -> bool {
            for entry in table.iter() {
                if !entry.flags().contains(PageTableFlags::PRESENT) {
                    continue;
                }
                let leaf = level == 1 || entry.flags().contains(PageTableFlags::HUGE_PAGE);
                if leaf {
                    let user = entry.flags().contains(PageTableFlags::USER_ACCESSIBLE);
                    let writable = entry.flags().contains(PageTableFlags::WRITABLE);
                    let nx = entry.flags().contains(PageTableFlags::NO_EXECUTE);
                    if user && writable && !nx {
                        return false;
                    }
                } else {
                    let virt = offset + entry.addr().as_u64();
                    // SAFETY: present entry -> page-table frame; phys alias;
                    // read-only; recursion bounded by the four levels.
                    let child = unsafe { &*(virt as *const PageTable) };
                    if !walk(child, offset, level - 1) {
                        return false;
                    }
                }
            }
            true
        }
        let offset = phys_offset();
        self.with_mapper(|mapper, _| walk(mapper.level_4_table(), offset, 4))
    }

    /// The lowest-level page-table entry flags for `virt` in this space, if
    /// mapped. Selftest plumbing for the W^X assertions.
    #[cfg(feature = "selftest")]
    pub fn translate_flags(&mut self, virt: u64) -> Option<PageTableFlags> {
        self.with_mapper(|mapper, _| {
            use x86_64::structures::paging::Translate;
            use x86_64::structures::paging::mapper::TranslateResult;
            match mapper.translate(VirtAddr::new(virt)) {
                TranslateResult::Mapped { flags, .. } => Some(flags),
                _ => None,
            }
        })
    }
}

/// The lowest-level page-table entry flags for `virt` in the KERNEL's table,
/// if mapped. Selftest plumbing for the heap-NX assertion; per-space lookups
/// go through [`AddressSpace::translate_flags`].
#[cfg(feature = "selftest")]
pub fn translate_flags(virt: u64) -> Option<PageTableFlags> {
    use x86_64::structures::paging::Translate;
    use x86_64::structures::paging::mapper::TranslateResult;
    let mapper = MAPPER.lock();
    let mapper = mapper.as_ref().expect("memory not initialised");
    match mapper.translate(VirtAddr::new(virt)) {
        TranslateResult::Mapped { flags, .. } => Some(flags),
        _ => None,
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

/// (gross bump hand-outs ever, reclamations ever) — see
/// [`BootFrameAllocator::gross_stats`]. The first number going quiet across
/// warm ring-3 runs is the battery's proof that reuse actually happens.
pub fn frame_gross_stats() -> (usize, usize) {
    FRAME_ALLOCATOR
        .lock()
        .as_ref()
        .map(BootFrameAllocator::gross_stats)
        .unwrap_or((0, 0))
}

/// Proves the free-time scrub (M11): allocate a frame, soil it through the
/// physical alias, free it, and read the SAME alias back BEFORE any
/// reallocation. True iff every byte is zero — i.e. the scrub happened when
/// the frame was let go, not lazily at its next hand-out.
#[cfg(feature = "selftest")]
pub fn scrub_on_free_probe() -> bool {
    let mut guard = FRAME_ALLOCATOR.lock();
    let allocator = guard.as_mut().expect("memory not initialised");
    let frame = allocator
        .allocate_frame()
        .expect("out of physical frames for the scrub probe");
    let alias = (phys_offset() + frame.start_address().as_u64()) as *mut u8;
    // SAFETY: the frame was just handed out exclusively; the alias maps all
    // physical memory.
    unsafe { core::ptr::write_bytes(alias, 0xA5, 4096) };
    allocator.deallocate_frame(frame);
    // SAFETY: reading RAM the allocator holds in its free list; volatile so
    // the soil-then-check cannot be folded away.
    (0..4096).all(|i| unsafe { alias.add(i).read_volatile() } == 0)
}

unsafe fn active_level_4_table(offset: VirtAddr) -> &'static mut PageTable {
    let (frame, _flags) = Cr3::read();
    let virt = offset + frame.start_address().as_u64();
    // SAFETY: the caller guarantees `offset` maps all physical memory, so
    // this points at the live level-4 table, and only one reference is made.
    unsafe { &mut *virt.as_mut_ptr() }
}

/// Audits the KERNEL's page tables for user-accessible entries. True iff no
/// entry anywhere — leaf or intermediate — carries `USER_ACCESSIBLE`. This is
/// the M9 form, strictly stronger than its predecessor: user mappings now
/// live only in per-task [`AddressSpace`]s, so the kernel's own table has no
/// legitimate USER bit at all, ever — not before a run, not during one, not
/// after. A single USER bit at any level, any time the battery looks, fails
/// it.
///
/// The walk holds the MAPPER lock for its whole duration and reaches the
/// root through the mapper (a shared reborrow, not a second CR3 read — which
/// also means it audits the KERNEL table even if a task's CR3 were somehow
/// active), so it cannot race a mapping operation or alias the mapper's
/// exclusive borrow.
#[cfg(feature = "selftest")]
pub fn no_stray_user_mappings() -> bool {
    fn walk(table: &PageTable, offset: u64, level: u8) -> bool {
        for entry in table.iter() {
            // Skip anything not present, not merely all-zero: a non-present
            // entry that is nonetheless non-zero must not be dereferenced as a
            // page-table frame below.
            if !entry.flags().contains(PageTableFlags::PRESENT) {
                continue;
            }
            if entry.flags().contains(PageTableFlags::USER_ACCESSIBLE) {
                return false;
            }
            let leaf = level == 1 || entry.flags().contains(PageTableFlags::HUGE_PAGE);
            if !leaf {
                let virt = offset + entry.addr().as_u64();
                // SAFETY: the entry is present and points at a page-table
                // frame; `offset` maps all physical memory, the reference is
                // read-only, and recursion is bounded by the four levels.
                let child = unsafe { &*(virt as *const PageTable) };
                if !walk(child, offset, level - 1) {
                    return false;
                }
            }
        }
        true
    }

    let mapper = MAPPER.lock();
    let mapper = mapper.as_ref().expect("memory not initialised");
    let offset = mapper.phys_offset().as_u64();
    walk(mapper.level_4_table(), offset, 4)
}

/// Maps one fresh page at a fixed probe address and returns it; the self-test
/// battery uses it to prove mapping works and frames arrive zeroed. The
/// `user_accessible` argument is normally `false`; flipping it to `true` is
/// the mutation that proves [`no_stray_user_mappings`] catches a
/// user-accessible entry in the kernel table.
#[cfg(feature = "selftest")]
pub fn map_probe_page() -> VirtAddr {
    map_probe_page_inner(false)
}

#[cfg(feature = "selftest")]
fn map_probe_page_inner(user_accessible: bool) -> VirtAddr {
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
