# TDD — Osmium v1 kernel

**Status:** draft — one open question blocks milestone M2; M0 and M1 are unblocked
**Date:** 2026-08-17 · **PRD:** [PRD.md](PRD.md) · **Repo:** `Leo-Y-Zhang/Osmium`

## Approach

Osmium is a three-crate Cargo workspace. `kernel/` is a `no_std` binary built for
`x86_64-unknown-none`; it owns everything that touches hardware. `kshared/` is a
`no_std` library of pure logic — address arithmetic, memory-region maths, the line
editor, command parsing — with no hardware access and no allocator requirement, so
it compiles for the host and is covered by ordinary `cargo test`. `xtask/` is a host
binary that builds the kernel, wraps it into BIOS and UEFI disk images with the
`bootloader` crate, drives QEMU, and enforces the size and RAM budgets; the
workspace's `default-members` is `xtask` so that a bare `cargo build` operates on
tooling and never tries to build the kernel for the host.

The workspace is pinned to a specific nightly in `rust-toolchain.toml`. This is not
because the kernel wants nightly features: the `bootloader` crate's build script
uses `-Zbuild-std` to compile its boot stages, which requires a nightly Cargo. The
kernel's own use of unstable features is held to an allowlist enforced in CI (see
Open Questions; the allowlist is currently proposed as exactly one entry, and that
proposal is the open question).

Everything the kernel claims about itself is proved by an in-kernel self-test
battery compiled behind the `selftest` Cargo feature. The battery runs at boot,
writes its progress to the serial port, and ends by writing a verdict byte to the
`isa-debug-exit` device at port `0xf4`; QEMU then exits with 33 for success or 35
for failure. `xtask test` requires both the exit code and the string
`SELFTEST PASSED` in the captured serial log, because either signal alone can be
produced accidentally.

## Data model

There are no tables. The equivalent artefact for a kernel — the thing that must be
laid out before code is written, and that everything else depends on — is the
memory map and the table of global state. Both are reproduced here in full.

### Address space at kernel entry

The bootloader hands over with paging already enabled and the kernel mapped. The
configuration is fixed in `kernel/src/main.rs` and is load-bearing:

| Setting | Value | Why |
|---|---|---|
| `mappings.physical_memory` | `Some(Mapping::Dynamic)` | All physical memory is mapped at a bootloader-chosen virtual offset. Required from day one because M3's frame allocator and page-table code cannot function without it, and changing it later would invalidate every address in this table. |
| `kernel_stack_size` | 100 KiB | Deep enough for glyph rendering and the battery; small enough that the deliberate stack-overflow test terminates quickly. |
| Physical memory offset | `boot_info.physical_memory_offset` | An `Optional<u64>`. **The `None` case is real** and must fail closed with a legible panic naming the missing mapping, never `unwrap()` on the assumption that the config above was honoured. |
| Framebuffer | `boot_info.framebuffer` | Also an `Optional`. **The `None` case is real**: firmware may provide no framebuffer. The console degrades to serial-only, the shell still runs over serial, and the self-test battery still passes. It is not a panic. |

### Fixed virtual addresses

| Region | Virtual address | Size | Notes |
|---|---|---|---|
| Kernel heap | `0x_4444_4444_0000` | 1 MiB | Chosen to be obviously synthetic in a fault dump. Mapped page by page at M3 from frames supplied by the frame allocator. |
| Double-fault stack (IST 0) | Allocated in `.bss` | 20 KiB | A `static mut` array; its top is written into `TSS.interrupt_stack_table[0]`. Never overlaps the kernel stack; that separation is the entire point of it. |
| Physical memory window | Bootloader-chosen | All of RAM | Read via `boot_info.physical_memory_offset`; never hard-coded. |

### Global state table

Every mutable global in the kernel, what protects it, and whether an interrupt
handler may touch it. This table is the contract that keeps the system free of
interrupt-context deadlocks.

