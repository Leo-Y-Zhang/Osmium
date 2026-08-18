//! Ring 3 and a software-interrupt (`int 0x80`) system-call path — Osmium's
//! privilege boundary — plus the ELF64 loader that feeds it.
//!
//! The user programs are real, linker-scripted Rust ELFs (`user/hello` at the
//! base of the user window, `user/counter` at its upper half), built by the
//! kernel's build script and embedded here. [`run_programs`] parses each with
//! `kshared::elf` (host-tested, refusal-by-default), refuses any cross-image
//! page overlap before anything maps, maps each `PT_LOAD` per-segment W^X,
//! gives every program its own stack page and its own kernel stack, and hands
//! the set to the scheduler (`sched`), which runs them at CPL 3 under
//! preemptive round-robin — a timer tick taken from ring 3 moves the CPU to
//! the next ready task whether or not the current one ever yields. Each
//! program leaves by issuing `int 0x80` with `SYS_EXIT`; when the last one
//! exits, the entry stub restores the kernel stack saved before the first
//! launch and returns to the launcher. Other syscalls dispatch to Rust and
//! `iretq` back into whichever task made them.
//!
//! Privacy carries over: the frames the user runs on are zeroed on hand-out
//! like every other (which is also what makes BSS correct for free), the
//! self-tests audit the page tables both before ring 3 has run and again
//! after teardown — no kernel mapping is ever user-accessible, and no user
//! leaf survives a run — and a dead task's kernel stack is zeroed when it is
//! freed (the allocator scrubs on free).
//!
//! Boundaries, stated so they are not mistaken for isolation guarantees:
//! - **A misbehaving user program is fatal.** Every exception handler panics,
//!   so a ring-3 fault (bad memory access, `#DE`, a privileged instruction,
//!   `int n` for any vector but 0x80) takes the whole kernel down — and with
//!   it every other task. Fault isolation (terminate the offender, keep the
//!   rest) is a later milestone.
//! - **Tasks share one address space.** Each program is linked at its own
//!   slot in the user window and the loader refuses overlap, but nothing
//!   stops a program *reading or writing* its neighbour's pages: separation
//!   is W^X and ring 3 vs ring 0, not per-process page tables. Address-space
//!   isolation (per-task CR3) is the next milestone on that road.
//! - **One run at a time.** The loader uses fixed slot addresses and a single
//!   continuation, and the shell issues runs synchronously; the scheduler
//!   asserts it is never installed while active.
//! - **Frames are not reclaimed.** The bump allocator never frees, so each
//!   run leaks its mapped frames (a few pages); many manual `user`/`sched`
//!   shell invocations would eventually exhaust RAM and panic. The parser's
//!   64-page budget bounds how fast a single load can drain the allocator.

use crate::sched::{self, RunReport};
use core::sync::atomic::AtomicU64;
use kshared::elf::{ElfError, LoadPlan};

pub const SYSCALL_VECTOR: u8 = 0x80;

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;

/// The kernel stack pointer saved by `sched::enter_tasks`, so the last
/// `SYS_EXIT` can return to the launcher instead of `iretq`-ing back to a
/// finished program. Invariant: the `int 0x80` entry reads this blindly,
/// sound only because ring 3 is reachable *only* through `enter_tasks`,
/// which always writes it before any ring-3 instruction can execute.
pub(crate) static KERNEL_CONTINUATION_RSP: AtomicU64 = AtomicU64::new(0);

/// Per-task user stack pages, indexed by launch position. Program segments
/// live in the image window (`kshared::elf::USER_IMAGE_BASE..USER_IMAGE_END`);
/// the stacks sit outside it in one shared 2 MiB page-table region, and the
/// page-table audit (`memory::no_stray_user_mappings`) allows user-accessible
/// intermediate entries only where they reach the window or a stack.
pub(crate) const USER_STACK_ADDRS: [u64; 2] = [0x80_0000, 0x81_0000];
/// The single-program stack (task slot 0), kept under its M6 name for the
/// battery's W^X probe test and the audit's documentation.
#[cfg(feature = "selftest")]
pub(crate) const USER_STACK_ADDR: u64 = USER_STACK_ADDRS[0];

/// The most programs one run can schedule — one per stack slot.
pub const MAX_TASKS: usize = USER_STACK_ADDRS.len();

/// The embedded user programs: real linker-scripted Rust ELFs built from
/// `user/hello` and `user/counter` by the kernel's build script.
static HELLO_ELF: &[u8] = include_bytes!(env!("HELLO_ELF"));
static COUNTER_ELF: &[u8] = include_bytes!(env!("COUNTER_ELF"));

/// The `int 0x80` entry, as a raw address for the IDT gate (installed at DPL 3
/// in `interrupts.rs` so ring 3 may issue the instruction).
pub fn syscall_entry_addr() -> u64 {
    int80_entry as *const () as u64
}

