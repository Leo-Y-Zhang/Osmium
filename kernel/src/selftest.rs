//! The in-kernel self-test battery — the boot proof CI asserts on. Each check
//! prints one `[selftest]` line to serial; any failure panics, and the panic
//! handler exits QEMU with the failure code.

use crate::serial_println;

pub fn run() -> ! {
    serial_println!("[selftest] boot: reached kernel_main ... ok");
    console_renders_and_advances();
    breakpoint_handled_and_returns();
    timer_ticks_advance();
    serial_println!("SELFTEST PASSED");
    crate::qemu::exit(crate::qemu::ExitCode::Success)
}

fn breakpoint_handled_and_returns() {
    use core::sync::atomic::Ordering;
    let before = crate::interrupts::BREAKPOINT_HITS.load(Ordering::Relaxed);
    x86_64::instructions::interrupts::int3();
    let after = crate::interrupts::BREAKPOINT_HITS.load(Ordering::Relaxed);
    assert!(after > before, "breakpoint handler did not run");
    serial_println!("[selftest] interrupts: int3 handled, execution resumed ... ok");
}

fn timer_ticks_advance() {
    use core::sync::atomic::Ordering;
    let start = crate::interrupts::TICKS.load(Ordering::Relaxed);
    // 3 ticks at 100 Hz is ~30 ms; the hlt bound keeps a broken PIT from
    // hanging the battery (the panic is the diagnosis, the CI timeout is not).
    for _ in 0..10_000 {
        if crate::interrupts::TICKS.load(Ordering::Relaxed) >= start + 3 {
            serial_println!("[selftest] timer: PIT ticks advancing ... ok");
            return;
        }
        x86_64::instructions::hlt();
    }
    panic!("PIT ticks did not advance; PIC/PIT wiring is broken");
}

fn console_renders_and_advances() {
    if !crate::console::is_initialised() {
        // Serial-only boot (no framebuffer): the console path is untestable
        // here, and that is a property of the machine, not a kernel defect.
        serial_println!("[selftest] console: skipped, no framebuffer on this boot");
        return;
    }
    let advanced = crate::console::with_console(|console| {
        use core::fmt::Write;
        let before = console.cursor();
        writeln!(console, "selftest: console glyph rendering").unwrap();
        console.cursor() != before
    });
    assert_eq!(
        advanced,
        Some(true),
        "console cursor did not advance after writing a line"
    );
    serial_println!("[selftest] console: glyphs rendered, cursor advanced ... ok");
}
