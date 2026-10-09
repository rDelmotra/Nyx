//! The refusal gate for btrfs write operations.
//!
//! Every condition that would make writing unsafe or unsupported is checked here
//! before any allocation, CoW path computation, or device access begins.

use crate::error::{LuksError, Result};
use crate::fs::btrfs::superblock::{
    Superblock, BTRFS_FEATURE_COMPAT_RO_FREE_SPACE_TREE,
    BTRFS_FEATURE_COMPAT_RO_FREE_SPACE_TREE_VALID,
};
use crate::fs::btrfs::TreeRoot;

pub use crate::fs::btrfs::superblock::BTRFS_SYSTEM_CHUNK_ARRAY_SIZE;

/// Minimum byte size required to store a new SYSTEM chunk in `sys_chunk_array`.
///
/// In btrfs, a chunk entry in `sys_chunk_array` consists of a 17-byte `btrfs_disk_key`
/// followed by a `btrfs_chunk` item (48-byte header + 32 bytes per stripe).
/// For a 2-stripe (DUP) profile: 17 + 48 + (2 * 32) = 129 bytes.
pub const BTRFS_SYSTEM_CHUNK_ENTRY_SIZE: usize = 129;

/// Refuse chunk allocations if `sys_chunk_array` cannot fit another SYSTEM chunk.
///
/// The bootstrap `sys_chunk_array` is statically capped at 2048 bytes (`BTRFS_SYSTEM_CHUNK_ARRAY_SIZE`).
/// If allocating a chunk could necessitate adding a new SYSTEM chunk and `sys_chunk_array`
/// does not have sufficient remaining capacity (at least 129 bytes for a 2-stripe chunk item),
/// refuse before attempting allocation.
///
/// **This is a static precondition on pre-existing on-disk state, not a control this writer's
/// own behaviour can currently trigger.** The writer only ever allocates `DATA|single` chunks —
/// it never allocates SYSTEM chunks itself — so the scenario this check guards against (a chunk
/// allocation that would need to grow `sys_chunk_array`) is not reachable by any code path in
/// this codebase today. The check is still correct to keep: it protects against a `sys_chunk_array`
/// that arrived already near-full from whatever created the filesystem. Audited 2026-08-17
/// (`notes/feature-remediation.md` §5, item I.4) and accepted as-is rather than removed.
pub fn check_sys_chunk_array_capacity(sb: &Superblock) -> Result<()> {
    let current_size = sb.sys_chunk_array.len();
    if current_size + BTRFS_SYSTEM_CHUNK_ENTRY_SIZE > BTRFS_SYSTEM_CHUNK_ARRAY_SIZE {
        return Err(LuksError::UnsupportedFsFeature(format!(
            "btrfs sys_chunk_array capacity exhausted ({current_size}/{BTRFS_SYSTEM_CHUNK_ARRAY_SIZE} bytes used, needs {BTRFS_SYSTEM_CHUNK_ENTRY_SIZE} bytes for system chunk entry)"
        )));
    }
    Ok(())
}

/// All `compat_ro` flags our write path can safely handle.
///
/// Any other flag (e.g. block-group-tree 0x4, verity 0x8) is refused.
pub const SUPPORTED_WRITE_COMPAT_RO: u64 =
    BTRFS_FEATURE_COMPAT_RO_FREE_SPACE_TREE | BTRFS_FEATURE_COMPAT_RO_FREE_SPACE_TREE_VALID;

