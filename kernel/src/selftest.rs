//! The in-kernel self-test battery — the boot proof CI asserts on. Each check
//! prints one `[selftest]` line to serial; any failure panics, and the panic
//! handler exits QEMU with the failure code.

use crate::serial_println;

pub fn run() -> ! {
    serial_println!("[selftest] boot: reached kernel_main ... ok");
    console_renders_and_advances();
    console_pixels_reach_the_framebuffer();
    clear_is_correct_and_measured();
    breakpoint_handled_and_returns();
    idt_has_all_installed_vectors();
    timer_ticks_advance();
    heap_allocations_work();
    freed_heap_memory_is_zeroed();
    fresh_page_is_mapped_zeroed_writable();
    kernel_mappings_are_supervisor_only();
    supervisor_hardening_is_active();
    heap_is_not_executable();
    heap_refuses_oversized_allocation();
    async_task_with_waker_runs();
    shell_processes_a_scripted_session();
    elf_loader_refuses_wx();
    user_program_runs_in_ring3();
    preemptive_scheduling_is_real();
    round_robin_is_sustained();
    concurrent_overlap_is_refused();
    no_stray_user_mappings_after_ring3();
    update_user_page_enforces_wx();
    measure_console_scroll();
    report_memory_stats();
    // Must run LAST: the only acceptable exit from here is through the
    // double-fault handler, which prints the battery's final verdict.
    stack_overflow_is_survivable()
}

fn stack_overflow_is_survivable() -> ! {
    use core::sync::atomic::Ordering;
    serial_println!(
        "[selftest] resilience: overflowing the kernel stack, double fault expected ..."
    );
    crate::interrupts::EXPECTING_DOUBLE_FAULT.store(true, Ordering::SeqCst);

    #[allow(unconditional_recursion)]
    fn overflow() {
        overflow();
        // The volatile read stops the recursion being tail-call optimised
        // into a loop that never grows the stack.
        // SAFETY: reading a promoted constant.
        unsafe { core::ptr::read_volatile(&0u8) };
    }
    overflow();
    panic!("the stack overflow did not double fault; the IST guard is not working")
}

fn heap_allocations_work() {
    use alloc::boxed::Box;
    use alloc::vec::Vec;
    let boxed = Box::new(0xdead_beef_u64);
    assert_eq!(*boxed, 0xdead_beef);
    let mut vec: Vec<u64> = Vec::with_capacity(50_000);
    vec.extend(0..50_000);
    let sum: u64 = vec.iter().sum();
    assert_eq!(sum, 50_000 * 49_999 / 2);
    serial_println!("[selftest] heap: Box + 50k-element Vec ... ok");
}

fn freed_heap_memory_is_zeroed() {
    use alloc::alloc::{alloc, dealloc};
    use core::alloc::Layout;
    let layout = Layout::from_size_align(256, 8).unwrap();
    // SAFETY: matched alloc/dealloc pairs with a valid non-zero-size layout;
    // p1 is only read through p2 after the allocator hands the block back.
    unsafe {
        let p1 = alloc(layout);
        assert!(!p1.is_null(), "allocation failed");
        // Volatile: a non-volatile fill of a block that is freed on the next
        // line is a dead store, and LLVM deletes it (observed) — the
        // sentinel must actually reach RAM for this test to mean anything.
        for i in 0..layout.size() {
            p1.add(i).write_volatile(0xA5);
        }
        dealloc(p1, layout);
        let p2 = alloc(layout);
        assert!(!p2.is_null(), "reallocation failed");
        // First-fit with hole merging reuses the same block; if the
        // allocator ever changes strategy this fails loudly and the test
        // gets reworked rather than silently proving nothing.
        assert_eq!(p1, p2, "allocator did not reuse the freed block");
        // read_volatile is load-bearing: a plain read of freshly-allocated
        // memory is undef to LLVM, which constant-folds the comparison away
        // and turns this test into decoration (observed doing exactly that).
        //
        // The scan starts past the first 16 bytes: linked_list_allocator
        // writes its free-list node (Hole { size, next }) into the head of a
        // freed block AFTER our scrub — the scrub must come first or it would
        // corrupt the free list — and a metadata byte can legitimately equal
        // the sentinel (an aligned hole pointer's middle byte, say), which
        // would turn this privacy test red with no privacy bug. Bytes 16..
        // are caller data and still prove the scrub: deleting it leaves 240
        // sentinel bytes here.
        const HOLE_META: usize = 16;
        let stale = (HOLE_META..layout.size())
            .filter(|&i| p2.add(i).read_volatile() == 0xA5)
            .count();
        assert_eq!(stale, 0, "freed memory still holds {stale} sentinel bytes");
        dealloc(p2, layout);
    }
    serial_println!("[selftest] privacy: freed heap memory is zeroed ... ok");
}

