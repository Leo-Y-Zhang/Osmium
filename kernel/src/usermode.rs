//! Ring 3 and a software-interrupt (`int 0x80`) system-call path — Osmium's
//! privilege boundary.
//!
//! The kernel maps a user-only code page and stack, drops to ring 3 with an
//! `iretq`, and runs a flat code blob (ELF loading is a later milestone). The
//! program returns to the kernel by issuing `int 0x80` with `SYS_EXIT`; the
//! entry stub restores the kernel stack saved before the jump and returns to
//! `run_user`'s caller. Other syscalls dispatch to Rust and `iretq` back.
//!
//! Privacy carries over: the frame the user runs on is zeroed on hand-out like
//! every other, so no previous owner's bytes are visible to the program, and
//! the self-test proves no kernel mapping is user-accessible.

use crate::gdt;
use core::sync::atomic::{AtomicU64, Ordering};

pub const SYSCALL_VECTOR: u8 = 0x80;

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;

/// The kernel stack pointer saved by `jump_to_user`, so `SYS_EXIT` can return
/// to the launcher instead of `iretq`-ing back to a finished program.
static KERNEL_CONTINUATION_RSP: AtomicU64 = AtomicU64::new(0);
/// The exit code the user program passed to `SYS_EXIT`.
static USER_EXIT_CODE: AtomicU64 = AtomicU64::new(0);

const USER_CODE_ADDR: u64 = 0x40_0000;
const USER_STACK_ADDR: u64 = 0x80_0000;

/// The `int 0x80` entry, as a raw address for the IDT gate (installed at DPL 3
/// in `interrupts.rs` so ring 3 may issue the instruction).
pub fn syscall_entry_addr() -> u64 {
    int80_entry as *const () as u64
}

/// Runs a flat ring-3 code blob and returns the value it passed to `SYS_EXIT`.
/// Not reentrant — one user program at a time, which is all v1 needs.
pub fn run_user(code: &[u8]) -> u64 {
    assert!(code.len() <= 4096, "user blob must fit in one page");
    // Code page: user-accessible, writable while we copy, then read-only+exec.
    crate::memory::map_user_page(USER_CODE_ADDR, true, true);
    // SAFETY: the page was just mapped writable and is at least `code.len()`
    // bytes; we own it exclusively until the program runs.
    unsafe {
        core::ptr::copy_nonoverlapping(code.as_ptr(), USER_CODE_ADDR as *mut u8, code.len());
    }
    crate::memory::make_read_only(USER_CODE_ADDR); // W^X on the code page
    // Stack page: user-accessible, writable, never executable.
    crate::memory::map_user_page(USER_STACK_ADDR, true, false);

    let sel = gdt::selectors();
    let user_cs = u64::from(sel.user_code.0); // RPL 3 already in the selector
    let user_ss = u64::from(sel.user_data.0);
    let stack_top = USER_STACK_ADDR + 4096;

    // SAFETY: entering ring 3 at a mapped user code page with a mapped user
    // stack and the ring-3 selectors; SYS_EXIT returns control here.
    unsafe { jump_to_user(USER_CODE_ADDR, stack_top, user_cs, user_ss) };
    let code = USER_EXIT_CODE.load(Ordering::SeqCst);
    // The int 0x80 gate cleared IF on the way in; restore it now that we are
    // back in ordinary kernel context.
    x86_64::instructions::interrupts::enable();
    // Tear the user mappings down so run_user can be called again (the frames
    // are not reclaimed — bump allocator — only the mappings).
    crate::memory::unmap_user_page(USER_CODE_ADDR);
    crate::memory::unmap_user_page(USER_STACK_ADDR);
    code
}

/// Drops to ring 3. Saves the kernel stack (so `SYS_EXIT` can return to the
/// caller), then builds an `iretq` frame targeting the user selectors.
#[unsafe(naked)]
unsafe extern "C" fn jump_to_user(user_rip: u64, user_rsp: u64, user_cs: u64, user_ss: u64) {
    // args: rdi=rip, rsi=rsp, rdx=cs, rcx=ss
    core::arch::naked_asm!(
        "mov [rip + {cont}], rsp", // save the launcher's stack
        "push rcx",                // SS
        "push rsi",                // RSP
        "push 0x202",              // RFLAGS: IF set, reserved bit 1 set
        "push rdx",                // CS
        "push rdi",                // RIP
        "iretq",
        cont = sym KERNEL_CONTINUATION_RSP,
    )
}

/// The `int 0x80` handler. On entry (from ring 3) the CPU has pushed
/// SS/RSP/RFLAGS/CS/RIP and switched to the ring-0 stack (TSS RSP0). The user
/// passes the syscall number in `rax` and arguments in `rdi`/`rsi`.
#[unsafe(naked)]
unsafe extern "C" fn int80_entry() {
    core::arch::naked_asm!(
        "cmp rax, {sys_exit}",
        "jne 2f",
        // SYS_EXIT: record the code, restore the launcher's stack, return.
        "mov [rip + {exit_code}], rdi",
        "mov rsp, [rip + {cont}]",
        "ret",
        "2:",
        // Other syscalls: marshal (nr,a0,a1) into the SysV ABI, align, call.
        "mov rdx, rsi",
        "mov rsi, rdi",
        "mov rdi, rax",
        "sub rsp, 8",
        "call {dispatch}",
        "add rsp, 8",
        "iretq",
        sys_exit = const SYS_EXIT,
        exit_code = sym USER_EXIT_CODE,
        cont = sym KERNEL_CONTINUATION_RSP,
        dispatch = sym syscall_dispatch,
    )
}

extern "C" fn syscall_dispatch(nr: u64, a0: u64, _a1: u64) -> u64 {
    match nr {
        SYS_WRITE => {
            // Kernel-mediated output on behalf of the user program. This byte
            // is supplied by the program, not typed at the keyboard, so serial
            // is the right channel — it lets the CI log show the syscall ran.
            let byte = (a0 & 0xff) as u8 as char;
            crate::serial_println!("[user] SYS_WRITE {byte:?}");
            0
        }
        _ => u64::MAX,
    }
}

/// A tiny ring-3 program: report a byte via `SYS_WRITE`, then exit with its own
/// code segment (so the kernel can prove it ran in CPL 3). Hand-assembled flat
/// x86-64 machine code.
#[rustfmt::skip]
pub const DEMO_PROGRAM: &[u8] = &[
    // mov eax, 1 (SYS_WRITE)
    0xb8, 0x01, 0x00, 0x00, 0x00,
    // mov edi, 0x55 ('U')
    0xbf, 0x55, 0x00, 0x00, 0x00,
    // int 0x80
    0xcd, 0x80,
    // mov ax, cs
    0x8c, 0xc8,
    // and eax, 0xffff
    0x25, 0xff, 0xff, 0x00, 0x00,
    // mov edi, eax  (exit code = CS, whose low 2 bits are the CPL)
    0x89, 0xc7,
    // xor eax, eax  (SYS_EXIT)
    0x31, 0xc0,
    // int 0x80
    0xcd, 0x80,
];
