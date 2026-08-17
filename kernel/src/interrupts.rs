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
/// Raw scancodes received (M4 replaces this counter with a waker-backed queue).
pub static SCANCODES_SEEN: AtomicUsize = AtomicUsize::new(0);

static IDT: LazyLock<InterruptDescriptorTable> = LazyLock::new(|| {
    let mut idt = InterruptDescriptorTable::new();
    idt.breakpoint.set_handler_fn(breakpoint_handler);
    idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
    idt.general_protection_fault
        .set_handler_fn(general_protection_fault_handler);
    idt.page_fault.set_handler_fn(page_fault_handler);
    // SAFETY: the IST index is backed by a real, dedicated stack (gdt.rs).
    unsafe {
        idt.double_fault
            .set_handler_fn(double_fault_handler)
            .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
    }
    idt[InterruptIndex::Timer.as_u8()].set_handler_fn(timer_handler);
    idt[InterruptIndex::Keyboard.as_u8()].set_handler_fn(keyboard_handler);
    // A floating line can produce a spurious IRQ7 even with devices masked.
    idt[PIC_1_OFFSET + 7].set_handler_fn(spurious_handler);
    idt
});

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
    let _scancode = unsafe { port.read() };
    SCANCODES_SEEN.fetch_add(1, Ordering::Relaxed);
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
    panic!("double fault\n{frame:#?}");
}
