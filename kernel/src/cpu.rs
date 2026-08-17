//! CPU supervisor-mode hardening: SMEP, SMAP and UMIP.
//!
//! Three CR4 bits that make the ring-3 boundary harder to cross the wrong way.
//! Each is CPUID-gated — a bit is set only if the CPU advertises it — so this
//! is safe on any x86_64, and what actually took is read back from CR4 and
//! surfaced to the shell (`sysinfo`, `privacy`) rather than assumed.
//!
//! - **SMEP** (Supervisor-Mode Execution Prevention): the kernel cannot
//!   execute an instruction from a user-accessible page. It never intends to;
//!   SMEP turns "never intends to" into "cannot".
//! - **SMAP** (Supervisor-Mode Access Prevention): the kernel cannot read or
//!   write a user-accessible page without an explicit `stac`. The ELF loader
//!   therefore copies each segment through the kernel's own physical-memory
//!   alias (a supervisor mapping), never through the user virtual address —
//!   so a confused-deputy write to a user pointer faults instead of silently
//!   succeeding.
//! - **UMIP** (User-Mode Instruction Prevention): ring 3 cannot run `sgdt`,
//!   `sidt`, `sldt`, `smsw` or `str`, which would otherwise leak kernel
//!   structure addresses to an unprivileged program.

use core::sync::atomic::{AtomicU8, Ordering};

const SMEP: u8 = 1 << 0;
const SMAP: u8 = 1 << 1;
const UMIP: u8 = 1 << 2;

/// Bitmask of the features actually enabled (set once, at init).
static ENABLED: AtomicU8 = AtomicU8::new(0);

/// CPUID leaf 7, sub-leaf 0: EBX bit 7 = SMEP, bit 20 = SMAP; ECX bit 2 = UMIP.
fn supported() -> (bool, bool, bool) {
    // CPUID leaf 7 is available on every x86_64 CPU (the leaf count in EAX of
    // leaf 0 is >= 7 on all of them). `__cpuid_count` is safe on x86_64: the
    // instruction is unprivileged and has no side effects beyond its outputs.
    let leaf7 = core::arch::x86_64::__cpuid_count(7, 0);
    let smep = leaf7.ebx & (1 << 7) != 0;
    let smap = leaf7.ebx & (1 << 20) != 0;
    let umip = leaf7.ecx & (1 << 2) != 0;
    (smep, smap, umip)
}

/// Enables every supported feature and records the result. Call once, after
/// paging is up (SMAP changes how supervisor code may touch user pages, and
/// the ELF loader is already written for it) and before ring 3 is ever
/// entered.
pub fn init() {
    use x86_64::registers::control::{Cr4, Cr4Flags};
    let (smep, smap, umip) = supported();
    let mut add = Cr4Flags::empty();
    if smep {
        add |= Cr4Flags::SUPERVISOR_MODE_EXECUTION_PROTECTION;
    }
    if smap {
        add |= Cr4Flags::SUPERVISOR_MODE_ACCESS_PREVENTION;
    }
    if umip {
        add |= Cr4Flags::USER_MODE_INSTRUCTION_PREVENTION;
    }
    if !add.is_empty() {
        // SAFETY: only the advertised protection bits are added; no other CR4
        // bit is touched, and the kernel's own mappings are supervisor pages,
        // so SMEP/SMAP do not restrict any access the kernel already makes.
        unsafe { Cr4::update(|f| f.insert(add)) };
    }
    // Read CR4 back: record what the CPU actually latched, not what we asked.
    let cr4 = Cr4::read();
    let mut mask = 0;
    if cr4.contains(Cr4Flags::SUPERVISOR_MODE_EXECUTION_PROTECTION) {
        mask |= SMEP;
    }
    if cr4.contains(Cr4Flags::SUPERVISOR_MODE_ACCESS_PREVENTION) {
        mask |= SMAP;
    }
    if cr4.contains(Cr4Flags::USER_MODE_INSTRUCTION_PREVENTION) {
        mask |= UMIP;
    }
    ENABLED.store(mask, Ordering::Relaxed);
}

fn enabled(bit: u8) -> bool {
    ENABLED.load(Ordering::Relaxed) & bit != 0
}

pub fn smep_enabled() -> bool {
    enabled(SMEP)
}

pub fn smap_enabled() -> bool {
    enabled(SMAP)
}

pub fn umip_enabled() -> bool {
    enabled(UMIP)
}

/// A compact `SMEP+SMAP+UMIP` / `SMEP+UMIP` / `none` summary for the shell.
pub fn summary() -> &'static str {
    match (smep_enabled(), smap_enabled(), umip_enabled()) {
        (true, true, true) => "SMEP+SMAP+UMIP",
        (true, true, false) => "SMEP+SMAP",
        (true, false, true) => "SMEP+UMIP",
        (false, true, true) => "SMAP+UMIP",
        (true, false, false) => "SMEP",
        (false, true, false) => "SMAP",
        (false, false, true) => "UMIP",
        (false, false, false) => "none",
    }
}
