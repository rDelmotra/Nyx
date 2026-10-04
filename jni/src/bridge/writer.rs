//! The currently whole-file write entry point for [`super::VolumeHandle`].
//!
//! This is deliberately a module boundary rather than a second public handle:
//! Pass 2b will replace the one-shot API with a streaming writer, and its
//! lifecycle must remain coupled to the per-device writer claim documented
//! here. Keeping that state in the bridge also preserves key zeroisation when
//! a volume is closed.

use luks_core::error::{LuksError, Result};
use luks_core::fs::{FileType, MountedFs as Fs};
use std::sync::atomic::Ordering;

use super::{FileWriterEnum, MountedFs, VolumeHandle, WriterError, WriterTable};

/// Writing exists only when the write path was compiled in.
///
/// This separate implementation means a read-only build does not merely
/// refuse to write — it has nothing to refuse *with*.
impl VolumeHandle {
    /// Begin a bounded-memory file transfer. The writer's state is registered
    /// in this volume (F4-A1) and the returned id names it; the id carries no
    /// volume reference, so storing it in a JNI handle cannot prolong key
    /// lifetime.
    pub fn begin_file(&self, size: u64) -> Result<u64> {
        self.guarding_writes(|| {
            let mut fs = self.fs_for_writing()?;
            self.ensure_open()?;
            self.claim_writer()?;
            let w = match &mut *fs {
                Fs::Ext4(ext4) => FileWriterEnum::Ext4(ext4.begin_file(size)?),
                Fs::Btrfs(btrfs) => FileWriterEnum::Btrfs(btrfs.begin_file(size)?),
            };
            Ok(self.register_writer(w))
        })
    }

    /// Begin an unknown-size file transfer. Unlike `begin_file`, no upfront
    /// size is required — extents are allocated as chunks arrive in
    /// `write_chunk`. Registered like [`begin_file`](Self::begin_file).
    pub fn begin_file_streaming(&self) -> Result<u64> {
        self.guarding_writes(|| {
            let mut fs = self.fs_for_writing()?;
            self.ensure_open()?;
            self.claim_writer()?;
            let w = match &mut *fs {
                Fs::Ext4(ext4) => FileWriterEnum::Ext4(ext4.begin_file_streaming()?),
                Fs::Btrfs(btrfs) => FileWriterEnum::Btrfs(btrfs.begin_file_streaming()?),
            };
            Ok(self.register_writer(w))
        })
    }

    /// Number of writers this volume still owns (begun, neither finished nor
    /// released).
    pub fn open_writer_count(&self) -> usize {
        self.lock_writers().map.len()
    }

    fn lock_writers(&self) -> std::sync::MutexGuard<'_, WriterTable> {
        // Memory-only bookkeeping: a poisoned table is still releasable.
        self.writers.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Refuse to register a writer on a closed volume. Called under the `fs`
    /// lock, which `close()` also holds while it sets `closed`, so the check
    /// and the registration cannot straddle a close.
    fn ensure_open(&self) -> Result<()> {
        if self.lock_writers().closed {
            return Err(LuksError::WriteSessionFenced("volume closed".into()));
        }
        Ok(())
    }

    fn register_writer(&self, w: FileWriterEnum) -> u64 {
        let mut t = self.lock_writers();
        t.next += 1;
        let id = t.next;
        t.map.insert(id, w);
        id
    }

    /// Run `op` on the writer `id` under the `fs` lock, with the session fence
    /// policy of every other mutator. An unknown id is `BadHandle`, not a fence.
    fn with_writer<T>(
        &self,
        id: u64,
        op: impl FnOnce(&mut MountedFs, &mut FileWriterEnum) -> Result<T>,
    ) -> std::result::Result<T, WriterError> {
        let out = self.guarding_writes(|| {
            let mut fs = self.fs_for_writing()?;
            let mut t = self.lock_writers();
            match t.map.get_mut(&id) {
                None => Ok(None),
                Some(w) => op(&mut fs, w).map(Some),
            }
        })?;
        out.ok_or(WriterError::BadHandle("writer is finished or closed"))
    }