/// Check that the filesystem geometry and superblock state permit writing.
pub fn check_writeable_fs(sb: &Superblock) -> Result<()> {
    // 1. Filesystem must have been unmounted cleanly.
    if sb.log_root != 0 {
        return Err(LuksError::FsNeedsRecovery);
    }

    // 2. Extent bookkeeping requires skinny metadata format.
    if !sb.has_skinny_metadata() {
        return Err(LuksError::UnsupportedFsFeature(
            "btrfs non-skinny metadata".into(),
        ));
    }

    // 3. Reject unrecognised compat_ro flags.
    let unsupported_compat_ro = sb.compat_ro_flags & !SUPPORTED_WRITE_COMPAT_RO;
    if unsupported_compat_ro != 0 {
        return Err(LuksError::UnsupportedFsFeature(format!(
            "btrfs unsupported compat_ro flags: 0x{unsupported_compat_ro:x}"
        )));
    }

    // 4. Multi-device filesystems are single-device only for writing.
    if sb.num_devices != 1 {
        return Err(LuksError::UnsupportedFsFeature(format!(
            "btrfs spanning {} devices",
            sb.num_devices
        )));
    }

    // 5. Simple quotas: new data extents would need an owner ref this
    //    writer does not emit, and the squota counters would go stale.
    //    Regular qgroups have no superblock flag; see
    //    `check_no_shared_history_or_quota`.
    if sb.has_simple_quota() {
        return Err(LuksError::UnsupportedFsFeature(
            "btrfs simple quotas; this filesystem can be read but not written".into(),
        ));
    }

    // 6. Only crc32c checksums can be written correctly.
    //
    // `CsumType::Sha256` parses, mounts and *reads* correctly — `csum_type`
    // drives verification through `Superblock::verify_csum`, and the read path
    // is size-agnostic. The write path is not. `FileWriter::checksums` is
    // `Vec<(u64, u32)>` (`write/file.rs:15`) — one `u32` per sector, which
    // cannot hold a 32-byte digest — and the streaming builder that consumes
    // it, `build_extent_csum_items_from_checksums` (`write/node.rs:701`),
    // hardcodes the checksum width: `max_csum_item_bytes(node_size, 4)`,
    // `sectors_per_item = max_item / 4`, and a 4-byte `crc.to_le_bytes()`
    // payload per sector. That builder is what `Batch::commit` uses
    // (`write/batch.rs:624`), so it is the path every streamed file takes —
    // which is every file the Android app transfers.
    //
    // Measured 2026-09-09 on a `mkfs.btrfs --csum sha256` fixture: writing 40
    // files of 1,536,000 bytes emitted `total csum bytes: 60000` where 480,000
    // were required (40 x 375 sectors x 32 bytes), and `btrfs check` reported
    // `some csum missing` against every one of inodes 257-296. The write
    // reported success. Nothing refused it.
    //
    // Refusing by name is this module's convention for a shape we understand
    // but have not implemented, and it is strictly better than the alternative
    // here: a silently unverifiable filesystem is the one outcome this driver
    // treats as unacceptable. Reading such a volume stays fully supported.
    if !matches!(sb.csum_type, crate::fs::btrfs::superblock::CsumType::Crc32c) {
        return Err(LuksError::UnsupportedFsFeature(format!(
            "btrfs write on {:?} checksums (read is supported; only crc32c can be written)",
            sb.csum_type
        )));
    }

    Ok(())
}

/// Refuse a filesystem whose extent tree shows shared history, or that keeps
/// quota accounting.
///
/// Shared history: a tree block flagged `FULL_BACKREF`, or a shared, keyed or
/// unknown backref (`ExtentTree::shared_history`). The kernel leaves these
/// behind after a snapshot is deleted, after a reflink copy, or after a
/// balance. When the kernel CoWs such a block it rewrites the children's
/// backrefs to name the new copy (`update_ref_for_cow`); this writer only
/// knows plain inline owner refs and does not, so those backrefs would keep
/// naming a freed block. `refs > 1` is refused separately where it is read.
///
/// Quotas: this writer does not update qgroup counters, so a quota tree
/// would go stale (`btrfs check`: "counts for qgroup ... are different").
///
/// Both are rare on a single-disk personal drive, so refusing by name is the
/// right trade. Reading such a filesystem stays fully supported.
pub fn check_no_shared_history_or_quota<D: crate::device::ReadAt>(
    fs: &crate::fs::btrfs::Btrfs<D>,
    extent_tree: &crate::fs::btrfs::write::ExtentTree,
) -> Result<()> {
    if let Some(why) = &extent_tree.shared_history {
        return Err(LuksError::UnsupportedFsFeature(format!(
            "btrfs metadata with snapshot/reflink history ({why}); this filesystem can be read but not written"
        )));
    }
    if fs.find_tree_root(crate::fs::btrfs::tree::QUOTA_TREE_OBJECTID)?.is_some() {
        return Err(LuksError::UnsupportedFsFeature(
            "btrfs quotas (qgroups) enabled; this filesystem can be read but not written".into(),
        ));
    }
    Ok(())
}

/// Refuse to free a data extent unless its only reference is the plain inline
/// one this writer would remove with it.
///
/// With `refs == 1` the reference can still live in a separate keyed item
/// (left behind after a reflink copy was deleted); dropping only the
/// `EXTENT_ITEM` would orphan it.
pub fn check_data_extent_freeable(
    ext: &crate::fs::btrfs::write::AllocatedExtent,
) -> Result<()> {
    use crate::fs::btrfs::write::InlineBackref;
    if ext.refs > 1 {
        return Err(LuksError::UnsupportedFsFeature(
            "freeing a file with shared data extents (refs > 1) is not supported".into(),
        ));
    }
    if !matches!(ext.backrefs.as_slice(), [InlineBackref::ExtentData { count: 1, .. }]) {
        return Err(LuksError::UnsupportedFsFeature(format!(
            "freeing data extent {} whose backref is not a single inline file ref is not supported",
            ext.bytenr
        )));
    }
    Ok(())
}