| Global | Type | Initialised at | Protected by | Touched from IRQ? |
|---|---|---|---|---|
| `GDT` | `GlobalDescriptorTable` + selectors | M2, once | `spin::Lazy`, then immutable | No |
| `TSS` | `TaskStateSegment` | M2, once | Immutable after load | Read by CPU only |
| `IST0_STACK` | `[u8; 20 KiB]` | Static, in `.bss` | Written only by the CPU on double fault | Yes, by hardware |
| `IDT` | `InterruptDescriptorTable` | M2, once | `spin::Lazy`, then immutable | Read by CPU only |
| `PICS` | `ChainedPics` | M2 | `spin::Mutex` | **Yes** — end-of-interrupt write. Held for a few instructions; never nested with any other lock. |
| `TICKS` | `AtomicU64` | M2 | Atomic, `Relaxed` | **Yes** — incremented by the timer handler. |
| `SCANCODE_RING` | Fixed-capacity lock-free ring, 128 bytes, static | M2 | Atomic head/tail indices | **Yes** — the sole IRQ-to-task channel. Full ring drops the oldest byte and increments a counter; it never blocks and never allocates. |
| `SERIAL` | `SerialPort` | M1 | `spin::Mutex`, **`try_lock` only from IRQ context** | Yes, under `try_lock` — a contended log line is dropped, not deadlocked. |
| `CONSOLE` | Framebuffer writer + character shadow grid | M1 (grid at M3) | `spin::Mutex` | **Never.** This is the hard rule below. |
| `ALLOCATOR` | Zeroing wrapper over `LockedHeap` | M3 | `spin::Mutex` inside the heap | **Never** — no allocation in interrupt context, which is why the scancode ring is static. |
| `FRAME_ALLOCATOR` | Bump allocator over the boot memory map | M3 | `spin::Mutex` | No |
| `SELFTEST_PHASE` | `AtomicUsize` | M1 | Atomic | Read by the double-fault handler; see the battery protocol below. |
| `BOOT_TSC` | `AtomicU64` | M1 | Atomic | No |
| `ZEROED_BYTES` | `AtomicU64` | M3 | Atomic | No. Reported by the `privacy` command as evidence that zero-on-free is live. |

**The concurrency rule, stated once and enforced everywhere:** an interrupt handler
never takes the console lock, and never takes any lock it cannot acquire with
`try_lock`. The timer and keyboard handlers touch atomics and the static scancode
ring only. Kernel logging from interrupt context goes to serial under `try_lock` and
is dropped on contention. This rule is why the keyboard path is a static ring rather
than a heap-backed queue, and it is the reason there is no plausible deadlock
between the shell and the interrupt handlers.

### Correction to the original plan: three memory-design decisions

Recorded here because they contradict the approved plan and the correction must not
be quietly re-litigated during implementation.

1. **The console back-buffer is a character grid, not a pixel double buffer.** The
   plan called for a heap-backed double buffer. At the resolutions this bootloader
   negotiates, a pixel double buffer is 1.2 MiB (640×480×4) to 3 MiB (1024×768×4),
   larger than the entire 1 MiB heap, and a tenth of the target RAM floor. A
   character-cell shadow grid of `(cols × rows)` cells holding a byte and a colour
   costs roughly 8 KiB, makes scrolling a memory move in RAM rather than a read back
   from write-combining video memory, and supports redraw. The pixel double buffer
   is rejected on both the heap budget and the lightness pillar.
2. **Physical frames are zeroed when handed out, not when released.** The plan said
   released frames are zeroed. The v1 frame allocator is a bump allocator over the
   boot memory map and never releases a frame, so a zero-on-release claim would be
   unfalsifiable: the code path could be deleted and no test would notice. Zeroing
   on hand-out is the testable form of the same guarantee: a freshly mapped page
   never exposes what the firmware or bootloader left in that frame. The self-test
   asserts exactly that.
3. **The zero-on-free assertion is "the sentinel is gone", not "every byte is
   zero".** `linked_list_allocator` writes its free-list node — a size and a next
   pointer, 16 bytes — into the head of a block when that block is freed. The
   wrapper must zero the block **before** delegating to the inner `dealloc`, or it
   corrupts the free list; the allocator's metadata therefore lands in those first
   16 bytes afterwards, and a naive "all bytes are zero" assertion fails on
   correct code. The invariant that is both true and worth testing is that **no
   caller data survives**: write a sentinel word throughout a block, free it,
   allocate the same size again, and assert the sentinel word appears nowhere in
   what comes back. Remove the zeroing wrapper and this test fails, which is the
   property a test needs to have.

