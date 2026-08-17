//! The in-kernel self-test battery — the boot proof CI asserts on. Each check
//! prints one `[selftest]` line to serial; any failure panics, and the panic
//! handler exits QEMU with the failure code.

use crate::serial_println;

pub fn run() -> ! {
    serial_println!("[selftest] boot: reached kernel_main ... ok");
    console_renders_and_advances();
    serial_println!("SELFTEST PASSED");
    crate::qemu::exit(crate::qemu::ExitCode::Success)
}

fn console_renders_and_advances() {
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
