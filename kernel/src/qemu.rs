//! QEMU's `isa-debug-exit` device. Writing to port 0xf4 makes QEMU exit with
//! `(value << 1) | 1`, which is how the self-test battery reports its verdict
//! to `cargo xtask test`. On real hardware the port write is a no-op and we
//! fall back to halting.

use x86_64::instructions::port::Port;

#[derive(Clone, Copy)]
#[repr(u32)]
pub enum ExitCode {
    /// Host observes exit code 33.
    Success = 0x10,
    /// Host observes exit code 35.
    Failed = 0x11,
}

pub fn exit(code: ExitCode) -> ! {
    // SAFETY: 0xf4 is the isa-debug-exit device xtask configures; the write
    // has no side effect on machines where the device is absent.
    unsafe {
        let mut port = Port::new(0xf4);
        port.write(code as u32);
    }
    crate::halt_loop()
}