## Interfaces

Signatures are indicative; contracts are binding.

**`kshared`** — pure, host-tested, no allocator, no hardware.

```rust
pub const fn align_up(addr: u64, align: u64) -> u64;      // exists (M0)
pub const fn align_down(addr: u64, align: u64) -> u64;    // exists (M0)

/// Usable byte ranges derived from the boot memory map, with the ranges the
/// bootloader has already claimed removed. Pure arithmetic over a slice of
/// (start, len, kind) triples so it is testable without a machine.
pub fn usable_regions(regions: &[Region]) -> impl Iterator<Item = Region> + '_;

/// Line editor over a caller-supplied buffer. No allocation; the kernel owns
/// the storage. Returns what the caller must do to the screen, so the editor
/// itself never touches the console.
pub struct LineEditor<'a> { /* buffer, cursor, history ring */ }
impl<'a> LineEditor<'a> {
    pub fn feed(&mut self, key: Key) -> EditResult; // Redraw | Submit(&str) | Nothing
    pub fn line(&self) -> &str;
}

/// Command parsing: splits a submitted line into a verb and arguments, with the
/// quoting and whitespace rules the shell documents. Returns a parse error the
/// shell can print, never a panic.
pub fn parse_command(line: &str) -> Result<Command<'_>, ParseError>;
```

**`kernel`** — the module surface, in dependency order.

```rust
mod serial;      // SerialPort at 0x3F8; try_lock accessor for IRQ context
mod framebuffer; // PixelWriter keyed off Info { pixel_format, stride, bytes_per_pixel }
mod console;     // Console: character grid + glyph rendering + scrolling + ANSI colour
mod logger;      // log::Log impl fanning out to console and serial, honouring the lock rule
mod gdt;         // init(): GDT, TSS, IST0 for the double-fault stack
mod interrupts;  // init_idt(), PIC remap to 32..47, timer at 100 Hz, keyboard IRQ
mod memory;      // frames (bump allocator), paging (OffsetPageTable), heap (1 MiB)
mod task;        // executor (cooperative), keyboard (ScancodeStream)
mod shell;       // prompt, dispatch, command implementations
mod selftest;    // #[cfg(feature = "selftest")] battery + phase tracking
mod qemu;        // exit_success() / exit_failure() via port 0xf4
```

**`xtask`** — already implemented at M0; the contract is fixed.

```
cargo xtask build [--selftest]                 # both images, size budget asserted
cargo xtask run   [--bios|--uefi] [--mem=MB]   # interactive QEMU
cargo xtask test  [--bios|--uefi] [--mem=MB]   # headless; asserts exit 33 AND serial grep
```

`xtask test` forces the `selftest` feature on, streams serial output while capturing
it, kills QEMU after a 120-second timeout and reports a timeout as a hang rather
than a failure, and rejects any exit code that is neither 33 nor 35.

### Self-test battery protocol

The battery is a sequence of phases. Before each phase it stores the phase index in
`SELFTEST_PHASE` and logs the phase name to serial. On completion of the final phase
it prints `SELFTEST PASSED` and exits 33; any assertion failure prints
`SELFTEST FAILED: <phase> <reason>` and exits 35.

**The final phase is the deliberate stack overflow, and it cannot return**. The
double-fault handler ends it. That creates a trap the plan did not account for: if
the double-fault handler unconditionally prints `SELFTEST PASSED`, then an
*accidental* double fault during any earlier phase is reported as success, and the
CI gate silently inverts. The handler therefore reads `SELFTEST_PHASE` first:

- phase == `StackOverflow` → the expected outcome; print `SELFTEST PASSED`, exit 33.
- any other phase → print `SELFTEST FAILED: double fault during <phase>`, exit 35.

In a non-selftest build the double-fault handler renders the panic screen and halts.

## Access control

