//! Ring 3 and a software-interrupt (`int 0x80`) system-call path — Osmium's
//! privilege boundary — plus the ELF64 loader that feeds it.
//!
//! The user program is a real, linker-scripted Rust ELF (`user/hello`, built
//! by the kernel's build script and embedded here). [`run_elf`] parses it
//! with `kshared::elf` (host-tested, refusal-by-default), maps each `PT_LOAD`
//! segment writable-and-NX for the copy, locks every page to its final W^X
//! permissions, and drops to ring 3 with an `iretq`. The program returns to
//! the kernel by issuing `int 0x80` with `SYS_EXIT`; the entry stub restores
//! the kernel stack saved before the jump and returns to the launcher. Other
//! syscalls dispatch to Rust and `iretq` back.
//!
//! Privacy carries over: the frames the user runs on are zeroed on hand-out
//! like every other (which is also what makes BSS correct for free), and the
//! self-tests audit the page tables both before ring 3 has run and again
//! after teardown — no kernel mapping is ever user-accessible, and no user
//! leaf survives a run.
//!
//! Boundaries, stated so they are not mistaken for isolation guarantees:
//! - **A misbehaving user program is fatal.** Every exception handler panics,
//!   so a ring-3 fault (bad memory access, `#DE`, a privileged instruction,
//!   `int n` for any vector but 0x80) takes the whole kernel down. There is no
//!   process model yet to terminate just the program; that is a later
//!   milestone.
//! - **One program at a time.** The loader uses fixed addresses and a single
//!   continuation slot; it is not reentrant, which the cooperative single-core
//!   model guarantees today.
//! - **Frames are not reclaimed.** The bump allocator never frees, so each
//!   run leaks its mapped frames (a few pages); many manual `user` shell
//!   invocations would eventually exhaust RAM and panic. The parser's
//!   64-page budget bounds how fast a single load can drain the allocator.

use crate::gdt;
use core::sync::atomic::{AtomicU64, Ordering};
use kshared::elf::{ElfError, LoadPlan};

pub const SYSCALL_VECTOR: u8 = 0x80;

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;

/// The kernel stack pointer saved by `jump_to_user`, so `SYS_EXIT` can return
/// to the launcher instead of `iretq`-ing back to a finished program.
/// Invariant: `SYS_EXIT` reads this blindly, sound only because ring 3 is
/// reachable *only* through `enter_ring3`, which always writes it before any
/// ring-3 instruction can execute.
static KERNEL_CONTINUATION_RSP: AtomicU64 = AtomicU64::new(0);
/// The exit code the user program passed to `SYS_EXIT`.
static USER_EXIT_CODE: AtomicU64 = AtomicU64::new(0);

/// The user stack page. Program segments live in the image window
/// (`kshared::elf::USER_IMAGE_BASE..USER_IMAGE_END`); the stack sits outside
/// it, and the page-table audit (`memory::no_stray_user_mappings`) allows
/// user-accessible intermediate entries only where they reach one of the two.
pub(crate) const USER_STACK_ADDR: u64 = 0x80_0000;

/// The user program: a real linker-scripted Rust ELF built from `user/hello`
/// by the kernel's build script and embedded as bytes.
static HELLO_ELF: &[u8] = include_bytes!(env!("HELLO_ELF"));

/// The `int 0x80` entry, as a raw address for the IDT gate (installed at DPL 3
/// in `interrupts.rs` so ring 3 may issue the instruction).
pub fn syscall_entry_addr() -> u64 {
    int80_entry as *const () as u64
}

/// Runs the embedded `hello` ELF in ring 3.
pub fn run_hello() -> Result<u64, ElfError> {
    run_elf(HELLO_ELF)
}

/// Parses, maps, runs and tears down a static ELF64 user program; returns
/// the value it passed to `SYS_EXIT`. Refusal happens before anything is
/// mapped. Not reentrant — one user program at a time, which is all the
/// cooperative single-core model needs.
pub fn run_elf(image: &[u8]) -> Result<u64, ElfError> {
    let plan: LoadPlan = kshared::elf::parse_elf64(image)?;

    // Map every segment writable + NX for the copy: a page is never writable
    // and executable at the same time, even transiently.
    for seg in plan.segments() {
        for page in 0..seg.page_count() {
            crate::memory::map_user_page(seg.vaddr + page * 4096, true, false);
        }
        // SAFETY: the pages were just mapped writable and cover `memsz`
        // bytes; the parser bounds-checked `file_start..+filesz` against the
        // image. The `memsz` tail past `filesz` is BSS, and frames arrive
        // zeroed, so it is already correct.
        unsafe {
            core::ptr::copy_nonoverlapping(
                image.as_ptr().add(seg.file_start),
                seg.vaddr as *mut u8,
                seg.filesz as usize,
            );
        }
    }
    // Lock each page to its final W^X permissions (the parser refused any
    // segment claiming both).
    for seg in plan.segments() {
        for page in 0..seg.page_count() {
            crate::memory::update_user_page(seg.vaddr + page * 4096, seg.writable, seg.executable);
        }
    }
    // Stack page: user-accessible, writable, never executable.
    crate::memory::map_user_page(USER_STACK_ADDR, true, false);

    let code = enter_ring3(plan.entry, USER_STACK_ADDR + 4096);

    // Tear the user mappings down so the loader can run again (the frames
    // are not reclaimed — bump allocator — only the mappings).
    for seg in plan.segments() {
        for page in 0..seg.page_count() {
            crate::memory::unmap_user_page(seg.vaddr + page * 4096);
        }
    }
    crate::memory::unmap_user_page(USER_STACK_ADDR);
    Ok(code)
}

/// Drops to ring 3 at `entry` and returns the program's `SYS_EXIT` value.
/// The caller has already mapped the code and stack.
fn enter_ring3(entry: u64, stack_top: u64) -> u64 {
    let sel = gdt::selectors();
    let user_cs = u64::from(sel.user_code.0); // RPL 3 already in the selector
    let user_ss = u64::from(sel.user_data.0);

    // The int 0x80 gate clears IF on the way in, so control returns here with
    // interrupts disabled; restore them only if the caller had them enabled.
    let interrupts_were_enabled = x86_64::instructions::interrupts::are_enabled();

    // SAFETY: entering ring 3 at a mapped user code page with a mapped user
    // stack and the ring-3 selectors; SYS_EXIT returns control here with the
    // callee-saved registers restored by the entry stub.
    unsafe { jump_to_user(entry, stack_top, user_cs, user_ss) };
    let code = USER_EXIT_CODE.load(Ordering::SeqCst);
    if interrupts_were_enabled {
        x86_64::instructions::interrupts::enable();
    }
    code
}

/// Drops to ring 3. The SysV callee-saved registers are pushed first and the
/// saved stack pointer points at them, so `SYS_EXIT` restores them and hands
/// the launcher's caller back the register file the compiler expects.
///
/// This is ABI hygiene closing a latent hole a security review found: because
/// `jump_to_user` is `extern "C"` and appears to return normally, a caller
/// that held a live value in a callee-saved register across the call would
/// otherwise get back whatever ring 3 left there. It is not reproducible by a
/// Rust-level test — the intervening function epilogues re-establish the
/// callee-saved invariant themselves, and the one currently-live value happens
/// to corrupt benignly — so it is kept as defense-in-depth, the way every real
/// syscall stub does. The GP registers are also scrubbed before entering ring
/// 3, and the caller-saved set scrubbed on the syscall return, so the program
/// never sees kernel register contents.
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
