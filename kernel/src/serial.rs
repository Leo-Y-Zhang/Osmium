//! COM1 serial output — the log channel that `cargo xtask test` and CI
//! capture. Interrupt handlers must never take this lock; the panic path
//! reclaims it explicitly because its holder can never resume.

use spin::{LazyLock, Mutex};
use uart_16550::SerialPort;

pub static SERIAL1: LazyLock<Mutex<SerialPort>> = LazyLock::new(|| {
    // SAFETY: 0x3F8 is the standard COM1 port base and nothing else drives it.
    let mut port = unsafe { SerialPort::new(0x3F8) };
    port.init();
    Mutex::new(port)
});

#[doc(hidden)]
pub fn _print(args: core::fmt::Arguments) {
    use core::fmt::Write;
    let _ = SERIAL1.lock().write_fmt(args);
}

/// Reclaims the serial lock on the panic path. The caller must have disabled
/// interrupts first; the previous holder never resumes, so this cannot race.
pub unsafe fn force_unlock_for_panic() {
    if SERIAL1.is_locked() {
        // SAFETY: guaranteed by the caller as documented above.
        unsafe { SERIAL1.force_unlock() };
    }
}

#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => ($crate::serial::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! serial_println {
    () => ($crate::serial_print!("\n"));
    ($($arg:tt)*) => ($crate::serial_print!("{}\n", format_args!($($arg)*)));
}
