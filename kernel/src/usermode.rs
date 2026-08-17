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
//!
//! M6 boundaries, stated so they are not mistaken for isolation guarantees:
//! - **A misbehaving user program is fatal.** Every exception handler panics,
//!   so a ring-3 fault (bad memory access, `#DE`, a privileged instruction,
//!   `int n` for any vector but 0x80) takes the whole kernel down. There is no
//!   process model yet to terminate just the program; that is a later
//!   milestone.
//! - **One program at a time.** `run_user` uses fixed addresses and a single
//!   continuation slot; it is not reentrant, which the cooperative single-core
//!   model guarantees today.
//! - **Frames are not reclaimed.** The bump allocator never frees, so each
//!   `run_user` leaks two leaf frames (8 KiB); many manual `user` invocations
//!   would eventually exhaust RAM and panic.

use crate::gdt;
use core::sync::atomic::{AtomicU64, Ordering};

pub const SYSCALL_VECTOR: u8 = 0x80;

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;

/// The kernel stack pointer saved by `jump_to_user`, so `SYS_EXIT` can return
/// to the launcher instead of `iretq`-ing back to a finished program.
/// Invariant: `SYS_EXIT` reads this blindly, sound only because ring 3 is
/// reachable *only* through `run_user`, which always writes it before any
/// ring-3 instruction can execute.
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

    // The int 0x80 gate clears IF on the way in, so control returns here with
    // interrupts disabled; restore them only if the caller had them enabled.
    let interrupts_were_enabled = x86_64::instructions::interrupts::are_enabled();

    // SAFETY: entering ring 3 at a mapped user code page with a mapped user
    // stack and the ring-3 selectors; SYS_EXIT returns control here with the
    // callee-saved registers restored by the entry stub.
    unsafe { jump_to_user(USER_CODE_ADDR, stack_top, user_cs, user_ss) };
    let code = USER_EXIT_CODE.load(Ordering::SeqCst);
    if interrupts_were_enabled {
        x86_64::instructions::interrupts::enable();
    }
    // Tear the user mappings down so run_user can be called again (the frames
    // are not reclaimed — bump allocator — only the mappings).
    crate::memory::unmap_user_page(USER_CODE_ADDR);
    crate::memory::unmap_user_page(USER_STACK_ADDR);
    code
}

/// The exit code from the most recent `run_user` (also its return value; this
/// getter lets a caller that reached `run_user` through an asm trampoline read
/// it without threading the return through the trampoline).
#[cfg_attr(not(feature = "selftest"), allow(dead_code))]
pub fn last_exit_code() -> u64 {
    USER_EXIT_CODE.load(Ordering::SeqCst)
}

/// Drops to ring 3. The SysV callee-saved registers are pushed first and the
/// saved stack pointer points at them, so `SYS_EXIT` can restore them and hand
/// the launcher's caller back the register file the compiler expects —
/// otherwise ring 3 could return arbitrary `rbx/rbp/r12-r15` into the kernel.
/// The general-purpose registers are then scrubbed before entering ring 3, so
/// the program never sees kernel register contents.
#[unsafe(naked)]
unsafe extern "C" fn jump_to_user(user_rip: u64, user_rsp: u64, user_cs: u64, user_ss: u64) {
    // args: rdi=rip, rsi=rsp, rdx=cs, rcx=ss
    core::arch::naked_asm!(
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rip + {cont}], rsp", // save the stack just above the saved regs
        "push rcx",                // SS
        "push rsi",                // RSP
        "push 0x202",              // RFLAGS: IF set, reserved bit 1 set, IOPL 0
        "push rdx",                // CS
        "push rdi",                // RIP
        // Scrub GP registers so ring 3 sees no kernel state (the iretq frame is
        // already built, so consuming rcx/rdx/rsi/rdi now is fine).
        "xor eax, eax",
        "xor ebx, ebx",
        "xor ecx, ecx",
        "xor edx, edx",
        "xor esi, esi",
        "xor edi, edi",
        "xor ebp, ebp",
        "xor r8d, r8d",
        "xor r9d, r9d",
        "xor r10d, r10d",
        "xor r11d, r11d",
        "xor r12d, r12d",
        "xor r13d, r13d",
        "xor r14d, r14d",
        "xor r15d, r15d",
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
        // The interrupt gate clears IF but NOT DF; SysV requires DF=0 at a call
        // boundary, so clear it before any Rust code runs.
        "cld",
        "cmp rax, {sys_exit}",
        "jne 2f",
        // SYS_EXIT: record the code, restore the launcher's stack AND its
        // callee-saved registers, then return into run_user.
        "mov [rip + {exit_code}], rdi",
        "mov rsp, [rip + {cont}]",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "ret",
        "2:",
        // Other syscalls: marshal (nr,a0,a1) into the SysV ABI, align, call.
        "mov rdx, rsi",
        "mov rsi, rdi",
        "mov rdi, rax",
        "sub rsp, 8",
        "call {dispatch}",
        "add rsp, 8",
        // Scrub caller-saved registers (keep rax, the return value) so no
        // kernel pointer left by the dispatcher leaks back to ring 3.
        "xor ecx, ecx",
        "xor edx, edx",
        "xor esi, esi",
        "xor edi, edi",
        "xor r8d, r8d",
        "xor r9d, r9d",
        "xor r10d, r10d",
        "xor r11d, r11d",
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

/// A tiny ring-3 program: report a byte via `SYS_WRITE`, then **deliberately
/// set every callee-saved register to -1** before exiting with its own code
/// segment. The clobbering makes the register-preservation guarantee testable:
/// the battery pins a sentinel in `r15` across `run_user`, and if the entry
/// stub failed to save and restore the callee-saved set, this `-1` would reach
/// the caller. The exit code is CS, whose low two bits are the CPL.
/// Hand-assembled flat x86-64.
#[rustfmt::skip]
pub const DEMO_PROGRAM: &[u8] = &[
    // mov eax, 1 (SYS_WRITE)
    0xb8, 0x01, 0x00, 0x00, 0x00,
    // mov edi, 0x55 ('U')
    0xbf, 0x55, 0x00, 0x00, 0x00,
    // int 0x80
    0xcd, 0x80,
    // clobber every callee-saved register with -1
    0x48, 0xc7, 0xc3, 0xff, 0xff, 0xff, 0xff, // mov rbx, -1
    0x48, 0xc7, 0xc5, 0xff, 0xff, 0xff, 0xff, // mov rbp, -1
    0x49, 0xc7, 0xc4, 0xff, 0xff, 0xff, 0xff, // mov r12, -1
    0x49, 0xc7, 0xc5, 0xff, 0xff, 0xff, 0xff, // mov r13, -1
    0x49, 0xc7, 0xc6, 0xff, 0xff, 0xff, 0xff, // mov r14, -1
    0x49, 0xc7, 0xc7, 0xff, 0xff, 0xff, 0xff, // mov r15, -1
    // mov ax, cs
    0x8c, 0xc8,
    // and eax, 0xffff
    0x25, 0xff, 0xff, 0x00, 0x00,
    // mov edi, eax  (exit code = CS)
    0x89, 0xc7,
    // xor eax, eax  (SYS_EXIT)
    0x31, 0xc0,
    // int 0x80
    0xcd, 0x80,
];