    pub fn write_file_chunk_with_cancel(
        &self,
        writer: u64,
        data: &[u8],
        cancel_token: u64,
    ) -> std::result::Result<(), WriterError> {
        if cancel_token != 0 && crate::bridge::is_cancelled(cancel_token) {
            return Err(LuksError::Cancelled.into());
        }
        self.with_writer(writer, |fs, w| match (fs, w) {
            (Fs::Ext4(ext4), FileWriterEnum::Ext4(w)) => ext4.write_chunk(w, data),
            (Fs::Btrfs(btrfs), FileWriterEnum::Btrfs(w)) => btrfs.write_chunk(w, data),
            _ => Err(LuksError::CorruptFs("writer kind does not match the filesystem")),
        })
    }

    pub fn write_file_chunk(&self, writer: u64, data: &[u8]) -> std::result::Result<(), WriterError> {
        self.write_file_chunk_with_cancel(writer, data, 0)
    }

    /// Publish the writer's file. **The writer is spent on every exit**: on
    /// success the driver consumed it; on a driver error the driver released it
    /// (ext4 on every path, btrfs by rollback); and when the session is
    /// fenced/poisoned at entry (so the driver is never reached) it is released
    /// here through [`release_writer`](Self::release_writer) (F4-A3), never
    /// just dropped.
    pub fn finish_file(&self, writer: u64, parent_path: &str, name: &str) -> std::result::Result<u64, WriterError> {
        let out = self.guarding_writes(|| {
            let mut fs = match self.fs_for_writing() {
                Ok(g) => g,
                Err(e) => {
                    // No guard is held here (fs_for_writing returned Err).
                    self.release_registered(writer);
                    return Err(e);
                }
            };
            let Some(w) = self.lock_writers().map.remove(&writer) else {
                return Ok(None);
            };
            let ino = match (&mut *fs, w) {
                (Fs::Ext4(ext4), FileWriterEnum::Ext4(w)) => {
                    let dir_ino = match ext4.resolve(parent_path) {
                        Ok(d) => d.number,
                        Err(e) => {
                            ext4.abandon_file(w);
                            return Err(e);
                        }
                    };
                    // The driver's finish ends with the clean-mark flush.
                    ext4.finish_file(w, dir_ino, name, FileType::Regular)?
                }
                // btrfs adds to the active batch inside finish_file; the batch
                // is committed by `commit_active_batch` or `close`.
                (Fs::Btrfs(btrfs), FileWriterEnum::Btrfs(w)) => btrfs.finish_file(w, parent_path, name)?,
                (fs, w) => {
                    self.release_writer(fs, w)?;
                    return Err(LuksError::CorruptFs("writer kind does not match the filesystem"));
                }
            };
            Ok(Some(ino))
        })?;
        out.ok_or(WriterError::BadHandle("writer is finished or closed"))
    }

    /// Publish a btrfs batch (no-op on ext4, which commits in `finish_file`).
    /// Cheap when nothing is pending: the driver returns before any I/O.
    pub fn commit_active_batch(&self) -> Result<()> {
        self.guarding_writes(|| {
            let mut fs = self.fs_for_writing()?;
            match &mut *fs {
                Fs::Ext4(_) => Ok(()), // Ext4 commits immediately in finish_file
                Fs::Btrfs(btrfs) => btrfs.commit_active_batch(),
            }
        })
    }

