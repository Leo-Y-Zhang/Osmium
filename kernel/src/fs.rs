//! The kernel's single RAM-only filesystem (M12).
//!
//! All of the logic lives in [`kshared::ramfs`], which is allocator-free and
//! host-tested; this module owns the one instance and the lock discipline
//! around it. The arena is a plain static, so it costs nothing at boot beyond
//! its `.bss` bytes and it starts empty on every cold boot — which is the
//! whole of the persistence story.
//!
//! **Never touched from interrupt context.** The shell and the battery are
//! the only callers, both in task context, so this mutex cannot deadlock
//! against a handler (the TDD's global-state rule). Nothing here allocates.
//!
//! The no-persistence claim covers the filesystem for free, and deliberately
//! so: the battery creates, reads and deletes files during the boot that CI
//! hashes the disk image around. A filesystem that ever reached the disk
//! would change that hash and fail the build.

use kshared::ramfs::Ramfs;
use spin::Mutex;

static FS: Mutex<Ramfs> = Mutex::new(Ramfs::new());

/// Runs `f` with exclusive access to the filesystem.
pub fn with_fs<R>(f: impl FnOnce(&mut Ramfs) -> R) -> R {
    f(&mut FS.lock())
}
