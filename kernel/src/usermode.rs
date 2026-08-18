//! Ring 3 and a software-interrupt (`int 0x80`) system-call path — Osmium's
//! privilege boundary — plus the ELF64 loader that feeds it.
//!
//! The user programs are real, linker-scripted Rust ELFs (`user/hello`,
//! `user/counter`), built by the kernel's build script and embedded here.
//! Since M9 every program runs in its **own address space**
//! ([`crate::memory::AddressSpace`]): [`run_programs`] parses each image with
//! `kshared::elf` (host-tested, refusal-by-default), maps its `PT_LOAD`
//! segments per-segment W^X into a private per-task page table, gives it its
//! own stack page and kernel stack, and hands the set to the scheduler
//! (`sched`), which runs them at CPL 3 under preemptive round-robin, loading
//! each task's CR3 as it becomes current. Two programs may therefore occupy
//! the **same virtual addresses** — same code window, same stack address —
//! and neither can see or touch the other's memory; the battery proves it by
//! running two instances of the same image at the same VA and asserting each
//! saw its own private, freshly-initialised data segment.
//!
//! Each program leaves by issuing `int 0x80` with `SYS_EXIT`; when the last
//! one exits, the scheduler restores the kernel's own CR3 and the entry stub
//! returns to the launcher. Other syscalls dispatch to Rust and `iretq` back
//! into whichever task made them.
//!
//! Privacy carries over: the frames a task runs on are zeroed on hand-out
//! like every other (which is also what makes BSS correct for free), a dead
//! task's kernel stack is zeroed when it is freed (the allocator scrubs on
//! free), and the self-tests audit the KERNEL's page table for user bits —
//! which, since user mappings only ever exist in per-task spaces, must never
//! carry a single one, before, during or after a run.
//!
//! Boundaries, stated so they are not mistaken for isolation guarantees:
//! - **A misbehaving user program is terminated alone (M10).** Every fault
//!   handler forks on the faulting CPL: a ring-3 fault calls
//!   `sched::kill_current`, which records the vector and resumes the next
//!   ready task or returns to the launcher — the machine and every other
//!   task keep running. A kernel-context fault still panics (a kernel bug is
//!   not a schedulable event), and NMI/#MC/#DF stay panic-only at any CPL
//!   (machine-level events, not something the current task did).
//! - **One run at a time.** The shell issues runs synchronously; the
//!   scheduler asserts it is never installed while active.
//! - **Frames are not reclaimed.** The bump allocator never frees, so each
//!   run leaks its mapped frames and its page-table frames (a handful per
//!   task); many manual `user`/`sched` shell invocations would eventually
//!   exhaust RAM and panic. The parser's 64-page budget bounds how fast a
//!   single load can drain the allocator.

use crate::memory::AddressSpace;
use crate::sched::{self, LaunchSpec, RunReport};
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

/// The user stack page — the SAME virtual address in every task's space
/// (M9), which is itself a statement of the isolation model: program
/// segments live in the image window
/// (`kshared::elf::USER_IMAGE_BASE..USER_IMAGE_END`), the stack sits outside
/// it, and each task's page table maps both privately.
pub(crate) const USER_STACK_ADDR: u64 = 0x80_0000;

/// The most programs one run can schedule. Two exercises every multi-task
/// path (rotation, same-VA isolation) without inviting an unbounded fleet
/// onto a bump allocator.
pub const MAX_TASKS: usize = 2;

/// The embedded user programs: real linker-scripted Rust ELFs built from
/// `user/hello` and `user/counter` by the kernel's build script. The battery
/// additionally embeds the counter linked at a second base (`link_alt.ld`) —
/// a leftover convenience from M8's shared-address-space era that still earns
/// its place: scheduling it against the primary counter proves sustained
/// rotation with DIFFERENT layouts, while two same-base instances prove
/// same-VA isolation.
static HELLO_ELF: &[u8] = include_bytes!(env!("HELLO_ELF"));
static COUNTER_ELF: &[u8] = include_bytes!(env!("COUNTER_ELF"));
/// The fault-isolation demo program (M10): announces itself, then page-faults
/// at CPL 3. Linked at hello's base — legal since M9, and running the pair at
/// identical addresses exercises address-space isolation in the same breath.
static CRASHER_ELF: &[u8] = include_bytes!(env!("CRASHER_ELF"));
#[cfg(feature = "selftest")]
static COUNTER_ALT_ELF: &[u8] = include_bytes!(env!("COUNTER_ALT_ELF"));

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

/// The M9 isolation proof: two instances of the SAME image, occupying the
/// SAME virtual addresses, each in its own address space. Before M9 this was
/// the overlap-refusal case; now both must run — and each must see its own
/// private, freshly-initialised data segment (hello reports the value it
/// read back, and a shared page would show the first instance's write to the
/// second).
#[cfg(feature = "selftest")]
pub fn run_hello_twice() -> Result<RunReport, ElfError> {
    run_programs(&[HELLO_ELF, HELLO_ELF])
}

/// Two unyielding compute programs — the counter at two different bases —
/// scheduled against each other for the battery's sustained round-robin
/// proof: neither ever yields, so every quantum boundary for the whole run
/// is a timer-driven switch to the other task.
#[cfg(feature = "selftest")]
pub fn run_two_counters() -> Result<RunReport, ElfError> {
    run_programs(&[COUNTER_ELF, COUNTER_ALT_ELF])
}

/// The M10 fault-isolation demonstration, shared by the `crash` shell command
/// and the battery: `crasher` (announces itself, then page-faults at CPL 3)
/// launched first, `hello` second. The kernel must terminate the crasher and
/// nothing else — hello completes normally and the machine keeps running.
pub fn run_crasher_and_hello() -> Result<RunReport, ElfError> {
    run_programs(&[CRASHER_ELF, HELLO_ELF])
}

