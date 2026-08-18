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
    address_spaces_isolate_same_va();
    faulting_task_is_contained();
    frames_are_reclaimed_and_reused();
    freed_frame_is_scrubbed();
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

/// Re-audits the KERNEL's page table AFTER every ring-3 scenario has run.
/// The M9 claim is total: user mappings only ever exist in per-task address
/// spaces, so the kernel's own table must not carry a single user-accessible
/// bit — leaf or intermediate — no matter how many programs just ran.
fn no_stray_user_mappings_after_ring3() {
    assert!(
        crate::memory::no_stray_user_mappings(),
        "a user-accessible entry reached the kernel table during the ring-3 scenarios"
    );
    serial_println!(
        "[selftest] privacy: kernel table carried no user-accessible entry through every \
         ring-3 run ... ok"
    );
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

/// Exercises the loader's flag plumbing on a probe page in a scratch address
/// space (M9: user mappings never touch the kernel table): tightening to
/// read-only must keep NX; granting execute must clear it; nothing else may
/// change. (The wholesale-rewrite mutation of `update_user_page` fails
/// exactly here.) The kernel-table audit afterwards proves the whole exercise
/// left the kernel's own table untouched.
fn update_user_page_enforces_wx() {
    use x86_64::structures::paging::PageTableFlags;
    let addr = crate::usermode::USER_STACK_ADDR;
    let mut space = crate::memory::AddressSpace::new_user();
    space.map_user_page(addr, true, false);
    space.update_user_page(addr, false, false);
    let flags = space
        .translate_flags(addr)
        .expect("probe page is not mapped");
    assert!(
        !flags.contains(PageTableFlags::WRITABLE),
        "update_user_page left the page writable"
    );
    assert!(
        flags.contains(PageTableFlags::NO_EXECUTE),
        "update_user_page dropped NO_EXECUTE while tightening to read-only"
    );
    space.update_user_page(addr, false, true);
    let flags = space
        .translate_flags(addr)
        .expect("probe page is not mapped");
    assert!(
        !flags.contains(PageTableFlags::NO_EXECUTE),
        "update_user_page failed to clear NX for an executable page"
    );
    // The scratch space (abandoned here) held the only user mapping; the
    // kernel's table must never have seen it.
    assert!(
        crate::memory::no_stray_user_mappings(),
        "a scratch address space's user mapping reached the kernel table"
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
    // segment W^X mapping into its own address space, a volatile write into
    // its own data segment (which faults if .data is mapped read-only), and
    // syscalls. It exits with its CS in the low byte — whose low two bits are
    // the CPL, the discriminating assertion (launching in ring 0 instead
    // makes it fail, which the M6 mutation confirmed) — and the data value it
    // read in the next byte, which must be the pristine 'E'.
    let exit = crate::usermode::run_hello().expect("the embedded hello ELF was refused");
    let cs = exit & 0xff;
    assert_eq!(
        cs & 3,
        3,
        "user program ran at CPL {} (expected ring 3)",
        cs & 3
    );
    assert_eq!(
        (exit >> 8) & 0xff,
        u64::from(b'E'),
        "hello read a non-pristine data segment on a fresh run"
    );
    serial_println!(
        "[selftest] usermode: hello ELF ran in ring 3 (CS={cs:#x}), returned via syscall ... ok"
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
/// - **The switch floor proves rotation kept happening** — with both tasks
///   compute-bound, nearly every ring-3 tick while both live is a switch to
///   the other task; under TCG that is dozens, and 4 is the floor. The
///   rotate-once and task-0-only mutations both count exactly 1 and fail it
///   (observed). Exit ORDER is deliberately not asserted: task 0's head
///   start is only its partial first quantum (the tick phase is random) and
///   per-task costs are not identical (a progress dot that lands on a row
///   end pays a ~1 ms console scroll), so the order is a coin flip — an
///   earlier version asserted it and flaked at ~50%.
/// - **Both checksums exact** extends the register-integrity proof to a task
///   that is descheduled AND rescheduled dozens of times mid-computation
///   (hello's single quantum never exercised a resume-after-preemption), and
///   also proves both tasks genuinely COMPLETED under rotation.
fn round_robin_is_sustained() {
    let report = crate::usermode::run_two_counters()
        .expect("an embedded counter ELF was refused for the two-counter run");
    let first = &report.exits[0];
    let second = &report.exits[1];
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

/// The M9 isolation proof: two instances of the SAME image, at the SAME
/// virtual addresses, run in one schedule — legal only because each lives in
/// its own address space. Three properties, one run:
///
/// 1. **Same-VA coexistence.** Before M9 this exact call was the
///    overlap-refusal case; now both instances must run to completion at
///    CPL 3.
/// 2. **Write isolation, witnessed.** Each hello reads its `GREETING` static
///    (initially `'E'`), then writes back `'F'`, and reports the value it
///    READ in its exit code. If the two instances shared pages in any way —
///    aliased frames, a shared low slot, a CR3 switch that never happened —
///    the second-scheduled instance would read the first's `'F'`. Both must
///    report the pristine `'E'`. (Mutation observed: deleting the scheduler's
///    CR3 loads makes the second instance run on the first's tables and
///    report 0x46.)
/// 3. **The kernel table stays clean throughout** — the audit that follows
///    this test asserts the kernel's own page table never carried a single
///    user-accessible bit, which is only possible because everything above
///    happened in per-task tables.
fn address_spaces_isolate_same_va() {
    // Three rounds, because the witness is probabilistic in one direction
    // (adversarial-review finding): under a SHARING regression, a timer tick
    // landing in the sub-microsecond window between instance 0's volatile
    // read and write would let both instances read pristine 'E' and hide the
    // bug — roughly a 1e-4 chance per round, so three independent rounds put
    // a missed detection below 1e-12. For correct, isolated code the reads
    // are deterministic, so the repetition adds no flake risk.
    for round in 0..3 {
        let report = crate::usermode::run_hello_twice()
            .expect("two same-VA hello instances were refused; per-task address spaces are broken");
        for (i, exit) in report.exits.iter().enumerate() {
            assert_eq!(
                exit.code & 3,
                3,
                "same-VA hello instance {i} (round {round}) ran at CPL {} (expected ring 3)",
                exit.code & 3
            );
            assert_eq!(
                (exit.code >> 8) & 0xff,
                u64::from(b'E'),
                "same-VA hello instance {i} (round {round}) read {:#x} from its data segment, \
                 not the pristine 'E': the instances share memory",
                (exit.code >> 8) & 0xff
            );
        }
    }
    serial_println!(
        "[selftest] isolation: two programs at the SAME virtual addresses ran in private \
         address spaces, each seeing pristine data (3 rounds) ... ok"
    );

    // The scheduler must have left us back in the kernel's own address space
    // — the terminal CR3 restore in sys_exit is load-bearing, and without
    // this read-back nothing in the battery could ever see it deleted
    // (adversarial-review finding: the audit reads the kernel table through
    // the init-time mapper regardless of the live CR3).
    use x86_64::registers::control::Cr3;
    assert_eq!(
        Cr3::read().0,
        crate::memory::kernel_cr3(),
        "the scheduler did not restore the kernel's CR3 after the last task exited"
    );
    serial_println!("[selftest] isolation: kernel CR3 restored after every run ... ok");
}

/// The M10 fault-isolation proof: `crasher` (announces itself, then
/// dereferences an unmapped address at CPL 3) runs beside `hello`. Before
/// M10 that page fault panicked the whole kernel — this test existing at all
/// is the milestone. Assertions, in order of what they prove:
///
/// 1. **The offender was terminated, by the right cause.** Its exit record
///    carries fault vector 14 (#PF), not a voluntary exit — the unreachable
///    `SYS_EXIT(0xBAD)` in crasher makes a non-faulting run loud.
/// 2. **The neighbour was untouched**: hello completes at CPL 3 with its
///    pristine data segment, exactly as in a fault-free run.
/// 3. **The machine survived**: this function continuing to execute is the
///    real assertion, and the CR3 read-back plus the kernel-table audit that
///    follows in the battery close the run out like any other.
///
/// (Mutations observed: removing the handlers' ring-3 branch panics the
/// kernel here; mis-reporting the vector fails assertion 1; omitting the
/// kill path's CR3 switch marks the neighbour faulted; removing the
/// `fault_kill_resumes` increment fails the resume-coverage floor.)
///
/// Which branch of the kill path the pair run takes depends on tick phase: a
/// 100 Hz tick landing in crasher's short announce-to-fault window lets hello
/// finish first, routing the kill through the launcher return instead of
/// `resume_context`. The isolation assertions hold either way, but the
/// milestone's headline path — kill the offender and RESUME the neighbour
/// from inside a fault handler — would then go silently unexercised. So the
/// run repeats (up to 5×) until the report's `fault_kill_resumes` counter
/// proves the resume branch ran: correct code reaches it on the first try in
/// ≫99% of runs, so five misses in a row (~1e-10) means the branch is gone,
/// not unlucky — the M9 repetition-not-hope precedent.
fn faulting_task_is_contained() {
    use x86_64::registers::control::Cr3;
    let mut resume_proven = false;
    for _attempt in 0..5 {
        let report = crate::usermode::run_crasher_and_hello()
            .expect("an embedded ELF was refused for the fault-isolation run");
        let crasher = &report.exits[0];
        let hello = &report.exits[1];
        assert_eq!(
            crasher.fault,
            Some(14),
            "crasher was not terminated by a page fault (fault={:?}, code={:#x})",
            crasher.fault,
            crasher.code
        );
        assert_eq!(
            hello.fault, None,
            "the innocent neighbour was marked faulted"
        );
        assert_eq!(
            hello.code & 3,
            3,
            "the surviving hello ran at CPL {} (expected ring 3)",
            hello.code & 3
        );
        assert_eq!(
            (hello.code >> 8) & 0xff,
            u64::from(b'E'),
            "the surviving hello read a non-pristine data segment"
        );
        assert_eq!(
            Cr3::read().0,
            crate::memory::kernel_cr3(),
            "the kernel's CR3 was not restored after a run ending in a fault kill"
        );
        if report.fault_kill_resumes >= 1 {
            resume_proven = true;
            break;
        }
    }
    assert!(
        resume_proven,
        "five pair runs and the fault kill never resumed the survivor — the \
         kill path's resume_context branch is unreachable or uncounted"
    );
    serial_println!(
        "[selftest] isolation: a page-faulting task was terminated alone — its neighbour \
         and the kernel survived ... ok"
    );

    // The kill path's other branch, deterministically: the faulting task is
    // the LAST alive, so the fault handler must restore the kernel's own
    // RSP0 and CR3 and return into the launcher continuation. In the pair
    // run above this branch is only reached by tick-phase luck.
    let report = crate::usermode::run_crasher_alone()
        .expect("the crasher ELF was refused for the solo fault run");
    assert_eq!(
        report.exits[0].fault,
        Some(14),
        "the solo crasher was not terminated by a page fault"
    );
    assert_eq!(
        Cr3::read().0,
        crate::memory::kernel_cr3(),
        "the kernel's CR3 was not restored after a fault kill of the last task"
    );
    serial_println!(
        "[selftest] isolation: a fault in the LAST task returns cleanly to the kernel ... ok"
    );
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
    let (_, frames_reclaimed) = crate::memory::frame_gross_stats();
    serial_println!(
        "[selftest] mem: heap {heap_used} B used / {heap_free} B free, {frames_used}/{frames_total} frames in use ({frames_reclaimed} reclaimed)"
    );
}

/// The M11 reclamation proof, in two halves.
///
/// **Exactness**: the in-use frame count must land exactly back on its
/// pre-run baseline after every ring-3 run — one frame short means the
/// recording missed an allocation, one frame over means something freed
/// what it did not own. The crasher+hello pair is the run under test
/// because its teardown covers the fault-kill path too.
///
/// **Reuse**: after a warm-up run has stocked the free list, further runs
/// must be served ENTIRELY from it — the gross bump-cursor count must not
/// move. Without this half, an allocator that freed but never reused would
/// pass the baseline check while still marching towards RAM exhaustion
/// (in-use = gross minus freed stays flat even when gross grows).
///
/// (Mutations observed: Drop not deallocating fails the baseline half;
/// allocate ignoring the free list fails the reuse half.)
fn frames_are_reclaimed_and_reused() {
    let (baseline, _) = crate::memory::frame_stats();
    crate::usermode::run_crasher_and_hello().expect("an ELF was refused for the reclaim run");
    let (after_warm, _) = crate::memory::frame_stats();
    assert_eq!(
        after_warm, baseline,
        "a ring-3 run leaked frames: {baseline} in use before, {after_warm} after"
    );
    let (gross_warm, _) = crate::memory::frame_gross_stats();
    for _ in 0..2 {
        crate::usermode::run_crasher_and_hello().expect("an ELF was refused for the reuse run");
        let (in_use, _) = crate::memory::frame_stats();
        assert_eq!(in_use, baseline, "a warm ring-3 run leaked frames");
    }
    let (gross_after, _) = crate::memory::frame_gross_stats();
    assert_eq!(
        gross_after, gross_warm,
        "warm runs allocated fresh frames instead of reusing the free list \
         ({gross_warm} gross before, {gross_after} after)"
    );
    serial_println!(
        "[selftest] mem: every frame of a ring-3 run reclaimed, warm runs served \
         entirely from the free list ({baseline} in use) ... ok"
    );
}

/// The M11 privacy half: a freed frame's contents must be gone the moment
/// it is freed — not at some later reuse. The probe soils a frame through
/// the physical alias, frees it, and re-reads the SAME alias before any
/// reallocation. (Mutation observed: with the free-time scrub removed the
/// sentinel survives into the free list and this fails.)
fn freed_frame_is_scrubbed() {
    assert!(
        crate::memory::scrub_on_free_probe(),
        "a freed frame kept its bytes — the free-time scrub is gone"
    );
    serial_println!("[selftest] mem: a freed frame is scrubbed at free time ... ok");
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