/// Check that the target subvolume is writeable.
pub fn check_writeable_subvolume(root: &TreeRoot) -> Result<()> {
    if root.is_read_only() {
        return Err(LuksError::UnsupportedFsFeature(
            "btrfs read-only subvolume".into(),
        ));
    }
    Ok(())
}


/// Refuse chunk allocations for profiles other than `DATA|single`.
///
/// Refuses METADATA, SYSTEM, DUP, RAID0, RAID1, RAID10, RAID5, RAID6, etc.
pub fn check_chunk_allocation_profile(flags: u64) -> Result<()> {
    if flags != crate::fs::btrfs::chunk::BLOCK_GROUP_DATA {
        return Err(LuksError::UnsupportedFsFeature(format!(
            "btrfs chunk allocation only supports DATA|single profile (0x1), got flags 0x{flags:x}"
        )));
    }
    Ok(())
}

/// Refuse a free-space tree that contains `FREE_SPACE_BITMAP` (key type 200) items.
///
/// Our free-space tree emitter only produces extent items (`FREE_SPACE_EXTENT`).
/// If bitmap items are present, rewriting the tree would lose fine-grained allocation state.
pub fn check_free_space_tree_no_bitmaps<D: crate::device::ReadAt>(
    fs: &crate::fs::btrfs::Btrfs<D>,
) -> Result<()> {
    if fs.superblock().compat_ro_flags & BTRFS_FEATURE_COMPAT_RO_FREE_SPACE_TREE != 0 {
        if let Ok(fst_root) = fs.tree_root(crate::fs::btrfs::tree::FREE_SPACE_TREE_OBJECTID) {
            fs.walk_tree(fst_root.bytenr, &mut |key, _| {
                if key.item_type == crate::fs::btrfs::tree::FREE_SPACE_BITMAP_KEY {
                    return Err(LuksError::UnsupportedFsFeature(
                        "btrfs free-space tree bitmap items (key type 200) not supported".into(),
                    ));
                }
                Ok(())
            })?;
        }
    }
    Ok(())
}

/// Maximum length in bytes of a btrfs directory entry name (`BTRFS_NAME_LEN`).
pub const BTRFS_NAME_LEN: usize = 255;

/// Validate a name about to become a directory entry (file, directory, or the
/// destination of a rename).
///
/// Refuses: empty, `.`, `..`, any `/` or NUL, and anything longer than
/// [`BTRFS_NAME_LEN`] **bytes** (UTF-8 length, not characters). Must run before
/// anything is written. The error text never includes the name (it crosses into
/// logs); it reuses `UnsupportedFsFeature`, which the JNI layer reports as code
/// `UNSUPPORTED`.
pub fn check_entry_name(name: &str) -> Result<()> {
    let reason = if name.is_empty() {
        "is empty"
    } else if name == "." || name == ".." {
        "is a reserved name"
    } else if name.contains('/') {
        "contains '/'"
    } else if name.contains('\0') {
        "contains a NUL byte"
    } else if name.len() > BTRFS_NAME_LEN {
        "is longer than 255 bytes"
    } else {
        return Ok(());
    };
    Err(LuksError::UnsupportedFsFeature(format!(
        "invalid btrfs file name: name {reason}"
    )))
}

/// `BTRFS_INODE_IMMUTABLE` (`1 << 6`) from the kernel's `btrfs_tree.h`
/// (`chattr +i`). Note `1 << 4` is `PREALLOC` and `1 << 5` is `SYNC`.
pub const BTRFS_INODE_IMMUTABLE: u64 = 1 << 6;
/// `BTRFS_INODE_APPEND` (`1 << 7`, `chattr +a`).
pub const BTRFS_INODE_APPEND: u64 = 1 << 7;

/// Refuse to remove, replace, rename, or otherwise modify an inode (or remove
/// an entry from a directory) whose `INODE_ITEM.flags` carry immutable or
/// append-only. `flags` is `Inode::flags`; `what` is a static description for
/// the message (never a user name).
pub fn check_inode_mutable(flags: u64, what: &str) -> Result<()> {
    if flags & (BTRFS_INODE_IMMUTABLE | BTRFS_INODE_APPEND) != 0 {
        return Err(LuksError::UnsupportedFsFeature(format!(
            "{what} is immutable or append-only"
        )));
    }
    Ok(())
}

