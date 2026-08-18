//! Preemptive round-robin scheduling of ring-3 tasks (M8).
//!
//! The kernel itself is never preempted: the timer switches contexts only
//! when the interrupted code was running at CPL 3, so every kernel path —
//! syscall dispatch, the shell, the launcher — runs to completion exactly as
//! it did before this module existed. What changed is what happens to *user*
//! programs: each timer tick taken from ring 3 lands in [`timer_entry`],
//! which saves the interrupted program's complete register file on its own
//! kernel stack, asks [`timer_tick`] for the next runnable task, and resumes
//! that task wherever *it* was interrupted. A program that never yields —
//! never blocks, never syscalls — loses the CPU anyway. That is the whole
//! definition of preemption, and the self-test battery proves it by exit
//! order: a short program launched second exits first, while an unyielding
//! compute loop launched first is still running.
//!
//! Design invariants, each load-bearing:
//! - **One kernel stack per task, and TSS RSP0 always names the current
//!   task's.** A trap from ring 3 pushes its frame wherever RSP0 points; if
//!   two tasks shared one privilege stack, the second task's trap would
//!   overwrite the first task's saved context. Every switch therefore
//!   rewrites RSP0 (`gdt::set_privilege_stack`) before it resumes the next
//!   task.
//! - **The saved context is the 15 GP registers plus the CPU's interrupt
//!   frame, and that is complete.** Both the kernel and the user programs are
//!   compiled for `x86_64-unknown-none`, which has no SSE/AVX and soft
//!   floats, so there is no SIMD or x87 state to save; segment bases (FS/GS)
//!   are unused. The layout is fixed by [`timer_entry`]'s push order and
//!   shared by fabricated initial frames — see [`REGS_SAVED`].
//! - **Scheduler state is locked only with interrupts off.** `SCHED` is a
//!   spinlock touched from the timer handler and the `int 0x80` exit path
//!   (both run with IF=0, interrupt gates) and from the launcher (which
//!   disables interrupts around install/collect). On this single-core kernel
//!   that makes contention impossible rather than merely handled; real
//!   `assert!`s pin the discipline in every build (the kernel only ever
//!   builds --release, where a `debug_assert` is dead code).
//! - **`EFLAGS.AC` is scrubbed at every kernel entry ring 3 controls.** An
//!   interrupt gate clears IF/TF/NT/RF/VM but NOT AC, and SMAP is inert while
//!   AC is set. The syscall gate has scrubbed AC since M7; the timer entry
//!   must do the same or a hostile program could hold SMAP off for every
//!   asynchronous kernel entry taken while it runs. The battery proves the
//!   scrub by running exactly such a program (`user/counter` holds AC set for
//!   its entire run) and asserting the kernel never observed the flag.
//! - **RSP0 and CR3 change together, or not at all (M9).** A switch is three
//!   writes under one lock with IF=0: `current`, RSP0 (where the next trap
//!   lands) and CR3 (which memory the next task sees). Loading a task's CR3
//!   under the kernel's feet is sound because every space's kernel half is a
//!   clone of the kernel's own table — only the low user slot differs.

use alloc::boxed::Box;
use alloc::vec::Vec;
use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::structures::paging::PhysFrame;

/// Bytes of kernel stack per task. Matches the default privilege stack: deep
/// enough for any trap the task can take (syscall dispatch, an exception's
/// panic path), small enough that two tasks cost 40 KiB of the 1 MiB heap.
pub const KSTACK_SIZE: usize = 20 * 1024;

/// How many general-purpose registers [`timer_entry`] saves below the CPU's
/// interrupt frame. Push order (top of stack last): rax, rcx, rdx, rbx, rbp,
/// rsi, rdi, r8..r15 — so the saved CS sits at `rsp + REGS_SAVED*8 + 8`.
const REGS_SAVED: u64 = 15;
/// Byte offset of the saved CS selector from the post-save stack pointer:
/// 15 saved registers, then RIP, then CS.
const SAVED_CS_OFFSET: u64 = REGS_SAVED * 8 + 8;

