//! GDT + TSS. Three IST slots carry dedicated stacks so the faults most
//! likely to arrive on a broken stack — a double fault (e.g. from a kernel
//! stack overflow), an NMI, and a machine check — execute on known-good
//! memory instead of whatever stack was in trouble.

use core::cell::UnsafeCell;
use spin::LazyLock;
use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;
pub const NMI_IST_INDEX: u16 = 1;
pub const MACHINE_CHECK_IST_INDEX: u16 = 2;

const IST_STACK_SIZE: usize = 20 * 1024;
/// Stack the CPU switches to (via TSS RSP0) when `int 0x80` traps in from
/// ring 3 — the ring-0 privilege stack.
const KERNEL_PRIVILEGE_STACK_SIZE: usize = 20 * 1024;

/// Writable static backing for one interrupt stack. A plain (non-mut) static
/// would land in read-only memory; `UnsafeCell` keeps it in `.bss`.
#[repr(C, align(16))]
struct IstStack(UnsafeCell<[u8; IST_STACK_SIZE]>);

// SAFETY: only the CPU touches this memory, and only while handling the fault
// its IST slot is wired to; the kernel never reads or writes it directly.
unsafe impl Sync for IstStack {}

impl IstStack {
    const fn new() -> Self {
        IstStack(UnsafeCell::new([0; IST_STACK_SIZE]))
    }

    /// Top of the stack (stacks grow downwards).
    fn top(&self) -> VirtAddr {
        VirtAddr::from_ptr(self.0.get()) + IST_STACK_SIZE as u64
    }
}

static DOUBLE_FAULT_STACK: IstStack = IstStack::new();
static NMI_STACK: IstStack = IstStack::new();
static MACHINE_CHECK_STACK: IstStack = IstStack::new();

/// Backing for the ring-0 privilege stack (TSS RSP0).
#[repr(C, align(16))]
struct PrivilegeStack(UnsafeCell<[u8; KERNEL_PRIVILEGE_STACK_SIZE]>);
// SAFETY: only the CPU writes it, on a privilege transition into the kernel.
unsafe impl Sync for PrivilegeStack {}
static PRIVILEGE_STACK: PrivilegeStack =
    PrivilegeStack(UnsafeCell::new([0; KERNEL_PRIVILEGE_STACK_SIZE]));

static TSS: LazyLock<TaskStateSegment> = LazyLock::new(|| {
    let mut tss = TaskStateSegment::new();
    tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = DOUBLE_FAULT_STACK.top();
    tss.interrupt_stack_table[NMI_IST_INDEX as usize] = NMI_STACK.top();
    tss.interrupt_stack_table[MACHINE_CHECK_IST_INDEX as usize] = MACHINE_CHECK_STACK.top();
    tss.privilege_stack_table[0] =
        VirtAddr::from_ptr(PRIVILEGE_STACK.0.get()) + KERNEL_PRIVILEGE_STACK_SIZE as u64;
    tss
});

/// Segment selectors the rest of the kernel needs by name.
pub struct Selectors {
    pub kernel_code: SegmentSelector,
    pub kernel_data: SegmentSelector,
    pub user_code: SegmentSelector,
    pub user_data: SegmentSelector,
    tss: SegmentSelector,
}

static GDT: LazyLock<(GlobalDescriptorTable, Selectors)> = LazyLock::new(|| {
    let mut gdt = GlobalDescriptorTable::new();
    let kernel_code = gdt.append(Descriptor::kernel_code_segment());
    let kernel_data = gdt.append(Descriptor::kernel_data_segment());
    let user_data = gdt.append(Descriptor::user_data_segment());
    let user_code = gdt.append(Descriptor::user_code_segment());
    let tss = gdt.append(Descriptor::tss_segment(&TSS));
    (
        gdt,
        Selectors {
            kernel_code,
            kernel_data,
            user_code,
            user_data,
            tss,
        },
    )
});

/// The GDT selectors, for the ring-3 launch path.
pub fn selectors() -> &'static Selectors {
    &GDT.1
}

/// Loads our GDT and TSS, replacing the bootloader's temporary tables.
pub fn init() {
    GDT.0.load();
    // SAFETY: the selectors index the GDT loaded on the line above, and the
    // TSS's IST entries point at valid, unused stack memory.
    unsafe {
        CS::set_reg(GDT.1.kernel_code);
        SS::set_reg(GDT.1.kernel_data);
        load_tss(GDT.1.tss);
    }
}
