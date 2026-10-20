//! Volumes the writer must refuse by name: quotas on, or metadata that was
//! once shared (a deleted snapshot, a reflink copy, a balance).
//!
//! ```text
//! cargo test -p luks_core --features dangerous-write-support --test btrfs_shared_history_gate
//! ```
//!
//! # Why
//!
//! When the kernel copies a tree block flagged `FULL_BACKREF`, it rewrites the
//! backrefs of everything under it to name the new copy. This writer only
//! knows plain inline owner refs, so it would move the block and leave those
//! backrefs naming a freed address: `btrfs check` reports backref mismatches,
//! and a later kernel delete aborts the transaction. A data extent whose only
//! ref sits in a separate keyed item has the same problem on delete. Quota
//! counters are never updated by this writer, so they would go stale.
//!
//! None of these is normal on a personal single-disk drive, so the gate
//! (`write::gate::check_no_shared_history_or_quota`, `check_data_extent_freeable`)
//! refuses them before anything is written. Reading stays supported.
//!
//! The shared-history volumes are made by editing one extent-tree item of
//! `states-4k.img` in memory: making them for real needs a kernel mount
//! (snapshot, delete, wait for the cleaner). `quota-4k.img` is real, made by
//! `mkfs.btrfs -O quota`.

#![cfg(feature = "dangerous-write-support")]

#[path = "../common/mod.rs"]
mod common;

use common::mem_device::MemoryDevice;
use luks_core::device::WriteAt;
use luks_core::error::LuksError;
use luks_core::fs::btrfs::tree::{
    Key, Node, EXTENT_DATA_KEY, EXTENT_ITEM_KEY, EXTENT_TREE_OBJECTID, METADATA_ITEM_KEY,
    SHARED_BLOCK_REF_KEY, TREE_BLOCK_REF_KEY,
};
use luks_core::fs::btrfs::extent::FileExtent;
use luks_core::fs::btrfs::write::extent_tree::BLOCK_FLAG_FULL_BACKREF;
use luks_core::fs::btrfs::write::node::Leaf;
use luks_core::fs::btrfs::Btrfs;

/// Try every high-level write entry point; each must be refused with a
/// message containing `why`, and the device bytes must not change.
fn assert_every_write_refused(dev: &MemoryDevice, why: &str) {
    let before = dev.snapshot();
    let mut fs = Btrfs::mount(dev.clone()).expect("mount");
    let data = vec![0xABu8; 8192];

    let attempts: Vec<(&str, Result<(), LuksError>)> = vec![
        ("create_file_with_data", fs.create_file_with_data("/", "new.bin", &data).map(|_| ())),
        ("create_file", fs.create_file("/", "empty.bin")),
        ("create_directory", fs.create_directory("/", "newdir").map(|_| ())),
        ("begin_file", fs.begin_file(data.len() as u64).map(|_| ())),
        ("begin_file_streaming", fs.begin_file_streaming().map(|_| ())),
        ("set_mtime", fs.set_mtime("/", 1_700_000_000, 0)),
        ("allocate_data_chunk", fs.allocate_data_chunk().map(|_| ())),
    ];
    for (label, result) in attempts {
        match result {
            Err(LuksError::UnsupportedFsFeature(msg)) => {
                assert!(msg.contains(why), "{label}: refused for the wrong reason: {msg}");
            }
            other => panic!("{label}: expected a refusal mentioning {why:?}, got {other:?}"),
        }
    }
    drop(fs);
    assert!(dev.snapshot() == before, "a refused write changed the device");

    // Refusing writes must not cost reads.
    let fs = Btrfs::mount(dev.clone()).expect("remount");
    fs.list_dir("/").expect("read still works");
}

/// Rewrite the first extent-tree item matching `pick` with `edit`, in place.
///
/// Edits mirror 0 only, which is the copy this driver reads.
fn edit_extent_item(
    dev: &MemoryDevice,
    pick: impl Fn(&Key) -> bool,
    edit: impl Fn(&mut Vec<u8>),
) {
    let fs = Btrfs::mount(dev.clone()).expect("mount");
    let csum_type = fs.superblock().csum_type;
    let metadata_uuid = fs.superblock().metadata_uuid;
    let node_size = fs.superblock().node_size;
    let extent_root = fs.tree_root(EXTENT_TREE_OBJECTID).expect("extent root");

    // Walk leaf by leaf, from the root down the leftmost path and across.
    let mut stack = vec![extent_root.bytenr];
    while let Some(bytenr) = stack.pop() {
        let node = fs.read_node(bytenr).expect("read node");
        if node.level > 0 {
            for i in (0..node.nr_items).rev() {
                stack.push(node.key_ptr(i as usize).expect("ptr").blockptr);
            }
            continue;
        }
        let (phys, _) = fs.chunk_map().map(bytenr).expect("map");
        let mut raw = vec![0u8; node_size as usize];
        luks_core::device::ReadAt::read_at(&dev.clone(), phys, &mut raw).expect("read raw");
        let parsed = Node::parse(raw, bytenr, csum_type, &metadata_uuid).expect("parse");
        let mut leaf = Leaf::from_node(&parsed, csum_type).expect("leaf");
        if let Some(idx) = leaf.items.iter().position(|it| pick(&it.key)) {
            edit(&mut leaf.items[idx].data);
            let emitted = leaf.emit(node_size).expect("emit");
            dev.write_at(phys, &emitted).expect("write leaf");
            return;
        }
    }
    panic!("no extent-tree item matched");
}