There is no RLS, no definer function and no `anon` role; there is one privilege
level and whoever holds the keyboard holds it. The analogous artefact — the list of
places where the compiler's guarantees are suspended, and the invariant that must
hold at each — is the unsafe-block inventory. **Every `unsafe` block in the kernel
belongs to one of these categories and carries a `// SAFETY:` comment naming the
invariant. An `unsafe` block that does not fit a category is a design change and
needs this document updated first.**

| # | Category | Where | Invariant that makes it sound |
|---|---|---|---|
| 1 | **Port I/O** | `serial`, `interrupts` (PIC/PIT/keyboard), `qemu` | The port number is a compile-time constant naming a device this kernel owns exclusively. No other code writes that port. Widths match the device (`0x3F8` byte, `0xF4` doubleword, `0x60` byte). Reads have no side effects beyond the documented device behaviour. Notably, port `0x60` **must** be read in the keyboard handler or the controller stops delivering interrupts. |
| 2 | **Page-table manipulation** | `memory::paging`, `memory::heap` | The `OffsetPageTable` is constructed once from `boot_info.physical_memory_offset`, which the caller has already checked is `Some`. The complete physical memory is mapped at that offset, guaranteed by `Mapping::Dynamic` in the bootloader config and re-checked, not assumed. A frame handed to `map_to` came from the frame allocator and is therefore unaliased. The TLB is flushed before the new mapping is read. |
| 3 | **Descriptor-table loading** | `gdt`, `interrupts::init_idt` | The GDT, TSS and IDT are `'static` and are never mutated after being loaded. Segment selectors written to `CS` and `SS` index entries that exist in the GDT that was just loaded. The IST0 pointer is the top of a 20 KiB static array that nothing else uses. |
| 4 | **Framebuffer writes** | `framebuffer` | The base pointer, `stride`, `bytes_per_pixel` and `pixel_format` come from the `FrameBufferInfo` the firmware negotiated, never hard-coded, because UEFI GOP and BIOS VBE differ, and both are boot-tested for exactly this reason. Every write is bounds-checked against `stride × height × bytes_per_pixel` before the pointer is formed. Writes are volatile; the memory is device memory, not ordinary RAM. |
| 5 | **`hlt` and interrupt-flag control** | idle loop, `panic`, critical sections | `hlt` touches no memory (`options(nomem, nostack, preserves_flags)`). The idle path uses the enable-then-halt sequence so that a wake-up racing the halt is not lost. Interrupts are disabled only around a section that provably cannot block. |
| 6 | **Static mutable access** | `IST0_STACK`, ring indices | Access is through raw pointers or atomics, never a `&mut` to a `static mut`. The double-fault stack is written only by the CPU. |
| 7 | **Zeroing a block being freed** | `memory::heap` allocator wrapper | The pointer and layout are those the caller passed to `dealloc`, so the block is live, owned by the allocator at that instant, and exactly `layout.size()` bytes. Zeroing happens **before** the inner `dealloc`; afterwards the allocator owns those bytes and writing them would corrupt the free list. |
| 8 | **Panic-time console lock recovery** | `panic` handler, double-fault handler | The panic path may run while the console lock is held, including from interrupt context, which is the one case the concurrency rule cannot prevent. The lock is therefore force-released before rendering. Sound only because the machine is stopping: the handler halts and never returns, no other code will observe the console again, and rendering a legible panic is worth more than a lock invariant that has no future reader. **This is the only place the console lock may be broken, and the only place any lock is force-released.** It is never a pattern to copy. |
| 9 | **Selftest-only reads of freed memory** | `selftest`, `#[cfg(feature = "selftest")]` only | Present only in test builds. Reads are volatile, through a raw pointer, within a block whose size is known, and the value is used solely for an assertion. Not compiled into a shipped image. Prefer the sentinel-absence test, which needs no such read; if a direct read is used, it is confined to this row. |

## Migrations

There is no database, so the section that carries the same risk — a change that must
be sequenced correctly or it cannot be undone cheaply — is the milestone plan. Each
milestone is additive over the last, each ends at a gate, and **the gate is a green
CI run, not a local success**. Nothing starts before the previous gate is green.
Every green milestone is committed and pushed.

