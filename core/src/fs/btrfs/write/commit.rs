//! Committing in-memory btrfs transactions to the underlying device.
//!
//! Strict write ordering:
//! 1. All new metadata blocks (written to EVERY DUP stripe).
//! 2. Flush device (`device.flush()`).
//! 3. Updated superblock copies (written to all valid `SUPER_OFFSETS` with the transaction's generation).
//!    The FST VALID bit is left as found: the free-space tree is reconciled in
//!    the same transaction, so it is never stale on disk.
//! 4. Final flush (`device.flush()`).
//! 5. Update in-memory mount state & clear node cache (and, only now, learn
//!    any chunk this transaction created).
//!
//! # Retry safety
//!
//! [`commit_transaction_ref`] never consumes or mutates the transaction, and
//! on failure leaves `fs` exactly as it was (no mount-state update). A caller
//! that still holds the batch the transaction was built from can therefore
//! rebuild and re-run the commit:
//! * Failure before the first superblock write: every block written so far
//!   went to an address that is free in the committed tree (the allocator is
//!   derived from the committed extent tree plus the batch's own, still
//!   uncommitted, reservations). Overwriting them again is harmless, and the
//!   old tree is untouched.
//! * Failure after a superblock write began: the disk may already hold this
//!   transaction. The rebuild is deterministic (same batch, same committed
//!   trees, same generation), so re-running it writes identical blocks and an
//!   identical superblock: idempotent. Callers must not change the batch in
//!   between (see `CommitError::superblock_write_started`).

use crate::device::WriteAt;
use crate::error::Result;
use crate::fs::btrfs::superblock::{Superblock, SUPER_OFFSETS, SUPER_SIZE};
use crate::fs::btrfs::write::txn::Transaction;
use crate::fs::btrfs::Btrfs;

/// A failed commit, with enough detail for the caller to retry safely.
#[derive(Debug)]
pub struct CommitError {
    pub error: crate::error::LuksError,
    /// `true` once the first superblock mirror write was attempted: the disk
    /// may already hold the new generation even though the caller got an
    /// error. The batch must then be retried unchanged (idempotent) rather
    /// than rolled back.
    pub superblock_write_started: bool,
}

impl From<crate::error::LuksError> for CommitError {
    fn from(error: crate::error::LuksError) -> Self {
        Self { error, superblock_write_started: false }
    }
}

/// Commit an in-memory transaction to `fs.device_mut()`, consuming it.
pub fn commit_transaction<D: WriteAt>(
    fs: &mut Btrfs<D>,
    txn: Transaction,
) -> Result<()> {
    commit_transaction_ref(fs, &txn).map_err(|e| e.error)
}

/// Commit without consuming the transaction; see the module docs for what a
/// failure guarantees.
pub fn commit_transaction_ref<D: WriteAt>(
    fs: &mut Btrfs<D>,
    txn: &Transaction,
) -> std::result::Result<(), CommitError> {
    match commit_transaction_inner(fs, txn) {
        Ok(()) => {
            fs.commit_pending = false;
            Ok(())
        }
        Err(e) => {
            // M1: the disk may hold this generation. Refuse new writes until the
            // identical commit is retried (or the batch discarded).
            if e.superblock_write_started {
                fs.commit_pending = true;
            }
            Err(e)
        }
    }
}