/// One schedulable ring-3 task.
struct Task {
    /// The task's own kernel stack (heap-allocated; freed — and therefore
    /// zeroed, the allocator scrubs on free — when the run's tasks are
    /// collected).
    kstack: Box<[u8]>,
    /// Where the task's saved context lives: the value RSP had after
    /// [`timer_entry`] finished saving, or the fabricated initial frame for a
    /// task that has not run yet. Only meaningful while `ready`.
    saved_rsp: u64,
    /// The task's address-space root (M9): loaded into CR3 whenever this task
    /// becomes current, so each task sees its own private user mappings under
    /// a shared kernel half. The kernel keeps running across the load because
    /// every kernel mapping exists identically in every space.
    cr3: PhysFrame,
    ready: bool,
    exit_code: u64,
    /// Set when the task was TERMINATED by the kernel for a ring-3 fault
    /// (M10) rather than exiting voluntarily: the exception vector that
    /// killed it. A faulted task's `exit_code` is 0.
    fault: Option<u8>,
    /// Order of exit across the run: 0 for the first task to exit, 1 for the
    /// next. The battery's preemption proof is this field.
    exit_seq: u64,
}

/// Loads a task's (or the kernel's) page-table root. The context-switch half
/// of M9's isolation: RSP0 says where the next trap lands, CR3 says which
/// memory the next task sees, and every switch updates both together.
///
/// SAFETY wrapper: callers pass either `memory::kernel_cr3()` or an
/// [`crate::memory::AddressSpace`]'s root, both valid tables whose kernel
/// halves are identical — which is the invariant that makes switching under
/// the kernel's feet sound. Interrupts are off at every call site.
fn load_cr3(root: PhysFrame) {
    // SAFETY: per the doc comment — a valid PML4 sharing the kernel half.
    unsafe { Cr3::write(root, Cr3Flags::empty()) };
}

impl Task {
    /// The 16-byte-aligned top of this task's kernel stack — the value RSP0
    /// holds while the task is current.
    fn kstack_top(&self) -> u64 {
        (self.kstack.as_ptr() as u64 + self.kstack.len() as u64) & !0xf
    }
}

/// The scheduler: a fixed task table (built before activation, never resized
/// while active — the timer path must not allocate), the current index, and
/// the run's counters.
struct Sched {
    tasks: Vec<Task>,
    current: usize,
    active: bool,
    /// Timer-driven switches that landed on a DIFFERENT task.
    preemptive_switches: u64,
    /// Timer interrupts taken from ring 3 while active — each one is a full
    /// save/restore round trip through the context-switch machinery, even
    /// when the round-robin lands back on the same task.
    ring3_round_trips: u64,
    /// Fault kills that resumed a SURVIVING task (`kill_current`'s
    /// `Some(next)` branch, M10). Which branch a pair run takes depends on
    /// tick phase — a tick landing before the fault lets the neighbour finish
    /// first and routes the kill through the launcher return instead — so the
    /// battery repeats the run until this counter proves the resume path ran.
    fault_kill_resumes: u64,
    exit_counter: u64,
}

impl Sched {
    /// The next ready task after `from`, round-robin with wraparound. While
    /// the scheduler is active at least one task is ready or exiting is in
    /// progress, so the timer path (where `from` itself is still ready)
    /// always finds one.
    fn next_ready(&self, from: usize) -> Option<usize> {
        let n = self.tasks.len();
        (1..=n)
            .map(|k| (from + k) % n)
            .find(|&i| self.tasks[i].ready)
    }
}

static SCHED: Mutex<Sched> = Mutex::new(Sched {
    tasks: Vec::new(),
    current: 0,
    active: false,
    preemptive_switches: 0,
    ring3_round_trips: 0,
    fault_kill_resumes: 0,
    exit_counter: 0,
});

/// Records whether `EFLAGS.AC` was ever still set when the timer handler's
/// Rust half ran — proof the naked entry's AC scrub executed. `user/counter`
/// holds AC set for its whole run, so with the scrub deleted this reads true
/// on the first preemption.
#[cfg(feature = "selftest")]
pub static TIMER_ENTRY_AC: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// What one task did with its run, reported to the launcher.
pub struct TaskExit {
    pub code: u64,
    /// 0 = exited first, 1 = second, ...
    pub seq: u64,
    /// `Some(vector)` if the kernel terminated the task for a ring-3 fault
    /// (M10) instead of the task exiting voluntarily.
    pub fault: Option<u8>,
}