| # | Does | Reversible? | Rollback |
|---|---|---|---|
| **M0** | Workspace, pinned toolchain, halt-loop kernel, `xtask` producing both images, CI for fmt/clippy/stable/host-tests/image-build, these four documents. **Complete on disk; not yet committed.** | Yes | Delete the tree; nothing is published. |
| **M1** | Framebuffer pixel writer, glyph console with scrolling, serial, `log` fan-out, panic screen to both sinks. Under `--features selftest`, print `SELFTEST PASSED` and exit 33 so CI has a boot gate from the first milestone. Add the `boot-test` matrix job. | Yes | `git revert` the range; M0 still boots. |
| **M2** | GDT, TSS, IST0 (20 KiB), IDT with breakpoint, general-protection and page-fault handlers (page fault reports CR2) and double fault. PIC remapped to 32..47, PIT at 100 Hz, keyboard IRQ reading port `0x60` into the static scancode ring. Self-tests: `int3` returns, ticks advance, page-fault formatting. | Yes | `git revert`; M1's console still boots and still passes its battery. |
| **M3** | Frame allocator over the boot memory map (region maths in `kshared`, host-tested), `OffsetPageTable`, 1 MiB heap so `alloc` is live, zeroing allocator wrapper, frames zeroed on hand-out, console gains its character shadow grid. Self-tests: `Box`, 100k-element `Vec`, reuse after free, fresh page reads zero, sentinel absent after free. | Yes | `git revert`; M2 is heap-free and unaffected. |
| **M4** | Cooperative executor over `alloc::task::Wake` with a `crossbeam_queue::ArrayQueue` ready queue (valid here: the heap exists from M3), enable-then-halt idle, scancode ring drained into a `ScancodeStream`, decoded with `pc-keyboard` 0.9's `ScancodeSet1::advance_state` and `EventDecoder::process_keyevent`. Self-test: a spawned task provably runs. | Yes | `git revert`; M3's synchronous battery still passes. |
| **M5** | Line editor and history from `kshared`, the full command surface, boot time in the banner from a PIT-calibrated timestamp counter, the complete battery ending with the stack-overflow phase, the RAM floor measured and pinned, the size budget tightened to the measured value, the shipped-image boot job, README screenshot. | Yes | `git revert` to the M4 tag; every earlier gate still holds. |

**The sequencing correction the plan needs:** the keyboard interrupt lands at M2 and
must read port `0x60` and store the byte somewhere, but the heap does not exist until
M3, and `crossbeam_queue::ArrayQueue::new` allocates. The M2 destination is therefore
the static, fixed-capacity `SCANCODE_RING` described above, which allocates nothing
and blocks nothing. `crossbeam-queue` is used at M4 for the executor's ready queue
only. This keeps the interrupt path allocation-free permanently, which is the right
end state anyway.

## Failure modes

