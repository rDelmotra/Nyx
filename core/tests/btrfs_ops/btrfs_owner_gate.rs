//! A node reached while descending tree X for a write must be stamped with
//! owner X. The write gate has already refused volumes with shared-block
//! history, so a foreign owner means another tree's pages are linked into the
//! tree being modified. Writes refuse; reads must keep working, because
//! blocks are legitimately shared between trees on snapshot volumes.
//!
//! ```text
//! cargo test -p luks_core --features dangerous-write-support --test btrfs_owner_gate
//! ```
//!
//! The image is made by growing the fs tree of `states-4k.img` to two levels
//! through the writer, then rewriting one node with a different `owner` and a
//! fresh checksum, so the owner is the only thing wrong.

#![cfg(feature = "dangerous-write-support")]

#[path = "../common/mod.rs"]
mod common;

use common::mem_device::MemoryDevice;
use luks_core::device::{ReadAt, WriteAt};
use luks_core::error::LuksError;
use luks_core::fs::btrfs::tree::Node;
use luks_core::fs::btrfs::write::node::{InteriorNode, Leaf};
use luks_core::fs::btrfs::Btrfs;

const FIXTURE: &str = "btrfs/states-4k.img";
const DIR: &str = "/d";
const MISMATCH: &str = "owner does not match";

fn long_name(i: usize) -> String {
    format!("{i:05}-{}", "n".repeat(150))
}

/// An image whose fs tree has an interior root.
fn two_level_image() -> MemoryDevice {
    let dev = MemoryDevice::from_fixture(FIXTURE);
    let mut fs = Btrfs::mount(dev.clone()).expect("mount");
    fs.create_directory("/", "d").expect("mkdir");
    for i in 0..400 {
        fs.create_file_with_data(DIR, &long_name(i), &[i as u8; 64]).expect("write");
    }
    fs.commit_active_batch().expect("commit");
    drop(fs);
    let fs = Btrfs::mount(dev.clone()).expect("remount");
    let root = fs.read_node(fs.fs_tree().bytenr).expect("root");
    assert!(root.level >= 1, "fs tree must have an interior root, level {}", root.level);
    dev
}

/// Rewrite the node at `bytenr` with `owner` stamped and a fresh checksum.
fn set_owner(dev: &MemoryDevice, bytenr: u64, owner: u64) {
    let fs = Btrfs::mount(dev.clone()).expect("mount");
    let sb = fs.superblock();
    let (phys, _) = fs.chunk_map().map(bytenr).expect("map");
    let mut raw = vec![0u8; sb.node_size as usize];
    dev.read_at(phys, &mut raw).expect("read node");
    let parsed = Node::parse(raw, bytenr, sb.csum_type, &sb.metadata_uuid).expect("parse");
    let out = if parsed.is_leaf() {
        let mut n = Leaf::from_node(&parsed, sb.csum_type).expect("leaf");
        n.owner = owner;
        n.emit(sb.node_size).expect("emit")
    } else {
        let mut n = InteriorNode::from_node(&parsed, sb.csum_type).expect("interior");
        n.owner = owner;
        n.emit(sb.node_size).expect("emit")
    };
    dev.write_at(phys, &out).expect("write node");
}

fn fs_root(dev: &MemoryDevice) -> u64 {
    Btrfs::mount(dev.clone()).expect("mount").fs_tree().bytenr
}

/// Stamp every child of the fs tree root with a foreign owner.
fn foreign_owner_on_children(dev: &MemoryDevice, owner: u64) {
    let fs = Btrfs::mount(dev.clone()).expect("mount");
    let root = fs.read_node(fs.fs_tree().bytenr).expect("root");
    let kids: Vec<u64> = (0..root.nr_items)
        .map(|i| root.key_ptr(i).expect("ptr").blockptr)
        .collect();
    drop(fs);
    assert!(!kids.is_empty());
    for k in kids {
        set_owner(dev, k, owner);
    }
}

fn is_mismatch(e: &LuksError) -> bool {
    matches!(e, LuksError::CorruptFs(m) if m.contains(MISMATCH))
}

fn assert_reads_still_work(dev: &MemoryDevice) {
    let fs = Btrfs::mount(dev.clone()).expect("mount");
    assert_eq!(fs.list_dir(DIR).expect("list").len(), 400);
    assert_eq!(fs.read_file(&format!("{DIR}/{}", long_name(7))).expect("read"), vec![7u8; 64]);
    fs.walk_tree(fs.fs_tree().bytenr, &mut |_, _| Ok(())).expect("walk");
}