fn commit_transaction_inner<D: WriteAt>(
    fs: &mut Btrfs<D>,
    txn: &Transaction,
) -> std::result::Result<(), CommitError> {
    // Read raw superblock copy #0 as a template.
    let mut template = vec![0u8; SUPER_SIZE];
    fs.device().read_at(SUPER_OFFSETS[0], &mut template)?;

    // Verify the template before trusting a single byte of it. This used to
    // be the only unverified superblock read in the codebase: `emit_copy`
    // below stamps a FRESH, valid checksum over whatever `template` holds
    // and writes the result to all three mirrors. Without this check, a
    // transport glitch on this one read (desynced USB pipe, torn cable)
    // would not be caught — our own checksum would certify the garbage and
    // launder it into permanent, "verified" corruption across every
    // mirror, surviving a replug. `Superblock::parse` is the exact
    // mechanism every other superblock read in this codebase is checked
    // with (magic at 0x40, then checksum per `csum_type`); reuse it here
    // instead of duplicating the CRC/SHA256 dispatch. See
    // notes/btrfs-recon-2026-08-30/PLAN-2026-09-01.md, item H5.
    Superblock::parse(&template, SUPER_OFFSETS[0])?;

    let dev_len = fs.device().len();

    // A chunk-allocation transaction places some of its own metadata inside
    // the chunk it creates, so the writes below must be able to resolve it.
    // The live chunk map is NOT touched until the commit has succeeded (B4):
    // resolve through a private copy instead.
    let write_map = if txn.new_chunks.is_empty() {
        None
    } else {
        let mut m = fs.chunk_map().clone();
        for c in &txn.new_chunks {
            m.insert(c.clone());
        }
        Some(m)
    };

    // 1. Write all new data blocks to all physical stripes.
    for (bytenr, data_bytes) in &txn.pending_data {
        let stripes = write_map.as_ref().unwrap_or_else(|| fs.chunk_map()).map_all_stripes(*bytenr)?;
        for phys in stripes {
            fs.device_mut().write_at(phys, data_bytes)?;
        }
    }

    // 2. Write all new metadata blocks to all physical stripes.
    for (bytenr, block_bytes) in &txn.pending_blocks {
        let stripes = write_map.as_ref().unwrap_or_else(|| fs.chunk_map()).map_all_stripes(*bytenr)?;
        for phys in stripes {
            fs.device_mut().write_at(phys, block_bytes)?;
        }
    }

    // 3. Push all data and metadata blocks to medium before writing superblock.
    fs.device_mut().flush()?;

    // 3. Prepare updated superblock.
    let mut new_sb = fs.superblock().clone();
    new_sb.generation = txn.new_generation;
    new_sb.root = txn.final_root_bytenr;
    new_sb.root_level = txn.final_root_level;
    if let Some((chunk_root_bytenr, chunk_root_level)) = txn.final_chunk_root {
        new_sb.chunk_root = chunk_root_bytenr;
        new_sb.chunk_root_level = chunk_root_level;
        new_sb.chunk_root_generation = txn.new_generation;
    }
    new_sb.bytes_used = txn.final_bytes_used;
    // B5/M2: the superblock's embedded dev_item.bytes_used is made equal to the
    // chunk tree's DEV_ITEM value this transaction wrote (absolute, so a skew
    // left by an older build is healed); btrfs-progs `check_super_dev_item`
    // compares the two.
    if let Some(used) = txn.final_dev_bytes_used {
        new_sb.dev_bytes_used = used;
    }
    // Keep compat_ro_flags consistent with the emitted Free Space Tree.

    // Write superblock mirrors.
    let mut sb_started = false;
    for offset in SUPER_OFFSETS {
        if let Some(len) = dev_len {
            if offset + SUPER_SIZE as u64 > len {
                continue;
            }
        }
        let raw_copy = new_sb.emit_copy(&template, offset);
        // From the first mirror write on, the disk may hold this generation.
        sb_started = true;
        fs.device_mut()
            .write_at(offset, &raw_copy)
            .map_err(|error| CommitError { error, superblock_write_started: true })?;
    }

    // 4. Final flush ensuring superblock is committed to disk.
    fs.device_mut()
        .flush()
        .map_err(|error| CommitError { error, superblock_write_started: sb_started })?;

    // 5. Update in-memory mount state and clear node cache. Only now (B4) does
    // the live chunk map learn the chunks this transaction created.
    for c in &txn.new_chunks {
        fs.chunk_map_mut().insert(c.clone());
    }
    let tree = txn.new_fs_tree.objectid;
    fs.update_mount_state(new_sb, txn.new_fs_tree);

    crate::forensic::record_btrfs(crate::forensic::BtrfsEvent::Commit {
        tree,
        generation: txn.new_generation,
        transid: txn.new_generation,
        nodes_written: txn.pending_blocks.len(),
    });

    Ok(())
}