/// The launcher-facing summary of a completed scheduler run.
pub struct RunReport {
    /// Per launched task, in launch order.
    pub exits: Vec<TaskExit>,
    pub preemptive_switches: u64,
    pub ring3_round_trips: u64,
    /// How many fault kills resumed a surviving task (M10) — the kill path's
    /// `resume_context` branch, as opposed to its return-to-launcher branch.
    pub fault_kill_resumes: u64,
}

/// What the launcher hands `install` per task: where to start it, where its
/// user stack tops out, and which address space it runs in.
pub struct LaunchSpec {
    pub entry: u64,
    pub user_stack_top: u64,
    pub cr3: PhysFrame,
}

/// Builds the task table for `specs`, fabricates each task's initial context
/// frame, marks the scheduler active, points RSP0 at task 0's kernel stack
/// and loads task 0's address space. Returns task 0's initial `saved_rsp`
/// for [`enter_tasks`].
///
/// Must be called with interrupts disabled; allocates (kernel stacks), which
/// is fine in the launcher's context and forbidden in the timer's — which is
/// why the table is fully built here and only indexed there.
pub fn install(specs: &[LaunchSpec]) -> u64 {
    // Real asserts throughout this module, not debug_asserts: the kernel is
    // only ever built --release, where a debug_assert is dead code — a lesson
    // an adversarial review of M8 landed the same day the milestone did.
    assert!(
        !x86_64::instructions::interrupts::are_enabled(),
        "scheduler installed with interrupts enabled"
    );
    let sel = crate::gdt::selectors();
    let user_cs = u64::from(sel.user_code.0); // RPL 3 already in the selector
    let user_ss = u64::from(sel.user_data.0);

    let mut tasks = Vec::with_capacity(specs.len());
    for spec in specs {
        let mut kstack = alloc::vec![0u8; KSTACK_SIZE].into_boxed_slice();
        let saved_rsp = build_initial_frame(
            &mut kstack,
            spec.entry,
            spec.user_stack_top,
            user_cs,
            user_ss,
        );
        tasks.push(Task {
            kstack,
            saved_rsp,
            cr3: spec.cr3,
            ready: true,
            exit_code: 0,
            fault: None,
            exit_seq: 0,
        });
    }

    let mut sched = SCHED.lock();
    assert!(!sched.active, "scheduler installed while already active");
    sched.tasks = tasks;
    sched.current = 0;
    sched.active = true;
    sched.preemptive_switches = 0;
    sched.ring3_round_trips = 0;
    sched.fault_kill_resumes = 0;
    sched.exit_counter = 0;
    crate::gdt::set_privilege_stack(VirtAddr::new(sched.tasks[0].kstack_top()));
    // Enter task 0's world before `enter_tasks` iretqs into it: its user
    // pages exist only in its own space. The kernel keeps running on the
    // shared kernel half until then.
    load_cr3(sched.tasks[0].cr3);
    sched.tasks[0].saved_rsp
}

/// Drains the finished run's results. Must be called with interrupts still
/// disabled, after [`enter_tasks`] has returned (which only happens once the
/// last task exited and `sys_exit` deactivated the scheduler). Dropping the
/// task table frees each kernel stack, and the allocator's zero-on-free scrub
/// means a dead task's saved registers do not linger in the heap.
pub fn collect() -> RunReport {
    assert!(
        !x86_64::instructions::interrupts::are_enabled(),
        "scheduler results collected with interrupts enabled"
    );
    let mut sched = SCHED.lock();
    assert!(!sched.active, "collect() while the scheduler is active");
    let tasks = core::mem::take(&mut sched.tasks);
    RunReport {
        exits: tasks
            .iter()
            .map(|t| TaskExit {
                code: t.exit_code,
                seq: t.exit_seq,
                fault: t.fault,
            })
            .collect(),
        preemptive_switches: sched.preemptive_switches,
        ring3_round_trips: sched.ring3_round_trips,
        fault_kill_resumes: sched.fault_kill_resumes,
    }
}

