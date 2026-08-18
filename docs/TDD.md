# TDD — Osmium v1 kernel

**Status:** current — matches the shipped code through M10 (fault isolation, 2026-08-18; per-task address spaces M9 and preemptive multitasking M8 the same day); both open questions are resolved below
**Date:** 2026-08-18 · **PRD:** [PRD.md](PRD.md) · **Repo:** `Leo-Y-Zhang/Osmium`

## Approach

Osmium is a three-crate Cargo workspace, plus `user/hello`, `user/counter` and
`user/crasher` — standalone crates the
kernel's build script compiles into the embedded user ELFs. `kernel/` is a `no_std`
binary built for
`x86_64-unknown-none`; it owns everything that touches hardware. `kshared/` is a
`no_std` library of pure logic — address arithmetic, memory-region maths, the line
editor, command parsing, the ELF loader front-end — with no hardware access and no
allocator requirement, so
it compiles for the host and is covered by ordinary `cargo test`. `xtask/` is a host
binary that builds the kernel, wraps it into BIOS and UEFI disk images with the
`bootloader` crate, drives QEMU, and enforces the size and RAM budgets; the
workspace's `default-members` is `xtask` so that a bare `cargo build` operates on
tooling and never tries to build the kernel for the host.

The workspace is pinned to a specific nightly in `rust-toolchain.toml`. This is not
because the kernel wants nightly features: the `bootloader` crate's build script
uses `-Zbuild-std` to compile its boot stages, which requires a nightly Cargo. The
kernel's own use of unstable features is held to an allowlist enforced in CI —
exactly one entry, `abi_x86_interrupt`; the Open Questions section records how
that was decided.

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
| Framebuffer | `boot_info.framebuffer` | Also an `Optional`. **The `None` case is real**: firmware may provide no framebuffer. The kernel logs the fallback and continues; boot and the self-test battery are proven over serial. The shell renders to the console only (the keystroke-privacy rule), so this is a supported *degraded* configuration — boot-proven, not interactive. It is not a panic. |

### Fixed virtual addresses

| Region | Virtual address | Size | Notes |
|---|---|---|---|
| Kernel heap | `0x_4444_4444_0000` | 1 MiB | Chosen to be obviously synthetic in a fault dump. Mapped page by page at M3 from frames supplied by the frame allocator. |
| Double-fault stack (IST 0) | Allocated in `.bss` | 20 KiB | `DOUBLE_FAULT_STACK`: a byte array inside an `UnsafeCell` (which is what keeps it out of read-only memory) rather than a `static mut`, so no `&mut` to a mutable static is ever formed. Its top is written into `TSS.interrupt_stack_table[0]`. Never overlaps the kernel stack; that separation is the entire point of it. |
| NMI stack (IST 1), machine-check stack (IST 2) | Allocated in `.bss` | 20 KiB each | Same `IstStack` arrangement as IST 0, wired in `gdt.rs`, so those faults too run on known-good memory. |
| Default ring-0 privilege stack (TSS RSP0) | Allocated in `.bss` | 20 KiB | `PRIVILEGE_STACK`: the stack the CPU switches to when ring 3 traps in *outside* a scheduler run. While tasks run, RSP0 points at the current task's own kernel stack instead — see the next row. |
| Per-task kernel stacks | Heap-allocated (`Box<[u8]>`) | 20 KiB each (M8) | One per scheduled task, created at `sched::install` and freed at `collect` (the allocator's zero-on-free scrub erases a dead task's saved registers). RSP0 is retargeted to the current task's stack on every context switch, so each task's trap frames land on its own memory — two tasks sharing one privilege stack would overwrite each other's saved context. |
| User image window | `0x40_0000`–`0x60_0000` | 2 MiB window | `kshared::elf::USER_IMAGE_BASE..USER_IMAGE_END`: the only region a user segment may occupy — **per address space** since M9. Two programs may claim the same pages; they land in different page tables. Mapped only in per-task spaces, never in the kernel's table. |
| User stack page | `0x80_0000` | 4 KiB | `usermode::USER_STACK_ADDR` — the SAME virtual address in every task's space, which is itself a statement of the M9 isolation model: each space maps it to its own private frame. |
| Per-task page tables | Allocated frames (M9) | ~5 frames per space | Each [`AddressSpace`]: a cloned PML4 sharing every kernel subtree, plus a deep-copied entry-0 chain (PDPT + first-GiB PD) privatising only the user window's and stack's 2 MiB PD slots. The bootloader's two measured low mappings (its handover region at `0x0..0x200000`, its early-GDT region at `0x1000000..0x1200000`) stay shared; `new_user` asserts the user slots are vacant kernel-side, pinning the measurement. Not reclaimed on drop (bump allocator). |
| Physical memory window | Bootloader-chosen | All of RAM | Read via `boot_info.physical_memory_offset`; never hard-coded. |

### Global state table

Every mutable global in the kernel, what protects it, and whether an interrupt
handler may touch it. This table is the contract that keeps the system free of
interrupt-context deadlocks.