/// Refuse to add an entry to a directory flagged immutable. Append-only
/// directories still allow creation (but not removal/rename: use
/// [`check_inode_mutable`] for those).
pub fn check_dir_allows_create(dir_flags: u64) -> Result<()> {
    if dir_flags & BTRFS_INODE_IMMUTABLE != 0 {
        return Err(LuksError::UnsupportedFsFeature(
            "directory is immutable".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::FileDevice;
    use crate::fs::btrfs::chunk::{BLOCK_GROUP_DATA, BLOCK_GROUP_DUP, BLOCK_GROUP_METADATA, BLOCK_GROUP_SYSTEM};
    use crate::fs::btrfs::Btrfs;

    fn dummy_superblock() -> Superblock {
        Superblock {
            fsid: [0; 16],
            bytenr: 0x10000,
            generation: 1,
            root: 1048576,
            chunk_root: 2097152,
            chunk_root_generation: 1,
            log_root: 0,
            total_bytes: 104857600,
            bytes_used: 1048576,
            root_dir_objectid: 6,
            num_devices: 1,
            dev_id: 1,
            dev_bytes_used: 0,
            sector_size: 4096,
            node_size: 16384,
            stripe_size: 65536,
            incompat_flags: 0x100,
            compat_ro_flags: SUPPORTED_WRITE_COMPAT_RO,
            csum_type: crate::fs::btrfs::superblock::CsumType::Crc32c,
            root_level: 0,
            chunk_root_level: 0,
            label: "test".into(),
            metadata_uuid: [0; 16],
            sys_chunk_array: vec![],
        }
    }

    #[test]
    fn test_chunk_allocation_profile_gate() {
        assert!(check_chunk_allocation_profile(BLOCK_GROUP_DATA).is_ok());
        assert!(check_chunk_allocation_profile(BLOCK_GROUP_METADATA).is_err());
        assert!(check_chunk_allocation_profile(BLOCK_GROUP_SYSTEM).is_err());
        assert!(check_chunk_allocation_profile(BLOCK_GROUP_DATA | BLOCK_GROUP_DUP).is_err());
        assert!(check_chunk_allocation_profile(0).is_err());
    }

    #[test]
    fn test_simple_quota_refused() {
        let mut sb = dummy_superblock();
        assert!(check_writeable_fs(&sb).is_ok());
        sb.incompat_flags |= 0x8000; // SIMPLE_QUOTA
        match check_writeable_fs(&sb) {
            Err(LuksError::UnsupportedFsFeature(msg)) => assert!(msg.contains("simple quotas"), "{msg}"),
            other => panic!("expected simple-quota refusal, got {other:?}"),
        }
    }

    #[test]
    fn test_free_space_tree_no_bitmaps_on_fixtures() {
        let fixtures = ["plain.img", "compress.img", "mixed-4k.img", "subvol.img"];
        for name in fixtures {
            let path = format!("{}/../fixtures/btrfs/{name}", env!("CARGO_MANIFEST_DIR"));
            let dev = FileDevice::open(&path).expect("open fixture");
            let fs = Btrfs::mount(dev).expect("mount fixture");
            assert!(check_free_space_tree_no_bitmaps(&fs).is_ok());
        }
    }

    #[test]
    fn test_check_sys_chunk_array_capacity() {
        let mut sb = dummy_superblock();

        // 1. Empty sys_chunk_array (0 bytes) -> OK
        assert!(check_sys_chunk_array_capacity(&sb).is_ok());

        // 2. Fits exactly at limit (2048 - 129 = 1919 bytes) -> OK
        sb.sys_chunk_array = vec![0u8; BTRFS_SYSTEM_CHUNK_ARRAY_SIZE - BTRFS_SYSTEM_CHUNK_ENTRY_SIZE];
        assert!(check_sys_chunk_array_capacity(&sb).is_ok());

        // 3. Deliberate break: Near 2048 (1920 bytes, 1 byte too large) -> Refused
        sb.sys_chunk_array = vec![0u8; BTRFS_SYSTEM_CHUNK_ARRAY_SIZE - BTRFS_SYSTEM_CHUNK_ENTRY_SIZE + 1];
        match check_sys_chunk_array_capacity(&sb) {
            Err(LuksError::UnsupportedFsFeature(msg)) => {
                assert!(
                    msg.contains("sys_chunk_array capacity exhausted"),
                    "unexpected message: {msg}"
                );
            }
            res => panic!("expected UnsupportedFsFeature, got {res:?}"),
        }

        // 4. Deliberate break: Full 2048 bytes -> Refused
        sb.sys_chunk_array = vec![0u8; BTRFS_SYSTEM_CHUNK_ARRAY_SIZE];
        assert!(check_sys_chunk_array_capacity(&sb).is_err());
    }
}