/// Fabricates the initial saved context for a task that has not run yet, at
/// the top of its kernel stack: the CPU interrupt frame (RIP=entry, ring-3
/// CS/SS, RFLAGS with IF set so the timer can preempt it, RSP=its user
/// stack), below it the 15 general-purpose registers all zero — the same
/// scrub-before-ring-3 guarantee `jump_to_user` gave M6, expressed as data.
/// The restore path (`pop` x15, `iretq`) cannot tell this frame from one
/// [`timer_entry`] saved, which is the point: first launch and every later
/// resume go through one code path.
fn build_initial_frame(kstack: &mut [u8], entry: u64, user_rsp: u64, cs: u64, ss: u64) -> u64 {
    let top = ((kstack.as_ptr() as u64 + kstack.len() as u64) & !0xf) - kstack.as_ptr() as u64;
    let mut off = top as usize;
    let mut push = |kstack: &mut [u8], val: u64| {
        off -= 8;
        kstack[off..off + 8].copy_from_slice(&val.to_le_bytes());
    };
    // iretq frame, pushed in hardware order (SS first — highest address).
    push(kstack, ss);
    push(kstack, user_rsp);
    push(kstack, 0x202); // RFLAGS: IF set, reserved bit 1 set, IOPL 0
    push(kstack, cs);
    push(kstack, entry);
    // 15 zeroed GP registers in timer_entry's save order (rax pushed first,
    // r15 last, so r15 ends lowest).
    for _ in 0..REGS_SAVED {
        push(kstack, 0);
    }
    kstack.as_ptr() as u64 + off as u64
}

/// The timer-interrupt entry (vector 32), installed raw in the IDT. Saves
/// the full register file, lets [`timer_tick`] account the tick and pick the
/// next context, then restores whichever context it returned — the same one
/// when no switch is due, another task's when one is.
///
/// The AC scrub at the top mirrors `int80_entry`'s and exists for the same
/// reason: an interrupt gate does not clear AC, ring 3 owns it, and SMAP is
/// only as strong as the scrub at every entry (module docs). `cld` likewise:
/// the gate clears IF but not DF, and SysV requires DF=0 at a call boundary.
#[unsafe(naked)]
unsafe extern "C" fn timer_entry() {
    core::arch::naked_asm!(
        // Scrub EFLAGS.AC (bit 18, low dword of the pushed RFLAGS; the same
        // portable masked-AND int80_entry uses — no `clac`, which is #UD on a
        // CPU without SMAP). IF is already 0 from the gate, so popfq cannot
        // re-enable interrupts.
        "pushfq",
        "and dword ptr [rsp], 0xFFFBFFFF",
        "popfq",
        "cld",
        // Save the interrupted context's GP registers. Order is the contract
        // shared with build_initial_frame and the restore sequence below.
        "push rax",
        "push rcx",
        "push rdx",
        "push rbx",
        "push rbp",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        // rsp now points at the complete saved context (15 regs + CPU frame)
        // and is 16-byte aligned: the CPU aligned the stack before pushing
        // its 5-qword frame (leaving rsp ≡ 8 mod 16), and 15 pushes restore
        // ≡ 0 — the SysV requirement at a call instruction.
        "mov rdi, rsp",
        "call {tick}",
        // rax = the context to resume: unchanged rsp, or another task's.
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
        tick = sym timer_tick,
    )
}

/// The timer entry's address for the IDT gate.
pub fn timer_entry_addr() -> u64 {
    timer_entry as *const () as u64
}

