//! Negative controls for the "untouched" judge in `common::states`.
//!
//! A judge that can never fail proves nothing, so each test here shows both
//! halves: an idle filesystem passes `Untouched::assert_same`, and a write
//! into a captured tree or directory makes it panic.

#![cfg(feature = "dangerous-write-support")]

#[path = "../common/mod.rs"]
mod common;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::Path;

use common::mem_device::MemoryDevice;
use common::states::{fresh_from, mount, payload, subvol_id, write_batch, Untouched};

fn put(dev: &MemoryDevice, dir: &str, name: &str, seed: u64) {
    let mut fs = mount(dev);
    write_batch(&mut fs, &[(dir.to_string(), name.to_string(), payload(5000, seed))]);
}

fn panic_message(r: std::thread::Result<()>) -> Option<String> {
    r.err().map(|e| {
        e.downcast_ref::<String>()
            .cloned()
            .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_default()
    })
}

#[test]
fn idle_fs_is_untouched() {
    let dev = fresh_from("btrfs/states-4k.img");
    let before = Untouched::capture(&mount(&dev), &[5], &["/"]);
    before.assert_same(&mount(&dev), "idle");
}

#[test]
fn write_in_captured_tree_is_detected() {
    let dev = fresh_from("btrfs/states-4k.img");
    let before = Untouched::capture(&mount(&dev), &[5], &["/"]);
    put(&dev, "/", "intruder", 1);

    let fs = mount(&dev);
    let msg = panic_message(catch_unwind(AssertUnwindSafe(|| before.assert_same(&fs, "after write"))))
        .expect("judge must panic when the fs tree changed");
    assert!(msg.contains("after write") && msg.contains("tree 5"), "unclear message: {msg}");

    // The file listing alone must also notice.
    let files_only = Untouched { trees: vec![], files: before.files.clone() };
    let msg = panic_message(catch_unwind(AssertUnwindSafe(|| files_only.assert_same(&fs, "files"))))
        .expect("file judge must panic");
    assert!(msg.contains("intruder"), "message must name the new file: {msg}");
}

/// The Fedora layout: `root` and `home` subvolumes, writes only in
/// `/home/user`. Skipped while the fixture is absent.
#[test]
fn fedora_write_in_home_leaves_root_untouched() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/btrfs/fedora-4k.img");
    if !fixture.exists() {
        eprintln!("SKIP fedora_write_in_home_leaves_root_untouched: {} missing", fixture.display());
        return;
    }
    let dev = fresh_from("btrfs/fedora-4k.img");
    let (root_id, home_id) = {
        let fs = mount(&dev);
        (subvol_id(&fs, "/root"), subvol_id(&fs, "/home"))
    };
    assert_ne!(root_id, home_id);
    let before = Untouched::capture(&mount(&dev), &[5, root_id], &["/root"]);

    // The allowed write.
    put(&dev, "/home/user", "ok.bin", 2);
    before.assert_same(&mount(&dev), "write in /home/user");
    assert_eq!(mount(&dev).read_file("/home/user/ok.bin").unwrap(), payload(5000, 2));
    let _ = home_id;

    // Negative control: a write into the protected subvolume is caught.
    put(&dev, "/root/etc", "intruder", 3);
    let fs = mount(&dev);
    let msg = panic_message(catch_unwind(AssertUnwindSafe(|| before.assert_same(&fs, "write in /root/etc"))))
        .expect("judge must panic when the root subvolume changed");
    assert!(msg.contains(&format!("tree {root_id}")), "message must name the root tree: {msg}");
}
