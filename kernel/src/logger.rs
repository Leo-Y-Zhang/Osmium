//! One `log` sink fanning out to serial (always) and the framebuffer console
//! (once initialised). Interrupt handlers never log — see the TDD's locking
//! rule — so taking the console lock here is safe.

use crate::framebuffer::{ACCENT, AMBER, DANGER, FOREGROUND};
use log::{Level, LevelFilter, Metadata, Record};

struct KernelLogger;

static LOGGER: KernelLogger = KernelLogger;

impl log::Log for KernelLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        true
    }

    fn log(&self, record: &Record) {
        crate::serial_println!("[{:5}] {}", record.level(), record.args());
        crate::console::with_console(|console| {
            use core::fmt::Write;
            let color = match record.level() {
                Level::Error => DANGER,
                Level::Warn => AMBER,
                Level::Info => FOREGROUND,
                Level::Debug | Level::Trace => ACCENT,
            };
            console.set_color(color);
            let _ = writeln!(console, "{}", record.args());
            console.set_color(FOREGROUND);
        });
    }

    fn flush(&self) {}
}

pub fn init() {
    log::set_logger(&LOGGER).expect("logger initialised twice");
    log::set_max_level(LevelFilter::Trace);
}
