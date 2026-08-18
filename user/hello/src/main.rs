//! `hello` — the Osmium user program: a real, linker-scripted Rust ELF that
//! runs in ring 3 and talks to the kernel only through `int 0x80`.
//!
//! It is deliberately built to exercise every part of the loader: code in an
//! R+X text segment, a mutable value in an R+W data segment (the volatile
//! write-back would page-fault if the loader mapped .data read-only), and an
//! exit code carrying CS so the kernel can assert the program really ran at
//! CPL 3.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;

/// Lives in the RW data segment. Read and written back volatilely so the
/// segment must really be materialised, mapped, and writable at runtime.
static mut GREETING: u64 = b'E' as u64;

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
    // Set EFLAGS.AC (bit 18) — a program hostile to the kernel's SMAP: if the
    // kernel did not scrub AC at the syscall gate, SMAP would be inert for the
    // whole kernel entry. Harmless in ring 3 (CR0.AM is clear, so no alignment
    // faults), and the kernel's self-test asserts it observed AC already
    // cleared. SAFETY: pushfq/popfq only toggle a flag; no memory effect.
    unsafe {
        core::arch::asm!("pushfq", "or dword ptr [rsp], 0x40000", "popfq");
    }

    // Prove the data segment is real and writable: volatile read, then a
    // volatile write-back (a read-only .data mapping faults here).
    // SAFETY: raw-pointer access to our own static; no references are formed.
    let v = unsafe { core::ptr::read_volatile(core::ptr::addr_of!(GREETING)) };
    // SAFETY: as above.
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(GREETING), v + 1) };
    syscall(SYS_WRITE, v);

    // Exit with CS: its low two bits are the CPL, which is the kernel-side
    // assertion that this program really ran in ring 3.
    let cs: u64;
    // SAFETY: reading a segment register has no side effects.
    unsafe { core::arch::asm!("mov {}, cs", out(reg) cs) };
    syscall(SYS_EXIT, cs);
    // SYS_EXIT never returns; this satisfies the diverging signature.
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    // No unwinding and nothing to report to: a user panic just exits with a
    // recognisable code.
    syscall(SYS_EXIT, 0xdead);
    loop {
        core::hint::spin_loop();
    }
}
