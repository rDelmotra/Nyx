//! Deleting is all or nothing: a delete that would be refused anywhere in its
//! subtree is refused whole, before any write, and subvolume roots are never
//! deletable.
//!
//! ```text
//! cargo test -p luks_core --features dangerous-write-support --test btrfs_delete_all_or_nothing
//! ```
//!
//! Objects the writer cannot make (hardlinks, symlinks, fifos, immutable
//! flags) are made by patching the inode item of an ordinary file in place
//! and re-checksumming its leaf. Those patched images are only ever used to
//! check a refusal (byte-identical afterwards) or a successful delete that
//! leaves nothing of the object behind (then graded with `btrfs check`).

#![cfg(feature = "dangerous-write-support")]

#[path = "../common/mod.rs"]
mod common;

use common::mem_device::MemoryDevice;
use common::states::{fresh_from, judge, mount, subvol_id, Depth, Untouched};
use luks_core::device::WriteAt;
use luks_core::error::LuksError;
use luks_core::fs::btrfs::tree::{Key, INODE_ITEM_KEY};
use luks_core::fs::btrfs::write::node::{InteriorNode, Leaf};
use luks_core::fs::btrfs::Btrfs;

const FIXTURE: &str = "btrfs/fedora-4k.img";
use luks_core::fs::btrfs::write::gate::{BTRFS_INODE_APPEND as APPEND, BTRFS_INODE_IMMUTABLE as IMMUTABLE};
/// PREALLOC (1 << 4) and SYNC (1 << 5): common on real disks, must not block deletes.
const PREALLOC_AND_SYNC: u64 = (1 << 4) | (1 << 5);

/// Edit the INODE_ITEM of `path` in place (leaf re-checksummed).
fn patch_inode(dev: &MemoryDevice, path: &str, edit: impl FnOnce(&mut Vec<u8>)) {
    let fs = Btrfs::mount(dev.clone()).expect("mount");
    let sb = fs.superblock();
    let found = fs.resolve_no_follow(fs.fs_tree(), path).expect("resolve");
    let key = Key::new(found.inode.objectid, INODE_ITEM_KEY, 0);
    let mut bytenr = found.tree.bytenr;
    loop {
        let node = fs.read_node(bytenr).expect("node");
        if node.is_leaf() {
            let mut leaf = Leaf::from_node(&node, sb.csum_type).expect("leaf");
            let idx = leaf.find_item(&key).expect("inode item in leaf");
            edit(&mut leaf.items[idx].data);
            let (phys, _) = fs.chunk_map().map(bytenr).expect("map");
            dev.write_at(phys, &leaf.emit(sb.node_size).expect("emit")).expect("write");
            return;
        }
        let int = InteriorNode::from_node(&node, sb.csum_type).expect("interior");
        bytenr = int.entries.iter().rev().find(|e| e.key <= key).expect("child").blockptr;
    }
}

fn set_flags(dev: &MemoryDevice, path: &str, flags: u64) {
    patch_inode(dev, path, |d| d[64..72].copy_from_slice(&flags.to_le_bytes()));
}
fn set_nlink(dev: &MemoryDevice, path: &str, n: u32) {
    patch_inode(dev, path, |d| d[40..44].copy_from_slice(&n.to_le_bytes()));
}
fn set_type(dev: &MemoryDevice, path: &str, ty: u32) {
    patch_inode(dev, path, |d| {
        let mode = u32::from_le_bytes(d[52..56].try_into().unwrap());
        d[52..56].copy_from_slice(&((mode & 0o7777) | ty).to_le_bytes());
    });
}

