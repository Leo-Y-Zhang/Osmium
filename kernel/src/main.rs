#![no_std]
#![no_main]

mod console;
mod framebuffer;
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
    let framebuffer = framebuffer
        .as_mut()
        .expect("bootloader provided no framebuffer");
    let info = framebuffer.info();
    console::init(framebuffer::Display::new(framebuffer));
    logger::init();

    log::info!("Osmium v{}", env!("CARGO_PKG_VERSION"));
    log::info!(
        "framebuffer: {}x{} px, {:?}, stride {}, {} B/px",
        info.width,
        info.height,
        info.pixel_format,
        info.stride,
        info.bytes_per_pixel
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

    // try_lock: if the console lock is held we lose the on-screen report but
    // keep the serial one, instead of deadlocking.
    if let Some(mut guard) = console::CONSOLE.try_lock()
        && let Some(console) = guard.as_mut()
    {
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