/// The Rust half of the timer interrupt: counts the tick, acknowledges the
/// PIC, and — only when the interrupted context was ring 3 and the scheduler
/// is active — performs the round-robin switch. Returns the stack pointer of
/// the context [`timer_entry`] should restore.
extern "C" fn timer_tick(rsp: u64) -> u64 {
    use core::sync::atomic::Ordering;
    crate::interrupts::TICKS.fetch_add(1, Ordering::Relaxed);
    // EOI first, and never while holding SCHED: handlers hold at most one
    // lock at a time (TDD concurrency rule).
    crate::interrupts::end_of_interrupt(crate::interrupts::InterruptIndex::Timer);

    #[cfg(feature = "selftest")]
    {
        use x86_64::registers::rflags::{self, RFlags};
        if rflags::read().contains(RFlags::ALIGNMENT_CHECK) {
            TIMER_ENTRY_AC.store(true, Ordering::SeqCst);
        }
    }

    // Preempt only ring-3 contexts: the saved CS's low two bits are the
    // interrupted CPL. Kernel code (the shell, the launcher, a syscall that
    // re-enabled nothing — IF is 0 in handlers, so that last cannot even
    // occur) is never preempted.
    // SAFETY: `rsp` is the just-saved context timer_entry built; the CPU
    // frame sits at the documented fixed offset above the 15 saved registers.
    let saved_cs = unsafe { *((rsp + SAVED_CS_OFFSET) as *const u64) };
    // Layout tripwire, live in every build on every tick: the only code
    // selectors an interrupt frame can carry are the kernel's and the user's,
    // so anything else here means SAVED_CS_OFFSET no longer points at CS —
    // e.g. a re-ordered push sequence would land on a saved register or SS,
    // some of which also read as CPL 3 and would silently mis-gate the
    // switch. (An adversarial review found the offset was otherwise asserted
    // by comment alone.)
    let sel = crate::gdt::selectors();
    assert!(
        saved_cs == u64::from(sel.kernel_code.0) || saved_cs == u64::from(sel.user_code.0),
        "saved CS {saved_cs:#x} is neither the kernel nor the user code selector: \
         the saved-context layout is broken"
    );
    if saved_cs & 3 != 3 {
        return rsp;
    }

    let mut sched = SCHED.lock();
    if !sched.active {
        return rsp;
    }
    sched.ring3_round_trips += 1;
    let cur = sched.current;
    sched.tasks[cur].saved_rsp = rsp;
    // The current task is still ready, so next_ready always finds a task
    // (itself, when it is the only one left).
    let next = sched
        .next_ready(cur)
        .expect("active scheduler with no ready task");
    if next != cur {
        sched.preemptive_switches += 1;
        sched.current = next;
        crate::gdt::set_privilege_stack(VirtAddr::new(sched.tasks[next].kstack_top()));
        load_cr3(sched.tasks[next].cr3);
    }
    sched.tasks[next].saved_rsp
}

/// The `SYS_EXIT` half of scheduling, called from `int80_entry` (IF is 0 —
/// interrupt gate). Marks the current task exited and returns either the next
/// ready task's saved context (the asm resumes it with the shared
/// pop-15/`iretq` sequence) or 0, meaning no task remains: deactivate, point
/// RSP0 back at the default privilege stack, and let the asm restore the
/// launcher continuation.
pub extern "C" fn sys_exit(code: u64) -> u64 {
    let mut sched = SCHED.lock();
    assert!(sched.active, "SYS_EXIT outside a scheduler run");
    let cur = sched.current;
    sched.tasks[cur].ready = false;
    sched.tasks[cur].exit_code = code;
    sched.tasks[cur].exit_seq = sched.exit_counter;
    sched.exit_counter += 1;
    match sched.next_ready(cur) {
        Some(next) => {
            sched.current = next;
            crate::gdt::set_privilege_stack(VirtAddr::new(sched.tasks[next].kstack_top()));
            load_cr3(sched.tasks[next].cr3);
            sched.tasks[next].saved_rsp
        }
        None => {
            sched.active = false;
            crate::gdt::set_privilege_stack(crate::gdt::default_privilege_stack_top());
            // Leave the dead task's world: the launcher resumes in the
            // kernel's own address space, whose table never carried a user
            // mapping (the M9 audit's whole claim).
            load_cr3(crate::memory::kernel_cr3());
            0
        }
    }
}

/// Enters the first task of an installed run and does not return until the
/// last task has exited (`sys_exit` returns 0 to the `int 0x80` entry, whose
/// launcher path restores the continuation saved here). The callee-saved
/// registers are pushed first and the stack pointer saved above them, exactly
/// as M6's `jump_to_user` did — `KERNEL_CONTINUATION_RSP`'s invariant (written
/// before any ring-3 instruction can execute) is preserved because this is
/// now the only road to ring 3.
///
/// # Safety
/// `first_rsp` must be the value [`install`] just returned, interrupts must
/// be disabled, and the task's code/stack pages must be mapped.
#[unsafe(naked)]
pub unsafe extern "C" fn enter_tasks(first_rsp: u64) {
    core::arch::naked_asm!(
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rip + {cont}], rsp", // save the stack just above the saved regs
        "mov rsp, rdi",
        // Restore the fabricated initial context: 15 (zeroed) GP registers,
        // then iretq into ring 3. Identical to timer_entry's restore tail —
        // first launch and every later resume are the same operation.
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
        cont = sym crate::usermode::KERNEL_CONTINUATION_RSP,
    )
}

