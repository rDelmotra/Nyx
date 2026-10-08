//! A child tree block whose header generation differs from the generation its
//! parent recorded is a stale copy (a lost write) and must be refused, on read
//! and on write.
//!
//! ```text
//! cargo test -p luks_core --features dangerous-write-support --test btrfs_stale_block_gate
//! ```
//!
//! The checksum, address and fsid of a stale block are all valid, so only the
//! parent's key pointer can tell. The image is made by growing the fs tree of
//! `states-4k.img` to two levels through the writer, then rewriting the root
//! with every key pointer's generation off by one and a fresh checksum, so
//! the generation is the only thing wrong.

#![cfg(feature = "dangerous-write-support")]

#[path = "../common/mod.rs"]
mod common;

use common::mem_device::MemoryDevice;
use luks_core::device::{ReadAt, WriteAt};
use luks_core::error::LuksError;
use luks_core::fs::btrfs::tree::Node;
use luks_core::fs::btrfs::write::node::InteriorNode;
use luks_core::fs::btrfs::Btrfs;

const FIXTURE: &str = "btrfs/states-4k.img";
const DIR: &str = "/d";
const STALE: &str = "stale block";

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

/// Shift the generation recorded in every key pointer of the fs tree root,
/// re-checksumming the root so nothing else is wrong.
fn make_root_pointers_stale(dev: &MemoryDevice) {
    let fs = Btrfs::mount(dev.clone()).expect("mount");
    let sb = fs.superblock();
    let bytenr = fs.fs_tree().bytenr;
    let (phys, _) = fs.chunk_map().map(bytenr).expect("map");
    let mut raw = vec![0u8; sb.node_size as usize];
    dev.read_at(phys, &mut raw).expect("read root");
    let parsed = Node::parse(raw, bytenr, sb.csum_type, &sb.metadata_uuid).expect("parse");
    let mut root = InteriorNode::from_node(&parsed, sb.csum_type).expect("interior");
    for e in &mut root.entries {
        e.generation -= 1;
    }
    dev.write_at(phys, &root.emit(sb.node_size).expect("emit")).expect("write root");
}

fn is_stale(e: &LuksError) -> bool {
    matches!(e, LuksError::CorruptFs(m) if m.contains(STALE))
}

#[test]
fn control_unmodified_image_reads_and_writes() {
    let dev = two_level_image();
    let mut fs = Btrfs::mount(dev.clone()).expect("mount");
    assert_eq!(fs.list_dir(DIR).expect("list").len(), 400);
    assert_eq!(fs.read_file(&format!("{DIR}/{}", long_name(7))).expect("read"), vec![7u8; 64]);
    fs.create_file_with_data(DIR, "more.bin", &[1u8; 100]).expect("write");
    fs.delete_file(&format!("{DIR}/{}", long_name(3))).expect("delete");
    fs.commit_active_batch().expect("commit");
}

#[test]
fn stale_child_is_refused_on_read() {
    let dev = two_level_image();
    make_root_pointers_stale(&dev);
    let fs = Btrfs::mount(dev.clone()).expect("mount");
    let e = fs.list_dir(DIR).expect_err("list must refuse a stale child");
    assert!(is_stale(&e), "wrong error: {e}");
    let e = fs.read_file(&format!("{DIR}/{}", long_name(7))).expect_err("read must refuse");
    assert!(is_stale(&e), "wrong error: {e}");
    let e = fs.walk_tree(fs.fs_tree().bytenr, &mut |_, _| Ok(())).expect_err("walk must refuse");
    assert!(is_stale(&e), "wrong error: {e}");
}

#[test]
fn stale_child_is_refused_on_write_and_nothing_is_written() {
    let dev = two_level_image();
    make_root_pointers_stale(&dev);
    let before = dev.snapshot();

    let mut fs = Btrfs::mount(dev.clone()).expect("mount");
    let e = fs.create_directory(DIR, "sub").expect_err("mkdir must refuse");
    assert!(is_stale(&e), "wrong error: {e}");
    let e = fs.delete_file(&format!("{DIR}/{}", long_name(7))).expect_err("delete must refuse");
    assert!(is_stale(&e), "wrong error: {e}");
    let _ = fs.commit_active_batch();
    drop(fs);

    assert!(dev.snapshot() == before, "a refused write changed the image");
}

/// File data is written before the tree insert that names it, so a refused
/// create may leave bytes in free space; no metadata may change, so the file
/// must not exist afterwards.
#[test]
fn stale_child_refuses_file_create() {
    let dev = two_level_image();
    make_root_pointers_stale(&dev);
    let mut fs = Btrfs::mount(dev.clone()).expect("mount");
    let e = fs
        .create_file_with_data(DIR, &long_name(1000), &[9u8; 100])
        .expect_err("create must refuse");
    assert!(is_stale(&e), "wrong error: {e}");
    let _ = fs.commit_active_batch();
    drop(fs);

    let fs = Btrfs::mount(dev.clone()).expect("remount");
    let e = fs.read_file(&format!("{DIR}/{}", long_name(1000))).expect_err("tree is still refused");
    assert!(is_stale(&e), "wrong error: {e}");
}
