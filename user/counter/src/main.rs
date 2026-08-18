//! `counter` — the unyielding user program that proves preemption is real.
//!
//! It runs a long register-heavy checksum loop and never once yields: no
//! syscall breaks the compute phase apart from eight progress dots, and a
//! `SYS_WRITE` does not release the CPU (only the timer does). Under purely
//! cooperative scheduling a program launched after this one would wait for
//! the whole loop; under preemptive scheduling it runs — and exits — while
//! this loop is still going, which is exactly what the kernel's self-test
//! battery asserts.
//!
//! Two adversarial properties, both load-bearing:
//! - The checksum keeps eight live accumulators mixing across the entire run,
//!   so every context switch must save and restore the full register file
//!   perfectly — one corrupted callee-saved register anywhere in the
//!   scheduler's save/restore path and the final `SYS_EXIT` value is wrong.
//!   The kernel computes the same fold independently and compares.
//! - `EFLAGS.AC` is set before the loop and stays set for the whole run, so
//!   every timer interrupt taken from this program enters the kernel with the
//!   flag ring 3 controls at its most hostile — SMAP is only real if the
//!   timer entry scrubs it, and the battery asserts it was never seen set.

#![no_std]
#![no_main]

use core::panic::PanicInfo;

const SYS_EXIT: u64 = 0;
const SYS_WRITE: u64 = 1;

/// Iterations of the mixing loop. Must match the kernel's independent
/// recomputation (`kernel/src/selftest.rs`, `expected_counter_checksum`).
/// Sized to span many 10 ms timer quanta under QEMU TCG (the only
/// environment the battery runs in) without dragging the battery: tens of
/// timer round-trips, well under two seconds.
const ITERS: u64 = 30_000_000;

fn syscall(nr: u64, a0: u64) -> u64 {
    let mut ret = nr;
    // SAFETY: `int 0x80` is Osmium's syscall gate. The kernel scrubs the
    // caller-saved registers on return (rax carries the result), which the
    // clobber list reflects; callee-saved registers are preserved.
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inout("rax") ret,
            inout("rdi") a0 => _,
            out("rcx") _,
            out("rdx") _,
            out("rsi") _,
            out("r8") _,
            out("r9") _,
            out("r10") _,
            out("r11") _,
        );
    }
    ret
}

#[unsafe(no_mangle)]
extern "C" fn _start() -> ! {
    // Set EFLAGS.AC (bit 18) and LEAVE it set for the entire run: `iretq`
    // restores the flag image saved at each interrupt, so the flag survives
    // every syscall and every preemption — meaning every asynchronous timer
    // entry the kernel takes from this program starts with AC hostile.
    // Harmless in ring 3 (CR0.AM is clear, so no alignment faults).
    // SAFETY: pushfq/popfq only toggle a flag; no memory effect.
    unsafe {
        core::arch::asm!("pushfq", "or dword ptr [rsp], 0x40000", "popfq");
    }

    // Eight accumulators, all live across every iteration. The seeds and
    // multipliers are arbitrary odd constants; what matters is that the fold
    // is deterministic and register-hungry. The kernel's twin of this loop
    // must stay byte-for-byte in step (selftest.rs cross-references here).
    let mut a: u64 = 0x243F_6A88_85A3_08D3;
    let mut b: u64 = 0x1319_8A2E_0370_7344;
    let mut c: u64 = 0xA409_3822_299F_31D0;
    let mut d: u64 = 0x082E_FA98_EC4E_6C89;
    let mut e: u64 = 0x4528_21E6_38D0_1377;
    let mut f: u64 = 0xBE54_66CF_34E9_0C6C;
    let mut g: u64 = 0xC0AC_29B7_C97C_50DD;
    let mut h: u64 = 0x3F84_D5B5_B547_0917;

    let dot_every = ITERS / 8;
    let mut next_dot = dot_every;
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
        // Progress dots so the interactive `sched` demo shows this program's
        // output interleaving with its neighbour's. A syscall is NOT a yield
        // — the scheduler switches on timer ticks only — so these do not
        // soften the preemption claim.
        if i == next_dot {
            syscall(SYS_WRITE, u64::from(b'.'));
            next_dot += dot_every;
        }
    }

    let sum = a ^ b ^ c ^ d ^ e ^ f ^ g ^ h;
    syscall(SYS_EXIT, sum);
    // SYS_EXIT never returns; this satisfies the diverging signature.
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    // No unwinding and nothing to report to: a user panic just exits with a
    // recognisable code.
    syscall(SYS_EXIT, 0xdead);
    loop {
        core::hint::spin_loop();
    }
}