/// The crasher alone: deterministically exercises the kill path's other
/// branch — the faulting task is the LAST one alive, so the kernel must
/// restore its own world and return to the launcher from inside a fault
/// handler. (In the pair run that branch is only reached if a timer tick
/// happens to let hello exit first.)
#[cfg(feature = "selftest")]
pub fn run_crasher_alone() -> Result<RunReport, ElfError> {
    run_programs(&[CRASHER_ELF])
}

/// Parses, maps into per-task address spaces, schedules and tears down up to
/// [`MAX_TASKS`] static ELF64 user programs. Refusal happens before anything
/// is mapped: every image must parse (`kshared::elf`, refusal-by-default).
/// There is no cross-image overlap check any more, and its absence is the
/// point — images that claim the same virtual pages land in different page
/// tables, which is exactly what M9 exists to allow.
pub fn run_programs(images: &[&[u8]]) -> Result<RunReport, ElfError> {
    assert!(
        !images.is_empty() && images.len() <= MAX_TASKS,
        "run_programs takes 1..={MAX_TASKS} images"
    );
    // The syscall dispatcher writes SYS_WRITE output to the console, so the
    // caller must not hold the console lock across a ring-3 run or SYS_WRITE
    // would deadlock against it. All callers (the `user`/`sched` shell
    // commands and the battery) call this outside any `with_console`; this
    // pins that. A real assert (the kernel is only ever built --release,
    // where a debug_assert is dead code): with the lock held, a task's
    // SYS_WRITE would spin forever behind an interrupt gate — an
    // undiagnosable silent hang, which this turns into a named panic.
    assert!(
        !crate::console::CONSOLE.is_locked(),
        "run_programs called while the console lock is held; SYS_WRITE would deadlock"
    );

    let mut plans: alloc::vec::Vec<LoadPlan> = alloc::vec::Vec::with_capacity(images.len());
    for image in images {
        plans.push(kshared::elf::parse_elf64(image)?);
    }

    // Build each program its own world: a fresh address space, its segments
    // mapped and locked W^X, its stack page — all before any task runs.
    let mut spaces: alloc::vec::Vec<AddressSpace> = alloc::vec::Vec::with_capacity(images.len());
    for (plan, image) in plans.iter().zip(images) {
        let mut space = AddressSpace::new_user();
        // Map every segment writable + NX for the copy: a page is never
        // writable and executable at the same time, even transiently.
        for seg in plan.segments() {
            for page in 0..seg.page_count() {
                space.map_user_page(seg.vaddr + page * 4096, true, false);
            }
            // Copy the file-backed bytes page by page through the kernel's
            // physical alias (SMAP-safe, and the only route that exists —
            // the user VA is not mapped in the kernel's own table). The
            // parser bounds-checked `file_start..+filesz` against the image.
            // The `memsz` tail past `filesz` is BSS, and frames arrive
            // zeroed, so it is already correct.
            let src = &image[seg.file_start..seg.file_start + seg.filesz as usize];
            let mut copied = 0usize;
            while copied < src.len() {
                let page_off = (seg.vaddr as usize + copied) & 0xfff;
                let chunk = (4096 - page_off).min(src.len() - copied);
                space.copy_into_user_page(seg.vaddr + copied as u64, &src[copied..copied + chunk]);
                copied += chunk;
            }
        }
        // Lock each page to its final W^X permissions (the parser refused
        // any segment claiming both).
        for seg in plan.segments() {
            for page in 0..seg.page_count() {
                space.update_user_page(seg.vaddr + page * 4096, seg.writable, seg.executable);
            }
        }
        // Stack page: user-accessible, writable, never executable — the same
        // virtual address in every space.
        space.map_user_page(USER_STACK_ADDR, true, false);
        // The battery audits every REAL loaded space for W^X, not just the
        // synthetic probe: a loader bug that left the stack or a data segment
        // executable would otherwise pass, since no program executes from
        // them (adversarial-review finding).
        #[cfg(feature = "selftest")]
        assert!(
            space.user_mappings_are_wx_clean(),
            "a loaded address space contains a writable+executable user page"
        );
        spaces.push(space);
    }

    // Hand the set to the scheduler. The int 0x80 gate clears IF on the way
    // in, so control returns here with interrupts disabled; restore them only
    // if the caller had them enabled.
    let specs: alloc::vec::Vec<LaunchSpec> = plans
        .iter()
        .zip(&spaces)
        .map(|(plan, space)| LaunchSpec {
            entry: plan.entry,
            user_stack_top: USER_STACK_ADDR + 4096,
            cr3: space.cr3(),
        })
        .collect();
    let interrupts_were_enabled = x86_64::instructions::interrupts::are_enabled();
    x86_64::instructions::interrupts::disable();
    let first_rsp = sched::install(&specs);
    // SAFETY: `first_rsp` is the frame `install` just fabricated, interrupts
    // are disabled, task 0's address space is loaded and its code and stack
    // pages are mapped in it; control returns (via the launcher continuation,
    // with the kernel's own CR3 restored) once the last task has exited and
    // the scheduler has deactivated itself.
    unsafe { sched::enter_tasks(first_rsp) };
    let report = sched::collect();
    if interrupts_were_enabled {
        x86_64::instructions::interrupts::enable();
    }

    // Teardown is dropping the spaces: the kernel's own table was never
    // touched, so there is nothing to unmap — the per-task tables and user
    // frames leak (bump allocator), the same accepted cost as before, now
    // including a handful of page-table frames per task.
    drop(spaces);
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
        // ready task's saved context (having switched RSP0 and CR3 to that
        // task), or 0 meaning the run is complete (kernel CR3 restored).
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
