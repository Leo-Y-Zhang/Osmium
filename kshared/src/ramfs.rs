//! A RAM-only filesystem: a flat namespace of small files in a fixed byte
//! arena, with no allocator, no hardware and no disk anywhere in sight.
//!
//! It lives in `kshared` because it is pure logic — every rule it enforces is
//! decidable from its own state, so all of it is host-testable and every
//! refusal can be observed failing on a laptop rather than inferred from a
//! boot log. The kernel owns exactly one instance and wires the shell to it.
//!
//! **Refusal-first**, in the same shape as [`crate::elf`]: a name that is
//! empty, over-long, non-printable or contains a path separator is refused;
//! so is a duplicate, a full table and a full arena. Every [`FsError`]
//! variant names the check that failed, so a refusal is a diagnosis.
//!
//! **Nothing persists and nothing leaks.** The arena is ordinary kernel
//! memory: a cold boot starts empty, and CI hashes the disk image before and
//! after every boot, so a filesystem that ever reached the disk would fail
//! the build. Deleting a file **scrubs its bytes and compacts the arena** —
//! deleted content does not linger, and freed space is genuinely reusable
//! rather than lost to a bump cursor, which is the same standard the frame
//! allocator was held to in M11.

/// The most files that can exist at once. Small on purpose: the point is a
/// working, provable filesystem, not capacity.
pub const MAX_FILES: usize = 8;
/// The longest permitted file name, in bytes.
pub const MAX_NAME: usize = 32;
/// Total bytes available for file CONTENTS across all files.
///
/// Deliberately smaller than `MAX_FILES` × the longest line the shell can
/// carry (`LINE_CAP` is 256, so eight `write`s could offer ~1980 bytes):
/// **every refusal this module can return is reachable from the shipped
/// shell**, `ArenaFull` included. An error path that only a host test can
/// reach is the same kind of decoration as a check no mutation can falsify.
pub const ARENA_BYTES: usize = 1024;

/// Why a filesystem operation was refused. Each variant names the check that
/// failed rather than collapsing into a generic error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
    /// The name was empty.
    NameEmpty,
    /// The name exceeded [`MAX_NAME`] bytes.
    NameTooLong,
    /// The name contained a byte outside printable ASCII, a space, or a path
    /// separator. A flat namespace with no `/` is what makes "there is no
    /// path traversal here" a refusal rather than a promise.
    NameNotAllowed,
    /// A file of that name already exists. Writes never silently replace.
    DuplicateName,
    /// [`MAX_FILES`] files already exist.
    TableFull,
    /// The contents would not fit in the remaining arena.
    ArenaFull,
    /// No file of that name exists.
    NotFound,
}

impl FsError {
    /// The sentence a shell should print for this refusal. It lives here
    /// rather than in the kernel so the mapping is host-testable: every
    /// variant must produce its own message, and a new variant that forgot
    /// one would be caught natively instead of on a screen nobody is reading.
    pub fn message(self) -> &'static str {
        match self {
            FsError::NameEmpty => "a file name is required",
            FsError::NameTooLong => "that name is too long",
            FsError::NameNotAllowed => {
                "names must be printable ASCII with no spaces and no path separators"
            }
            FsError::DuplicateName => "a file of that name already exists (delete it first)",
            FsError::TableFull => "no free file slots",
            FsError::ArenaFull => "not enough space left in the filesystem",
            FsError::NotFound => "no such file",
        }
    }
}

/// Splits a `write` command's arguments into a name and the contents that
/// follow it. Pure string handling, so the edge cases a shell hits — no
/// arguments at all, a name with no text, runs of spaces before the text —
/// are decided here and tested on the host rather than discovered on screen.
///
/// The contents keep their internal spacing and their trailing spaces; only
/// the separator between name and contents is consumed. A name with no text
/// yields an empty file, which is legal.
pub fn split_write_args(args: &str) -> (&str, &str) {
    match args.split_once(char::is_whitespace) {
        Some((name, rest)) => (name, rest.trim_start()),
        None => (args, ""),
    }
}

/// One file's table entry. Contents live in the shared arena at
/// `start..start + len`.
#[derive(Clone, Copy)]
struct Entry {
    name: [u8; MAX_NAME],
    name_len: usize,
    start: usize,
    len: usize,
}