fn fresh_page_is_mapped_zeroed_writable() {
    let addr = crate::memory::map_probe_page();
    // SAFETY: map_probe_page just mapped this page PRESENT|WRITABLE.
    unsafe {
        let bytes = core::slice::from_raw_parts(addr.as_ptr::<u8>(), 4096);
        assert!(
            bytes.iter().all(|&b| b == 0),
            "freshly mapped page contains stale bytes"
        );
        addr.as_mut_ptr::<u64>()
            .write_volatile(0x1234_5678_9abc_def0);
        assert_eq!(addr.as_ptr::<u64>().read_volatile(), 0x1234_5678_9abc_def0);
    }
    serial_println!("[selftest] paging: fresh page mapped, zeroed, writable ... ok");
}

fn async_task_with_waker_runs() {
    use core::future::Future;
    use core::pin::Pin;
    use core::sync::atomic::{AtomicBool, Ordering};
    use core::task::{Context, Poll};

    /// Returns Pending once, waking itself — so completing exercises the
    /// waker → ready-queue → re-poll path, not just a straight-through poll.
    struct YieldOnce(bool);
    impl Future for YieldOnce {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    static RAN: AtomicBool = AtomicBool::new(false);
    let mut executor = crate::task::executor::Executor::new();
    executor.spawn(crate::task::Task::new(async {
        YieldOnce(false).await;
        RAN.store(true, Ordering::SeqCst);
    }));
    executor.run_ready_tasks();
    assert!(
        RAN.load(Ordering::SeqCst),
        "spawned async task did not run to completion via its waker"
    );
    serial_println!("[selftest] executor: task yielded, woke itself, completed ... ok");
}

fn kernel_mappings_are_supervisor_only() {
    assert!(
        crate::memory::no_stray_user_mappings(),
        "a stray user-accessible mapping exists before ring 3 has ever run"
    );
    serial_println!("[selftest] privacy: no mapping is user-accessible ... ok");
}

/// Re-audits the page tables AFTER the ring-3 round trip: the teardown must
/// leave no user-accessible leaf anywhere, and user-accessible intermediates
/// only where they reach the declared user window. Without this second look,
/// battery ordering alone hid whatever `run_elf` left behind.
fn no_stray_user_mappings_after_ring3() {
    assert!(
        crate::memory::no_stray_user_mappings(),
        "a user-accessible mapping survived the ring-3 teardown"
    );
    serial_println!("[selftest] privacy: ring-3 teardown left no user-accessible leaf ... ok");
}

/// Every supervisor-hardening bit the CPU advertises must actually be set in
/// CR4 — read back from the register, not from what we asked for. Under the
/// `-cpu max` model CI boots, all three (SMEP, SMAP, UMIP) are advertised, so
/// all three must be live. Removing the `Cr4::update` in `cpu::init` fails
/// here. (The behavioural negative probe — a ring-0 read of a user page
/// faulting under SMAP — is deferred: it needs page-fault-recovery plumbing
/// that redirects a faulting RIP, and the positive controls below already
/// prove SMAP is active without that risk. The ELF loader copies through the
/// physical alias precisely because SMAP is on, and `hello` runs to
/// completion under it — a broken SMAP configuration would fault that copy.)
fn supervisor_hardening_is_active() {
    use x86_64::registers::control::{Cr4, Cr4Flags};
    let cr4 = Cr4::read();
    // CPUID leaf 7 is the source of truth for what the CPU supports — but only
    // if the CPU has a leaf 7 (see cpu::supported). This battery runs under
    // `-cpu max`, where it does; the guard just refuses to read garbage.
    let max_leaf = core::arch::x86_64::__cpuid(0).eax;
    let leaf7 = core::arch::x86_64::__cpuid_count(7, 0);
    let advertised = |bit_set: bool| max_leaf >= 7 && bit_set;
    let checks = [
        (
            advertised(leaf7.ebx & (1 << 7) != 0),
            Cr4Flags::SUPERVISOR_MODE_EXECUTION_PROTECTION,
            "SMEP",
        ),
        (
            advertised(leaf7.ebx & (1 << 20) != 0),
            Cr4Flags::SUPERVISOR_MODE_ACCESS_PREVENTION,
            "SMAP",
        ),
        (
            advertised(leaf7.ecx & (1 << 2) != 0),
            Cr4Flags::USER_MODE_INSTRUCTION_PREVENTION,
            "UMIP",
        ),
    ];
    for (supported, flag, name) in checks {
        if supported {
            assert!(cr4.contains(flag), "{name} is supported but not set in CR4");
        }
    }
    // Under `-cpu max` all three are advertised; if any is missing here the
    // boot CPU model regressed and the hardening claim is not being tested.
    assert!(
        crate::cpu::smep_enabled() && crate::cpu::smap_enabled() && crate::cpu::umip_enabled(),
        "expected SMEP+SMAP+UMIP under the test CPU model, got {}",
        crate::cpu::summary()
    );
    serial_println!(
        "[selftest] security: supervisor hardening active ({}) ... ok",
        crate::cpu::summary()
    );
}

fn heap_is_not_executable() {
    use x86_64::structures::paging::PageTableFlags;
    let flags = crate::memory::translate_flags(crate::memory::heap::HEAP_START)
        .expect("heap start is not mapped");
    assert!(
        flags.contains(PageTableFlags::NO_EXECUTE),
        "the kernel heap is executable (W^X violation)"
    );
    serial_println!("[selftest] security: kernel heap is non-executable ... ok");
}

/// Exercises the loader's flag plumbing on a probe page: tightening to
/// read-only must keep NX; granting execute must clear it; nothing else may
/// change. (The wholesale-rewrite mutation of `update_user_page` fails
/// exactly here.)
fn update_user_page_enforces_wx() {
    use x86_64::structures::paging::PageTableFlags;
    let addr = crate::usermode::USER_STACK_ADDR;
    crate::memory::map_user_page(addr, true, false);
    crate::memory::update_user_page(addr, false, false);
    let flags = crate::memory::translate_flags(addr).expect("probe page is not mapped");
    assert!(
        !flags.contains(PageTableFlags::WRITABLE),
        "update_user_page left the page writable"
    );
    assert!(
        flags.contains(PageTableFlags::NO_EXECUTE),
        "update_user_page dropped NO_EXECUTE while tightening to read-only"
    );
    crate::memory::update_user_page(addr, false, true);
    let flags = crate::memory::translate_flags(addr).expect("probe page is not mapped");
    assert!(
        !flags.contains(PageTableFlags::NO_EXECUTE),
        "update_user_page failed to clear NX for an executable page"
    );
    crate::memory::unmap_user_page(addr);
    // This probe mapped a user page of its own; audit its teardown the same
    // way the ring-3 run's teardown is audited.
    assert!(
        crate::memory::no_stray_user_mappings(),
        "the W^X probe's own teardown left a user-accessible leaf"
    );
    serial_println!("[selftest] security: user page flags narrow correctly (W^X plumbing) ... ok");
}

fn heap_refuses_oversized_allocation() {
    use alloc::alloc::{alloc, dealloc};
    use core::alloc::Layout;
    // black_box on the layout is load-bearing: without it LLVM propagates the
    // allocator shim's returns-non-null assumption and folds the check away
    // (same failure class as the zero-on-free sentinel — observed here too).
    let huge = core::hint::black_box(
        Layout::from_size_align(crate::memory::heap::HEAP_SIZE as usize * 2, 8).unwrap(),
    );
    // SAFETY: a valid, non-zero layout; a null return is the documented refusal.
    let refused = core::hint::black_box(unsafe { alloc(huge) });
    if !refused.is_null() {
        // SAFETY: undo it so the battery can continue (should be unreachable).
        unsafe { dealloc(refused, huge) };
    }
    assert!(
        refused.is_null(),
        "oversized allocation unexpectedly succeeded"
    );
    // The allocator is still usable and zero-on-free still holds afterwards.
    let ok = Layout::from_size_align(64, 8).unwrap();
    // SAFETY: matched alloc/dealloc of a valid layout; the block is only
    // touched while owned.
    unsafe {
        let p = alloc(ok);
        assert!(
            !p.is_null(),
            "heap did not recover after refusing an oversized request"
        );
        p.write_volatile(0x5a);
        dealloc(p, ok);
    }
    serial_println!("[selftest] heap: oversized request refused, allocator intact ... ok");
}

fn shell_processes_a_scripted_session() {
    if !crate::console::is_initialised() {
        serial_println!("[selftest] shell: skipped, no framebuffer on this boot");
        return;
    }
    use pc_keyboard::ScancodeSet1;
    let mut shell = crate::shell::Shell::new();
    let mut scancodes = ScancodeSet1::new();
    // Clear first so the row count is a deterministic baseline, not whatever
    // the boot log left on screen (which could be near the bottom and scroll,
    // hiding the delta). `help` prints its command list plus the keys block —
    // well over ten rows — so a broken `help` arm (which would fall through to
    // the two-row "unknown command" path) fails the delta below. That is the
    // discrimination the old `after > before` lacked: an unknown command also
    // advances the cursor, so the old check passed with `help` broken.
    crate::console::with_console(|c| c.clear_screen());
    let before = crate::console::with_console(|c| c.cursor().0).unwrap_or(0);
    // Scancode-set-1 make codes for "hel" + 'x' + Backspace + 'p' + Enter,
    // i.e. a typed-and-corrected `help`. This drives the real input path:
    // decode -> editor insert/erase -> submit -> execute -> console output.
    // The typed text is asserted structurally and never echoed to serial
    // (privacy rule): only the pass line below reaches the log.
    for &code in &[0x23u8, 0x12, 0x26, 0x2d, 0x0e, 0x19, 0x1c] {
        shell.feed_scancode(&mut scancodes, code);
    }
    let after = crate::console::with_console(|c| c.cursor().0).unwrap_or(0);
    assert!(
        after.saturating_sub(before) >= 10,
        "the scripted 'help' advanced only {} rows; the help output is missing",
        after.saturating_sub(before)
    );
    serial_println!("[selftest] shell: scripted 'help' renders its full output ... ok");

    // Ctrl-A wiring: type "elp", Ctrl-A to the line start, insert 'h' -> "help".
    // If Ctrl-A does not move home, 'h' lands at the end giving "elph", an
    // unknown command whose two-row output fails the delta.
    let mut shell = crate::shell::Shell::new();
    let mut scancodes = ScancodeSet1::new();
    crate::console::with_console(|c| c.clear_screen());
    let before = crate::console::with_console(|c| c.cursor().0).unwrap_or(0);
    // "elp", then Ctrl (0x1D down) + 'a' (0x1E) + releases, then 'h', Enter.
    for &code in &[
        0x12u8, 0x26, 0x19, // e l p
        0x1d, 0x1e, 0x9e, 0x9d, // Ctrl-A (ctrl down, a, a up, ctrl up)
        0x23, // h
        0x1c, // Enter
    ] {
        shell.feed_scancode(&mut scancodes, code);
    }
    let after = crate::console::with_console(|c| c.cursor().0).unwrap_or(0);
    assert!(
        after.saturating_sub(before) >= 10,
        "Ctrl-A did not move to the line start: the line was not 'help' ({} rows)",
        after.saturating_sub(before)
    );
    serial_println!("[selftest] shell: Ctrl-A moves to the line start ... ok");

    // Tab completion: "he" + Tab uniquely completes to "help", then Enter runs
    // it. If completion is dead, "he" stays and is an unknown two-row command.
    let mut shell = crate::shell::Shell::new();
    let mut scancodes = ScancodeSet1::new();
    crate::console::with_console(|c| c.clear_screen());
    let before = crate::console::with_console(|c| c.cursor().0).unwrap_or(0);
    for &code in &[0x23u8, 0x12, 0x0f, 0x1c] {
        // h e Tab Enter
        shell.feed_scancode(&mut scancodes, code);
    }
    let after = crate::console::with_console(|c| c.cursor().0).unwrap_or(0);
    assert!(
        after.saturating_sub(before) >= 10,
        "Tab did not complete 'he' to 'help' ({} rows)",
        after.saturating_sub(before)
    );
    serial_println!("[selftest] shell: Tab completes a command verb ... ok");
}

fn user_program_runs_in_ring3() {
    // The embedded `hello` ELF proves the whole loader path: parse, per-
    // segment W^X mapping, a volatile write into its own data segment (which
    // faults if .data is mapped read-only), and syscalls. It exits with its
    // own CS, whose low two bits are the CPL — the discriminating assertion
    // (launching in ring 0 instead makes it fail, which the M6 mutation
    // confirmed).
    let exit = crate::usermode::run_hello().expect("the embedded hello ELF was refused");
    assert_eq!(
        exit & 3,
        3,
        "user program ran at CPL {} (expected ring 3)",
        exit & 3
    );
    serial_println!(
        "[selftest] usermode: hello ELF ran in ring 3 (CS={exit:#x}), returned via syscall ... ok"
    );

    // hello set EFLAGS.AC before its SYS_WRITE; the kernel must have scrubbed
    // it at the int 0x80 gate, or SMAP was inert for the whole kernel entry.
    use core::sync::atomic::Ordering;
    assert!(
        !crate::usermode::SYSCALL_ENTRY_AC.load(Ordering::SeqCst),
        "EFLAGS.AC survived the syscall gate: ring 3 can turn SMAP off"
    );
    serial_println!("[selftest] security: syscall gate scrubs EFLAGS.AC (SMAP stays on) ... ok");
}

/// The M8 preemption proof, in one run: `counter` — a ~30M-iteration compute
/// loop that NEVER yields (a syscall is not a yield; only the timer moves the
/// CPU) — is launched first, `hello` second. Four independent properties:
///
/// 1. **Preemption is real.** `hello` exits FIRST even though `counter` was
///    launched first and never gave up the CPU. Under cooperative scheduling
///    the exit order is the launch order; only a timer-driven switch can
///    invert it. (The named mutation — gating the timer's switch path off —
///    was observed producing exactly the cooperative order.)
/// 2. **Context switching preserves the register file exactly.** `counter`
///    keeps eight accumulators live across every switch and exits with their
///    fold; the kernel recomputes the same fold independently and demands
///    equality. Tens of save/restore round trips with one corrupted register
///    anywhere produce a different checksum. (Mutation: clobbering one
///    callee-saved register in the restore path was observed failing here.)
/// 3. **Both programs really ran at CPL 3** — hello exits with its CS.
/// 4. **The timer entry scrubs EFLAGS.AC.** `counter` holds AC set for its
///    entire run, so every timer interrupt taken from it enters the kernel
///    with AC at its most hostile; SMAP is only real if the entry scrubs it
///    before any Rust runs. (Mutation: removing the scrub was observed
///    setting `TIMER_ENTRY_AC`.)
fn preemptive_scheduling_is_real() {
    use core::sync::atomic::Ordering;
    let report = crate::usermode::run_counter_and_hello()
        .expect("an embedded ELF was refused for the concurrent run");
    let counter = &report.exits[0];
    let hello = &report.exits[1];
    assert_eq!(
        (hello.seq, counter.seq),
        (0, 1),
        "hello (launched second) did not exit first: scheduling is not preemptive"
    );
    assert_eq!(
        hello.code & 3,
        3,
        "the concurrently-scheduled hello ran at CPL {} (expected ring 3)",
        hello.code & 3
    );
    assert!(
        report.preemptive_switches >= 1,
        "no timer-driven switch was counted, yet the exit order says one happened"
    );
    // Under QEMU TCG (the only environment the battery runs in) counter's
    // loop spans dozens of 10 ms ticks; ≥2 is the floor that proves the
    // save/restore machinery cycled repeatedly, not once by luck.
    assert!(
        report.ring3_round_trips >= 2,
        "only {} timer round-trips were taken from ring 3",
        report.ring3_round_trips
    );
    assert_eq!(
        counter.code,
        expected_counter_checksum(),
        "counter's checksum is wrong: a context switch corrupted its registers"
    );
    serial_println!(
        "[selftest] sched: unyielding counter preempted — hello (launched 2nd) exited 1st \
         ({} switches, {} round-trips) ... ok",
        report.preemptive_switches,
        report.ring3_round_trips
    );
    serial_println!("[selftest] sched: counter checksum exact across every context switch ... ok");
    assert!(
        !crate::sched::TIMER_ENTRY_AC.load(Ordering::SeqCst),
        "EFLAGS.AC survived the timer entry: ring 3 can turn SMAP off for async kernel entries"
    );
    serial_println!(
        "[selftest] security: timer entry scrubs EFLAGS.AC (SMAP holds under preemption) ... ok"
    );
}

/// The sustained-rotation proof an adversarial review demanded: the
/// counter+hello scenario above inherently sees exactly ONE preemptive
/// switch (hello exits at its first quantum), so a scheduler that rotates
/// once and then pins — or that only ever preempts task 0 — passed it.
/// Here the SAME counter program, linked at two disjoint bases, is scheduled
/// against itself: two unyielding tasks alive for many quanta each.
///
/// - **Exit order pins sustained rotation.** The programs do identical work
///   and task 0 runs its quantum first, so under genuine round-robin task 0
///   stays strictly ahead and exits FIRST. A rotate-once (or task-0-only)
///   scheduler parks task 1 with the CPU until it finishes, inverting the
///   order. (Mutation observed: preempt-once-then-pin flipped it.)
/// - **The switch floor proves rotation kept happening** — with both tasks
///   compute-bound, nearly every ring-3 tick while both live is a switch to
///   the other task; under TCG that is dozens, and 4 is the floor.
/// - **Both checksums exact** extends the register-integrity proof to a task
///   that is descheduled AND rescheduled dozens of times mid-computation
///   (hello's single quantum never exercised a resume-after-preemption).
fn round_robin_is_sustained() {
    let report = crate::usermode::run_two_counters()
        .expect("an embedded counter ELF was refused for the two-counter run");
    let first = &report.exits[0];
    let second = &report.exits[1];
    assert_eq!(
        (first.seq, second.seq),
        (0, 1),
        "task 0 (identical work, first quantum) did not exit first: rotation stopped"
    );
    let expected = expected_counter_checksum();
    assert_eq!(
        first.code, expected,
        "counter A's checksum is wrong across sustained preemption"
    );
    assert_eq!(
        second.code, expected,
        "counter B's checksum is wrong across sustained preemption"
    );
    assert!(
        report.preemptive_switches >= 4,
        "only {} preemptive switches across two long-lived tasks: rotation is not sustained",
        report.preemptive_switches
    );
    serial_println!(
        "[selftest] sched: round-robin sustained across two unyielding tasks \
         ({} switches, {} round-trips), both checksums exact ... ok",
        report.preemptive_switches,
        report.ring3_round_trips
    );
}

/// The kernel's independent twin of `user/counter`'s checksum loop — same
/// seeds, same mixing, same fold, same iteration count. The two must stay in
/// step or the battery fails, which is the intended failure mode for a drift:
/// loud, at the first CI boot. Kept deliberately in ring 0 Rust (not shared
/// through kshared) so the user program and the expectation cannot share a
/// single implementation that a bug could hide in.
fn expected_counter_checksum() -> u64 {
    const ITERS: u64 = 30_000_000; // must match user/counter/src/main.rs
    let mut a: u64 = 0x243F_6A88_85A3_08D3;
    let mut b: u64 = 0x1319_8A2E_0370_7344;
    let mut c: u64 = 0xA409_3822_299F_31D0;
    let mut d: u64 = 0x082E_FA98_EC4E_6C89;
    let mut e: u64 = 0x4528_21E6_38D0_1377;
    let mut f: u64 = 0xBE54_66CF_34E9_0C6C;
    let mut g: u64 = 0xC0AC_29B7_C97C_50DD;
    let mut h: u64 = 0x3F84_D5B5_B547_0917;
    let mut i: u64 = 0;
    while i < ITERS {
        a = a.rotate_left(7) ^ i;
        b = b.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(i);
        c = c.wrapping_add(a ^ b);
        d ^= c.rotate_right(11);
        e = e.wrapping_add(d.wrapping_mul(0xC2B2_AE3D_27D4_EB4F));
        f = (f ^ e).rotate_left(3);
        g = g.wrapping_add(f);
        h ^= g.wrapping_add(i);
        i += 1;
    }
    a ^ b ^ c ^ d ^ e ^ f ^ g ^ h
}

/// Two copies of the same image claim the same pages; the cross-image overlap
/// check must refuse the run BEFORE anything is mapped — the audit right
/// after proves the refusal left no trace.
fn concurrent_overlap_is_refused() {
    use kshared::elf::ElfError;
    assert!(
        matches!(crate::usermode::run_hello_twice(), Err(ElfError::Overlap)),
        "two programs at the same base were not refused"
    );
    assert!(
        crate::memory::no_stray_user_mappings(),
        "the refused overlapping run left a user-accessible mapping behind"
    );
    serial_println!("[selftest] sched: two programs claiming the same pages are refused ... ok");
}

/// Feeds the loader a crafted image whose single segment claims to be both
/// writable and executable; the parse must refuse it. Asserts on the PURE
/// `parse_elf64`, not `run_elf` — a refusal gate whose failure mode is
/// "map the hostile bytes and iretq to ring 3" is the wrong shape (that is
/// exactly what happened when the guard was mutated out during verification).
fn elf_loader_refuses_wx() {
    use kshared::elf::{ElfError, USER_IMAGE_BASE};
    let mut img = alloc::vec![0u8; 64 + 56 + 16];
    img[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
    img[4] = 2; // 64-bit
    img[5] = 1; // little-endian
    img[6] = 1; // version
    img[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
    img[18..20].copy_from_slice(&62u16.to_le_bytes()); // x86-64
    img[24..32].copy_from_slice(&USER_IMAGE_BASE.to_le_bytes()); // entry
    img[32..40].copy_from_slice(&64u64.to_le_bytes()); // phoff
    img[54..56].copy_from_slice(&56u16.to_le_bytes()); // phentsize
    img[56..58].copy_from_slice(&1u16.to_le_bytes()); // phnum
    let ph = 64;
    img[ph..ph + 4].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    img[ph + 4..ph + 8].copy_from_slice(&7u32.to_le_bytes()); // RWX
    img[ph + 8..ph + 16].copy_from_slice(&120u64.to_le_bytes()); // offset
    img[ph + 16..ph + 24].copy_from_slice(&USER_IMAGE_BASE.to_le_bytes());
    img[ph + 32..ph + 40].copy_from_slice(&16u64.to_le_bytes()); // filesz
    img[ph + 40..ph + 48].copy_from_slice(&16u64.to_le_bytes()); // memsz
    assert!(
        matches!(
            kshared::elf::parse_elf64(&img),
            Err(ElfError::WritableAndExecutable)
        ),
        "a writable-and-executable segment was not refused"
    );
    serial_println!("[selftest] security: loader refuses a W+X segment ... ok");
}

/// Measures the current scroll implementation (framebuffer `copy_within`) so
/// the cost is recorded, not guessed. Under QEMU's software renderer this is
/// dominated by emulation overhead; the number exists to decide whether the
/// write-only cell-grid rework is worth it (see the TDD's roadmap note).
fn measure_console_scroll() {
    if !crate::console::is_initialised() {
        return;
    }
    const SCROLLS: u64 = 100;
    let start = crate::time::rdtsc();
    crate::console::with_console(|con| {
        for _ in 0..SCROLLS {
            con.write_char('\n');
        }
    });
    let cycles = crate::time::rdtsc().wrapping_sub(start);
    match crate::time::cycles_to_us(cycles) {
        Some(us) => serial_println!(
            "[selftest] perf: {SCROLLS} newlines (scroll-heavy) in {cycles} cyc (~{us} us) ... ok"
        ),
        None => serial_println!("[selftest] perf: {SCROLLS} newlines in {cycles} cyc ... ok"),
    }
}

fn report_memory_stats() {
    let (heap_used, heap_free) = crate::memory::heap::stats();
    let (frames_used, frames_total) = crate::memory::frame_stats();
    serial_println!(
        "[selftest] mem: heap {heap_used} B used / {heap_free} B free, {frames_used}/{frames_total} frames handed out"
    );
}

fn breakpoint_handled_and_returns() {
    use core::sync::atomic::Ordering;
    let before = crate::interrupts::BREAKPOINT_HITS.load(Ordering::Relaxed);
    x86_64::instructions::interrupts::int3();
    let after = crate::interrupts::BREAKPOINT_HITS.load(Ordering::Relaxed);
    assert!(after > before, "breakpoint handler did not run");
    serial_println!("[selftest] interrupts: int3 handled, execution resumed ... ok");
}

fn idt_has_all_installed_vectors() {
    use x86_64::instructions::tables::sidt;
    let base = sidt().base.as_u64();
    for &vec in crate::interrupts::INSTALLED_EXCEPTION_VECTORS {
        // Each IDT gate is 16 bytes; the options half-word (present is bit 15)
        // sits at offset 4.
        // SAFETY: `base` is the loaded IDT and `vec` < 256, so this is in-bounds.
        let options =
            unsafe { core::ptr::read_volatile((base + u64::from(vec) * 16 + 4) as *const u16) };
        assert!(
            options & (1 << 15) != 0,
            "IDT exception vector {vec} is not present"
        );
    }
    serial_println!("[selftest] idt: all installed exception vectors present ... ok");
}

fn timer_ticks_advance() {
    use core::sync::atomic::Ordering;
    let start = crate::interrupts::TICKS.load(Ordering::Relaxed);
    // 3 ticks at 100 Hz is ~30 ms; the hlt bound keeps a broken PIT from
    // hanging the battery (the panic is the diagnosis, the CI timeout is not).
    for _ in 0..10_000 {
        if crate::interrupts::TICKS.load(Ordering::Relaxed) >= start + 3 {
            serial_println!("[selftest] timer: PIT ticks advancing ... ok");
            return;
        }
        x86_64::instructions::hlt();
    }
    panic!("PIT ticks did not advance; PIC/PIT wiring is broken");
}

/// The cursor test above proves the bookkeeping; this proves pixels. Without
/// it the whole rendering path is mutation-blind — deleting the buffer write
/// in `set_pixel` or swapping the Bgr channel arm passes every other check.
fn console_pixels_reach_the_framebuffer() {
    if !crate::console::is_initialised() {
        serial_println!("[selftest] console: pixel probe skipped, no framebuffer");
        return;
    }
    let ok = crate::console::with_console(|c| c.pixel_probe()).unwrap_or(false);
    assert!(
        ok,
        "a drawn glyph did not reach the framebuffer, or a colour landed in the wrong channel"
    );
    serial_println!(
        "[selftest] console: glyph pixels reach the framebuffer, correct channel ... ok"
    );
}

/// The row-template `clear` and the `mix` endpoint fast-paths: correctness
/// first (a wrong fill or a swapped `mix` early-out is a visible bug), then
/// the measured cost so the boot-path saving is archived in CI.
fn clear_is_correct_and_measured() {
    use crate::framebuffer::{BACKGROUND, FOREGROUND, Rgb};
    // mix endpoints: 0 is background, 255 is foreground; a swapped early-out
    // fails one of these. (Rgb is Eq but not Debug, so assert! not assert_eq!.)
    assert!(
        Rgb::mix(FOREGROUND, BACKGROUND, 0) == BACKGROUND,
        "mix(.., 0) must be the background colour"
    );
    assert!(
        Rgb::mix(FOREGROUND, BACKGROUND, 255) == FOREGROUND,
        "mix(.., 255) must be the foreground colour"
    );
    if !crate::console::is_initialised() {
        serial_println!("[selftest] console: clear probe skipped, no framebuffer");
        return;
    }
    let (ok, cycles) = crate::console::with_console(|c| c.clear_probe())
        .flatten()
        .expect("clear probe needs a framebuffer");
    assert!(ok, "clear left a soiled corner — a row was not filled");
    match crate::time::cycles_to_us(cycles) {
        Some(us) => {
            serial_println!("[selftest] perf: full-screen clear in {cycles} cyc (~{us} us) ... ok")
        }
        None => serial_println!("[selftest] perf: full-screen clear in {cycles} cyc ... ok"),
    }
}

fn console_renders_and_advances() {
    if !crate::console::is_initialised() {
        // Serial-only boot (no framebuffer): the console path is untestable
        // here, and that is a property of the machine, not a kernel defect.
        serial_println!("[selftest] console: skipped, no framebuffer on this boot");
        return;
    }
    let advanced = crate::console::with_console(|console| {
        use core::fmt::Write;
        let before = console.cursor();
        writeln!(console, "selftest: console glyph rendering").unwrap();
        console.cursor() != before
    });
    assert_eq!(
        advanced,
        Some(true),
        "console cursor did not advance after writing a line"
    );
    serial_println!("[selftest] console: glyphs rendered, cursor advanced ... ok");
}
