//! Identity-based file reads (F4 Part B).
//!
//! `open_file` resolves a path **once** and returns a handle that pins the
//! file's identity; `read_at` reads by inode, never by path, and re-checks the
//! identity under the filesystem lock on every call. So a delete, a
//! rename-over or an inode-number reuse between two chunks of an export can
//! only produce [`LuksError::FileChanged`], never another file's bytes (N3),
//! and a btrfs commit between chunks cannot leave a read on a freed tree root
//! (R3r: the handle names the subvolume, the current root is looked up per
//! read). Paths cost one directory walk, at open.
//!
//! Errors from this module carry no path: the caller holds a handle, and
//! error text reaches logs.

use luks_core::error::{LuksError, Result};
use luks_core::fs::OpenFile;
use std::sync::atomic::{AtomicU64, Ordering};

use super::VolumeHandle;

/// A file pinned by identity. Holds no volume reference (like a writer id), so
/// a stray handle cannot prolong the life of an unlocked volume or its key.
pub struct OpenFileHandle {
    volume_id: u64,
    file: OpenFile,
    verified: AtomicU64,
    unverified: AtomicU64,
}

impl OpenFileHandle {
    /// The volume this file was opened on (`VolumeHandle::id`).
    pub fn volume_id(&self) -> u64 {
        self.volume_id
    }

    /// Size when opened. A later size change is a `FileChanged`, so this is
    /// also the size of every successful read's file.
    pub fn size(&self) -> u64 {
        self.file.size()
    }

    /// `(verified, unverified)` bytes returned by `read_at` so far. Verified
    /// means matched against a stored data checksum (btrfs); unverified is
    /// ext4 (no data checksums) and btrfs `NODATASUM` files. Lets a caller
    /// say how much of an export was integrity-checked.
    pub fn coverage(&self) -> (u64, u64) {
        (
            self.verified.load(Ordering::Relaxed),
            self.unverified.load(Ordering::Relaxed),
        )
    }
}

impl VolumeHandle {
    /// Resolve `path` once and pin the file it names. Directories and
    /// non-regular files are refused.
    pub fn open_file(&self, path: &str) -> Result<OpenFileHandle> {
        let file = self.fs_for_reading().open(path)?;
        if !file.file_type().is_file() {
            return Err(LuksError::IsADirectory(path.to_string()));
        }
        Ok(OpenFileHandle {
            volume_id: self.id,
            file,
            verified: AtomicU64::new(0),
            unverified: AtomicU64::new(0),
        })
    }

    /// The size of the pinned file, after re-checking its identity under the
    /// `fs` lock. `FileChanged` if it is no longer the file that was opened,
    /// so a caller never pairs a handle with another file's size.
    pub fn file_size(&self, file: &OpenFileHandle) -> Result<u64> {
        self.read_at(file, 0, &mut [])?;
        Ok(file.size())
    }

    /// Fill `buf` from `offset` of the pinned file, returning how many bytes
    /// were written (short only at end of file).
    ///
    /// One `fs` lock acquisition covers the identity check and the data read,
    /// so nothing can change between them.
    pub fn read_at(&self, file: &OpenFileHandle, offset: u64, buf: &mut [u8]) -> Result<usize> {
        if file.volume_id != self.id {
            // Not the volume it was opened on: identity cannot be verified.
            return Err(LuksError::FileChanged);
        }
        let (n, cov) = self.fs_for_reading().read_open_covered(&file.file, offset, buf)?;
        file.verified.fetch_add(cov.verified_bytes, Ordering::Relaxed);
        file.unverified.fetch_add(cov.unverified_bytes, Ordering::Relaxed);
        Ok(n)
    }
}
