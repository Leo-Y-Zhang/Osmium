//! Cycle-accurate boot timing via the timestamp counter. Phase marks are
//! recorded on every boot — a handful of cheap `rdtsc` reads — while the
//! self-test battery is the only build that spends time calibrating
//! cycles-to-microseconds against the PIT and printing the numbers, so the
//! measured figures land in the CI serial logs on every push without slowing
//! the shipped boot path.

use core::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy)]
pub enum Phase {
    ConsoleReady = 0,
    InterruptsOn = 1,
    MemoryReady = 2,
}
const PHASE_COUNT: usize = 3;

static BOOT_TSC: AtomicU64 = AtomicU64::new(0);
static MARKS: [AtomicU64; PHASE_COUNT] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
#[cfg(feature = "selftest")]
static CYCLES_PER_MS: AtomicU64 = AtomicU64::new(0);

#[inline]
pub fn rdtsc() -> u64 {
    // SAFETY: rdtsc reads the timestamp counter and has no memory effects.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Records the boot origin; call as early in `kernel_main` as possible.
pub fn mark_boot() {
    BOOT_TSC.store(rdtsc(), Ordering::Relaxed);
}

/// Records cycles-since-boot for a boot phase. Cheap enough to leave in the
/// shipped path.
pub fn stamp(phase: Phase) {
    let since = rdtsc().wrapping_sub(BOOT_TSC.load(Ordering::Relaxed));
    MARKS[phase as usize].store(since, Ordering::Relaxed);
}

/// Calibrates cycles-per-millisecond against the PIT. Must run after
/// interrupts are enabled; costs ~50 ms, which is why only the battery
/// (not the shipped boot) pays it.
#[cfg(feature = "selftest")]
pub fn calibrate() {
    use crate::interrupts::{TICK_HZ, TICKS};
    let edge = TICKS.load(Ordering::Relaxed);
    while TICKS.load(Ordering::Relaxed) == edge {
        x86_64::instructions::hlt();
    }
    let start_ticks = TICKS.load(Ordering::Relaxed);
    let start_tsc = rdtsc();
    while TICKS.load(Ordering::Relaxed) < start_ticks + 5 {
        x86_64::instructions::hlt();
    }
    let elapsed_ticks = TICKS.load(Ordering::Relaxed) - start_ticks;
    let elapsed_tsc = rdtsc().wrapping_sub(start_tsc);
    let elapsed_ms = elapsed_ticks * 1000 / TICK_HZ;
    if let Some(cpm) = elapsed_tsc.checked_div(elapsed_ms) {
        CYCLES_PER_MS.store(cpm, Ordering::Relaxed);
    }
}

#[cfg(feature = "selftest")]
fn cycles_to_us(cycles: u64) -> Option<u64> {
    let cpm = CYCLES_PER_MS.load(Ordering::Relaxed);
    (cpm > 0).then(|| cycles * 1000 / cpm)
}

/// Prints each boot phase to serial as cycles and, once calibrated,
/// microseconds. The battery calls this so CI archives the figures.
#[cfg(feature = "selftest")]
pub fn report() {
    const LABELS: [&str; PHASE_COUNT] = ["console-ready", "interrupts-on", "memory-ready"];
    let cpm = CYCLES_PER_MS.load(Ordering::Relaxed);
    if cpm > 0 {
        crate::serial_println!("[boot] TSC calibrated at {cpm} cycles/ms");
    }
    for (i, label) in LABELS.iter().enumerate() {
        let cyc = MARKS[i].load(Ordering::Relaxed);
        match cycles_to_us(cyc) {
            Some(us) => crate::serial_println!("[boot] {label}: {cyc} cyc (~{us} us from entry)"),
            None => crate::serial_println!("[boot] {label}: {cyc} cyc from entry"),
        }
    }
}