impl Entry {
    const EMPTY: Self = Self {
        name: [0; MAX_NAME],
        name_len: 0,
        start: 0,
        len: 0,
    };

    fn name(&self) -> &[u8] {
        &self.name[..self.name_len]
    }
}

/// The filesystem itself: a fixed table of entries over a fixed byte arena.
/// Contents are stored contiguously in creation order; deleting compacts.
pub struct Ramfs {
    entries: [Entry; MAX_FILES],
    count: usize,
    arena: [u8; ARENA_BYTES],
    used: usize,
}

// No `impl Default`, and clippy's suggestion to add one is declined on
// purpose. `Ramfs` is a kilobyte of arena plus its table; constructing one
// anywhere but const context materialises that as a stack temporary, and a
// 20 KiB kernel stack is not the place for it. That is true of `new()` too —
// the difference is that `new()` is `const`, so the kernel's single `static`
// costs no stack at all, whereas `Default::default()` can never be used that
// way and would exist only to be called at runtime.
#[allow(clippy::new_without_default)]
impl Ramfs {
    pub const fn new() -> Self {
        Self {
            entries: [Entry::EMPTY; MAX_FILES],
            count: 0,
            arena: [0; ARENA_BYTES],
            used: 0,
        }
    }

    /// Validates a name against every rule, in a fixed order so the reported
    /// diagnosis is deterministic.
    fn check_name(name: &str) -> Result<(), FsError> {
        if name.is_empty() {
            return Err(FsError::NameEmpty);
        }
        if name.len() > MAX_NAME {
            return Err(FsError::NameTooLong);
        }
        // Printable ASCII only, and no space or path separator: the namespace
        // is flat by construction, so "no traversal" needs no parser.
        if !name
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'/' && b != b'\\')
        {
            return Err(FsError::NameNotAllowed);
        }
        Ok(())
    }

    fn index_of(&self, name: &str) -> Option<usize> {
        self.entries[..self.count]
            .iter()
            .position(|e| e.name() == name.as_bytes())
    }

    /// Creates a file. Refuses rather than replacing an existing name, so a
    /// write can never silently destroy data.
    pub fn create(&mut self, name: &str, contents: &[u8]) -> Result<(), FsError> {
        Self::check_name(name)?;
        if self.index_of(name).is_some() {
            return Err(FsError::DuplicateName);
        }
        if self.count == MAX_FILES {
            return Err(FsError::TableFull);
        }
        if contents.len() > ARENA_BYTES - self.used {
            return Err(FsError::ArenaFull);
        }
        let start = self.used;
        self.arena[start..start + contents.len()].copy_from_slice(contents);
        let mut entry = Entry::EMPTY;
        entry.name[..name.len()].copy_from_slice(name.as_bytes());
        entry.name_len = name.len();
        entry.start = start;
        entry.len = contents.len();
        self.entries[self.count] = entry;
        self.count += 1;
        self.used += contents.len();
        Ok(())
    }

    /// The contents of `name`, or [`FsError::NotFound`].
    ///
    /// Validates the name first, so a malformed one is diagnosed as what it
    /// is rather than as a miss: `cat a/b` should say the name is not allowed,
    /// not "no such file", which would imply such a file could exist. The
    /// refusal-first contract holds on every entry point, not just `create`.
    pub fn read(&self, name: &str) -> Result<&[u8], FsError> {
        Self::check_name(name)?;
        let i = self.index_of(name).ok_or(FsError::NotFound)?;
        let e = &self.entries[i];
        Ok(&self.arena[e.start..e.start + e.len])
    }

    /// Deletes `name`, scrubbing its bytes and compacting the arena so the
    /// freed space is reusable — a filesystem that only ever bumped forwards
    /// would leak capacity exactly the way the frame allocator once leaked
    /// frames.
    ///
    /// The scrub happens on the vacated TAIL after the survivors slide down,
    /// which is what makes "deleted contents are gone" true of the whole
    /// arena rather than just of the hole.
    pub fn delete(&mut self, name: &str) -> Result<(), FsError> {
        Self::check_name(name)?;
        let i = self.index_of(name).ok_or(FsError::NotFound)?;
        let (start, len) = (self.entries[i].start, self.entries[i].len);
        // Slide every later file's bytes down over the hole.
        self.arena.copy_within(start + len..self.used, start);
        // Scrub the tail the survivors vacated: without this, a deleted
        // file's bytes (or a copy of a survivor's) linger in the arena.
        let new_used = self.used - len;
        self.arena[new_used..self.used].fill(0);
        self.used = new_used;
        // Fix up the table: later entries' contents moved down by `len`.
        for e in self.entries[..self.count].iter_mut() {
            if e.start > start {
                e.start -= len;
            }
        }
        for j in i..self.count - 1 {
            self.entries[j] = self.entries[j + 1];
        }
        self.count -= 1;
        self.entries[self.count] = Entry::EMPTY;
        Ok(())
    }

    /// File names in creation order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries[..self.count].iter().map(|e| {
            // SAFETY-free: names are validated printable ASCII on the way in,
            // so this is infallible; `unwrap_or` keeps it panic-free anyway.
            core::str::from_utf8(e.name()).unwrap_or("?")
        })
    }

    /// The length of one file, without borrowing its contents.
    pub fn len_of(&self, name: &str) -> Result<usize, FsError> {
        Self::check_name(name)?;
        let i = self.index_of(name).ok_or(FsError::NotFound)?;
        Ok(self.entries[i].len)
    }

    /// (files, bytes used, bytes total).
    pub fn stats(&self) -> (usize, usize, usize) {
        (self.count, self.used, ARENA_BYTES)
    }

    /// True iff every arena byte beyond the live contents is zero. The
    /// filesystem's own privacy invariant, checkable at any time: nothing
    /// deleted, and no stale copy left by compaction, survives past `used`.
    ///
    /// This is the sole oracle for the delete-scrub claim, so it needs its
    /// own proof that it can say `false` — a version that always returned
    /// `true` would satisfy every other test in this module. `soil_tail`
    /// exists purely so one test can watch it do that.
    pub fn tail_is_clean(&self) -> bool {
        self.arena[self.used..].iter().all(|&b| b == 0)
    }

    /// Dirties the free tail of the arena, so a test can confirm
    /// [`Self::tail_is_clean`] actually discriminates instead of always
    /// agreeing. Test-only: nothing in the kernel can reach it.
    #[cfg(test)]
    fn soil_tail(&mut self, pattern: u8) {
        self.arena[self.used..].fill(pattern);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_read_roundtrip() {
        let mut fs = Ramfs::new();
        fs.create("notes.txt", b"hello").unwrap();
        assert_eq!(fs.read("notes.txt").unwrap(), b"hello");
        assert_eq!(fs.len_of("notes.txt").unwrap(), 5);
        assert_eq!(fs.stats(), (1, 5, ARENA_BYTES));
    }

    #[test]
    fn names_are_validated() {
        let mut fs = Ramfs::new();
        assert_eq!(fs.create("", b"x"), Err(FsError::NameEmpty));
        let long = "a".repeat(MAX_NAME + 1);
        assert_eq!(fs.create(&long, b"x"), Err(FsError::NameTooLong));
        assert_eq!(fs.create("a/b", b"x"), Err(FsError::NameNotAllowed));
        assert_eq!(fs.create("a\\b", b"x"), Err(FsError::NameNotAllowed));
        assert_eq!(fs.create("a b", b"x"), Err(FsError::NameNotAllowed));
        assert_eq!(fs.create("a\tb", b"x"), Err(FsError::NameNotAllowed));
        assert_eq!(fs.create("café", b"x"), Err(FsError::NameNotAllowed));
        // Exactly MAX_NAME is allowed: the boundary is inclusive.
        assert!(fs.create(&"a".repeat(MAX_NAME), b"x").is_ok());
    }

    #[test]
    fn duplicate_names_are_refused_and_do_not_replace() {
        let mut fs = Ramfs::new();
        fs.create("f", b"original").unwrap();
        assert_eq!(fs.create("f", b"replacement"), Err(FsError::DuplicateName));
        assert_eq!(fs.read("f").unwrap(), b"original");
        assert_eq!(fs.stats().0, 1);
    }

    #[test]
    fn table_fills_and_refuses() {
        let mut fs = Ramfs::new();
        for i in 0..MAX_FILES {
            fs.create(&format!("f{i}"), b"x").unwrap();
        }
        assert_eq!(fs.create("one_more", b"x"), Err(FsError::TableFull));
        assert_eq!(fs.stats().0, MAX_FILES);
    }

    #[test]
    fn arena_boundary_is_exact() {
        let mut fs = Ramfs::new();
        let full = vec![0xAB; ARENA_BYTES];
        fs.create("big", &full).unwrap();
        assert_eq!(fs.stats().1, ARENA_BYTES);
        assert_eq!(fs.create("another", b"x"), Err(FsError::ArenaFull));
        // And one byte too many is refused from empty.
        let mut fs2 = Ramfs::new();
        let over = vec![0u8; ARENA_BYTES + 1];
        assert_eq!(fs2.create("big", &over), Err(FsError::ArenaFull));
        assert_eq!(fs2.stats().1, 0);
    }

    #[test]
    fn missing_files_are_diagnosed() {
        let mut fs = Ramfs::new();
        assert_eq!(fs.read("nope"), Err(FsError::NotFound));
        assert_eq!(fs.delete("nope"), Err(FsError::NotFound));
        assert_eq!(fs.len_of("nope"), Err(FsError::NotFound));
    }

    #[test]
    fn every_entry_point_validates_the_name() {
        // A malformed name is not a miss: reporting NotFound would imply such
        // a file could exist. The refusal-first contract is not create's
        // alone.
        let mut fs = Ramfs::new();
        assert_eq!(fs.read("a/b"), Err(FsError::NameNotAllowed));
        assert_eq!(fs.len_of("a/b"), Err(FsError::NameNotAllowed));
        assert_eq!(fs.delete("a/b"), Err(FsError::NameNotAllowed));
        assert_eq!(fs.read(""), Err(FsError::NameEmpty));
        assert_eq!(fs.delete(""), Err(FsError::NameEmpty));
    }

    #[test]
    fn every_error_has_its_own_message() {
        let all = [
            FsError::NameEmpty,
            FsError::NameTooLong,
            FsError::NameNotAllowed,
            FsError::DuplicateName,
            FsError::TableFull,
            FsError::ArenaFull,
            FsError::NotFound,
        ];
        for (i, a) in all.iter().enumerate() {
            assert!(!a.message().is_empty(), "{a:?} has no message");
            for b in &all[i + 1..] {
                assert_ne!(
                    a.message(),
                    b.message(),
                    "{a:?} and {b:?} share a message, so a refusal is ambiguous"
                );
            }
        }
    }

    #[test]
    fn write_arguments_split_at_the_first_space_only() {
        assert_eq!(
            split_write_args("notes hello world"),
            ("notes", "hello world")
        );
        // No text at all is an empty file, which is legal.
        assert_eq!(split_write_args("notes"), ("notes", ""));
        assert_eq!(split_write_args("notes   "), ("notes", ""));
        // Runs of spaces before the text are separator, not content...
        assert_eq!(split_write_args("notes    hi"), ("notes", "hi"));
        // ...but spacing inside the text is preserved verbatim.
        assert_eq!(split_write_args("notes a  b"), ("notes", "a  b"));
        // Nothing at all yields an empty name, which `create` then refuses
        // with NameEmpty rather than this function guessing.
        assert_eq!(split_write_args(""), ("", ""));
        assert_eq!(Ramfs::new().create("", b""), Err(FsError::NameEmpty));
    }

    #[test]
    fn tail_is_clean_can_say_no() {
        // The oracle every scrub assertion leans on must be able to fail, or
        // those assertions prove nothing: `-> true` would satisfy them all.
        let mut fs = Ramfs::new();
        fs.create("a", b"x").unwrap();
        assert!(fs.tail_is_clean());
        fs.soil_tail(0xEE);
        assert!(
            !fs.tail_is_clean(),
            "tail_is_clean cannot tell a dirty arena from a clean one"
        );
    }

    #[test]
    fn zero_length_files_survive_compaction_around_them() {
        // Zero-length files share a `start` with whatever follows them, which
        // is the one case where the compaction fix-up's `e.start > start`
        // boundary could go wrong in either direction.
        let mut fs = Ramfs::new();
        fs.create("empty_first", b"").unwrap();
        fs.create("data", b"WXYZ").unwrap();
        fs.create("empty_last", b"").unwrap();
        // Deleting the only sized file must not disturb either empty file.
        fs.delete("data").unwrap();
        assert_eq!(fs.read("empty_first").unwrap(), b"");
        assert_eq!(fs.read("empty_last").unwrap(), b"");
        assert_eq!(fs.stats(), (2, 0, ARENA_BYTES));
        assert!(fs.tail_is_clean(), "the deleted file's bytes survived");
        // And a sized file created afterwards still reads back correctly.
        fs.create("again", b"PQ").unwrap();
        assert_eq!(fs.read("again").unwrap(), b"PQ");
        assert_eq!(fs.read("empty_first").unwrap(), b"");
    }

    #[test]
    fn arena_boundary_is_exact_from_a_partly_used_arena() {
        // The refusal arithmetic is `contents.len() > ARENA_BYTES - used`;
        // proving it only from an empty arena leaves the `used > 0` term
        // untested in the ACCEPTING direction.
        let mut fs = Ramfs::new();
        fs.create("head", &vec![1u8; 100]).unwrap();
        let exact = vec![2u8; ARENA_BYTES - 100];
        let one_over = vec![3u8; ARENA_BYTES - 99];
        assert_eq!(fs.create("over", &one_over), Err(FsError::ArenaFull));
        fs.create("exact", &exact)
            .expect("a file filling the arena exactly was refused");
        assert_eq!(fs.stats().1, ARENA_BYTES);
        assert_eq!(fs.read("head").unwrap(), &vec![1u8; 100][..]);
        assert_eq!(fs.read("exact").unwrap(), &exact[..]);
    }

    #[test]
    fn delete_scrubs_the_contents() {
        let mut fs = Ramfs::new();
        fs.create("secret", b"TOPSECRETBYTES").unwrap();
        fs.delete("secret").unwrap();
        assert_eq!(fs.stats(), (0, 0, ARENA_BYTES));
        assert!(
            fs.tail_is_clean(),
            "a deleted file's bytes survived in the arena"
        );
    }

    #[test]
    fn delete_compacts_and_survivors_are_intact() {
        let mut fs = Ramfs::new();
        fs.create("a", b"AAAA").unwrap();
        fs.create("b", b"BBBBBBBB").unwrap();
        fs.create("c", b"CC").unwrap();
        assert_eq!(fs.stats().1, 14);
        // Delete the MIDDLE file: this is the case that catches a compaction
        // that forgets to shift later entries' offsets.
        fs.delete("b").unwrap();
        assert_eq!(fs.read("a").unwrap(), b"AAAA");
        assert_eq!(fs.read("c").unwrap(), b"CC");
        assert_eq!(fs.stats(), (2, 6, ARENA_BYTES));
        assert!(fs.tail_is_clean(), "compaction left a stale copy behind");
        let names: Vec<&str> = fs.names().collect();
        assert_eq!(names, ["a", "c"]);
    }

    #[test]
    fn freed_space_is_genuinely_reusable() {
        let mut fs = Ramfs::new();
        // Fill the arena exactly, then free it and fill it again: a bump-only
        // arena would refuse the second fill, which is the filesystem form of
        // the frame-leak bug M11 removed.
        let full = vec![0x11; ARENA_BYTES];
        fs.create("first", &full).unwrap();
        fs.delete("first").unwrap();
        assert_eq!(fs.stats(), (0, 0, ARENA_BYTES));
        let again = vec![0x22; ARENA_BYTES];
        fs.create("second", &again)
            .expect("freed arena space was not reusable");
        assert_eq!(fs.read("second").unwrap(), &again[..]);
    }

    #[test]
    fn deleting_the_last_file_needs_no_shift() {
        let mut fs = Ramfs::new();
        fs.create("a", b"AAAA").unwrap();
        fs.create("b", b"BB").unwrap();
        fs.delete("b").unwrap();
        assert_eq!(fs.read("a").unwrap(), b"AAAA");
        assert_eq!(fs.stats(), (1, 4, ARENA_BYTES));
        assert!(fs.tail_is_clean());
    }

    #[test]
    fn empty_contents_are_allowed_and_read_back_empty() {
        let mut fs = Ramfs::new();
        fs.create("empty", b"").unwrap();
        assert_eq!(fs.read("empty").unwrap(), b"");
        assert_eq!(fs.stats(), (1, 0, ARENA_BYTES));
    }
}
