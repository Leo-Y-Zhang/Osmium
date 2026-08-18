//! `crasher` — the program whose job is to die badly.
//!
//! It announces itself with one byte of output, then dereferences an
//! unmapped address, taking a page fault at CPL 3. Before M10 that panicked
//! the whole kernel; the fault-isolation milestone turns it into this task's
//! termination and nothing else's — which the battery proves by running it
//! next to `hello` and asserting the neighbour, the run report and the
//! kernel all came through intact.
//!
//! The `SYS_EXIT` at the bottom is deliberately unreachable: if it ever runs,
//! the fault did not happen and the exit code (0xBAD) says so — the battery
//! asserts the task was terminated by vector 14, not that it exited.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;

fn syscall(nr: u64, a0: u64) -> u64 {
    let mut ret = nr;
    // SAFETY: `int 0x80` is Osmium's syscall gate. The kernel scrubs the
    // caller-saved registers on return (rax carries the result), which the
    // clobber list reflects; callee-saved registers are preserved.
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inout("rax") ret,
            inout("rdi") a0 => _,
            out("rcx") _,
            out("rdx") _,
            out("rsi") _,
            out("r8") _,
            out("r9") _,
            out("r10") _,
            out("r11") _,
        );
    }
    ret
}

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    // Prove this program really ran before dying: one byte on the console.
    syscall(SYS_WRITE, u64::from(b'!'));

    // Dereference an address that is mapped in NO address space (below the
    // user image window, above null) — a page fault at CPL 3.
    // SAFETY: not safe at all; that is the entire point of this program.
    unsafe {
        core::ptr::read_volatile(0x30_0000 as *const u64);
    }

    // Unreachable: the read above faults. An exit here means fault isolation
    // was never exercised, and 0xBAD makes that loud in the run report.
    syscall(SYS_EXIT, 0xBAD);
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    syscall(SYS_EXIT, 0xdead);
    loop {
        core::hint::spin_loop();
    }
}
