//! IDT, exception handlers, and the legacy PIC + PIT wiring.
//!
//! Locking rules (see TDD): interrupt handlers never take the console or
//! serial locks and never allocate — they only touch atomics, lock-free
//! queues and the PIC (whose mutex the main thread only uses before
//! interrupts are first enabled).

use crate::gdt;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use pic8259::ChainedPics;
use spin::{LazyLock, Mutex};
use x86_64::instructions::port::Port;
use x86_64::registers::control::Cr2;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};

pub const PIC_1_OFFSET: u8 = 32;
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

// SAFETY: 32/40 remap both PICs clear of the CPU-exception vectors 0-31;
// the hardware is only programmed in init(), before interrupts are enabled.
static PICS: Mutex<ChainedPics> =
    Mutex::new(unsafe { ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) });

#[derive(Clone, Copy)]
#[repr(u8)]
pub enum InterruptIndex {
    Timer = PIC_1_OFFSET,
    Keyboard,
}

impl InterruptIndex {
    fn as_u8(self) -> u8 {
        self as u8
    }
}

/// PIT channel-0 ticks since boot (100 Hz).
pub static TICKS: AtomicU64 = AtomicU64::new(0);
/// Breakpoint exceptions handled; the self-test battery asserts this moves.
pub static BREAKPOINT_HITS: AtomicUsize = AtomicUsize::new(0);

/// Set by the self-test battery immediately before it deliberately overflows
/// the kernel stack. Only THAT double fault converts into the battery's
/// success verdict; an unexpected one is still a fatal bug.
#[cfg(feature = "selftest")]
pub static EXPECTING_DOUBLE_FAULT: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Generates a named panicking handler for a fault that carries no error
/// code. A named handler means a fault gets its correct diagnosis instead of
/// re-faulting into a misleading #GP off a non-present gate.
macro_rules! exception {
    ($name:ident, $msg:literal) => {
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame) {
            panic!(concat!($msg, "\n{:#?}"), frame);
        }
    };
}

/// The same, for faults that push an error code.
macro_rules! exception_ec {
    ($name:ident, $msg:literal) => {
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame, error_code: u64) {
            panic!(
                concat!($msg, " (error code {:#x})\n{:#?}"),
                error_code, frame
            );
        }
    };
}

exception!(divide_error_handler, "divide error (#DE)");
exception!(debug_handler, "debug exception (#DB)");
exception!(nmi_handler, "non-maskable interrupt (NMI)");
exception!(overflow_handler, "overflow (#OF)");
exception!(bound_range_handler, "bound range exceeded (#BR)");
exception!(device_not_available_handler, "device not available (#NM)");
exception!(x87_floating_point_handler, "x87 floating-point (#MF)");
exception!(simd_floating_point_handler, "SIMD floating-point (#XM)");
exception!(virtualization_handler, "virtualization (#VE)");
exception_ec!(invalid_tss_handler, "invalid TSS (#TS)");
exception_ec!(segment_not_present_handler, "segment not present (#NP)");
exception_ec!(stack_segment_fault_handler, "stack-segment fault (#SS)");
exception_ec!(alignment_check_handler, "alignment check (#AC)");
exception_ec!(cp_protection_handler, "control-protection (#CP)");

extern "x86-interrupt" fn machine_check_handler(frame: InterruptStackFrame) -> ! {
    panic!("machine check (#MC) — hardware reported an unrecoverable error\n{frame:#?}");
}

static IDT: LazyLock<InterruptDescriptorTable> = LazyLock::new(|| {
    let mut idt = InterruptDescriptorTable::new();
    // Faults with no error code.
    idt.divide_error.set_handler_fn(divide_error_handler);
    idt.debug.set_handler_fn(debug_handler);
    idt.breakpoint.set_handler_fn(breakpoint_handler);
    idt.overflow.set_handler_fn(overflow_handler);
    idt.bound_range_exceeded.set_handler_fn(bound_range_handler);
    idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
    idt.device_not_available
        .set_handler_fn(device_not_available_handler);
    idt.x87_floating_point
        .set_handler_fn(x87_floating_point_handler);
    idt.simd_floating_point
        .set_handler_fn(simd_floating_point_handler);
    idt.virtualization.set_handler_fn(virtualization_handler);
    // Faults with an error code.
    idt.invalid_tss.set_handler_fn(invalid_tss_handler);
    idt.segment_not_present
        .set_handler_fn(segment_not_present_handler);
    idt.stack_segment_fault
        .set_handler_fn(stack_segment_fault_handler);
    idt.general_protection_fault
        .set_handler_fn(general_protection_fault_handler);
    idt.page_fault.set_handler_fn(page_fault_handler);
    idt.alignment_check.set_handler_fn(alignment_check_handler);
    idt.cp_protection_exception
        .set_handler_fn(cp_protection_handler);
    // SAFETY: each IST index below is backed by a real, dedicated stack
    // (gdt.rs) — the whole point is to run these on known-good memory.
    unsafe {
        idt.double_fault
            .set_handler_fn(double_fault_handler)
            .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
        idt.non_maskable_interrupt
            .set_handler_fn(nmi_handler)
            .set_stack_index(gdt::NMI_IST_INDEX);
        idt.machine_check
            .set_handler_fn(machine_check_handler)
            .set_stack_index(gdt::MACHINE_CHECK_IST_INDEX);
    }
    idt[InterruptIndex::Timer.as_u8()].set_handler_fn(timer_handler);
    idt[InterruptIndex::Keyboard.as_u8()].set_handler_fn(keyboard_handler);
    // A floating line can produce a spurious IRQ7 even with devices masked.
    idt[PIC_1_OFFSET + 7].set_handler_fn(spurious_handler);
    // int 0x80 software system call, callable from ring 3.
    // SAFETY: the address is a valid naked entry that ends in `iretq` (or, for
    // SYS_EXIT, restores the saved kernel stack and returns); DPL 3 lets a
    // ring-3 program issue `int 0x80` without a #GP.
    unsafe {
        idt[crate::usermode::SYSCALL_VECTOR]
            .set_handler_addr(x86_64::VirtAddr::new(crate::usermode::syscall_entry_addr()))
            .set_privilege_level(x86_64::PrivilegeLevel::Ring3);
    }
    idt
});

