//! Minimal ELF64 loader front-end: pure parsing and validation, no hardware,
//! no allocator. The kernel maps and copies what a [`LoadPlan`] describes;
//! everything that can be checked without a machine is checked here, so it is
//! host-testable — and refusal is the default: an image is loadable only if
//! every check passes.
//!
//! Scope is deliberate: static `ET_EXEC` images, `PT_LOAD` segments only, no
//! relocations, no dynamic linking, no interpreter. The user program is
//! linked at a fixed base inside the declared user image window.

/// User program segments must live inside `[USER_IMAGE_BASE, USER_IMAGE_END)`.
/// The window is one 2 MiB page-table region: every leaf inside it shares the
/// same intermediate entries as the base address, which is what lets the
/// kernel's page-table audit allow user-accessible intermediates only where
/// they reach this window (plus the stack page).
pub const USER_IMAGE_BASE: u64 = 0x40_0000;
pub const USER_IMAGE_END: u64 = 0x60_0000;

/// Fixed capacity: a static no_std program has 2-4 loadable segments; eight
/// is generous, and refusing more keeps the plan allocator-free.
pub const MAX_SEGMENTS: usize = 8;

/// Bounds the frames a single load can consume: reclamation (M11) returns a
/// run's frames afterwards, but nothing caps a SINGLE image's appetite, so a
/// huge BSS would still be a resource-exhaustion lever within one load.
pub const MAX_TOTAL_PAGES: u64 = 64;

const PAGE_SIZE: u64 = 4096;

const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

/// One `PT_LOAD` segment, validated. `file_start..file_start+filesz` are the
/// image bytes to copy to `vaddr`; the tail up to `memsz` is BSS and stays
/// zero (the kernel's frames arrive zeroed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub vaddr: u64,
    pub file_start: usize,
    pub filesz: u64,
    pub memsz: u64,
    pub writable: bool,
    pub executable: bool,
}

impl Segment {
    /// Number of whole pages this segment occupies (`vaddr` is page-aligned,
    /// enforced at parse time).
    pub fn page_count(&self) -> u64 {
        self.memsz.div_ceil(PAGE_SIZE)
    }
}

/// A validated load plan: the segments to map and the entry point.
#[derive(Debug)]
pub struct LoadPlan {
    segments: [Segment; MAX_SEGMENTS],
    count: usize,
    pub entry: u64,
}

impl LoadPlan {
    pub fn segments(&self) -> &[Segment] {
        &self.segments[..self.count]
    }
}

/// Why an image was refused. Every variant names the check that failed, so a
/// refusal is a diagnosis rather than a shrug.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfError {
    /// Too short for the structure being read, or a file range out of bounds.
    Truncated,
    BadMagic,
    /// Not 64-bit little-endian v1.
    BadClass,
    /// Not an `ET_EXEC` x86-64 image (relocatable/dynamic images need a
    /// relocator this loader deliberately does not have).
    NotStaticExecutable,
    /// A `PT_LOAD` segment is both writable and executable.
    WritableAndExecutable,
    /// A segment (or the entry point) lies outside the user image window.
    OutsideUserWindow,
    /// A segment's virtual address is not page-aligned.
    Unaligned,
    /// `filesz > memsz`, zero `memsz`, or arithmetic overflow.
    BadSegmentSize,
    /// Two segments' page ranges overlap (each page is mapped exactly once).
    Overlap,
    /// More than [`MAX_SEGMENTS`] loadable segments.
    TooManySegments,
    /// The load would exceed [`MAX_TOTAL_PAGES`] pages.
    TooManyPages,
    /// The entry point is not inside an executable segment's file-backed bytes.
    EntryNotExecutable,
}

// The `off.checked_add(N)` matters: `off` derives from a fully attacker-
// controlled `e_phoff`, and `off + N` would wrap in a release build (overflow
// checks off) — producing `start > end`, which `slice::get` rejects — but
// PANIC in a debug/test build. checked_add makes the refusal profile-
// independent so a future fuzz harness over `parse_elf64` cannot trip a panic.
fn le_u16(image: &[u8], off: usize) -> Result<u16, ElfError> {
    let end = off.checked_add(2).ok_or(ElfError::Truncated)?;
    let b = image
        .get(off..end)
        .ok_or(ElfError::Truncated)?
        .try_into()
        .map_err(|_| ElfError::Truncated)?;
    Ok(u16::from_le_bytes(b))
}