fn fresh_with_one_file() -> MemoryDevice {
    let dev = MemoryDevice::from_fixture("btrfs/states-4k.img");
    let mut fs = Btrfs::mount(dev.clone()).expect("mount");
    fs.create_file_with_data("/", "kept.bin", &vec![0x5Au8; 64 * 1024]).expect("seed file");
    fs.commit_active_batch().expect("commit");
    dev
}

/// Control: the untouched volume accepts every entry point, so the refusals
/// below are about the edit and nothing else.
#[test]
fn control_clean_volume_accepts_writes() {
    let dev = fresh_with_one_file();
    let mut fs = Btrfs::mount(dev.clone()).expect("mount");
    fs.create_file_with_data("/", "more.bin", &[1u8; 8192]).expect("write");
    fs.create_directory("/", "dir").expect("mkdir");
    fs.set_mtime("/", 1_700_000_000, 0).expect("mtime");
    fs.allocate_data_chunk().expect("chunk");
    fs.delete_file("/kept.bin").expect("delete");
}

#[test]
fn quota_volume_is_refused() {
    let dev = MemoryDevice::from_fixture("btrfs/quota-4k.img");
    assert_every_write_refused(&dev, "quotas");
}

#[test]
fn full_backref_tree_block_is_refused() {
    let dev = fresh_with_one_file();
    edit_extent_item(
        &dev,
        |k| k.item_type == METADATA_ITEM_KEY,
        |data| {
            let flags = u64::from_le_bytes(data[16..24].try_into().unwrap()) | BLOCK_FLAG_FULL_BACKREF;
            data[16..24].copy_from_slice(&flags.to_le_bytes());
        },
    );
    assert_every_write_refused(&dev, "FULL_BACKREF");
}

#[test]
fn shared_tree_block_backref_is_refused() {
    let dev = fresh_with_one_file();
    // Same payload size (8 bytes): owner root becomes parent address.
    edit_extent_item(
        &dev,
        |k| k.item_type == METADATA_ITEM_KEY,
        |data| {
            assert_eq!(data[24], TREE_BLOCK_REF_KEY, "fixture tree blocks carry one inline owner ref");
            data[24] = SHARED_BLOCK_REF_KEY;
        },
    );
    assert_every_write_refused(&dev, "shared tree block backref");
}

/// A data extent whose one ref lives outside its `EXTENT_ITEM`: other writes
/// are fine, deleting or overwriting that file is refused.
#[test]
fn data_extent_with_keyed_ref_is_not_freed() {
    let dev = fresh_with_one_file();
    let disk_bytenr = {
        let fs = Btrfs::mount(dev.clone()).expect("mount");
        let ino = fs.resolve_no_follow(fs.fs_tree(), "/kept.bin").expect("resolve").inode.objectid;
        let key = Key::new(ino, EXTENT_DATA_KEY, 0);
        let data = fs.find_item(fs.fs_tree().bytenr, &key).expect("search").expect("extent data");
        FileExtent::parse(0, &data).expect("parse").disk_bytenr
    };
    edit_extent_item(
        &dev,
        |k| k.item_type == EXTENT_ITEM_KEY && k.objectid == disk_bytenr,
        |data| data.truncate(24),
    );

    let before = dev.snapshot();
    let mut fs = Btrfs::mount(dev.clone()).expect("mount");
    match fs.delete_file("/kept.bin") {
        Err(LuksError::UnsupportedFsFeature(msg)) => {
            assert!(msg.contains("not a single inline file ref"), "wrong reason: {msg}")
        }
        other => panic!("delete must be refused, got {other:?}"),
    }
    fs.create_file_with_data("/", "other.bin", &[2u8; 4096]).expect("seed rename source");
    let after_seed = dev.snapshot();
    match fs.rename("/", "other.bin", "/", "kept.bin") {
        Err(LuksError::UnsupportedFsFeature(msg)) => {
            assert!(msg.contains("not a single inline file ref"), "wrong reason: {msg}")
        }
        other => panic!("rename over it must be refused, got {other:?}"),
    }
    drop(fs);
    assert!(before != after_seed, "the unrelated write landed");
    assert!(dev.snapshot() == after_seed, "a refused delete or rename changed the device");
}