/// The exception vectors this kernel installs a handler on. The self-test
/// battery reads the loaded IDT back and asserts each of these is present.
#[cfg(feature = "selftest")]
pub const INSTALLED_EXCEPTION_VECTORS: &[u8] = &[
    0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 12, 13, 14, 16, 17, 18, 19, 20, 21,
];

/// PIT channel 0 reload value for ~100 Hz (1_193_182 Hz / 11932).
const PIT_DIVISOR: u16 = 11932;
pub const TICK_HZ: u64 = 100;

/// Loads the IDT, programs the PIT, remaps the PIC and enables interrupts.
/// Must run after `gdt::init`.
pub fn init() {
    IDT.load();
    let mut pics = PICS.lock();
    // SAFETY: standard 8259 initialisation with vectors remapped to 32..47,
    // then PIT programming on ports 0x43/0x40; both are the canonical
    // sequences for this hardware and run with interrupts still disabled.
    unsafe {
        pics.initialize();
        let mut pit_cmd: Port<u8> = Port::new(0x43);
        let mut pit_ch0: Port<u8> = Port::new(0x40);
        pit_cmd.write(0x36); // channel 0, lobyte/hibyte, rate generator
        pit_ch0.write((PIT_DIVISOR & 0xff) as u8);
        pit_ch0.write((PIT_DIVISOR >> 8) as u8);
        // Unmask only the timer and keyboard lines.
        pics.write_masks(0b1111_1100, 0b1111_1111);
    }
    drop(pics); // release before enabling: handlers take this lock for EOI
    x86_64::instructions::interrupts::enable();
}

fn end_of_interrupt(index: InterruptIndex) {
    // SAFETY: acknowledging the interrupt we are currently handling. The main
    // thread never holds the PIC lock once interrupts are enabled, so this
    // cannot deadlock.
    unsafe { PICS.lock().notify_end_of_interrupt(index.as_u8()) };
}

extern "x86-interrupt" fn timer_handler(_frame: InterruptStackFrame) {
    TICKS.fetch_add(1, Ordering::Relaxed);
    end_of_interrupt(InterruptIndex::Timer);
}

extern "x86-interrupt" fn keyboard_handler(_frame: InterruptStackFrame) {
    // The controller only delivers the next IRQ once the buffer is read.
    let mut port: Port<u8> = Port::new(0x60);
    // SAFETY: reading the PS/2 data port inside its own interrupt handler.
    let scancode = unsafe { port.read() };
    crate::task::keyboard::enqueue_scancode(scancode);
    end_of_interrupt(InterruptIndex::Keyboard);
}

extern "x86-interrupt" fn spurious_handler(_frame: InterruptStackFrame) {
    // Spurious IRQ7: no EOI — the PIC does not consider it in service.
}

extern "x86-interrupt" fn breakpoint_handler(_frame: InterruptStackFrame) {
    BREAKPOINT_HITS.fetch_add(1, Ordering::Relaxed);
}

extern "x86-interrupt" fn invalid_opcode_handler(frame: InterruptStackFrame) {
    panic!("invalid opcode\n{frame:#?}");
}

extern "x86-interrupt" fn general_protection_fault_handler(
    frame: InterruptStackFrame,
    error_code: u64,
) {
    panic!("general protection fault (error code {error_code:#x})\n{frame:#?}");
}

extern "x86-interrupt" fn page_fault_handler(
    frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    panic!(
        "page fault at {:?} ({error_code:?})\n{frame:#?}",
        Cr2::read()
    );
}

extern "x86-interrupt" fn double_fault_handler(frame: InterruptStackFrame, _error_code: u64) -> ! {
    #[cfg(feature = "selftest")]
    if EXPECTING_DOUBLE_FAULT.load(Ordering::SeqCst) {
        x86_64::instructions::interrupts::disable();
        // SAFETY: terminal path — the interrupted lock holder never resumes.
        unsafe { crate::serial::force_unlock_for_panic() };
        crate::serial_println!(
            "[selftest] resilience: stack overflow caught on the IST double-fault stack ... ok"
        );
        crate::serial_println!("SELFTEST PASSED");
        crate::qemu::exit(crate::qemu::ExitCode::Success);
    }
    panic!("double fault\n{frame:#?}");
}
