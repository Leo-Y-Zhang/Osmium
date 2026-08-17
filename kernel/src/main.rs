#![no_std]
#![no_main]

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

fn kernel_main(_boot_info: &'static mut BootInfo) -> ! {
    halt_loop()
}

fn halt_loop() -> ! {
    loop {
        // SAFETY: `hlt` idles the CPU until the next interrupt; it touches no state.
        unsafe { core::arch::asm!("hlt", options(nomem, nostack, preserves_flags)) };
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    halt_loop()
}