/// `/home/<top>` with `n` files, a subdirectory with files, and a last entry
/// `zz` (a small file in `/home/<top>/<last_in>`, or directly in `<top>`).
fn build_tree(dev: &MemoryDevice, top: &str, n: usize, last_in_sub: bool) {
    let mut fs = mount(dev);
    let t = format!("/home/{top}");
    fs.create_directory("/home", top).expect("mkdir top");
    for i in 0..n {
        fs.create_file_with_data(&t, &format!("f{i:03}"), &vec![i as u8; 50 + i]).expect("file");
    }
    fs.create_directory(&t, "sub").expect("mkdir sub");
    for i in 0..5 {
        fs.create_file_with_data(&format!("{t}/sub"), &format!("g{i}"), &vec![9u8; 300 * (i + 1)])
            .expect("file");
    }
    if last_in_sub {
        fs.create_directory(&t, "zdir").expect("mkdir zdir");
        fs.create_file_with_data(&format!("{t}/zdir"), "zz", b"last").expect("zz");
    } else {
        fs.create_file_with_data(&t, "zz", b"last").expect("zz");
    }
    fs.commit_active_batch().expect("commit");
}

fn refuse_and_assert_untouched(dev: &MemoryDevice, path: &str, must_name: &str) {
    let before = dev.snapshot();
    let mut fs = mount(dev);
    let e = fs.delete_file(path).expect_err("delete must be refused");
    assert!(matches!(e, LuksError::UnsupportedFsFeature(_) | LuksError::IsADirectory(_)), "wrong error kind: {e}");
    assert!(e.to_string().contains(must_name), "error must name {must_name}: {e}");
    let _ = fs.commit_active_batch();
    drop(fs);
    assert!(dev.snapshot() == before, "refused delete of {path} changed the image");
}

#[test]
fn a_subvolume_roots_and_the_top_level_are_refused_untouched() {
    let dev = fresh_from(FIXTURE);
    for p in ["/home", "/root", "/", "/home/", "/home/."] {
        let before = dev.snapshot();
        let mut fs = mount(&dev);
        let e = fs.delete_file(p).expect_err("must refuse");
        let _ = fs.commit_active_batch();
        drop(fs);
        assert!(dev.snapshot() == before, "delete {p} ({e}) changed the image");
    }
    judge(&dev, "subvol_refusals", Depth::Full);
}

#[test]
fn b_one_refusable_item_last_refuses_the_whole_delete() {
    // (setup, expected-to-name)
    type Setup = fn(&MemoryDevice);
    let cases: Vec<(&str, bool, Setup, &str)> = vec![
        ("hl", false, |d| set_nlink(d, "/home/hl/zz", 2), "/home/hl/zz"),
        ("hl2", true, |d| set_nlink(d, "/home/hl2/zdir/zz", 2), "/home/hl2/zdir/zz"),
        ("imm", false, |d| set_flags(d, "/home/imm/zz", IMMUTABLE), "/home/imm/zz"),
        ("app", true, |d| set_flags(d, "/home/app/zdir/zz", APPEND), "/home/app/zdir/zz"),
        ("immdir", true, |d| set_flags(d, "/home/immdir/zdir", IMMUTABLE), "/home/immdir/zdir"),
    ];
    for (top, deep, setup, name) in cases {
        let dev = fresh_from(FIXTURE);
        build_tree(&dev, top, 30, deep);
        setup(&dev);
        refuse_and_assert_untouched(&dev, &format!("/home/{top}"), name);
    }
}

#[test]
fn d_immutable_and_append_only_are_refused_untouched() {
    let dev = fresh_from(FIXTURE);
    {
        let mut fs = mount(&dev);
        fs.create_directory("/home", "d").expect("mkdir");
        fs.create_file_with_data("/home/d", "inner", b"x").expect("f");
        fs.create_file_with_data("/home", "lonely", b"y").expect("f");
        fs.create_file_with_data("/home", "lonely2", b"y").expect("f");
        fs.commit_active_batch().expect("commit");
    }
    set_flags(&dev, "/home/lonely", IMMUTABLE);
    refuse_and_assert_untouched(&dev, "/home/lonely", "immutable");
    set_flags(&dev, "/home/lonely2", APPEND);
    refuse_and_assert_untouched(&dev, "/home/lonely2", "immutable");
    // File inside an immutable / append-only directory, and the directory's
    // own recursive delete.
    for flag in [IMMUTABLE, APPEND] {
        set_flags(&dev, "/home/d", flag);
        refuse_and_assert_untouched(&dev, "/home/d/inner", "immutable");
        refuse_and_assert_untouched(&dev, "/home/d", "immutable");
    }
}