fn le_u32(image: &[u8], off: usize) -> Result<u32, ElfError> {
    let end = off.checked_add(4).ok_or(ElfError::Truncated)?;
    let b = image
        .get(off..end)
        .ok_or(ElfError::Truncated)?
        .try_into()
        .map_err(|_| ElfError::Truncated)?;
    Ok(u32::from_le_bytes(b))
}

fn le_u64(image: &[u8], off: usize) -> Result<u64, ElfError> {
    let end = off.checked_add(8).ok_or(ElfError::Truncated)?;
    let b = image
        .get(off..end)
        .ok_or(ElfError::Truncated)?
        .try_into()
        .map_err(|_| ElfError::Truncated)?;
    Ok(u64::from_le_bytes(b))
}

// (The M8-era `plans_overlap` cross-image predicate was removed at M9: with
// per-task address spaces, two images claiming the same virtual pages is not
// a conflict to refuse but the isolation model working as designed. The
// parser's own per-image overlap check below is unchanged.)

/// Parses and validates a static ELF64 executable for the user window.
pub fn parse_elf64(image: &[u8]) -> Result<LoadPlan, ElfError> {
    if image.len() < 64 {
        return Err(ElfError::Truncated);
    }
    if image[0..4] != [0x7f, b'E', b'L', b'F'] {
        return Err(ElfError::BadMagic);
    }
    // class 2 = 64-bit, data 1 = little-endian, version 1.
    if image[4] != 2 || image[5] != 1 || image[6] != 1 {
        return Err(ElfError::BadClass);
    }
    if le_u16(image, 16)? != ET_EXEC || le_u16(image, 18)? != EM_X86_64 {
        return Err(ElfError::NotStaticExecutable);
    }
    let entry = le_u64(image, 24)?;
    let phoff = le_u64(image, 32)?;
    let phentsize = le_u16(image, 54)? as u64;
    let phnum = le_u16(image, 56)? as u64;
    if phentsize < 56 {
        return Err(ElfError::Truncated);
    }

    let mut segments = [Segment {
        vaddr: 0,
        file_start: 0,
        filesz: 0,
        memsz: 0,
        writable: false,
        executable: false,
    }; MAX_SEGMENTS];
    let mut count = 0usize;
    let mut total_pages = 0u64;

    for i in 0..phnum {
        let ph = phoff
            .checked_add(i.checked_mul(phentsize).ok_or(ElfError::Truncated)?)
            .ok_or(ElfError::Truncated)? as usize;
        if le_u32(image, ph)? != PT_LOAD {
            continue;
        }
        let flags = le_u32(image, ph + 4)?;
        let file_start = le_u64(image, ph + 8)?;
        let vaddr = le_u64(image, ph + 16)?;
        let filesz = le_u64(image, ph + 32)?;
        let memsz = le_u64(image, ph + 40)?;

        // W+X is refused BEFORE the empty-segment skip: a hostile flag combo
        // must not be able to hide inside a memsz==0 PT_LOAD (refusal is the
        // default, even for a segment that would map nothing).
        let writable = flags & PF_W != 0;
        let executable = flags & PF_X != 0;
        if writable && executable {
            return Err(ElfError::WritableAndExecutable);
        }
        if memsz == 0 {
            continue; // nothing to map; some linkers emit empty PT_LOADs
        }
        if vaddr % PAGE_SIZE != 0 {
            return Err(ElfError::Unaligned);
        }
        if filesz > memsz {
            return Err(ElfError::BadSegmentSize);
        }
        let vend = vaddr.checked_add(memsz).ok_or(ElfError::BadSegmentSize)?;
        if vaddr < USER_IMAGE_BASE || vend > USER_IMAGE_END {
            return Err(ElfError::OutsideUserWindow);
        }
        let file_end = file_start.checked_add(filesz).ok_or(ElfError::Truncated)?;
        if file_end > image.len() as u64 {
            return Err(ElfError::Truncated);
        }
        if count == MAX_SEGMENTS {
            return Err(ElfError::TooManySegments);
        }

        let seg = Segment {
            vaddr,
            file_start: file_start as usize,
            filesz,
            memsz,
            writable,
            executable,
        };
        // Page-granular overlap check: each page is mapped exactly once.
        let new_pages = vaddr..vaddr + seg.page_count() * PAGE_SIZE;
        for prior in &segments[..count] {
            let prior_pages = prior.vaddr..prior.vaddr + prior.page_count() * PAGE_SIZE;
            if new_pages.start < prior_pages.end && prior_pages.start < new_pages.end {
                return Err(ElfError::Overlap);
            }
        }
        total_pages += seg.page_count();
        if total_pages > MAX_TOTAL_PAGES {
            return Err(ElfError::TooManyPages);
        }
        segments[count] = seg;
        count += 1;
    }

    // The entry point must land in an executable segment's file-backed bytes
    // (an entry in BSS or a data segment is a malformed or hostile image).
    let entry_ok = segments[..count]
        .iter()
        .any(|seg| seg.executable && entry >= seg.vaddr && entry < seg.vaddr + seg.filesz);
    if !entry_ok {
        return Err(ElfError::EntryNotExecutable);
    }

    Ok(LoadPlan {
        segments,
        count,
        entry,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal valid ELF64 image: one RX text segment carrying
    /// `code_len` bytes at `USER_IMAGE_BASE` (entry at its start) and one RW
    /// data segment one page above it. Tests mutate the returned bytes.
    fn valid_image() -> Vec<u8> {
        build_image(&[
            (USER_IMAGE_BASE, 64, 64, PF_X | 4),
            (USER_IMAGE_BASE + 4096, 8, 16, PF_W | 4),
        ])
    }

    /// (vaddr, filesz, memsz, p_flags) per PT_LOAD segment; file bytes are
    /// appended in order after the program headers.
    fn build_image(segs: &[(u64, u64, u64, u32)]) -> Vec<u8> {
        let phoff = 64u64;
        let phentsize = 56u64;
        let data_off = phoff + phentsize * segs.len() as u64;
        let mut img = vec![0u8; data_off as usize];
        // ELF header.
        img[0..4].copy_from_slice(&[0x7f, b'E', b'L', b'F']);
        img[4] = 2; // 64-bit
        img[5] = 1; // little-endian
        img[6] = 1; // version
        img[16..18].copy_from_slice(&ET_EXEC.to_le_bytes());
        img[18..20].copy_from_slice(&EM_X86_64.to_le_bytes());
        img[24..32].copy_from_slice(&segs[0].0.to_le_bytes()); // entry = first vaddr
        img[32..40].copy_from_slice(&phoff.to_le_bytes());
        img[54..56].copy_from_slice(&(phentsize as u16).to_le_bytes());
        img[56..58].copy_from_slice(&(segs.len() as u16).to_le_bytes());
        // Program headers + appended file bytes.
        let mut file_pos = data_off;
        for (i, &(vaddr, filesz, memsz, flags)) in segs.iter().enumerate() {
            let ph = (phoff + phentsize * i as u64) as usize;
            img[ph..ph + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
            img[ph + 4..ph + 8].copy_from_slice(&flags.to_le_bytes());
            img[ph + 8..ph + 16].copy_from_slice(&file_pos.to_le_bytes());
            img[ph + 16..ph + 24].copy_from_slice(&vaddr.to_le_bytes());
            img[ph + 32..ph + 40].copy_from_slice(&filesz.to_le_bytes());
            img[ph + 40..ph + 48].copy_from_slice(&memsz.to_le_bytes());
            file_pos += filesz;
        }
        img.resize(file_pos as usize, 0xAB); // segment file bytes
        img
    }

    #[test]
    fn valid_image_parses_with_both_segments() {
        let plan = parse_elf64(&valid_image()).expect("valid image refused");
        assert_eq!(plan.entry, USER_IMAGE_BASE);
        assert_eq!(plan.segments().len(), 2);
        let text = plan.segments()[0];
        assert!(text.executable && !text.writable);
        assert_eq!(text.vaddr, USER_IMAGE_BASE);
        assert_eq!(text.filesz, 64);
        let data = plan.segments()[1];
        assert!(data.writable && !data.executable);
        assert_eq!(data.memsz, 16); // 8 file bytes + 8 BSS
        assert_eq!(data.page_count(), 1);
    }

    #[test]
    fn bad_magic_is_refused() {
        let mut img = valid_image();
        img[0] = 0x7e;
        assert!(matches!(parse_elf64(&img), Err(ElfError::BadMagic)));
    }

    #[test]
    fn truncated_phdr_table_is_refused() {
        let img = valid_image();
        // Cut inside the second program header.
        let cut = 64 + 56 + 20;
        assert!(matches!(parse_elf64(&img[..cut]), Err(ElfError::Truncated)));
    }

    #[test]
    fn writable_and_executable_segment_is_refused() {
        let img = build_image(&[(USER_IMAGE_BASE, 64, 64, PF_X | PF_W | 4)]);
        assert!(matches!(
            parse_elf64(&img),
            Err(ElfError::WritableAndExecutable)
        ));
    }

    #[test]
    fn segment_outside_the_user_window_is_refused() {
        for vaddr in [
            USER_IMAGE_BASE - 4096, // below
            USER_IMAGE_END,         // at the end
            USER_IMAGE_END - 4096,  // memsz 2 pages -> crosses the end
        ] {
            let img = build_image(&[(vaddr, 64, 8192, PF_X | 4)]);
            assert!(
                matches!(parse_elf64(&img), Err(ElfError::OutsideUserWindow)),
                "vaddr {vaddr:#x} was not refused"
            );
        }
    }

    #[test]
    fn unaligned_vaddr_is_refused() {
        let img = build_image(&[(USER_IMAGE_BASE + 8, 64, 64, PF_X | 4)]);
        assert!(matches!(parse_elf64(&img), Err(ElfError::Unaligned)));
    }

    #[test]
    fn filesz_larger_than_memsz_is_refused() {
        let img = build_image(&[(USER_IMAGE_BASE, 128, 64, PF_X | 4)]);
        assert!(matches!(parse_elf64(&img), Err(ElfError::BadSegmentSize)));
    }

    #[test]
    fn overlapping_page_ranges_are_refused() {
        // Distinct vaddrs, same page-rounded range once memsz is applied.
        let img = build_image(&[
            (USER_IMAGE_BASE, 64, 8192, PF_X | 4),
            (USER_IMAGE_BASE + 4096, 8, 8, PF_W | 4),
        ]);
        assert!(matches!(parse_elf64(&img), Err(ElfError::Overlap)));
    }

    #[test]
    fn file_range_past_the_image_end_is_refused() {
        let mut img = valid_image();
        img.truncate(img.len() - 32); // second segment's file bytes cut short
        assert!(matches!(parse_elf64(&img), Err(ElfError::Truncated)));
    }

    #[test]
    fn entry_outside_an_executable_segment_is_refused() {
        // Entry pointing into the RW data segment.
        let mut img = valid_image();
        img[24..32].copy_from_slice(&(USER_IMAGE_BASE + 4096).to_le_bytes());
        assert!(matches!(
            parse_elf64(&img),
            Err(ElfError::EntryNotExecutable)
        ));
    }

    #[test]
    fn phoff_near_u64_max_is_refused_not_panicked() {
        // A well-formed header whose e_phoff is at the top of the address
        // space must return Err(Truncated), not overflow-panic in a debug
        // build (the shipped kernel is --release, but the tests are not).
        let mut img = valid_image();
        for phoff in [u64::MAX, u64::MAX - 1, u64::MAX - 3] {
            img[32..40].copy_from_slice(&phoff.to_le_bytes());
            assert!(
                matches!(parse_elf64(&img), Err(ElfError::Truncated)),
                "phoff {phoff:#x} was not refused cleanly"
            );
        }
    }

    #[test]
    fn wx_segment_is_refused_even_when_empty() {
        // A W+X flag combo must be refused before the memsz==0 skip, so it
        // cannot hide inside an empty segment.
        let img = build_image(&[(USER_IMAGE_BASE, 0, 0, PF_X | PF_W | 4)]);
        assert!(matches!(
            parse_elf64(&img),
            Err(ElfError::WritableAndExecutable)
        ));
    }

    #[test]
    fn page_budget_is_enforced() {
        // One huge BSS: 65 pages of memsz against the 64-page budget.
        let img = build_image(&[(USER_IMAGE_BASE, 0, 65 * 4096, PF_W | 4)]);
        assert!(matches!(parse_elf64(&img), Err(ElfError::TooManyPages)));
    }
}