| Global | Type | Initialised at | Protected by | Touched from IRQ? |
|---|---|---|---|---|
| `GDT` | `GlobalDescriptorTable` + selectors | M2, once | `spin::LazyLock`, then immutable | No |
| `TSS` | `TaskStateSegment` in an `UnsafeCell` (`TssCell`) | M2 (ISTs, default RSP0); **RSP0 rewritten per context switch since M8** | Every RSP0 write happens with interrupts disabled (`set_privilege_stack` asserts it), so the CPU cannot take a ring-3 trap mid-update; the GDT descriptor holds only the TSS's address (`tss_segment_unchecked`), so no shared reference is held across a mutation | Read by CPU on privilege transitions; **written from the timer, `SYS_EXIT` and fault-kill (M10) paths** (IF=0 in all three — exception gates clear IF like interrupt gates) |
| `DOUBLE_FAULT_STACK` | `UnsafeCell<[u8; 20 KiB]>` | Static, in `.bss` | Written only by the CPU on double fault | Yes, by hardware |
| `IDT` | `InterruptDescriptorTable` | M2, once | `spin::LazyLock`, then immutable | Read by CPU only |
| `PICS` | `ChainedPics` | M2 | `spin::Mutex` | **Yes** — end-of-interrupt write. Held for a few instructions; never nested with any other lock. |
| `TICKS` | `AtomicU64` | M2 | Atomic, `Relaxed` | **Yes** — incremented by the timer handler. |
| `BREAKPOINT_HITS` | `AtomicUsize` | M2 | Atomic, `Relaxed` | **Yes** — bumped by the breakpoint handler. A counter only; nothing reads it in interrupt context. (The M2-era `SCANCODES_SEEN` counter was removed once M4's waker-backed queue superseded it and nothing read it.) |
| `SCANCODE_QUEUE` | `Once<ArrayQueue<u8>>`, capacity 128 | M4; the queue's storage is allocated on the first `ScancodeStream::new()`, in task context | Lock-free queue, `Once` for the one-time construction | **Yes** — the sole IRQ-to-task channel. The handler pushes and never blocks or allocates. A full queue drops the byte that has just arrived — the newest, not the oldest — and nothing counts the loss. Scancodes arriving before the queue exists are dropped for the same reason: nobody is typing during early boot. |
| `WAKER` | `AtomicWaker` | M4 | Its own atomics | **Yes** — the keyboard handler wakes the shell task through it after a successful push. See the `ALLOCATOR` row for the one drop that entails. |
| `SERIAL1` | `SerialPort` at `0x3F8` | M1 | `spin::Mutex`, taken with a plain `lock` | **No.** Handlers never write to serial at all. The single exception is the double-fault handler on its terminal path, which disables interrupts and reclaims the lock before printing, because the holder it interrupted can never resume. |
| `CONSOLE` | `Option<Console>`: glyph renderer writing straight to the framebuffer | M1 | `spin::Mutex` | **Never from an asynchronous IRQ** — the hard rule below. The `int 0x80` syscall dispatcher writes `SYS_WRITE` output to the console, but that is a *synchronous* trap from the running task (IF=0 for its duration — the timer cannot preempt a syscall mid-write), not a preempting interrupt; `run_programs` `assert`s (a real assert — see the note under the concurrency rule) the caller does not hold the lock across a run, so it cannot deadlock. |
| `ALLOCATOR` | Zeroing wrapper over `LockedHeap` | M3 | `spin::Mutex` inside the heap | **Never allocates from IRQ, and frees from it only in a case that cannot arise.** The keyboard handler's `WAKER.wake()` consumes a cloned `Waker` and drops its `Arc<TaskWaker>`; a *last* drop would deallocate here, in interrupt context, against a lock the interrupted code may hold. It is never the last, and the condition is worth stating because the code depends on it: the executor's `waker_cache` holds a reference for as long as the task exists, and the shell task never completes. A woken task that can finish would break this row, so this row changes before that code does. |
| `MAPPER`, `FRAME_ALLOCATOR` | `Option<OffsetPageTable>`, `Option<BootFrameAllocator>` | M3 | `spin::Mutex` | No |
| `EXPECTING_DOUBLE_FAULT` | `AtomicBool`, `#[cfg(feature = "selftest")]` only | M5, by the battery's last check | Atomic, `SeqCst` | Read by the double-fault handler; see the battery protocol below. |
| `PRIVILEGE_STACK` | `UnsafeCell<[u8; 20 KiB]>` | Static, in `.bss` (M6) | Written only by the CPU, on a ring-3 → ring-0 transition taken outside a scheduler run (TSS RSP0's default target) | Yes, by hardware |
| `KERNEL_CONTINUATION_RSP` | `AtomicU64` | M6; rewritten by every `sched::enter_tasks` | Written before any ring-3 instruction can execute | Read by the `int 0x80` entry's run-complete path — a software interrupt from ring 3, sound only because ring 3 is reachable *only* through `enter_tasks`, which always writes it first |
| `SCHED` | `Mutex<Sched>`: the task table (per-task kernel stack, saved RSP, **address-space root (CR3, M9)**, ready flag, exit code/order, **fault vector (M10)** — `Some(v)` when the kernel terminated the task for a ring-3 fault), current index, active flag, counters | M8; task table built at `install`, drained at `collect` | `spin::Mutex`, **locked only with interrupts off** — the timer, `SYS_EXIT` and fault-kill paths run behind interrupt/exception gates (IF=0), and the launcher wraps install/enter/collect in its own cli window. On one core that makes contention impossible; real `assert!`s pin the discipline in every build. The task table is fully allocated in `install` (launcher context) and only indexed from handlers, preserving the handlers-never-allocate rule | **Yes** — the timer handler saves/picks/switches; the `int 0x80` `SYS_EXIT` path marks exits; **`kill_current` (M10) marks a fault termination and leaves through the same two exits `sys_exit` uses** |
| `TIMER_ENTRY_AC` | `AtomicBool`, `#[cfg(feature = "selftest")]` only | M8 | Atomic, `SeqCst` | **Yes** — set by the timer's Rust half if `EFLAGS.AC` survived the naked entry's scrub; the battery asserts it never did while a hostile program held AC set |
| `KERNEL_CR3`, `PHYS_OFFSET` | `AtomicU64` × 2 | M9, once at `memory::init` | Write-once, then read-only | `KERNEL_CR3` is read by the scheduler's run-complete path (IF=0) to restore the kernel's root; `PHYS_OFFSET` only from launcher context |
| `BOOT_TSC`, `MARKS` | `AtomicU64`, `[AtomicU64; 3]` | First line of `kernel_main`; one `stamp` per boot phase (M5) | Atomic, `Relaxed` | No |
| `CYCLES_PER_MS` | `AtomicU64`, `#[cfg(feature = "selftest")]` only | `time::calibrate`, battery builds only | Atomic, `Relaxed` | No |

**The concurrency rule, stated once and enforced everywhere:** an interrupt handler
touches atomics, the lock-free scancode queue, and at most one lock at a time —
the PIC's, for the end-of-interrupt write, which the main thread never holds once
interrupts are enabled, and (timer and `SYS_EXIT` paths only, since M8) the
scheduler's, whose every non-IRQ taker runs with interrupts disabled, so the lock
is uncontendable rather than merely uncontended. The timer handler takes them
strictly in sequence — EOI completes and releases the PIC before `SCHED` is
touched — so no two locks are ever held together. Nothing else: a handler never
takes the console lock, never takes the serial lock,
never allocates and never logs; there is no `try_lock` fallback because there is no
contended access to fall back from. That is stronger than the rule this document
first proposed, and it is the reason there is no plausible deadlock between the
shell and the interrupt handlers. The one handler that does write to serial is the
double-fault handler, which is a terminal path: it disables interrupts, reclaims the
lock from a holder that can never resume, prints, and exits.

**Why the kernel is non-preemptible, stated as a decision:** the timer switches
contexts only when the interrupted CS's CPL is 3. Preempting kernel code would
invalidate the paragraph above (any lock could then be interrupted mid-hold and
re-entered), for no benefit a two-task round-robin needs — every kernel path here
is short. If kernel preemption is ever wanted, this section is rewritten first.

**The invariant checks are real `assert!`s, not `debug_assert!`s — a same-day
adversarial-review finding.** The kernel is only ever built `--release`
(`xtask::build_kernel` hard-codes it, and no `[profile.release]
debug-assertions` override exists), so a `debug_assert` is dead code in every
artifact CI boots or a person runs — the M8 commit initially shipped all seven
discipline checks (RSP0-with-IF-off, install/collect/sys_exit state, the
console-lock deadlock tripwire) in exactly that inert form, verified by string
absence in the built binary. All seven are now real asserts, and the rule is
general: **in this repository a `debug_assert` proves nothing, because no
debug build exists.** The timer path additionally carries an always-on layout
tripwire: the saved CS read at `SAVED_CS_OFFSET` must equal the kernel or user
code selector on every tick, so a re-ordered save sequence or a wrong offset
(SS also reads as CPL 3) panics on the first tick instead of silently
mis-gating the switch.

**One check that IS unreachable-as-false today, stated so nobody mistakes it
for tested:** the CPL-3 gate in `timer_tick` cannot currently be observed
failing, because while the scheduler is active every kernel path runs with
IF=0 (gates clear it; the launcher holds a cli window), so no ring-0 tick can
arrive to take the wrong branch. It is defense-in-depth for the day a kernel
path re-enables interrupts mid-run, and the CS-selector tripwire above pins
the offset it depends on. Deleting the gate changes no observable behaviour
today; that is a property of the design, recorded rather than discovered.

### Correction to the original plan: three memory-design decisions

Recorded here because they contradict the approved plan and the correction must not
be quietly re-litigated during implementation.

1. **The console keeps no back-buffer at all.** The plan called for a heap-backed
   pixel double buffer. At the resolutions this bootloader negotiates that is 1.2 MiB
   (640×480×4) to 3 MiB (1024×768×4) — larger than the entire 1 MiB heap and a tenth
   of the target RAM floor — so it is rejected on both the heap budget and the
   lightness pillar. What shipped is lighter still than the character-cell shadow
   grid this section originally proposed in its place: glyphs are rendered straight
   into the framebuffer, and scrolling moves whole pixel rows inside it with
   `copy_within`, so the console costs no kernel RAM beyond its cursor and geometry.
   Two consequences, stated rather than discovered later: scrolling reads video
   memory back, which the grid was meant to avoid; and the console cannot
   reconstruct what is on screen, so there is no redraw and no scroll-back. Whichever
   of those is wanted first is what brings the 8 KiB grid with it.
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
   property a test needs to have. One refinement the shipped test carries: the scan
   starts **past the first 16 bytes**. The Hole node lands there after the scrub,
   and a metadata byte can legitimately equal the sentinel (an aligned hole
   pointer's middle byte, say), which would turn the privacy test red with no
   privacy bug. Bytes 16.. are caller data and still prove the scrub — deleting the
   wrapper leaves 240 sentinel bytes in them.

## Interfaces

These are the shipped signatures; the contracts beside them are binding.

**`kshared`** — pure, host-tested, no allocator, no hardware.

```rust
pub const fn align_up(addr: u64, align: u64) -> u64;      // M0
pub const fn align_down(addr: u64, align: u64) -> u64;    // M0
pub const FRAME_SIZE: u64 = 4096;

/// The start address of every whole frame inside `[start, end)`, with partial
/// frames clipped at both edges. Pure arithmetic over two integers, so the
/// frame allocator's region maths is testable without a machine; the kernel
/// keeps the part that needs hardware — deciding which regions are usable.
/// Degenerate ranges (empty, reversed, or too small to hold one aligned
/// frame) yield nothing rather than erroring.
pub fn frame_starts(start: u64, end: u64) -> impl Iterator<Item = u64>;

/// What a fed character did to the line buffer. The editor never touches the
/// console: it reports the action and the caller renders exactly that, so the
/// screen and the buffer cannot drift apart.
pub enum EditAction { Echoed(char), Erased, Submitted, Ignored }

/// Fixed-capacity ASCII line editor with an insertion cursor. It owns its
/// storage — `[u8; LINE_CAP]`, inline — so it needs no allocator and no
/// caller-supplied buffer, and a character past capacity is `Ignored` rather
/// than written anywhere. Insertion and deletion happen at the cursor, so
/// mid-line editing keeps buffer and cursor consistent. History lives in the
/// shell, which owns the heap; the editor holds one line.
pub const LINE_CAP: usize = 256;
pub struct LineEditor { /* buf, len, cursor */ }
impl LineEditor {
    pub const fn new() -> Self;
    pub fn feed(&mut self, c: char) -> EditAction;
    pub fn line(&self) -> &str;
    pub fn cursor(&self) -> usize;         // byte index in 0..=len
    pub fn move_left(&mut self) -> bool;   // each returns whether it moved
    pub fn move_right(&mut self) -> bool;
    pub fn move_home(&mut self);
    pub fn move_end(&mut self);
    pub fn clear(&mut self);
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn set_line(&mut self, s: &str);   // history recall; truncates at capacity
}

/// ELF64 loader front-end (`kshared::elf`): pure parsing and validation, no
/// hardware, no allocator, host-tested. Refusal is the default — an image is
/// loadable only if every check passes (W+X, out-of-window, unaligned,
/// overlapping, oversize and entry-not-executable images are all `Err` before
/// anything maps). The kernel maps and copies what a `LoadPlan` describes.
pub mod elf {
    pub const USER_IMAGE_BASE: u64 = 0x40_0000;
    pub const USER_IMAGE_END: u64 = 0x60_0000;
    pub const MAX_SEGMENTS: usize = 8;
    pub const MAX_TOTAL_PAGES: u64 = 64;   // caps frames one load can consume
    pub struct Segment { /* vaddr, file range, memsz, writable, executable */ }
    pub struct LoadPlan { /* segments, entry */ }
    pub enum ElfError { /* one named variant per failed check */ }
    pub fn parse_elf64(image: &[u8]) -> Result<LoadPlan, ElfError>;
}

/// Command parsing: splits a submitted line into a verb and the remainder,
/// both trimmed. A blank line is `None`, which the shell treats as a no-op.
/// There are no quoting rules and no error type, because nothing a person can
/// type here is a parse failure — an unknown verb is the shell's business to
/// report, not the parser's.
pub fn parse_command(line: &str) -> Option<(&str, &str)>;
```

**`kernel`** — the module surface, in dependency order.

```rust
mod serial;      // COM1 at 0x3F8 behind a plain Mutex, plus the panic path's force-unlock
mod framebuffer; // Display: safe slice writes keyed off FrameBufferInfo
mod console;     // Console: glyph rendering, wrapping, scrolling, caller-set colour
mod cpu;         // init(): CPUID-gated SMEP/SMAP/UMIP in CR4, read back and surfaced
mod logger;      // log::Log impl fanning out to console and serial, honouring the lock rule
mod gdt;         // init(): GDT (kernel + user segments), TSS, three IST stacks, RSP0
mod interrupts;  // init(): IDT (incl. the DPL-3 int 0x80 gate), PIC remap to 32..47,
                 //   timer at 100 Hz, keyboard IRQ
mod memory;      // frames (bump allocator), paging (OffsetPageTable), heap (1 MiB, NX),
                 //   per-task AddressSpace (M9: clone + private user slots, per-space
                 //   map/update/copy), the kernel-table audit
mod time;        // TSC boot-phase marks; battery-only PIT calibration to microseconds
mod task;        // executor (cooperative), keyboard (ScancodeStream)
mod sched;       // M8: preemptive round-robin of ring-3 tasks — TCBs, per-task
                 //   kernel stacks, the naked timer entry, context switch, SYS_EXIT
mod usermode;    // ring 3: run_programs over kshared::elf, int 0x80 entry, teardown
mod shell;       // prompt, dispatch, command implementations
mod selftest;    // #[cfg(feature = "selftest")] battery; arms EXPECTING_DOUBLE_FAULT last
mod qemu;        // exit_success() / exit_failure() via port 0xf4
```

**`xtask`** — already implemented at M0; the contract is fixed.

```
cargo xtask build [--selftest]                 # both images, size budget asserted
cargo xtask run   [--bios|--uefi] [--mem=MB]   # interactive QEMU
cargo xtask test  [--bios|--uefi] [--mem=MB]   # headless; asserts exit 33 AND serial grep
cargo xtask test --shipped                     # boots the real image; asserts it logs
                                               #   "boot complete; shell ready"
cargo xtask test --image=<path>                # boots that exact file instead of building;
                                               #   the release workflow proves the very
                                               #   bytes it uploads
cargo xtask privacy                            # boots the shipped image, types a sentinel
                                               #   through the QEMU monitor, drives
                                               #   `shutdown` from the keyboard, and asserts
                                               #   post-boot serial is EMPTY (allowlist, not
                                               #   sentinel blocklist)
```

`xtask test` (without `--shipped`) forces the `selftest` feature on, streams serial
output while capturing it, kills QEMU after a 120-second timeout and reports a
timeout as a hang rather than a failure, and rejects any exit code that is neither
33 nor 35. `--mem` defaults to the pinned per-firmware values (24 MiB BIOS,
48 MiB UEFI — the measured floors of 21 and 46 plus a recorded headroom; the xtask
constants' comment is the authoritative record).

### Self-test battery protocol

The battery is a straight sequence of checks in `selftest::run`. Each one prints a
single `[selftest] <area>: <what it proved> ... ok` line to serial as it passes, so
the log names the last check that got through and a hang sits between that line and
the next check in `run`. A failed assertion panics, and the panic handler does the
rest: it prints the panic to serial and to the console, then `SELFTEST FAILED`, then
exits 35.

The order in `run` is the contract: boot reached `kernel_main`; console renders and
the cursor advances; a drawn glyph's pixels reach the framebuffer in the negotiated
channel order (a readback probe — cursor bookkeeping alone would pass with the pixel
write deleted); `int3` handled and execution resumed; every installed IDT exception
vector read back present; PIT ticks advance; heap `Box` + 50k-element `Vec`;
zero-on-free sentinel absent (the scan starts past the allocator's 16-byte Hole
node); a fresh, deliberately pre-soiled page maps zeroed and writable; the
page-table audit — no mapping is user-accessible before ring 3 has ever run; every
CPU-advertised supervisor-hardening bit (SMEP/SMAP/UMIP) is live in CR4; the kernel
heap is NX; an oversized allocation is refused with the allocator intact; an async
task completes through its waker; a scripted shell session via injected scancodes
renders `help`'s full output; a crafted W+X ELF image is refused; the embedded
`hello` ELF runs at CPL 3 and returns via syscall; the M8 preemption proof —
`counter` (an unyielding 30M-iteration compute loop holding `EFLAGS.AC` hostile)
launched first and `hello` second, asserting hello exits first, ≥1 preemptive
switch and ≥2 ring-3 timer round-trips were taken, counter's exit checksum equals
the kernel's independent recomputation (register integrity across every switch),
and the timer entry's AC scrub was never seen defeated; the sustained-rotation
proof — the same counter linked at two bases and scheduled against itself, so
both tasks outlive many quanta: ≥4 preemptive switches must be counted (the
counter+hello scenario inherently sees exactly one switch, which a
rotate-once scheduler satisfied) and both checksums must be exact across
dozens of descheduled-and-resumed cycles — exit order is deliberately NOT
asserted there: the head start is one partial quantum against non-identical
per-task costs, so it is a coin flip, and an earlier form of the test that
asserted it flaked at ~50%; the M9 isolation proof — two instances of ONE
image at the SAME virtual addresses both run at CPL 3 and each reports the
pristine `'E'` from its private data segment (a shared page would show the
first instance's write to the second); the M10 fault-isolation proof —
`crasher` page-faults beside `hello` and is terminated alone (vector 14 in
its exit record, the neighbour pristine, the kernel's CR3 restored), the
pair run repeated up to 5× until the kill path is seen RESUMING the
survivor rather than only returning to the launcher, and then the crasher
run alone, which forces that launcher-return branch deterministically; the
kernel-table audit again, AFTER
every ring-3 scenario, in its M9 total form — not one user-accessible entry,
leaf or intermediate, ever; `update_user_page` narrows
W and NX correctly (the W^X plumbing, now exercised in a scratch address
space); the console scroll cost is measured; memory stats are reported; and, last
and unreturning, the deliberate stack overflow.

**The final check is the deliberate stack overflow, and it cannot return.** The
double-fault handler ends it, which means the handler — not `run` — prints the
battery's `SELFTEST PASSED` and exits 33. That creates a trap the plan did not
account for: a handler that printed `SELFTEST PASSED` unconditionally would report an
*accidental* double fault in any earlier check as success, and the CI gate would
silently invert. The handler therefore takes its verdict from a flag the battery
arms: `EXPECTING_DOUBLE_FAULT`, an `AtomicBool` compiled in only under the `selftest`
feature, stored `true` immediately before the recursion that overflows the stack, and
never cleared.

- Flag set → the expected outcome. Disable interrupts, reclaim the serial lock from
  a holder that can never resume, print the resilience line and `SELFTEST PASSED`,
  exit 33.
- Flag clear → fall through to `panic!("double fault ...")` with the interrupt stack
  frame, which reaches the panic handler above and exits 35.

A boolean rather than a phase counter because the window it guards is one statement
wide: nothing except the overflow check ever sets it, and nothing runs after that
check. A counter would carry the same information about this trap and one more state
to keep in step with the code. In a non-selftest build the flag does not exist at
all, the handler always panics, and the panic path renders the panic screen and halts.

## Access control

There is no RLS, no definer function and no `anon` role; there is one privilege
level and whoever holds the keyboard holds it. The analogous artefact — the list of
places where the compiler's guarantees are suspended, and the invariant that must
hold at each — is the unsafe inventory. **Every `unsafe` block and every `unsafe
impl` in the kernel belongs to one of these categories and carries a `// SAFETY:`
comment naming the invariant. Anything unsafe that does not fit a category is a
design change and needs this document updated first.** The comment half of that rule
is machine-checked rather than reviewed: `main.rs` carries
`#![warn(clippy::undocumented_unsafe_blocks)]`, CI runs clippy with `-D warnings`,
and the lint covers `unsafe impl` items as well as blocks. Two rows below describe
categories that hold no `unsafe` at all any more; they are kept because the reason
each is empty is the design.

| # | Category | Where | Invariant that makes it sound |
|---|---|---|---|
| 1 | **Port I/O** | `serial`, `interrupts` (PIC/PIT/keyboard), `qemu` | The port number is a compile-time constant naming a device this kernel owns exclusively. No other code writes that port. Widths match the device (`0x3F8` byte, `0xF4` doubleword, `0x60` byte). Reads have no side effects beyond the documented device behaviour. Notably, port `0x60` **must** be read in the keyboard handler or the controller stops delivering interrupts. |
| 2 | **Page-table manipulation** | `memory`, `memory::heap` | The `OffsetPageTable` is constructed once from `boot_info.physical_memory_offset`, which the caller has already checked is `Some`. The complete physical memory is mapped at that offset, guaranteed by `Mapping::Dynamic` in the bootloader config and re-checked, not assumed. A frame handed to `map_to` came from the frame allocator and is therefore unaliased. The TLB is flushed before the new mapping is read. |
| 3 | **Descriptor-table loading** | `gdt`, `interrupts::init` | The GDT, TSS and IDT are `'static` and are never mutated after being loaded. Segment selectors written to `CS` and `SS` index entries that exist in the GDT that was just loaded. Each IST pointer (double fault, NMI, machine check) and the TSS RSP0 privilege stack is the top of a dedicated 20 KiB static array that nothing else uses. One honest limit, stated rather than implied away: these stacks have **no guard pages** — an overflow there does not fault, it silently corrupts adjacent `.bss`. The battery proves the *kernel* stack's overflow is caught; nothing yet proves the same for the IST/RSP0 stacks. |
| 4 | **Framebuffer writes** — *empty: `framebuffer` contains no `unsafe` at all* | `framebuffer` | The bootloader hands the framebuffer over as a `&'static mut [u8]`, so there is no pointer to form and nothing to suspend: `Display::set_pixel` rejects out-of-range coordinates, then indexes through `get_mut`, and `scroll_region_up` moves rows with `copy_within`. `stride`, `bytes_per_pixel` and `pixel_format` are read from the negotiated `FrameBufferInfo`, never hard-coded, because UEFI GOP and BIOS VBE differ and both are boot-tested for exactly that reason. This is stronger than the raw-pointer-with-a-bounds-check design this row used to describe: there, the check is a promise a reviewer has to re-verify after every edit; here a wrong stride or depth drops a pixel and cannot reach memory outside the buffer, because the compiler will not let it. |
| 5 | **`hlt` and interrupt-flag control** | idle loop, `panic`, critical sections | `hlt` touches no memory (`options(nomem, nostack, preserves_flags)`). The idle path uses the enable-then-halt sequence so that a wake-up racing the halt is not lost. Interrupts are disabled only around a section that provably cannot block. |
| 6 | **Mutable statics** — *also empty of `unsafe` blocks, by construction* | `gdt` | There is no `static mut` in the kernel, so no `&mut` to one is ever formed. The interrupt and privilege stacks — three ISTs and TSS RSP0 — are byte arrays inside `UnsafeCell`s, which is what keeps them writable and in `.bss`; taking an address is a safe call, and the memory itself is written only by the CPU: the ISTs while handling the fault each slot is wired to, RSP0 on a privilege transition into the kernel. Everything else that changes is an atomic or sits behind a `spin::Mutex`. The `unsafe impl Sync` pair this arrangement needs is row 10. |
| 7 | **Zeroing a block being freed** | `memory::heap` allocator wrapper | The pointer and layout are those the caller passed to `dealloc`, so the block is live, owned by the allocator at that instant, and exactly `layout.size()` bytes. Zeroing happens **before** the inner `dealloc`; afterwards the allocator owns those bytes and writing them would corrupt the free list. |
| 8 | **Panic-time lock recovery**, serial and console | `panic` handler, double-fault handler | A panic may happen while either lock is held, including from interrupt context, which is the one case the concurrency rule cannot prevent. The order is always the same: interrupts off, reclaim, then report. The panic handler reclaims both because it writes to both; the double-fault handler's selftest exit reclaims serial only, because serial is the only sink it uses. Sound only because the machine is stopping — the handler halts or exits and never returns, no other code will observe either sink again, and a legible panic is worth more than a lock invariant with no future reader. **These two handlers are the only places any lock is force-released.** It is never a pattern to copy. |
| 9 | **Test reads of freed memory** | `selftest` (`#[cfg(feature = "selftest")]`), and the shell's `selftest` command, which ships a runtime twin of the zero-on-free check | Reads are volatile, through a raw pointer, within a block whose size is known, and the value is used solely for an assertion. The battery copy is not compiled into a shipped image; the shell's twin is, under exactly the same discipline. Prefer the sentinel-absence test, which needs no such read; if a direct read is used, it is confined to this row. |
| 10 | **`unsafe impl`** — a promise about a whole type, not one expression | `memory::frames`, `memory::heap`, `gdt` | Four of them, each carrying its `// SAFETY:` comment on the impl itself, since there is no block to attach one to. **`FrameAllocator for BootFrameAllocator`** promises a frame is never handed out twice: `next` only ever grows, and it indexes a deterministic iterator over a boot memory map that does not change after hand-over, so frame *n* is returned exactly once. **`GlobalAlloc for ZeroOnFree`** adds nothing to `LockedHeap`'s contract and defers to it for both calls; the only extra act is scrubbing a block the caller has already relinquished, in the window between the caller's last legal access and the inner `dealloc` — after that `dealloc` the allocator owns those bytes, which is why the order is fixed (row 7). **`Sync for IstStack`** covers the three IST `UnsafeCell`s of row 6: nothing in the kernel reads or writes that memory, so there is no cross-thread access to make sound; only the CPU touches it, and only while handling the fault the slot is wired to. **`Sync for PrivilegeStack`** is the same promise for the TSS RSP0 stack: only the CPU writes it, on a privilege transition into the kernel. |
| 11 | **Naked ring transitions and context switches** | `sched::enter_tasks`, `sched::timer_entry`, `usermode::int80_entry` | `enter_tasks` pushes the SysV callee-saved registers, saves RSP into `KERNEL_CONTINUATION_RSP` *before* any ring-3 instruction can execute, and restores the first task's fabricated context (15 zeroed GP registers — ring 3 sees no kernel state — then `iretq` with the ring-3 selectors). `timer_entry` scrubs `EFLAGS.AC` and runs `cld` before anything else (a gate clears IF but neither AC nor DF), saves the 15 GP registers in the one canonical order, and restores whichever saved context `timer_tick` returns — the saved-context layout is a single contract shared by the fabricator, both entries and both restore tails. `int80_entry` scrubs AC and DF the same way; `SYS_EXIT` hands the code to `sched::sys_exit` and either resumes the next task's saved context or restores the launcher continuation; every other syscall marshals into the SysV ABI, aligns the stack, calls `syscall_dispatch`, scrubs the caller-saved registers (keeping `rax`, the return value) and `iretq`s back. |
| 12 | **User-image copy** | `usermode::run_programs` | The `copy_nonoverlapping` writes to pages just mapped writable that cover `memsz` bytes; the parser bounds-checked `file_start..+filesz` against the image before anything was mapped. The `memsz` tail past `filesz` is BSS, and frames arrive zeroed, so it is already correct. |
| 13 | **DPL-3 and raw gate installs** | `interrupts` (IDT construction) | The `int 0x80` handler address is a valid naked entry that ends in `iretq` (or, on the last `SYS_EXIT`, restores the saved kernel stack and returns); DPL 3 lets a ring-3 program issue `int 0x80` without a #GP — and only that vector: the timer's naked entry (vector 32, also installed by address, since the typed x86-interrupt convention cannot swap a full register file) stays DPL 0, reachable only by PIC delivery. |
| 14 | **`rdtsc`** | `time` | Reads the timestamp counter; no memory effects. |
| 15 | **EFER.NXE** | `memory::init` | The CPU is already in long mode (EFER.LME set by the bootloader); the update only adds NXE, which every x86_64 CPU supports, and it runs before any NX mapping is created so the bit is honoured everywhere it is set. |
| 16 | **TSS RSP0 mutation** (M8) | `gdt::set_privilege_stack`, `gdt::init` | The TSS lives in an `UnsafeCell` and its GDT descriptor is built from a raw pointer, so no `&'static` shared reference exists to alias the write. Every write happens with interrupts disabled at CPL 0 (asserted), and the CPU reads RSP0 only on a ring-3 → ring-0 transition, which cannot occur in that window — so the CPU never observes a torn or stale value. The accompanying `unsafe impl Sync for TssCell` carries the same argument. |
| 17 | **Saved-context reads and fabrication** (M8) | `sched::timer_tick` (frame CS read), `sched::build_initial_frame` | `timer_tick`'s one raw read is the saved CS at a fixed, documented offset inside the context `timer_entry` pushed moments earlier on the current stack — in-bounds by the layout contract the same module defines. `build_initial_frame` writes only within the task's own boxed kernel stack through slice indexing (no raw pointers), and the layout it fabricates is the same contract. |
| 18 | **Address-space construction and CR3 loads** (M9) | `memory::AddressSpace` (`new_user`, `with_mapper`, `copy_into_user_page`), `sched::load_cr3` | `new_user` reads and writes whole page-table frames only through the physical alias: sources are the live kernel tables (read-only), destinations are zeroed frames the allocator just handed out exclusively, and the user PD slots are asserted vacant kernel-side before being privatised. `with_mapper` forms the one `&mut PageTable` per space through `&mut self`, so the borrow is exclusive by construction. `Cr3::write` is confined to `load_cr3`, whose contract is that every root passed shares the kernel half — the kernel keeps executing across the load — and every call site runs with IF=0, in step with the RSP0 write beside it. |

## Migrations

There is no database, so the section that carries the same risk — a change that must
be sequenced correctly or it cannot be undone cheaply — is the milestone plan. Each
milestone is additive over the last, each ends at a gate, and **the gate is a green
CI run, not a local success**. Nothing starts before the previous gate is green.
Every green milestone is committed and pushed.

| # | Does | Reversible? | Rollback |
|---|---|---|---|
| **M0** | Workspace, pinned toolchain, halt-loop kernel, `xtask` producing both images, CI for fmt/clippy/host-tests/image-build, these four documents. **Committed, pushed, CI green — as are all later milestones.** | Yes | Delete the tree; nothing is published. |
| **M1** | Framebuffer pixel writer, glyph console with scrolling, serial, `log` fan-out, panic screen to both sinks. Under `--features selftest`, print `SELFTEST PASSED` and exit 33 so CI has a boot gate from the first milestone. Add the `boot-test` matrix job. | Yes | `git revert` the range; M0 still boots. |
| **M2** | GDT, TSS, IST0 (20 KiB), IDT with breakpoint, invalid-opcode, general-protection and page-fault handlers (page fault reports CR2) and double fault. PIC remapped to 32..47, PIT at 100 Hz, keyboard IRQ reading port `0x60` — with nowhere yet to put the byte, so it only counts it. Self-tests: `int3` returns, ticks advance. | Yes | `git revert`; M1's console still boots and still passes its battery. |
| **M3** | Frame allocator over the boot memory map (frame arithmetic in `kshared`, host-tested), `OffsetPageTable`, 1 MiB heap so `alloc` is live, zeroing allocator wrapper, frames zeroed on hand-out. Self-tests: `Box`, a 50k-element `Vec`, reuse after free, fresh page reads zero, sentinel absent after free. | Yes | `git revert`; M2 is heap-free and unaffected. |
| **M4** | Cooperative executor over `alloc::task::Wake` with a `crossbeam_queue::ArrayQueue` ready queue (valid here: the heap exists from M3), enable-then-halt idle, and the keyboard IRQ's real destination — a second `ArrayQueue` behind `ScancodeStream`, drained by the task side and decoded with `pc-keyboard` 0.9's `ScancodeSet1::advance_state` and `EventDecoder::process_keyevent`. Self-test: a spawned task yields, wakes itself and completes. | Yes | `git revert`; M3's synchronous battery still passes. |
| **M5** | The line editor from `kshared` (history is the shell's, because history needs the heap), the full command surface, boot-to-shell time in the banner from the PIT tick count, the complete battery ending with the stack-overflow check, the RAM floor measured and pinned per firmware, the size budget tightened to the measured value, the shipped-image boot job, README screenshot. | Yes | `git revert` to the M4 tag; every earlier gate still holds. |
| **M6** | Ring 3 and the `int 0x80` system-call path: user GDT segments, TSS RSP0 privilege stack, the DPL-3 gate, `jump_to_user`/`int80_entry` with register scrubbing both ways, and the page-table audit. Self-tests: the user program runs at CPL 3; no mapping is user-accessible. | Yes | `git revert`; M5's single-ring kernel still boots and passes its battery. |
| **M7** | ELF loading: `user/hello`, a linker-scripted Rust ELF built by the kernel's build script and embedded as bytes; `kshared::elf`, a host-tested refusal-first parser; per-segment W^X via `update_user_page`; the kernel heap made NX. Self-tests: a W+X image refused, the audit re-run after teardown, W^X flag plumbing. | Yes | `git revert`; M6 still proves the ring transition. |
| **M8** | Preemptive multitasking of ring-3 tasks: `sched` (TCBs, per-task heap-allocated kernel stacks, the naked timer entry with its AC scrub, round-robin switch, `SYS_EXIT` routing), TSS RSP0 made mutable and retargeted per switch, the multi-program loader with cross-image overlap refusal, `user/counter` (an unyielding register-heavy checksum program that holds AC hostile), and the `sched` shell command. Self-tests: exit-order preemption proof, exact checksum across every switch, timer-entry AC scrub, overlap refusal, the existing audits now covering the concurrent run. | Yes | `git revert`; M7's single-program cooperative kernel still boots and passes its battery. |
| **M9** | Per-task address spaces: `memory::AddressSpace` (kernel PML4 cloned, entry-0 chain deep-copied against the MEASURED bootloader low mappings, user PD slots private), CR3 switched with RSP0 at every context switch and restored when the last task exits, hello's exit code widened to carry its data-segment read (the isolation witness), the M8 cross-image overlap refusal REMOVED (same-VA programs are now the point — `plans_overlap` and its host tests deleted with it), and the audit strengthened to its total form: the kernel table never carries any user-accessible entry. Self-tests: two instances of one image at one VA both run and both read pristine data; the W^X probe moved into a scratch space. | Yes | `git revert`; M8's shared-address-space scheduler still boots and passes its battery (its overlap refusal returns with it). |
| **M10** | Fault isolation: every ring-3-reachable exception handler forks on the faulting CPL — ring 3 calls `sched::kill_current` (AC scrubbed, fault vector recorded in the exit report, RSP0+CR3 switched to the next ready task or the kernel's own world restored for the launcher return, via naked never-returning helpers); CPL 0 still panics, and NMI/#MC/#DF stay panic-only at any CPL. `user/crasher` (announces itself, then dereferences an unmapped address), the `crash` shell command, and `xtask privacy` typing `crash`. Self-tests: crasher terminated alone beside a surviving hello (repeated until the kill path is seen resuming the survivor), and a fault in the last task returning cleanly to the launcher. | Yes | `git revert`; M9's fault-fatal kernel still boots and passes its battery. |

**How the sequencing worked out:** the keyboard interrupt lands at M2 and must read
port `0x60` — the controller delivers no further interrupts until it is read — but
the heap does not exist until M3, and `crossbeam_queue::ArrayQueue::new` allocates.
The M2 handler therefore reads the byte, counts it in an atomic and drops it; there
is no shell to type at yet, so nothing is lost that anybody meant to send. The real
destination arrives at M4 once the heap is live: an `ArrayQueue<u8>` of 128 entries,
constructed once inside a `Once` on the first `ScancodeStream::new()` — that is, in
task context, before the shell reads its first key. The interrupt path is
allocation-free permanently, which is the property that mattered; it is bought by
allocating the queue exactly once on the task side rather than by keeping the queue
static, and the handler still allocates nothing and blocks on nothing.

## Failure modes

| What breaks | Who notices | How we detect it | How we undo it |
|---|---|---|---|
| **Triple fault** — a fault while handling a fault while handling a fault; the CPU resets. Most likely cause is a broken IDT or a double fault without a valid IST stack. | CI, as a boot that never produces a verdict | QEMU runs with `-no-reboot`, so it exits instead of looping; `xtask` sees an exit code that is neither 33 nor 35 and fails with that code. Without `-no-reboot` this presents as a hang. | `git revert` the commit touching `gdt` or `interrupts`. The M2 gate exists precisely to catch this in one milestone's worth of diff. |
| **Boot hang** — no faults, no progress: a spin lock taken twice, a loop with no exit, firmware that never hands over. | CI, as a job that stops producing output | `xtask` kills QEMU after 120 s and reports a timeout distinctly from a failure; the job also has its own timeout. The serial log is uploaded with `if: always()`, so the last check to pass is visible. | `git revert`. Each battery check prints its own `[selftest]` line as it passes, so the hang is between the last line in the log and the next check in `selftest::run` — one function, not one milestone. |
| **Interrupt-context deadlock** — a handler blocks on a lock the interrupted code holds. Classically: logging to the console from the timer handler. | Nobody, until the machine stops responding | Prevented structurally, not detected: the concurrency rule above, plus code review against the global-state table's "touched from IRQ" column. Handlers take one lock, the PIC's, and the main thread never holds it once interrupts are enabled. | Not applicable if the rule holds. If one is found, the fix is to move the offending access out of interrupt context, never to make the lock reentrant. |
| **Heap exhaustion** — the 1 MiB heap fills. | The person at the keyboard | There is no custom `alloc_error_handler`; Rust's default one panics, naming the size of the allocation that failed. That is a panic like any other here: interrupts off, the message on serial and on the panic screen, halted — and in a selftest build, exit 35. Not a silent hang and not a corrupt allocation. It does not report the heap's used and free counts, and it does not need to: `mem` reports those on demand, before anything has failed. | Reduce the allocation, or raise `HEAP_SIZE` deliberately and re-measure the RAM floor in the same commit. |
| **Insufficient physical memory at boot** — the machine has less RAM than the boot needs. | CI at the pinned floor; a person on a small machine | Measured at 0.25 MB granularity around the floor: the **bootloader** runs out first, panicking with `FrameAllocationFailed` while mapping the kernel, and the kernel never runs — there is no observed window where the kernel-side `mapping the kernel heap failed` panic fires instead (that path exists but is shadowed by the earlier failure). The bootloader's panic reaches serial; `xtask` recognises it in the captured log and reports a bootloader OOM rather than calling the timeout a kernel hang. | Boot with more memory, or lower `HEAP_SIZE`. The pinned CI RAM value — measured floor plus recorded headroom, per firmware — is the regression test for this. |
| **No framebuffer from firmware** | A person seeing a blank screen | `boot_info.framebuffer` is `None`. Handled, not a fault: the console stays absent, the kernel logs the fallback over serial, and boot and the battery still run and pass. The shell renders to the console only, so this configuration is boot-proven, not interactive. | None needed; this is a supported degraded configuration. |
| **Firmware-specific pixel format assumption** — code that works on BIOS VBE and corrupts under UEFI GOP, or vice versa. | Whoever boots the other firmware | The renderer reads `pixel_format`, `stride` and `bytes_per_pixel` from the negotiated `FrameBufferInfo`, and **both firmwares are boot-tested in the CI matrix**. A hard-coded assumption fails one leg of the matrix. | `git revert`; the matrix names which firmware broke. |
| **A ring-3 program faults** — page fault, #GP, #UD or any other ring-3-reachable exception. | The person at the keyboard, from the `crash`-style report; the battery, from the exit records | **Contained since M10, not a machine failure:** the handler forks on the faulting CPL and `sched::kill_current` terminates only the offender, recording the vector in its exit report — the neighbour and the kernel keep running, which the battery proves every run. A KERNEL-context fault still panics (a kernel bug is not a schedulable event), and NMI/#MC/#DF panic at any CPL — machine-level events, not the current task's doing. | Nothing to undo — this is the designed behaviour. A ring-3 fault that DOES take the machine down is an M10 regression: `git revert` the commit touching `interrupts` or `sched`. |
| **Scancode queue overflow** — typing faster than the executor drains. | Nobody, in practice | The handler drops the byte it has just read — the newest — and nothing counts the loss, so there is no signal to watch. That is acceptable only because the case is unreachable in interactive use: the queue holds 128 scancodes, and the executor drains it on every wake, so filling it would need something on the order of a hundred keystrokes between two polls of a task that is woken by each one. Lossy by design, and never blocking, is the property that matters in an interrupt handler. | Not a fault. If it ever does happen the visible symptom is a swallowed keystroke, and the bug to fix is whatever is keeping the executor from being scheduled — not the queue. |
| **Pinned nightly stops resolving or changes behaviour** | The first CI run after the pin moves | Every job fails at toolchain install or at build. | The pin is bumped only in a dedicated pull request that changes nothing else, so reverting that one commit restores a known-good toolchain. |
| **Bootloader build stages fail to fetch** | First build on a cold cache | `bootloader` is pinned with `=0.11.17`, `Cargo.lock` is committed, and the CI cache is keyed on it. A fetch failure fails the `image-build` job loudly. | Re-run; if persistent, the pin is the thing to investigate, not the kernel. |
| **A self-test passes vacuously** — asserts a property it does not exercise. | Nobody, which is what makes it the worst entry in this table | Each self-test must be **observed failing once** under a deliberate mutation before it counts as a test: break the zeroing wrapper, break the frame zeroing, remove the IST assignment, stop reading port `0x60`. Both outputs — the failing run and the passing run — are recorded. | Rewrite the test. A test that cannot be made to fail is deleted, not kept for reassurance. |

## Deferred, with measurement — write-only console scroll

`console::scroll_region_up` scrolls by `copy_within` on the live framebuffer,
which **reads** framebuffer memory. On a real, write-combining or uncached
framebuffer that read is slow; under QEMU's software renderer it is invisible.
Rather than guess, the battery measures it (`[selftest] perf:` line, archived in
CI). The rework — a small character-cell shadow grid scrolled in RAM and
repainted forward-only, so the framebuffer is never read — is deferred until
either the measured number or a real-hardware boot shows it matters. Recording
the decision here, with the number, is the point; a silent rewrite for an
unmeasured cost is exactly what this repo's culture rejects.

## Rollback

**The undo is `git revert` of the offending commit or milestone range, and the boot
gate is what makes that safe.** Because CI boots both firmwares on every push to
`main`, any commit on `main` is a known-bootable state; reverting to one restores a
system that is known to start, not one that is merely believed to. There is nothing
deployed, nothing migrated and nothing persisted, so a revert is complete by
construction: no data has to be reconciled, because no data exists.

Time to undo: one `git revert` plus one CI run, under fifteen minutes on a warm
cache, and the person doing it at 2am needs to know nothing beyond which milestone
introduced the fault, which the serial log's `[selftest]` lines tell them.

Two rollbacks are cheap but need naming, because they are the ones that will
actually be reached for:

- **A toolchain pin bump** is always its own commit that changes nothing else, so
  reverting it cannot take working kernel code with it.
- **A dependency addition** is likewise its own commit, so reverting it cannot take
  working kernel code with it either — and if the crate trips the
  `no-network-no-storage` gate, that has to be faced in the same commit rather than
  discovered in someone else's.

Nothing in v1 is irreversible. The only genuinely destructive act available in this
project is repository deletion, which is out of scope for any automated process.

## Test plan

The tests that would fail without this design, by layer. **A test that has never
been observed failing does not count**. Every entry below names the mutation that
must be seen to break it.

**Host tests — `cargo test -p kshared`** (fast, run on every push)

- *Positive:* `align_up`/`align_down` round correctly at, above and below a page
  boundary (present at M0). `frame_starts` yields every whole frame of a range and
  clips the partial ones at both edges. The editor echoes, accumulates, backspaces
  and submits without storing the newline; `set_line` replaces the line for history
  recall. `parse_command` splits a verb from its arguments and trims both.
- *Negative:* the editor ignores everything it cannot hold or render — a character
  past `LINE_CAP`, a control character, a non-ASCII one — reporting `Ignored`
  instead of writing anywhere, and backspace on an empty line is `Ignored` rather
  than an underflow. `parse_command` returns `None` on a blank line instead of an
  empty verb. Neither has an error type, because neither has a way to fail: an
  unknown verb is the shell's to report.
- *Boundary:* a reversed range, an empty one, and one too small to hold a single
  aligned frame all yield nothing; a range that is already frame-aligned is not
  truncated; a line of exactly `LINE_CAP` characters is accepted and the next
  character refused; `set_line` truncates at capacity rather than running past it.
- *Contract:* one test rebuilds the line buffer from the returned `EditAction`s
  alone and compares it with `line()` after every single keystroke — through a
  typed-and-corrected line, over the capacity ceiling and back, then a
  20,000-keystroke storm mixing printable, control, edit and non-ASCII characters.
  That is what makes "the renderer cannot drift from the buffer" a checked property
  rather than a convention, and it is why `feed` returns an action at all.
- *Mutation:* invert the rounding direction in `align_up`; clip one frame too many
  in `frame_starts`; return `Echoed` for a character the editor did not store. Each
  must turn the suite red.

**In-kernel battery — `cargo xtask test --bios` and `--uefi`, at the pinned floor**

- *Positive, per milestone:* the banner reaches serial (M1); `int3` returns to the
  next instruction and the timer's tick count advances (M2); a `Box` allocates, a
  50,000-element `Vec` grows and frees, and a freed block is reused (M3); a spawned
  task yields, wakes itself through the ready queue and completes, setting a flag the
  battery reads (M4); the full battery completes and every line it logged is a check
  that ran (M5); the embedded `hello` ELF runs at CPL 3 and returns via syscall, a
  crafted W+X image is refused before anything maps, and the page-table audit holds
  both before ring 3 has ever run and again after its teardown (M6/M7).
- *Negative — the privacy claims, which are the tests that justify the project:*
  a sentinel word written throughout a heap block appears **nowhere** in the
  allocation handed back after that block is freed; a freshly mapped page reads as
  all zero before anything writes to it.
- *Negative — the survivability claim:* a deliberate kernel stack overflow raises a
  double fault, the handler runs on IST0, and the verdict it prints depends on
  `EXPECTING_DOUBLE_FAULT`, which only the overflow check arms — so an accidental
  double fault anywhere earlier panics, prints `SELFTEST FAILED` and exits 35.
- *Boundary:* the battery passes at the pinned minimal RAM, not merely at a
  comfortable default. Both firmwares pass. The default, non-selftest image reaches
  the interactive prompt; the shipped artefact is boot-proven, not just its test
  sibling. Both images are under the size budget.
- *Mutation, each observed failing once:* remove the zeroing wrapper (the sentinel
  test must fail); stop zeroing frames on hand-out (the fresh-page test must fail);
  remove the IST0 assignment (the stack-overflow check must triple-fault into a
  non-33 exit rather than passing); stop reading port `0x60` in the keyboard handler
  (input must stop after one keystroke); hard-code a pixel format (one firmware leg
  of the matrix must fail); launch the user program in ring 0 (the CPL assertion
  must fail); map its `.data` read-only (the program's volatile write-back must
  fault); leave a USER probe leaf or a surviving stack mapping (the audit must
  fail); rewrite `update_user_page` wholesale (the NX-preservation check must fail).
- *Mutation, M8 (all three observed failing 2026-08-18):* pin the timer's pick to
  the current task (`next = cur` — scheduling degrades to cooperative; the
  exit-order assertion fails with "scheduling is not preemptive"); delete the
  timer entry's `pushfq/and/popfq` AC scrub (the `TIMER_ENTRY_AC` assertion
  fails); zero the preempted task's saved `rbx` at the moment of a switch (the
  checksum-equality assertion fails with "a context switch corrupted its
  registers" — one register, one switch, caught).
- *Mutation, M8 adversarial-review round (all three observed failing
  2026-08-18):* preempt once then pin forever (the two-counter switch floor
  fails with "rotation is not sustained" at 1 switch — the mutation the
  original battery could not see); point `SAVED_CS_OFFSET` at the saved SS
  (the CS-selector tripwire panics on the first tick, `saved CS 0x10`);
  degrade `plans_overlap` to `ra.start == rb.start` (the host partial-overlap
  test fails — the battery's identical-images case alone could not see it;
  the predicate and its tests were later deleted at M9, which made cross-image
  overlap meaningless).
- *Mutation, M9 (all observed failing 2026-08-18):* delete the scheduler's
  switch-time CR3 loads (the first cross-layout switch resumes a task in the
  wrong space and page-faults — the battery reddens at the first concurrent
  scenario); delete only the `SYS_EXIT`-path CR3 load and run the same-VA
  scenario directly (the isolation assertion catches instance 1 reading
  `0x46` — the first instance's write — "the instances share memory"); map
  both images into ONE space (`PageAlreadyMapped` — same-VA coexistence is
  impossible in a single table, which is the capability M9 added). The
  targeted second observation used a reduced battery order (counter scenarios
  skipped) because any cross-layout switch faults before the same-layout case
  is reached — recorded here so the reduction is a fact, not a trick.
- *Mutation, M9 adversarial-review round (both observed failing 2026-08-18):*
  delete the terminal kernel-CR3 restore in `sys_exit` (the new CR3 read-back
  assertion fails with "did not restore the kernel's CR3" — before the review
  round, nothing in the battery could see this deletion); map the user stack
  executable (the new per-space W^X audit in `run_programs` fails with
  "writable+executable user page" — the synthetic probe alone never checked a
  REAL loaded space). The same round also added the overflow-checked
  `in_user_region` guard to BOTH user-page primitives (`map_user_page` had no
  bounds check at all, and a wild VA would have stamped USER bits into
  page-table frames shared with the kernel; `copy_into_user_page`'s check was
  wrap-around-bypassable at the top of the address space), made the same-VA
  isolation witness run three rounds (under a sharing regression, a timer
  tick in the sub-microsecond read-to-write window could hide the bug with
  ~1e-4 probability per round; correct code is deterministic, so the
  repetition adds no flake risk), and widened the documented no-new-mappings
  invariant (the deep-copied entry-0 chain makes ANY new kernel mapping under
  PML4[0] unshared, not just new PML4-level entries).
- *Mutation, M10 (all observed failing 2026-08-18):* remove the page-fault
  handler's ring-3 fork (the crasher's fault panics the whole kernel — the
  pre-M10 behaviour — and the battery reddens with the Ring3 frame in the
  panic dump); report the wrong vector from the same fork, 13 for 14 (the
  containment assertion fails with "crasher was not terminated by a page
  fault", `Some(13)` against `Some(14)`); omit the kill path's CR3 switch
  (the surviving neighbour resumes inside the DEAD task's address space,
  faults there, and is itself killed — "the innocent neighbour was marked
  faulted", which is the assertion that proves the kill path must switch
  worlds exactly as the timer path does).

**Build gates**

- `cargo fmt --check`, and `clippy -D warnings` on kernel, `kshared` and `xtask`.
- The unstable-feature allowlist gate (see Open Questions): the set of
  `#![feature(...)]` attributes in `kernel/` must equal the allowlist exactly.
  *Mutation:* add any feature attribute; the job must fail.
- The `no-network-no-storage` gate: a job that greps `kernel/src`, `kshared/src`,
  `user` and both kernel-side manifests for the tokens a network or storage driver
  would have to bring with it — `smoltcp`, `tcp`, `socket`, `ethernet`, `virtio`,
  `dhcp`, `http`, `fatfs`, `ext4`, `nvme`, `ahci`, `ata` and their neighbours — and
  fails on a hit. Source, manifests **and** the resolved dependency graph: the job
  also greps `cargo tree -p kernel` output, so a transitive crate cannot smuggle
  networking in unseen, and it catches a driver written by hand as well as one
  pulled in as a crate. Only grep exit 1 (searched, found nothing) passes, and the
  paths are preflighted — a renamed path fails the gate instead of disarming it.
  *Mutation:* add a crate or a module with any of those
  names; the job must fail. This is what makes "no network stack exists" a checked
  statement rather than a promise. Its reach is worth stating too: it is a token
  grep, so a crate with an innocuous name still needs a person to notice it. The gate
  makes adding networking a deliberate act, not an impossible one.
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
4. **Add the `no-network-no-storage` CI job.** Cheap, and it is the gate that makes
   the PRD's first privacy claim checkable. Doing it before more code lands means it
   never has to be retrofitted against a tree that already violates it.
5. **M2:** GDT and TSS with IST0 first, then the IDT with breakpoint and double
   fault, then general protection and page fault, then the PIC remap, then the PIT,
   then the keyboard handler, which reads port `0x60` and counts the byte until M4
   gives it somewhere to go. Self-tests as each lands.
6. **M3:** frame arithmetic in `kshared` with host tests first, then the frame
   allocator, then `OffsetPageTable`, then the heap, then the zeroing wrapper. The two
   privacy self-tests land with the code they test, and each is observed failing under
   its mutation before the milestone is called done.
7. **M4:** executor, waker, the scancode queue behind `ScancodeStream`, `pc-keyboard`
   decode. Note that `pc-keyboard` 0.9's API differs from the widely copied 0.7-era
   examples.
8. **M5:** the line editor in `kshared` (host-tested before wiring), then the command
   surface and its history, then the banner's boot time, then the stack-overflow
   check last.
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

## Open questions (both now resolved)

**1. Does the kernel keep a zero-unstable-features rule, or an allowlist of exactly
one? — RESOLVED: option (a) shipped.** The kernel carries exactly
`#![feature(abi_x86_interrupt)]`, and the `feature-allowlist` CI job asserts the
set of `#![feature(...)]` attributes under `kernel/` and `kshared/` is exactly that
one entry. The `stable-compat` job was removed — it could never go green anyway,
since the `x86_64` crate's own `step_trait` code needs nightly. The original
analysis is kept below as the record of why.

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

**2. What is the actual RAM floor? — RESOLVED: measured 2026-08-17 on QEMU 11.1.**
BIOS boots and passes the full battery at **21 MiB** and fails at 20; UEFI passes at
**46 MiB** and fails at 45, the difference being OVMF's own footprint rather than
the kernel's. `xtask` pins `BIOS_TEST_MEM_MB = 24` and `UEFI_TEST_MEM_MB = 48` —
floor plus a small, recorded headroom (3 MB BIOS, 2 MB UEFI) to absorb QEMU-version
variance — and CI boots at exactly those values, so the lightness gate is a
measured gate: a regression larger than that headroom fails the build. The BIOS
floor beats the 32 MiB target.