#[test]
fn c_symlinks_fifos_and_devices_are_deleted_with_their_folder() {
    let dev = fresh_from(FIXTURE);
    let root_tree = subvol_id(&mount(&dev), "/root");
    let guard = Untouched::capture(&mount(&dev), &[root_tree], &["/root"]);
    {
        let mut fs = mount(&dev);
        fs.create_directory("/home", "links").expect("mkdir");
        fs.create_file_with_data("/home/links", "keep-reg", &vec![5u8; 6000]).expect("f");
        fs.create_file_with_data("/home/links", "sym", b"../target/path").expect("f");
        fs.create_file_with_data("/home/links", "fifo", b"").expect("f");
        fs.create_file_with_data("/home/links", "sock", b"").expect("f");
        fs.create_file_with_data("/home/links", "chr", b"").expect("f");
        fs.create_file_with_data("/home/links", "blk", b"").expect("f");
        fs.create_file_with_data("/home", "outside", b"stay").expect("f");
        fs.commit_active_batch().expect("commit");
    }
    set_type(&dev, "/home/links/sym", 0o120000);
    set_type(&dev, "/home/links/fifo", 0o010000);
    set_type(&dev, "/home/links/sock", 0o140000);
    set_type(&dev, "/home/links/chr", 0o020000);
    set_type(&dev, "/home/links/blk", 0o060000);

    let mut fs = mount(&dev);
    fs.delete_file("/home/links").expect("folder with symlink and special files deletes");
    fs.commit_active_batch().expect("commit");
    drop(fs);
    judge(&dev, "special_delete", Depth::Full);
    let fs = mount(&dev);
    assert!(fs.list_dir("/home/links").is_err());
    assert_eq!(fs.read_file("/home/outside").expect("read"), b"stay");
    guard.assert_same(&fs, "special_delete");
}

#[test]
fn e_ordinary_recursive_delete_still_works() {
    let dev = fresh_from(FIXTURE);
    build_tree(&dev, "plain", 30, true);
    let mut fs = mount(&dev);
    fs.delete_file("/home/plain").expect("delete");
    fs.commit_active_batch().expect("commit");
    drop(fs);
    judge(&dev, "recursive_delete", Depth::Full);
    assert!(mount(&dev).list_dir("/home/plain").is_err());
}

/// Real objects made by mkfs (not patched): a symlink and a fifo in `root`, a
/// symlink and an xattr file in `home`. Each delete must pass `btrfs check`.
/// PREALLOC and SYNC inode flags sit next to IMMUTABLE/APPEND in the flags
/// word and must not be mistaken for them.
#[test]
fn f_real_symlinks_fifos_xattr_files_and_prealloc_sync_flags_delete_cleanly() {
    let dev = fresh_from(FIXTURE);
    for path in [
        "/root/usr/bin/link",
        "/root/usr/bin/fifo",
        "/root/usr/bin/tagged",
        "/home/user/link",
        "/home/user/tagged.txt",
    ] {
        mount(&dev).delete_file(path).unwrap_or_else(|e| panic!("delete {path}: {e}"));
        assert!(mount(&dev).resolve_no_follow(mount(&dev).fs_tree(), path).is_err(), "{path} still there");
        judge(&dev, &format!("deleted {path}"), Depth::Full);
    }
    {
        let mut fs = mount(&dev);
        fs.create_file_with_data("/home/user", "flagged", b"z").expect("f");
        fs.commit_active_batch().expect("commit");
    }
    set_flags(&dev, "/home/user/flagged", PREALLOC_AND_SYNC);
    mount(&dev).delete_file("/home/user/flagged").expect("PREALLOC|SYNC flags must not block a delete");
    judge(&dev, "deleted flagged", Depth::Full);
}
