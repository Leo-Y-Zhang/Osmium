//! The in-kernel self-test battery — the boot proof CI asserts on. Each check
//! prints one `[selftest]` line to serial; any failure panics, and the panic
//! handler exits QEMU with the failure code.

use crate::serial_println;

pub fn run() -> ! {
    serial_println!("[selftest] boot: reached kernel_main ... ok");
    console_renders_and_advances();
    breakpoint_handled_and_returns();
    timer_ticks_advance();
    heap_allocations_work();
    freed_heap_memory_is_zeroed();
    fresh_page_is_mapped_zeroed_writable();
    async_task_with_waker_runs();
    report_memory_stats();
    serial_println!("SELFTEST PASSED");
    crate::qemu::exit(crate::qemu::ExitCode::Success)
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
        core::ptr::write_bytes(p1, 0xA5, layout.size());
        dealloc(p1, layout);
        let p2 = alloc(layout);
        assert!(!p2.is_null(), "reallocation failed");
        // First-fit with hole merging reuses the same block; if the
        // allocator ever changes strategy this fails loudly and the test
        // gets reworked rather than silently proving nothing.
        assert_eq!(p1, p2, "allocator did not reuse the freed block");
        let stale = (0..layout.size()).filter(|&i| *p2.add(i) == 0xA5).count();
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