    /// Release writer `id` and everything it reserved. Never depends on the
    /// fence (F4-A2): see [`release_writer`](Self::release_writer). `BadHandle`
    /// if the id is unknown (already finished, released, or closed with the
    /// volume), which leaks nothing.
    pub fn abandon_file(&self, writer: u64) -> std::result::Result<(), WriterError> {
        let out = self.guarding_writes(|| {
            // Recovering lock on purpose: memory-only for ext4 and for a
            // fenced/poisoned btrfs session, so it must run on a poisoned fs.
            let mut fs = self.fs_for_reading();
            let Some(w) = self.lock_writers().map.remove(&writer) else {
                return Ok(false);
            };
            self.release_writer(&mut fs, w)?;
            Ok(true)
        })?;
        if out {
            Ok(())
        } else {
            Err(WriterError::BadHandle("writer is finished or closed"))
        }
    }

    /// Whether the session may no longer touch the medium.
    fn session_unusable(&self) -> bool {
        self.fence_reason().is_some() || self.fs.is_poisoned() || self.dev.is_poisoned()
    }

    /// F4-A2: release one writer's reservations. `fs` is the caller's guard
    /// (possibly recovered from poison).
    ///
    /// * ext4: `abandon_file` is memory-only, always allowed.
    /// * btrfs, session fenced or poisoned: `forget_writer` returns the runs in
    ///   memory and clears `has_open_writer` with **no commit**.
    /// * btrfs otherwise: `abandon_file` (which also publishes earlier pending
    ///   files as a courtesy).
    fn release_writer(&self, fs: &mut MountedFs, w: FileWriterEnum) -> Result<()> {
        match (fs, w) {
            (Fs::Ext4(ext4), FileWriterEnum::Ext4(w)) => {
                ext4.abandon_file(w);
                Ok(())
            }
            (Fs::Btrfs(btrfs), FileWriterEnum::Btrfs(w)) => {
                if self.session_unusable() {
                    btrfs.forget_writer(w)
                } else {
                    btrfs.abandon_file(w)
                }
            }
            _ => Err(LuksError::CorruptFs("writer kind does not match the filesystem")),
        }
    }

    /// Release writer `id` if registered, logging (never returning) a failure:
    /// for exit paths that already carry a more important error.
    fn release_registered(&self, id: u64) {
        let mut fs = self.fs_for_reading();
        let Some(w) = self.lock_writers().map.remove(&id) else {
            return;
        };
        if let Err(e) = self.release_writer(&mut fs, w) {
            crate::log::e(&format!("release_registered: release failed [{}] stage=exit-route", crate::bridge::error_code(&e)));
        }
    }

    /// F4-A5: end this volume's write session. Releases every open writer
    /// (A2), then, unless the session is fenced or poisoned, publishes the
    /// active btrfs batch. Returns the first failure so the caller knows whether
    /// acknowledged files reached the medium. Idempotent: only the first call does work, so a later
    /// `Drop` never commits again; after it, nothing registers or writes. `Drop` calls this best-effort.
    pub fn close(&self) -> Result<()> {
        let mut fs = self.fs_for_reading();
        // Poison latches the fence (and discards the batch) before we decide.
        if self.fs.is_poisoned() || self.dev.is_poisoned() {
            let why = if self.fs.is_poisoned() { "filesystem lock poisoned" } else { "device lock poisoned" };
            self.fence_writes_locked(&mut fs, why);
        }
        let drained: Vec<FileWriterEnum> = {
            let mut t = self.lock_writers();
            if t.closed {
                // Already closed (explicitly, or by an earlier Drop attempt):
                // never commit a second time.
                return Ok(());
            }
            t.closed = true;
            t.map.drain().map(|(_, w)| w).collect()
        };
        let mut first: Option<LuksError> = None;
        for w in drained {
            if let Err(e) = self.release_writer(&mut fs, w) {
                crate::log::e(&format!("close: writer release failed [{}] stage=release", crate::bridge::error_code(&e)));
                first.get_or_insert(e);
            }
        }
        if !self.session_unusable() {
            if let Fs::Btrfs(btrfs) = &mut *fs {
                if let Err(e) = btrfs.commit_active_batch() {
                    crate::log::e(&format!("close: final commit failed [{}] stage=commit", crate::bridge::error_code(&e)));
                    // A transport-class failure leaves the drive's state
                    // unknown: fence, which also discards the batch, so
                    // nothing re-commits it later (M1).
                    if e.fences_write_session() {
                        self.fence_writes_locked(&mut fs, &e);
                    }
                    first.get_or_insert(e);
                }
            }
        }
        first.map_or(Ok(()), Err)
    }