/// Runs the embedded `hello` ELF alone in ring 3 (the degenerate one-task
/// schedule) and returns its exit code.
pub fn run_hello() -> Result<u64, ElfError> {
    let report = run_programs(&[HELLO_ELF])?;
    Ok(report.exits[0].code)
}

/// Runs `counter` (unyielding compute) and `hello` (short) concurrently,
/// counter first — the preemption demonstration the `sched` shell command
/// and the self-test battery share. With cooperative scheduling hello would
/// wait ~a second for counter's loop; preemption lets it exit first.
pub fn run_counter_and_hello() -> Result<RunReport, ElfError> {
    run_programs(&[COUNTER_ELF, HELLO_ELF])
}

/// The refusal surface for the battery: two copies of the same image claim
/// the same pages, which must be refused before anything is mapped.
#[cfg(feature = "selftest")]
pub fn run_hello_twice() -> Result<RunReport, ElfError> {
    run_programs(&[HELLO_ELF, HELLO_ELF])
}

/// Parses, maps, schedules and tears down up to [`MAX_TASKS`] static ELF64
/// user programs. Refusal happens before anything is mapped: every image must
/// parse (`kshared::elf`, refusal-by-default), and no two images may claim
/// overlapping pages — two programs linked at the same base are an [`ElfError::Overlap`]
/// here for exactly the reason two segments of one program are in the parser.
pub fn run_programs(images: &[&[u8]]) -> Result<RunReport, ElfError> {
    assert!(
        !images.is_empty() && images.len() <= MAX_TASKS,
        "run_programs takes 1..={MAX_TASKS} images"
    );
    // The syscall dispatcher writes SYS_WRITE output to the console, so the
    // caller must not hold the console lock across a ring-3 run or SYS_WRITE
    // would deadlock against it. All callers (the `user`/`sched` shell
    // commands and the battery) call this outside any `with_console`; this
    // pins that.
    debug_assert!(
        !crate::console::CONSOLE.is_locked(),
        "run_programs called while the console lock is held; SYS_WRITE would deadlock"
    );

    let mut plans: alloc::vec::Vec<LoadPlan> = alloc::vec::Vec::with_capacity(images.len());
    for image in images {
        plans.push(kshared::elf::parse_elf64(image)?);
    }
    // Cross-image page overlap: the parser checks segments within one image;
    // this is the same check across images, and it must pass before any page
    // is mapped so a refused run leaves nothing behind.
    for (i, a) in plans.iter().enumerate() {
        for b in plans.iter().skip(i + 1) {
            for sa in a.segments() {
                let ra = sa.vaddr..sa.vaddr + sa.page_count() * 4096;
                for sb in b.segments() {
                    let rb = sb.vaddr..sb.vaddr + sb.page_count() * 4096;
                    if ra.start < rb.end && rb.start < ra.end {
                        return Err(ElfError::Overlap);
                    }
                }
            }
        }
    }

    for (plan, image) in plans.iter().zip(images) {
        // Map every segment writable + NX for the copy: a page is never
        // writable and executable at the same time, even transiently.
        for seg in plan.segments() {
            for page in 0..seg.page_count() {
                crate::memory::map_user_page(seg.vaddr + page * 4096, true, false);
            }
            // Copy the file-backed bytes page by page through the kernel's
            // physical alias (SMAP-safe: the loader never touches the user
            // VA). The parser bounds-checked `file_start..+filesz` against
            // the image. The `memsz` tail past `filesz` is BSS, and frames
            // arrive zeroed, so it is already correct.
            let src = &image[seg.file_start..seg.file_start + seg.filesz as usize];
            let mut copied = 0usize;
            while copied < src.len() {
                let page_off = (seg.vaddr as usize + copied) & 0xfff;
                let chunk = (4096 - page_off).min(src.len() - copied);
                crate::memory::copy_into_user_page(
                    seg.vaddr + copied as u64,
                    &src[copied..copied + chunk],
                );
                copied += chunk;
            }
        }
        // Lock each page to its final W^X permissions (the parser refused
        // any segment claiming both).
        for seg in plan.segments() {
            for page in 0..seg.page_count() {
                crate::memory::update_user_page(
                    seg.vaddr + page * 4096,
                    seg.writable,
                    seg.executable,
                );
            }
        }
    }
    // Stack pages: user-accessible, writable, never executable; one per task.
    for &stack in USER_STACK_ADDRS.iter().take(plans.len()) {
        crate::memory::map_user_page(stack, true, false);
    }

    // Hand the set to the scheduler. The int 0x80 gate clears IF on the way
    // in, so control returns here with interrupts disabled; restore them only
    // if the caller had them enabled.
    let entries: alloc::vec::Vec<(u64, u64)> = plans
        .iter()
        .enumerate()
        .map(|(i, plan)| (plan.entry, USER_STACK_ADDRS[i] + 4096))
        .collect();
    let interrupts_were_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let first_rsp = sched::install(&entries);
    // SAFETY: `first_rsp` is the frame `install` just fabricated, interrupts
    // are disabled, and every task's code and stack pages were mapped above;
    // control returns (via the launcher continuation) once the last task has
    // exited and the scheduler has deactivated itself.
    unsafe { sched::enter_tasks(first_rsp) };
    let report = sched::collect();
    if interrupts_were_enabled {
        x86_64::instructions::interrupts::enable();
    }

    // Tear the user mappings down so the loader can run again (the frames
    // are not reclaimed — bump allocator — only the mappings).
    for plan in &plans {
        for seg in plan.segments() {
            for page in 0..seg.page_count() {
                crate::memory::unmap_user_page(seg.vaddr + page * 4096);
            }
        }
    }
    for &stack in USER_STACK_ADDRS.iter().take(plans.len()) {
        crate::memory::unmap_user_page(stack);
    }
    Ok(report)
}