/// Every write kind must refuse and leave the image byte-identical.
fn assert_writes_refused_and_image_unchanged(dev: &MemoryDevice) {
    let before = dev.snapshot();
    let mut fs = Btrfs::mount(dev.clone()).expect("mount");
    let e = fs.create_directory(DIR, "sub").expect_err("mkdir must refuse");
    assert!(is_mismatch(&e), "wrong error: {e}");
    let e = fs.delete_file(&format!("{DIR}/{}", long_name(7))).expect_err("delete must refuse");
    assert!(is_mismatch(&e), "wrong error: {e}");
    let _ = fs.commit_active_batch();
    drop(fs);
    assert!(dev.snapshot() == before, "a refused write changed the image");
}

#[test]
fn control_unmodified_image_reads_and_writes() {
    let dev = two_level_image();
    let mut fs = Btrfs::mount(dev.clone()).expect("mount");
    assert_eq!(fs.list_dir(DIR).expect("list").len(), 400);
    fs.create_file_with_data(DIR, "more.bin", &[1u8; 100]).expect("write");
    fs.create_directory(DIR, "sub").expect("mkdir");
    fs.delete_file(&format!("{DIR}/{}", long_name(3))).expect("delete");
    fs.commit_active_batch().expect("commit");
}

/// Our own CoW output must carry the right owner, or a second round of writes
/// over the first would be refused.
#[test]
fn own_writes_are_stamped_and_rewritable() {
    let dev = two_level_image();
    for round in 0..3 {
        let mut fs = Btrfs::mount(dev.clone()).expect("mount");
        fs.create_file_with_data(DIR, &format!("r{round}.bin"), &[round as u8; 5000]).expect("write");
        fs.delete_file(&format!("{DIR}/{}", long_name(round))).expect("delete");
        fs.commit_active_batch().expect("commit");
    }
    let fs = Btrfs::mount(dev.clone()).expect("mount");
    let root = fs.read_node(fs.fs_tree().bytenr).expect("root");
    assert_eq!(root.owner, 5);
    fs.walk_tree(fs.fs_tree().bytenr, &mut |_, _| Ok(())).expect("walk");
}

#[test]
fn foreign_owner_on_child_refuses_writes_but_not_reads() {
    for foreign in [256u64, 1] {
        let dev = two_level_image();
        foreign_owner_on_children(&dev, foreign);
        assert_reads_still_work(&dev);
        assert_writes_refused_and_image_unchanged(&dev);
        assert_reads_still_work(&dev);
    }
}

/// File data is written before the tree insert that names it, so a refused
/// create may leave bytes in unreferenced free space; no metadata may change,
/// so the file must not exist afterwards.
#[test]
fn foreign_owner_on_child_refuses_file_create() {
    let dev = two_level_image();
    foreign_owner_on_children(&dev, 256);
    let mut fs = Btrfs::mount(dev.clone()).expect("mount");
    let e = fs
        .create_file_with_data(DIR, &long_name(1000), &[9u8; 100])
        .expect_err("create must refuse");
    assert!(is_mismatch(&e), "wrong error: {e}");
    let _ = fs.commit_active_batch();
    drop(fs);

    let fs = Btrfs::mount(dev.clone()).expect("remount");
    assert_eq!(fs.list_dir(DIR).expect("list").len(), 400);
    assert!(fs.read_file(&format!("{DIR}/{}", long_name(1000))).is_err());
}

#[test]
fn foreign_owner_on_fs_root_refuses_writes_but_not_reads() {
    for foreign in [256u64, 1] {
        let dev = two_level_image();
        set_owner(&dev, fs_root(&dev), foreign);
        assert_reads_still_work(&dev);
        assert_writes_refused_and_image_unchanged(&dev);
        assert_reads_still_work(&dev);
    }
}

#[test]
fn foreign_owner_on_extent_tree_root_refuses_writes() {
    let dev = two_level_image();
    let bytenr = Btrfs::mount(dev.clone()).expect("mount").tree_root(2).expect("extent root").bytenr;
    set_owner(&dev, bytenr, 5);
    let before = dev.snapshot();
    let mut fs = Btrfs::mount(dev.clone()).expect("mount");
    let e = fs.create_directory(DIR, "sub").expect_err("mkdir must refuse");
    assert!(is_mismatch(&e), "wrong error: {e}");
    let _ = fs.commit_active_batch();
    drop(fs);
    assert!(dev.snapshot() == before, "a refused write changed the image");
}

#[test]
fn foreign_owner_on_csum_tree_root_refuses_file_write() {
    let dev = two_level_image();
    let Some(csum) = Btrfs::mount(dev.clone()).expect("mount").tree_root(7).ok() else {
        return;
    };
    set_owner(&dev, csum.bytenr, 5);
    let mut fs = Btrfs::mount(dev.clone()).expect("mount");
    let e = fs
        .create_file_with_data(DIR, "c.bin", &[3u8; 8192])
        .and_then(|_| fs.commit_active_batch())
        .expect_err("csum-bearing write must refuse");
    assert!(is_mismatch(&e), "wrong error: {e}");
}
