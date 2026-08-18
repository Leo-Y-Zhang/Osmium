//! GDT + TSS. Three IST slots carry dedicated stacks so the faults most
//! likely to arrive on a broken stack — a double fault (e.g. from a kernel
//! stack overflow), an NMI, and a machine check — execute on known-good
//! memory instead of whatever stack was in trouble.
//!
//! Since M8 the TSS is *mutable state*, not a build-once table: RSP0 — the
//! stack the CPU switches to when ring 3 traps in — is rewritten on every
//! context switch so each user task traps onto its own kernel stack. The TSS
//! therefore lives in an `UnsafeCell` and its GDT descriptor is built from a
//! raw pointer (`tss_segment_unchecked`), so no `&'static` shared reference
//! is ever held across a mutation.

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
/// Stack the CPU switches to (via TSS RSP0) when `int 0x80` or an interrupt
/// traps in from ring 3 while no scheduler task is current — the default
/// ring-0 privilege stack. While the scheduler runs, RSP0 points at the
/// current task's own kernel stack instead ([`set_privilege_stack`]).
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

/// Backing for the default ring-0 privilege stack (TSS RSP0).
#[repr(C, align(16))]
struct PrivilegeStack(UnsafeCell<[u8; KERNEL_PRIVILEGE_STACK_SIZE]>);
// SAFETY: only the CPU writes it, on a privilege transition into the kernel.
unsafe impl Sync for PrivilegeStack {}
static PRIVILEGE_STACK: PrivilegeStack =
    PrivilegeStack(UnsafeCell::new([0; KERNEL_PRIVILEGE_STACK_SIZE]));

/// The TSS, in an `UnsafeCell` because RSP0 changes at runtime (per-task
/// kernel stacks). All other fields are written once in [`init`], before the
/// table is loaded.
struct TssCell(UnsafeCell<TaskStateSegment>);
// SAFETY: written by `init` (single-threaded early boot) and by
// `set_privilege_stack`, which is only called with interrupts disabled on
// this single-core kernel; the CPU reads it only on a privilege transition,
// which cannot happen while interrupts are off and CPL is 0.
unsafe impl Sync for TssCell {}
static TSS: TssCell = TssCell(UnsafeCell::new(TaskStateSegment::new()));

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
    // SAFETY: the pointer is to a static TSS that lives for the program's
    // whole lifetime; `init` populates it before the GDT is loaded, and later
    // mutation is confined to RSP0 under the `TssCell` discipline above.
    let tss = gdt.append(unsafe { Descriptor::tss_segment_unchecked(TSS.0.get()) });
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

/// Points TSS RSP0 at `top`: the stack the CPU will use for the next trap in
/// from ring 3. The scheduler calls this on every context switch so each task
/// traps onto its own kernel stack.
///
/// Must be called with interrupts disabled (every caller is either `init`,
/// the timer/syscall path where the interrupt gate already cleared IF, or the
/// launcher inside its own cli window) — otherwise an interrupt arriving from
/// ring 3 between the switch decision and this write would land on the wrong
/// stack.
pub fn set_privilege_stack(top: VirtAddr) {
    // A real assert, not a debug_assert: the kernel is only ever built
    // --release (xtask always passes it), where debug_asserts are dead code —
    // and this is the check that keeps a ring-3 trap off the wrong stack.
    assert!(
        !x86_64::instructions::interrupts::are_enabled(),
        "RSP0 rewritten with interrupts enabled"
    );
    // SAFETY: writing one field of the static TSS; no shared reference to the
    // TSS exists (the descriptor holds only its address), and the CPU cannot
    // read RSP0 concurrently because a ring-3 -> ring-0 transition cannot
    // occur while interrupts are disabled and we are already at CPL 0.
    unsafe { (*TSS.0.get()).privilege_stack_table[0] = top };
}

/// Top of the default (non-scheduler) privilege stack, so the scheduler can
/// restore RSP0 when the last task exits.
pub fn default_privilege_stack_top() -> VirtAddr {
    VirtAddr::from_ptr(PRIVILEGE_STACK.0.get()) + KERNEL_PRIVILEGE_STACK_SIZE as u64
}

/// Populates the TSS, then loads our GDT and TSS, replacing the bootloader's
/// temporary tables.
pub fn init() {
    // SAFETY: single-threaded early boot, before the GDT (and thus the TSS
    // descriptor) is loaded; nothing else references the TSS yet.
    unsafe {
        let tss = &mut *TSS.0.get();
        tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = DOUBLE_FAULT_STACK.top();
        tss.interrupt_stack_table[NMI_IST_INDEX as usize] = NMI_STACK.top();
        tss.interrupt_stack_table[MACHINE_CHECK_IST_INDEX as usize] = MACHINE_CHECK_STACK.top();
        tss.privilege_stack_table[0] = default_privilege_stack_top();
    }
    GDT.0.load();
    // SAFETY: the selectors index the GDT loaded on the line above, and the
    // TSS's IST entries point at valid, unused stack memory.
    unsafe {
        CS::set_reg(GDT.1.kernel_code);
        SS::set_reg(GDT.1.kernel_data);
        load_tss(GDT.1.tss);
    }
}