| What breaks | Who notices | How we detect it | How we undo it |
|---|---|---|---|
| **Triple fault** — a fault while handling a fault while handling a fault; the CPU resets. Most likely cause is a broken IDT or a double fault without a valid IST stack. | CI, as a boot that never produces a verdict | QEMU runs with `-no-reboot`, so it exits instead of looping; `xtask` sees an exit code that is neither 33 nor 35 and fails with that code. Without `-no-reboot` this presents as a hang. | `git revert` the commit touching `gdt` or `interrupts`. The M2 gate exists precisely to catch this in one milestone's worth of diff. |
| **Boot hang** — no faults, no progress: a spin lock taken twice, a loop with no exit, firmware that never hands over. | CI, as a job that stops producing output | `xtask` kills QEMU after 120 s and reports a timeout distinctly from a failure; the job also has its own timeout. The serial log is uploaded with `if: always()`, so the last phase reached is visible. | `git revert`. The phase logging means the log names the last phase that started, which localises the hang to one milestone's code. |
| **Interrupt-context deadlock** — a handler blocks on a lock the interrupted code holds. Classically: logging to the console from the timer handler. | Nobody, until the machine stops responding | Prevented structurally, not detected: the concurrency rule above, plus code review against the global-state table's "touched from IRQ" column, plus `try_lock`-only serial access from handlers. | Not applicable if the rule holds. If one is found, the fix is to move the offending access out of interrupt context, never to make the lock reentrant. |
| **Heap exhaustion** — the 1 MiB heap fills. | The person at the keyboard | `alloc_error_handler` is a clean panic naming the requested layout and the heap's used and free byte counts, rendered on the panic screen. Not a silent hang, not a corrupt allocation. | Reduce the allocation, or raise the heap size deliberately and re-measure the RAM floor in the same commit. |
| **Insufficient physical memory at boot** — the machine has less RAM than the heap and mappings need. | CI at the pinned floor; a person on a small machine | The heap initialiser checks the frame allocator's supply before mapping and panics with "need X KiB, have Y KiB" rather than page-faulting halfway through. | Boot with more memory, or lower the heap size. The pinned CI floor is the regression test for this. |
| **No framebuffer from firmware** | A person seeing a blank screen | `boot_info.framebuffer` is `None`. Handled, not a fault: the console falls back to serial-only, logs the fallback, and the shell and battery still run. | None needed; this is a supported configuration. |
| **Firmware-specific pixel format assumption** — code that works on BIOS VBE and corrupts under UEFI GOP, or vice versa. | Whoever boots the other firmware | The renderer reads `pixel_format`, `stride` and `bytes_per_pixel` from the negotiated `FrameBufferInfo`, and **both firmwares are boot-tested in the CI matrix**. A hard-coded assumption fails one leg of the matrix. | `git revert`; the matrix names which firmware broke. |
| **Scancode ring overflow** — typing faster than the executor drains. | Nobody, in practice | The ring drops the oldest byte and increments a dropped-keystroke counter reported by `sysinfo`. Documented as lossy by design; it must never block an interrupt handler. | Not a fault. If the counter is ever non-zero in normal use, the executor is too slow and that is the bug to fix. |
| **Pinned nightly stops resolving or changes behaviour** | The first CI run after the pin moves | Every job fails at toolchain install or at build. | The pin is bumped only in a dedicated pull request that changes nothing else, so reverting that one commit restores a known-good toolchain. |
| **Bootloader build stages fail to fetch** | First build on a cold cache | `bootloader` is pinned with `=0.11.17`, `Cargo.lock` is committed, and the CI cache is keyed on it. A fetch failure fails the `image-build` job loudly. | Re-run; if persistent, the pin is the thing to investigate, not the kernel. |
| **A self-test passes vacuously** — asserts a property it does not exercise. | Nobody, which is what makes it the worst entry in this table | Each self-test must be **observed failing once** under a deliberate mutation before it counts as a test: break the zeroing wrapper, break the frame zeroing, remove the IST assignment, stop reading port `0x60`. Both outputs — the failing run and the passing run — are recorded. | Rewrite the test. A test that cannot be made to fail is deleted, not kept for reassurance. |

## Rollback

**The undo is `git revert` of the offending commit or milestone range, and the boot
gate is what makes that safe.** Because CI boots both firmwares on every push to
`main`, any commit on `main` is a known-bootable state; reverting to one restores a
system that is known to start, not one that is merely believed to. There is nothing
deployed, nothing migrated and nothing persisted, so a revert is complete by
construction: no data has to be reconciled, because no data exists.

Time to undo: one `git revert` plus one CI run, under fifteen minutes on a warm
cache, and the person doing it at 2am needs to know nothing beyond which milestone
introduced the fault, which the serial log's phase names tell them.

Two rollbacks are cheap but need naming, because they are the ones that will
actually be reached for:

- **A toolchain pin bump** is always its own commit that changes nothing else, so
  reverting it cannot take working kernel code with it.
- **A dependency addition** is likewise its own commit, together with the
  allowlist update, so the allowlist and the tree cannot drift apart.

Nothing in v1 is irreversible. The only genuinely destructive act available in this
project is repository deletion, which is out of scope for any automated process.

## Test plan

The tests that would fail without this design, by layer. **A test that has never
been observed failing does not count**. Every entry below names the mutation that
must be seen to break it.

**Host tests — `cargo test -p kshared`** (fast, run on every push)

