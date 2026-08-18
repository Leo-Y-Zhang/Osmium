#![no_std]
#![no_main]
// The kernel's ONE allowed unstable feature: the x86-interrupt calling
// convention for IDT handlers (still feature-gated on stable rustc). CI's
// feature-allowlist job fails if anything else joins it.
#![feature(abi_x86_interrupt)]
// TDD rule, machine-checked: every unsafe block carries its SAFETY argument.
// CI's -D warnings turns this into a hard failure.
#![warn(clippy::undocumented_unsafe_blocks)]

extern crate alloc;

mod console;
mod cpu;
mod framebuffer;
mod gdt;
mod interrupts;
mod logger;
mod memory;
#[cfg_attr(not(feature = "selftest"), allow(dead_code))]
mod qemu;
mod sched;
#[cfg(feature = "selftest")]
mod selftest;
mod serial;
// Compiled in both builds: the selftest battery drives the shell's input path
// end-to-end, so most of this module is only exercised by the shipped build.
#[cfg_attr(feature = "selftest", allow(dead_code))]
mod shell;
mod task;
mod time;
mod usermode;

use bootloader_api::config::Mapping;
use bootloader_api::{BootInfo, BootloaderConfig, entry_point};
use core::panic::PanicInfo;

pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    // Map all physical memory into the kernel's address space; the frame
    // allocator and page-table code rely on this.
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config.kernel_stack_size = 100 * 1024;
    config
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    time::mark_boot();
    serial_println!("Osmium v{}: serial up", env!("CARGO_PKG_VERSION"));

    // Split the borrow once so each subsystem takes only its own field.
    let BootInfo {
        framebuffer,
        memory_regions,
        physical_memory_offset,
        ..
    } = boot_info;
    // No framebuffer is a degraded boot, not a dead one: run serial-only.
    let fb_info = match framebuffer.as_mut() {
        Some(fb) => {
            let info = fb.info();
            console::init(framebuffer::Display::new(fb));
            Some(info)
        }
        None => None,
    };
    logger::init();

    log::info!("Osmium v{}", env!("CARGO_PKG_VERSION"));
    match fb_info {
        Some(info) => log::info!(
            "framebuffer: {}x{} px, {:?}, stride {}, {} B/px",
            info.width,
            info.height,
            info.pixel_format,
            info.stride,
            info.bytes_per_pixel
        ),
        None => log::warn!("bootloader provided no framebuffer; running serial-only"),
    }
    time::stamp(time::Phase::ConsoleReady);

    gdt::init();
    interrupts::init();
    time::stamp(time::Phase::InterruptsOn);
    log::info!(
        "gdt+tss loaded, interrupts enabled (PIT {} Hz)",
        interrupts::TICK_HZ
    );

    // Fail closed: without the physical-memory mapping there is no safe way
    // to manage frames or page tables (TDD).
    let phys_offset = match *physical_memory_offset {
        bootloader_api::info::Optional::Some(offset) => offset,
        bootloader_api::info::Optional::None => {
            panic!("bootloader did not map physical memory; cannot continue")
        }
    };
    memory::init(phys_offset, memory_regions);
    time::stamp(time::Phase::MemoryReady);
    // Supervisor-mode hardening (SMEP/SMAP/UMIP): after paging is up — the
    // ELF loader already copies through the physical alias for SMAP — and
    // before any ring-3 program runs.
    cpu::init();
    let (heap_used, heap_free) = memory::heap::stats();
    let (frames_used, frames_total) = memory::frame_stats();
    log::info!(
        "memory: heap {} KiB ({heap_used} B used, {heap_free} B free), {frames_used}/{frames_total} frames",
        memory::heap::HEAP_SIZE / 1024
    );
    log::info!("cpu hardening: {}", cpu::summary());

    #[cfg(feature = "selftest")]
    {
        time::calibrate();
        time::report();
        selftest::run();
    }

    #[cfg(not(feature = "selftest"))]
    {
        // xtask's shipped-image boot proof greps the serial log for the
        // "boot complete; shell ready" prefix and parses the ms figure (the
        // same PIT-measured span the on-screen banner shows) to gate it. This
        // is a boot diagnostic, not keystroke output, so serial is correct.
        let boot_ms = kshared::ticks_to_ms(
            interrupts::TICKS.load(core::sync::atomic::Ordering::Relaxed),
            interrupts::TICK_HZ,
        );
        log::info!("boot complete; shell ready ({boot_ms} ms after interrupts-on)");
        let mut executor = task::executor::Executor::new();
        executor.spawn(task::Task::new(shell::run()));
        executor.run()
    }
}

pub(crate) fn halt_loop() -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    x86_64::instructions::interrupts::disable();
    // SAFETY: interrupts are off and this path never returns; a previous
    // holder of the serial lock can never resume, so reclaiming it is sound.
    unsafe { serial::force_unlock_for_panic() };
    serial_println!();
    serial_println!("*** KERNEL PANIC ***");
    serial_println!("{info}");
    // The strongest privacy line at the worst moment, and greppable in a
    // shipped-panic serial log: a panic destroys nothing, because nothing was
    // ever written to disk.
    serial_println!("nothing was written to disk.");

    // SAFETY: same argument as the serial lock — this path never returns, so
    // a previous holder can never resume; reclaiming keeps the panic screen
    // from being silently lost (the App Flow's "never a silent hang" rule).
    unsafe { console::force_unlock_for_panic() };
    if let Some(console) = console::CONSOLE.lock().as_mut() {
        use core::fmt::Write;
        console.set_color(framebuffer::DANGER);
        let _ = writeln!(console, "\n*** KERNEL PANIC ***\n{info}");
        let _ = writeln!(
            console,
            "The system is halted; nothing was written to disk. Reset the machine or close QEMU."
        );
    }

    #[cfg(feature = "selftest")]
    {
        serial_println!("SELFTEST FAILED");
        qemu::exit(qemu::ExitCode::Failed)
    }

    #[cfg(not(feature = "selftest"))]
    halt_loop()
}