    /// Create `name` in `parent_path` holding `data`, returning the new inode number.
    ///
    /// # Ordering
    ///
    /// The flush is not optional and not the caller's to skip. A USB bridge
    /// acknowledges a write into its own buffer, so without it a "written"
    /// file is one yanked cable away from never having existed — and pulling
    /// the cable is how a phone transfer normally ends.
    pub fn write_file(&self, parent_path: &str, name: &str, data: &[u8]) -> Result<u64> {
        self.guarding_writes(|| {
            let mut fs = self.fs_for_writing()?;
            self.ensure_open()?;
            self.claim_writer()?;

            match &mut *fs {
                Fs::Ext4(ext4) => {
                    let dir_ino = ext4.resolve(parent_path)?.number;
                    // `create_file` ends with the dirty bracket's clean-mark flush.
                    let ino = ext4.create_file(dir_ino, name, data, FileType::Regular)?;
                    Ok(ino)
                }
                Fs::Btrfs(btrfs) => {
                    // Now uses the single-transaction Phase 2 implementation,
                    // which guarantees atomicity: we either create the file AND write its data,
                    // or fail cleanly with no orphan file left behind.
                    let ino = btrfs.create_file_with_data(parent_path, name, data)?;
                    Ok(ino)
                }
            }
        })
    }

    /// Delete a file by path from the encrypted volume.
    pub fn delete_file(&self, path: &str) -> Result<()> {
        self.guarding_writes(|| {
            let mut fs = self.fs_for_writing()?;
            self.ensure_open()?;
            self.claim_writer()?;
            match &mut *fs {
                Fs::Ext4(ext4) => ext4.delete_file(path),
                Fs::Btrfs(btrfs) => btrfs.delete_file(path),
            }
        })
    }

    /// Create a directory at `parent_path` with name `name`.
    pub fn create_directory(&self, parent_path: &str, name: &str) -> Result<u64> {
        self.guarding_writes(|| {
            let mut fs = self.fs_for_writing()?;
            self.ensure_open()?;
            self.claim_writer()?;
            match &mut *fs {
                Fs::Ext4(ext4) => {
                    let dir_ino = ext4.resolve(parent_path)?.number as u32;
                    // Ends with the dirty bracket's clean-mark flush.
                    let ino = ext4.create_directory(dir_ino, name)?;
                    Ok(ino as u64)
                }
                Fs::Btrfs(btrfs) => {
                    let ino = btrfs.create_directory(parent_path, name)?;
                    Ok(ino)
                }
            }
        })
    }

    /// Rename an item from `(old_parent, old_name)` to `(new_parent, new_name)`.
    pub fn rename(&self, old_parent: &str, old_name: &str, new_parent: &str, new_name: &str) -> Result<()> {
        self.guarding_writes(|| {
            let mut fs = self.fs_for_writing()?;
            self.ensure_open()?;
            self.claim_writer()?;
            match &mut *fs {
                Fs::Ext4(ext4) => {
                    // Ends with the dirty bracket's clean-mark flush. (A no-op
                    // rename or a refusal writes nothing, so needs none.)
                    ext4.rename(old_parent, old_name, new_parent, new_name)?;
                    Ok(())
                }
                Fs::Btrfs(btrfs) => {
                    btrfs.rename(old_parent, old_name, new_parent, new_name)?;
                    Ok(())
                }
            }
        })
    }

    fn claim_writer(&self) -> Result<()> {
        if !self.holds_writer.load(Ordering::Acquire) {
            self.device_writer
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .map_err(|_| LuksError::WriterBusy)?;
            self.holds_writer.store(true, Ordering::Release);
        }
        Ok(())
    }
}