- *Positive:* `align_up`/`align_down` round correctly at, above and below a page
  boundary (present at M0). `usable_regions` yields exactly the usable ranges of a
  representative boot memory map. The line editor inserts, deletes, moves and
  submits; history recalls in order. `parse_command` splits verbs and arguments.
- *Negative:* `parse_command` returns a `ParseError` — never a panic — on an
  unterminated quote and on an unknown verb. The line editor refuses input past the
  end of the caller's buffer instead of writing past it.
- *Boundary:* an empty memory map yields no regions; a region of length zero is
  dropped; a region ending exactly at a page boundary is not truncated; an empty
  line submits as a no-op rather than an error; a line exactly the buffer's length
  submits, and one byte more is rejected.
- *Mutation:* invert the rounding direction in `align_up`; drop the last region in
  `usable_regions`. Both must turn the suite red.

**In-kernel battery — `cargo xtask test --bios` and `--uefi`, at the pinned floor**

- *Positive, per milestone:* the banner reaches serial (M1); `int3` returns to the
  next instruction and the timer's tick count advances (M2); a `Box` allocates, a
  100,000-element `Vec` grows and frees, and a freed block is reused (M3); a spawned
  task provably runs and sets a flag the battery reads (M4); the full battery
  completes and every phase logged is a phase that ran (M5).
- *Negative — the privacy claims, which are the tests that justify the project:*
  a sentinel word written throughout a heap block appears **nowhere** in the
  allocation handed back after that block is freed; a freshly mapped page reads as
  all zero before anything writes to it.
- *Negative — the survivability claim:* a deliberate kernel stack overflow raises a
  double fault, the handler runs on IST0, and the verdict it prints depends on
  `SELFTEST_PHASE`, so an accidental double fault in an earlier phase prints
  `FAILED` and exits 35.
- *Boundary:* the battery passes at the pinned minimal RAM, not merely at a
  comfortable default. Both firmwares pass. The default, non-selftest image reaches
  the interactive prompt; the shipped artefact is boot-proven, not just its test
  sibling. Both images are under the size budget.
- *Mutation, each observed failing once:* remove the zeroing wrapper (the sentinel
  test must fail); stop zeroing frames on hand-out (the fresh-page test must fail);
  remove the IST0 assignment (the stack-overflow phase must triple-fault into a
  non-33 exit rather than passing); stop reading port `0x60` in the keyboard handler
  (input must stop after one keystroke); hard-code a pixel format (one firmware leg
  of the matrix must fail).

**Build gates**

- `cargo fmt --check`, and `clippy -D warnings` on kernel, `kshared` and `xtask`.
- The unstable-feature allowlist gate (see Open Questions): the set of
  `#![feature(...)]` attributes in `kernel/` must equal the allowlist exactly.
  *Mutation:* add any feature attribute; the job must fail.
- The dependency allowlist gate: the kernel's resolved dependency set must equal
  the allowlist. *Mutation:* add a crate; the job must fail. This is what makes "no
  network stack exists" a checked statement rather than a promise.
- Image size budget, asserted in `xtask build` on every build.

**Manual, once, before v1 is tagged**

- Boot interactively under QEMU, type into the shell, run every command in the
  surface, trigger `panic` deliberately and read the panic screen, and capture the
  README screenshot from that session rather than from a mock-up.

## Build order

1. **Commit M0.** The scaffold and these documents exist on disk and are not yet
   committed. Run the secret gate, commit, push, confirm CI is green on all four
   existing jobs.
2. **Resolve the open question below**, because it determines whether the
   `stable-compat` job survives M2 or is replaced by the feature-allowlist gate. It
   does not block steps 3 or 4.
3. **M1:** serial, then pixel writer, then glyph console, then the `log` fan-out,
   then the panic screen. Add `SELFTEST PASSED` and the exit device. Add the
   `boot-test` CI matrix over `{bios, uefi}`. From here on, every push is
   boot-proven.
4. **Add the dependency-allowlist CI job.** Cheap, and it is the gate that makes the
   PRD's first privacy claim checkable. Doing it before more crates land means it
   never has to be retrofitted.
