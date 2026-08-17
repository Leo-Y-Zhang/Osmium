#![no_std]
#![no_main]
// The kernel's ONE allowed unstable feature: the x86-interrupt calling
// convention for IDT handlers (still feature-gated on stable rustc). CI's
// feature-allowlist job fails if anything else joins it.
#![feature(abi_x86_interrupt)]

mod console;
mod framebuffer;
mod gdt;
mod interrupts;
mod logger;
#[cfg_attr(not(feature = "selftest"), allow(dead_code))]
mod qemu;
#[cfg(feature = "selftest")]
mod selftest;
mod serial;

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
    serial_println!("Osmium v{}: serial up", env!("CARGO_PKG_VERSION"));

    // Split the borrow once; later milestones take the memory map from here.
    let BootInfo { framebuffer, .. } = boot_info;
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

    gdt::init();
    interrupts::init();
    log::info!(
        "gdt+tss loaded, interrupts enabled (PIT {} Hz)",
        interrupts::TICK_HZ
    );

    #[cfg(feature = "selftest")]
    selftest::run();

    #[cfg(not(feature = "selftest"))]
    {
        log::info!("boot complete; halting (the shell arrives in a later milestone)");
        halt_loop()
    }
}

pub(crate) fn halt_loop() -> ! {
    loop {
        // SAFETY: `hlt` idles the CPU until the next interrupt; it touches no state.
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
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
            "The system is halted; reset the machine or close QEMU."
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