/// The `int 0x80` handler. On entry (from ring 3) the CPU has pushed
/// SS/RSP/RFLAGS/CS/RIP and switched to the current task's kernel stack
/// (TSS RSP0). The user passes the syscall number in `rax` and arguments in
/// `rdi`/`rsi`.
#[unsafe(naked)]
unsafe extern "C" fn int80_entry() {
    core::arch::naked_asm!(
        // Clear EFLAGS.AC first. An interrupt/trap gate clears IF, TF, NT, RF
        // and VM — but NOT AC, which ring 3 can set with `popfq`. SMAP only
        // suppresses supervisor accesses to user pages while AC == 0, so a
        // ring-3 program that left AC set would turn SMAP off for the whole
        // kernel entry (this handler and every IRQ/exception taken while it
        // runs). We clear it in the portable form — `and` a masked dword that
        // zeroes bit 18 — rather than `clac`, which is #UD on a CPU without
        // SMAP (the hardware funnel invites pre-Haswell machines).
        // IF is already 0 from the gate, so popfq cannot re-enable interrupts.
        "pushfq",
        // AC is bit 18, in the low dword of the pushed RFLAGS; masking just the
        // dword keeps the immediate within imm32 range (a qword AND would need
        // a sign-extended imm32 that 0xFFFBFFFF is not). The high dword of
        // RFLAGS is reserved zero, so leaving it untouched is correct.
        "and dword ptr [rsp], 0xFFFBFFFF",
        "popfq",
        // The interrupt gate clears IF but NOT DF; SysV requires DF=0 at a call
        // boundary, so clear it before any Rust code runs.
        "cld",
        "cmp rax, {sys_exit}",
        "jne 2f",
        // SYS_EXIT: hand the exit code (already in rdi — the user's a0) to the
        // scheduler. It marks the task exited and returns either the next
        // ready task's saved context, or 0 meaning the run is complete.
        // The 5-qword CPU frame leaves rsp ≡ 8 mod 16; align for the call.
        "sub rsp, 8",
        "call {exit}",
        "add rsp, 8",
        "test rax, rax",
        "jz 3f",
        // Resume the next task exactly where its last save left it: the same
        // 15-pop + iretq tail sched::timer_entry uses, because a saved
        // context has one format no matter which entry saved it.
        "mov rsp, rax",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rbp",
        "pop rbx",
        "pop rdx",
        "pop rcx",
        "pop rax",
        "iretq",
        "3:",
        // Run complete: restore the launcher's stack AND its callee-saved
        // registers, then return into run_programs.
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
        exit = sym sched::sys_exit,
        cont = sym KERNEL_CONTINUATION_RSP,
        dispatch = sym syscall_dispatch,
    )
}

/// Records whether `EFLAGS.AC` was still set when the kernel reached the
/// syscall dispatcher — proof that the AC scrub in `int80_entry` ran. The demo
/// programs set AC before their syscalls; with the scrub this reads false,
/// without it, true.
#[cfg(feature = "selftest")]
pub static SYSCALL_ENTRY_AC: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

extern "C" fn syscall_dispatch(nr: u64, a0: u64, _a1: u64) -> u64 {
    #[cfg(feature = "selftest")]
    {
        use core::sync::atomic::Ordering;
        use x86_64::registers::rflags::{self, RFlags};
        let ac = rflags::read().contains(RFlags::ALIGNMENT_CHECK);
        SYSCALL_ENTRY_AC.store(ac, Ordering::SeqCst);
    }
    match nr {
        SYS_WRITE => {
            // The user program's output belongs on the LOCAL console, never on
            // serial: serial is an off-device channel the keystroke-privacy
            // allowlist requires stays silent after boot, and a security review
            // found that a serial write here let the shipped `user` command
            // emit to serial post-boot. Rendering to the console keeps the byte
            // on screen (correct for program output) and serial empty. The CI
            // still proves the syscall ran through the ring-3 exit-code and
            // AC-scrub assertions, not through a serial line.
            let byte = (a0 & 0xff) as u8 as char;
            crate::console::with_console(|c| {
                use core::fmt::Write;
                let _ = write!(c, "{byte}");
            });
            0
        }
        _ => u64::MAX,
    }
}