5. **M2:** GDT and TSS with IST0 first, then the IDT with breakpoint and double
   fault, then general protection and page fault, then the PIC remap, then the PIT,
   then the keyboard handler and the static scancode ring. Self-tests as each lands.
6. **M3:** region maths in `kshared` with host tests first, then the frame allocator,
   then `OffsetPageTable`, then the heap, then the zeroing wrapper, then the console
   shadow grid. The two privacy self-tests land with the code they test, and each is
   observed failing under its mutation before the milestone is called done.
7. **M4:** executor, waker, `ScancodeStream`, `pc-keyboard` decode. Note that
   `pc-keyboard` 0.9's API differs from the widely copied 0.7-era examples.
8. **M5:** line editor and history in `kshared` (host-tested before wiring), then the
   command surface, then the banner's boot time, then the stack-overflow phase last.
9. **Measure, then pin.** Bisect the RAM floor by re-running `xtask test --mem=N`
   downwards until it fails; pin the CI value just above the floor and record both
   numbers in the README. Tighten the image budget to the measured size plus a
   stated margin.
10. **Add the shipped-image boot job**, which boots the default build and asserts it
    reaches the prompt.
11. **Adversarial pass:** run every mutation listed above, record both outputs, then
    the security review, honestly graded: most of the ten-point floor is genuinely
    not applicable to a bare-metal target with no network, and each such item is
    marked not-applicable with its reason rather than scored as a pass.
12. **Release:** README screenshot from a real boot, `SESSION_HANDOFF.md` current,
    tag v1.

## Open questions

**1. Does the kernel keep a zero-unstable-features rule, or an allowlist of exactly
one? — blocks M2 only. Owner's decision.**

The plan states that the kernel source stays stable-clean with zero `#![feature]`
attributes, enforced by the `stable-compat` CI job that exists today. **That is not
achievable as written once the IDT lands at M2.** Verified against the installed
toolchain on 2026-08-17, `rustc 1.97.1`:

```
error[E0658]: the extern "x86-interrupt" ABI is experimental and subject to change
 --> abi_probe.rs:1:12
  |
1 | pub extern "x86-interrupt" fn h(_f: u64) {}
  |            ^^^^^^^^^^^^^^^
  = note: see issue #40180 for more information
```

Every handler installed through `InterruptDescriptorTable::set_handler_fn` must have
that ABI, so at M2 the kernel needs `#![feature(abi_x86_interrupt)]` and the
`stable-compat` job fails. Three ways forward:

- **(a) Recommended.** Accept exactly one unstable feature, `abi_x86_interrupt`, and
  replace `stable-compat` with a **feature-allowlist gate**: CI asserts that the set
  of `#![feature(...)]` attributes under `kernel/` is exactly `{abi_x86_interrupt}`.
  This keeps the real goal — no nightly creep — and is a gate that can actually go
  green. The claim in the README becomes "one unstable feature, named, gated",
  which is both true and stronger than an unenforceable "none".
- **(b)** Stay strictly stable by installing handlers with `Entry::set_handler_addr`
  and writing `#[unsafe(naked)]` assembly trampolines for every vector. Confirmed
  feasible — naked functions and `naked_asm!` do compile on stable 1.97.1 — but it
  trades one allowlisted feature for a hand-written trampoline per vector, a
  materially larger unsafe surface, and the loss of the `x86_64` crate's typed
  handler API. That is a poor trade against the "least machinery" pillar.
- **(c)** Drop the stable-compatibility goal entirely. Rejected: the gate is what
  stops nightly features accumulating, and losing it costs more than it saves.

Until this is answered, M0 and M1 proceed unchanged; `stable-compat` stays green
because nothing before M2 needs the feature. **The answer must land before the first
IDT commit**, because retrofitting the gate after handlers exist means writing a
gate against code that already violates it.

**2. What is the actual RAM floor?** Not answerable by design; it is measured at step
9 above. The target is 32 MiB or less. The current `DEFAULT_TEST_MEM_MB` of 128 in
`xtask` is an explicit placeholder, and the lightness gate is not a gate until that
number is a measured one. This is tracked, not open in the blocking sense.
