//! GDT + TSS. IST slot 0 carries a dedicated stack so a double fault — e.g.
//! from a kernel stack overflow — executes on known-good memory instead of
//! the very stack that just overflowed.

use core::cell::UnsafeCell;
use spin::LazyLock;
use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;
const DOUBLE_FAULT_STACK_SIZE: usize = 20 * 1024;

/// Writable static backing for the double-fault stack. A plain (non-mut)
/// static would land in read-only memory; UnsafeCell keeps it in .bss.
#[repr(C, align(16))]
struct DoubleFaultStack(UnsafeCell<[u8; DOUBLE_FAULT_STACK_SIZE]>);

// SAFETY: only the CPU touches this memory, and only while handling a double
// fault; the kernel never reads or writes it directly.
unsafe impl Sync for DoubleFaultStack {}

static DOUBLE_FAULT_STACK: DoubleFaultStack =
    DoubleFaultStack(UnsafeCell::new([0; DOUBLE_FAULT_STACK_SIZE]));

static TSS: LazyLock<TaskStateSegment> = LazyLock::new(|| {
    let mut tss = TaskStateSegment::new();
    tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
        let start = VirtAddr::from_ptr(DOUBLE_FAULT_STACK.0.get());
        start + DOUBLE_FAULT_STACK_SIZE as u64 // stacks grow downwards
    };
    tss
});

struct Selectors {
    code: SegmentSelector,
    data: SegmentSelector,
    tss: SegmentSelector,
}

static GDT: LazyLock<(GlobalDescriptorTable, Selectors)> = LazyLock::new(|| {
    let mut gdt = GlobalDescriptorTable::new();
    let code = gdt.append(Descriptor::kernel_code_segment());
    let data = gdt.append(Descriptor::kernel_data_segment());
    let tss = gdt.append(Descriptor::tss_segment(&TSS));
    (gdt, Selectors { code, data, tss })
});

/// Loads our GDT and TSS, replacing the bootloader's temporary tables.
pub fn init() {
    GDT.0.load();
    // SAFETY: the selectors index the GDT loaded on the line above, and the
    // TSS's IST entry points at valid, unused stack memory.
    unsafe {
        CS::set_reg(GDT.1.code);
        SS::set_reg(GDT.1.data);
        load_tss(GDT.1.tss);
    }
}
