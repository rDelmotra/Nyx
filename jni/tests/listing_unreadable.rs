//! A listing reports how many entries had an unreadable inode (`unreadable`),
//! in both the full and the paged JSON.
//!
//! ```text
//! cargo test -p luks_jni --features dangerous-write-support --test listing_unreadable
//! ```
#![cfg(feature = "dangerous-write-support")]

#[path = "../../core/tests/common/temp_guard.rs"]
mod temp_guard;
use temp_guard::TempStr;

use luks_core::device::FileDevice;
use luks_core::fs::{FileType, MountedFs};
use luks_jni::bridge::{DeviceHandle, VolumeHandle};
use serde_json::Value;

const PASSWORD: &[u8] = b"test";

fn scratch(fixture: &str, test: &str) -> TempStr {
    let src = format!("{}/../fixtures/disks/{fixture}", env!("CARGO_MANIFEST_DIR"));
    let dst = std::env::temp_dir().join(format!("luks-jni-unreadable-{test}.img"));
    std::fs::copy(src, &dst).expect("copy fixture");
    TempStr::new(dst.to_string_lossy().into_owned())
}

fn unlock(path: &str) -> VolumeHandle {
    let len = std::fs::metadata(path).expect("stat").len();
    let dev = FileDevice::open_writable(path, len).expect("open writable");
    let handle =
        DeviceHandle::new(dev, 512, len / 512, "TEST".into(), "IMAGE".into()).expect("scan");
    let offset = handle.table.luks_partitions().next().expect("a LUKS partition").offset_bytes();
    handle.unlock(offset, PASSWORD).expect("unlock")
}

fn json(s: String) -> Value {
    serde_json::from_str(&s).expect("json")
}

#[test]
fn an_ext4_listing_reports_its_unreadable_entries() {
    let path = scratch("gpt-luks.img", "ext4");
    let vol = unlock(&path);

    let clean = json(vol.list_dir_json("/").expect("list"));
    assert_eq!(clean["unreadable"], 0, "a clean directory reports zero: {clean}");

    // A dirent whose inode number is past the end of the inode table.
    {
        let mut fs = vol.fs_for_reading();
        let MountedFs::Ext4(e) = &mut *fs else { panic!("fixture is ext4") };
        let beyond = e.superblock().inodes_count as u64 + 1;
        e.link_file(2, "dangling.bin", beyond, FileType::Regular).expect("craft dangling entry");
    }

    let full = json(vol.list_dir_json("/").expect("list"));
    assert_eq!(full["unreadable"], 1, "full listing: {full}");
    assert!(
        full["entries"].as_array().unwrap().iter().any(|e| e["name"] == "dangling.bin"),
        "the unreadable entry is still listed"
    );

    let paged = json(vol.list_directory_paged("/", 0, 1).expect("paged"));
    assert_eq!(paged["unreadable"], 1, "the count covers the directory, not the page: {paged}");
}

#[test]
fn a_btrfs_listing_reports_zero() {
    let path = scratch("gpt-luks-btrfs.img", "btrfs");
    let vol = unlock(&path);
    let full = json(vol.list_dir_json("/").expect("list"));
    assert_eq!(full["unreadable"], 0, "{full}");
    let paged = json(vol.list_directory_paged("/", 0, 0).expect("paged"));
    assert_eq!(paged["unreadable"], 0, "{paged}");
}
