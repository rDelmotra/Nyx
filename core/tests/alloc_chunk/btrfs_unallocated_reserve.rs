//! The writer only ever allocates DATA chunks, so filling a disk must never
//! turn the last unallocated device space into data chunks: the kernel needs
//! that space to allocate a METADATA chunk, and without it the filesystem
//! goes read-only with metadata ENOSPC.
//!
//! ```text
//! cargo test -p luks_core --features dangerous-write-support --release --test btrfs_unallocated_reserve
//! ```
#![cfg(feature = "dangerous-write-support")]

#[path = "../common/mod.rs"]
mod common;

use common::mem_device::MemoryDevice;
use common::states::{fresh_from, judge, mount, payload, Depth};
use luks_core::error::LuksError;
use luks_core::fs::btrfs::write::chunk_alloc::{
    decide_metadata_chunk_size, unallocated_dev_bytes, unallocated_reserve,
};
use luks_core::fs::btrfs::Btrfs;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;

fn unallocated(fs: &Btrfs<MemoryDevice>) -> u64 {
    let sb = fs.superblock();
    unallocated_dev_bytes(sb.total_bytes, &fs.read_dev_extents(sb.dev_id).unwrap())
}

#[test]
fn reserve_rule_numbers() {
    // Kernel metadata chunk: 256 MiB up to 50 GiB, 1 GiB above, <= 10% of device.
    assert_eq!(decide_metadata_chunk_size(256 * MIB), 256 * MIB / 10 / 65536 * 65536);
    assert_eq!(decide_metadata_chunk_size(20 * GIB), 256 * MIB);
    assert_eq!(decide_metadata_chunk_size(500 * GIB), GIB);
    // max(1 GiB, 2 * metadata chunk), capped at 10% of the device.
    assert_eq!(unallocated_reserve(20 * GIB), GIB);
    assert_eq!(unallocated_reserve(50 * GIB), GIB);
    assert_eq!(unallocated_reserve(500 * GIB), 2 * GIB);
    assert_eq!(unallocated_reserve(4 * GIB), 4 * GIB / 10 / 65536 * 65536);
    let small = unallocated_reserve(256 * MIB);
    assert!(small > 0 && small <= 256 * MIB / 10, "small device reserve {small}");
}

fn fill_until_refused(fixture: &str) {
    let dev = fresh_from(fixture);
    let tag = fixture.replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    let mut fs = mount(&dev);
    let reserve = unallocated_reserve(fs.superblock().total_bytes);
    assert!(unallocated(&fs) > reserve, "fixture must start above the reserve");

    let mut written: Vec<(String, Vec<u8>)> = Vec::new();
    let refusal = loop {
        let i = written.len();
        assert!(i < 400, "never refused");
        let data = payload(8 * MIB as usize, i as u64 + 1);
        let name = format!("fill{i:03}");
        match fs.create_file_with_data("/", &name, &data) {
            Ok(_) => written.push((format!("/{name}"), data)),
            Err(e) => break e,
        }
    };
    assert!(matches!(refusal, LuksError::FilesystemFull), "refusal was {refusal:?}");
    assert!(!written.is_empty());
    assert!(
        unallocated(&fs) >= reserve,
        "unallocated {} fell below reserve {reserve}",
        unallocated(&fs)
    );
    fs.commit_active_batch().expect("commit after refusal");
    judge(&dev, &format!("after_refusal_{tag}"), Depth::Full);

    // Reads of everything written before the refusal.
    let fs = mount(&dev);
    for (p, d) in &written {
        assert_eq!(&fs.read_file(p).expect("read back"), d, "{p}");
    }
    assert!(unallocated(&fs) >= reserve);

    // Not wedged: metadata-only operations still work, and so does a tiny file.
    let mut fs = mount(&dev);
    fs.create_directory("/", "after_full").expect("mkdir after refusal");
    fs.delete_file(&written[0].0).expect("delete after refusal");
    judge(&dev, &format!("after_mkdir_delete_{tag}"), Depth::Full);
    let mut fs = mount(&dev);
    let big = payload(8 * MIB as usize, 9999);
    // Deleting freed one file's worth of data space: it fits again.
    fs.create_file_with_data("/", "again", &big).expect("space freed by delete is reusable");
    assert!(unallocated(&fs) >= reserve);
    judge(&dev, &format!("after_reuse_{tag}"), Depth::Full);
}

#[test]
fn states_image_fill_stops_at_reserve() {
    fill_until_refused("btrfs/states-4k.img");
}

#[test]
fn fedora_image_fill_stops_at_reserve() {
    fill_until_refused("btrfs/fedora-4k.img");
}

/// A request that cannot fit is refused before any chunk is committed.
#[test]
fn oversized_request_is_refused_before_anything_is_written() {
    let dev = fresh_from("btrfs/states-4k.img");
    let mut fs = mount(&dev);
    let before = dev.snapshot();
    let gen = fs.superblock().generation;
    let err = fs.begin_file(400 * MIB).unwrap_err();
    assert!(matches!(err, LuksError::FilesystemFull), "{err:?}");
    assert_eq!(dev.snapshot(), before, "refusal wrote to the device");
    assert_eq!(fs.superblock().generation, gen);
    judge(&dev, "oversized", Depth::Ours);
}