/// Terminates the CURRENT task in response to a ring-3 fault (M10) and never
/// returns: the fault handlers call this instead of panicking when the
/// faulting context was CPL 3, which is what turns "a user program crashed"
/// from a machine-down event into a scheduling event. The dying task's
/// registers need no saving — it will never resume — so a typed
/// `x86-interrupt` handler can do this without a naked entry: mark the task
/// dead, then either resume the next ready task's saved context or restore
/// the launcher continuation, exactly as `sys_exit` does.
///
/// Must be called from an exception handler taken from ring 3 (IF is 0 —
/// exception gates — which the SCHED lock discipline requires). Kernel-context
/// faults must keep panicking: a kernel bug is not a schedulable event.
pub fn kill_current(fault_vector: u8) -> ! {
    // Scrub EFLAGS.AC before doing real work: an exception gate, like every
    // gate, clears IF but NOT AC, and the faulting program may have held AC
    // set (the M8 lesson). The typed handlers between the gate and here touch
    // no user memory, so scrubbing at the top of the kill path is early
    // enough.
    // SAFETY: pushfq/and/popfq only clears a flag; IF is already 0 from the
    // gate, so popfq cannot re-enable interrupts.
    unsafe {
        core::arch::asm!("pushfq", "and dword ptr [rsp], 0xFFFBFFFF", "popfq");
    }
    let mut sched = SCHED.lock();
    assert!(
        sched.active,
        "a ring-3 fault arrived while no scheduler run was active"
    );
    let cur = sched.current;
    sched.tasks[cur].ready = false;
    sched.tasks[cur].exit_code = 0;
    sched.tasks[cur].fault = Some(fault_vector);
    sched.tasks[cur].exit_seq = sched.exit_counter;
    sched.exit_counter += 1;
    match sched.next_ready(cur) {
        Some(next) => {
            sched.fault_kill_resumes += 1;
            sched.current = next;
            crate::gdt::set_privilege_stack(VirtAddr::new(sched.tasks[next].kstack_top()));
            load_cr3(sched.tasks[next].cr3);
            let rsp = sched.tasks[next].saved_rsp;
            drop(sched); // release before leaving this context forever
            // SAFETY: `rsp` is the next task's saved context (the one layout
            // every save/restore path shares); RSP0 and CR3 already point at
            // that task's world.
            unsafe { resume_context(rsp) }
        }
        None => {
            sched.active = false;
            crate::gdt::set_privilege_stack(crate::gdt::default_privilege_stack_top());
            load_cr3(crate::memory::kernel_cr3());
            drop(sched);
            // SAFETY: the continuation was saved by `enter_tasks` before any
            // ring-3 instruction could execute, and the kernel's own RSP0 and
            // CR3 are restored above.
            unsafe { return_to_launcher() }
        }
    }
}

/// Abandons the current (dying) context and resumes a saved one: the shared
/// 15-pop + `iretq` restore tail, callable from Rust for the kill path.
///
/// # Safety
/// `rsp` must be a saved context in the canonical layout, with RSP0 and CR3
/// already pointing at its task.
#[unsafe(naked)]
unsafe extern "C" fn resume_context(rsp: u64) -> ! {
    core::arch::naked_asm!(
        "mov rsp, rdi",
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
    )
}

/// Abandons the current (dying) context and returns into `run_programs`: the
/// launcher-continuation restore, callable from Rust for the kill path when
/// the faulting task was the last one alive.
///
/// # Safety
/// `KERNEL_CONTINUATION_RSP` must hold the continuation `enter_tasks` saved,
/// and the kernel's default RSP0 and own CR3 must already be restored.
#[unsafe(naked)]
unsafe extern "C" fn return_to_launcher() -> ! {
    core::arch::naked_asm!(
        "mov rsp, [rip + {cont}]",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "ret",
        cont = sym crate::usermode::KERNEL_CONTINUATION_RSP,
    )
}
