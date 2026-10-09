//! Real Fedora file kinds against the btrfs writer: hardlinks, symlinks, fifos,
//! xattrs, hostile names, and immutable / append-only inodes.
//!
//! ```text
//! cargo test -p luks_core --features dangerous-write-support --release --test btrfs_real_kinds
//! ```
//!
//! Runs on `fixtures/btrfs/fedora-4k.img` (`tools/gen-btrfs-fixtures.sh
//! fedora-4k`), which carries in `root/usr/bin` a file with three hardlinks in
//! one directory and a fourth in `root/usr/lib`, a symlink, a fifo and `user.*`
//! xattr files, and in `home/user` a hardlink trio (two names in the directory,
//! one in `Documents`), a symlink and xattr files.
//!
//! Every state is graded by `common::states::judge(.., Depth::Full)`, i.e. our
//! validators plus `btrfs check --readonly` (`$NYX_BTRFS`, else `btrfs`).
//!
//! # What "refused" means here
//!
//! A refused `rename` / `create_directory` must leave the image **byte
//! identical**. A refused `finish_file` has already written file data into free
//! space (the name is only known at finish), so there the guarantee is: no
//! superblock, tree root or tree node changed, nothing references the data
//! blocks, the batch is still usable and a later good write passes the judge.
#![cfg(feature = "dangerous-write-support")]

#[path = "../common/mod.rs"]
mod common;

use common::mem_device::MemoryDevice;
use common::states::{
    fresh_from, judge, mount, payload, snapshot_dir, subvol_id, write_batch, Depth, Untouched,
};
use luks_core::device::{ReadAt, WriteAt};
use luks_core::error::LuksError;
use luks_core::fs::btrfs::tree::{Key, Node, INODE_ITEM_KEY, XATTR_ITEM_KEY};
use luks_core::fs::btrfs::write::node::Leaf;
use luks_core::fs::btrfs::write::gate::{BTRFS_INODE_APPEND, BTRFS_INODE_IMMUTABLE};
use luks_core::fs::btrfs::Btrfs;

const FIXTURE: &str = "btrfs/fedora-4k.img";

fn fresh() -> MemoryDevice {
    fresh_from(FIXTURE)
}

fn ino_of(fs: &Btrfs<MemoryDevice>, path: &str) -> u64 {
    fs.resolve_no_follow(fs.fs_tree(), path)
        .unwrap_or_else(|e| panic!("resolve {path}: {e}"))
        .inode
        .objectid
}

fn tree_bytenr_of(fs: &Btrfs<MemoryDevice>, path: &str) -> u64 {
    fs.resolve_no_follow(fs.fs_tree(), path)
        .unwrap_or_else(|e| panic!("resolve {path}: {e}"))
        .tree
        .bytenr
}

/// Number of XATTR_ITEMs keyed under `ino` in the tree holding `tree_path`.
fn xattr_items(fs: &Btrfs<MemoryDevice>, tree_path: &str, ino: u64) -> usize {
    let root = tree_bytenr_of(fs, tree_path);
    let mut n = 0;
    fs.for_each_item(root, ino, XATTR_ITEM_KEY, &mut |_, _| {
        n += 1;
        Ok(true)
    })
    .expect("scan xattrs");
    n
}

fn exists(fs: &Btrfs<MemoryDevice>, dir: &str, name: &str) -> bool {
    fs.list_dir(dir).expect("list").iter().any(|e| e.name == name)
}

/// Overwrite the `flags` field of inode `ino`'s INODE_ITEM in subvolume tree
/// `tree_id` (mirror 0 only, which is the copy this driver reads) and
/// re-checksum the leaf.
fn patch_inode_flags(dev: &MemoryDevice, tree_id: u64, ino: u64, flags: u64) {
    let fs = Btrfs::mount(dev.clone()).expect("mount");
    let csum_type = fs.superblock().csum_type;
    let metadata_uuid = fs.superblock().metadata_uuid;
    let node_size = fs.superblock().node_size;
    let root = fs.tree_root(tree_id).expect("tree root");
    let want = Key::new(ino, INODE_ITEM_KEY, 0);

    let mut stack = vec![root.bytenr];
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
        ReadAt::read_at(&dev.clone(), phys, &mut raw).expect("read raw");
        let parsed = Node::parse(raw, bytenr, csum_type, &metadata_uuid).expect("parse");
        let mut leaf = Leaf::from_node(&parsed, csum_type).expect("leaf");
        if let Some(idx) = leaf.items.iter().position(|it| it.key == want) {
            let data = &mut leaf.items[idx].data;
            assert!(data.len() >= 72, "INODE_ITEM too short");
            data[64..72].copy_from_slice(&flags.to_le_bytes());
            let emitted = leaf.emit(node_size).expect("emit");
            dev.write_at(phys, &emitted).expect("write leaf");
            return;
        }
    }
    panic!("INODE_ITEM {ino} not found in tree {tree_id}");
}

#[track_caller]
fn assert_refused(res: Result<impl std::fmt::Debug, LuksError>, what: &str) -> LuksError {
    match res {
        Ok(v) => panic!("[{what}] expected a refusal, got Ok({v:?})"),
        Err(e) => {
            assert!(
                matches!(e, LuksError::UnsupportedFsFeature(_)),
                "[{what}] expected UnsupportedFsFeature, got {e:?}"
            );
            e
        }
    }
}

// ---------------------------------------------------------------- (a) hardlinks

/// Every name of every hardlinked inode, as `(parent dir, name, a cross-dir
/// destination parent, an existing file in that destination parent)`.
fn hardlinked_names() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
    vec![
        // root/usr/bin: one INODE_REF item holds hl_a, hl_b, hl_c (first, middle, last).
        ("/root/usr/bin", "hl_a", "/root/usr/share/doc", "readme.txt"),
        ("/root/usr/bin", "hl_b", "/root/usr/share/doc", "readme.txt"),
        ("/root/usr/bin", "hl_c", "/root/usr/share/doc", "readme.txt"),
        // The fourth name has its own INODE_REF item (other parent).
        ("/root/usr/lib", "hl_d", "/root/usr/bin", "tool"),
        // home: hl_a, hl_b share an item, Documents/hl_c has its own.
        ("/home/user", "hl_a", "/home/user/Photos", "img1.bin"),
        ("/home/user", "hl_b", "/home/user/Photos", "img1.bin"),
        ("/home/user/Documents", "hl_c", "/home/user", "hello.txt"),
    ]
}

#[test]
fn rename_of_every_hardlinked_name_is_refused_and_image_is_identical() {
    for (dir, name, xdir, xexisting) in hardlinked_names() {
        // (kind, new parent, new name)
        let variants = [
            ("same dir", dir, "renamed_hl"),
            ("cross dir", xdir, "renamed_hl"),
            ("over existing", xdir, xexisting),
        ];
        for (kind, new_parent, new_name) in variants {
            let dev = fresh();
            let before = dev.snapshot();
            let mut fs = mount(&dev);
            let what = format!("{dir}/{name} -> {new_parent}/{new_name} ({kind})");
            let res = fs.rename(dir, name, new_parent, new_name);
            drop(fs);
            if res.is_ok() {
                // Only reachable if the refusal is missing: show what it did
                // to the disk (btrfs check names the broken back-references).
                judge(&dev, &format!("UNREFUSED {what}"), Depth::Full);
            }
            assert_refused(res, &what);
            assert!(
                dev.snapshot() == before,
                "{dir}/{name} {kind}: refused rename changed the image"
            );
            judge(&dev, "hardlink refused", Depth::Full);
        }
    }
}

/// Control: the same fixture names are renamable once they are not
/// hardlinked (a single-link file in the same directory), so the refusals
/// above are about the link count and not about the paths.
#[test]
fn control_single_link_file_in_same_directory_renames() {
    let dev = fresh();
    let mut fs = mount(&dev);
    fs.rename("/root/usr/bin", "tool", "/root/usr/bin", "tool2").expect("rename");
    drop(fs);
    judge(&dev, "control rename", Depth::Full);
    let fs = mount(&dev);
    assert!(exists(&fs, "/root/usr/bin", "tool2") && !exists(&fs, "/root/usr/bin", "tool"));
}

// ---------------------------------------------------------------- (b) symlinks, fifos, xattrs

fn untouched_root(dev: &MemoryDevice) -> Untouched {
    let fs = mount(dev);
    Untouched::capture(&fs, &[5, subvol_id(&fs, "/root")], &["/root"])
}

#[test]
fn symlink_rename_same_dir_cross_dir_and_over_existing() {
    // Same directory.
    let dev = fresh();
    let guard = untouched_root(&dev);
    mount(&dev).rename("/home/user", "link", "/home/user", "link2").expect("same dir");
    judge(&dev, "symlink same dir", Depth::Full);
    let fs = mount(&dev);
    assert_eq!(fs.read_link("/home/user/link2").unwrap(), "hello.txt");
    assert!(!exists(&fs, "/home/user", "link"));
    guard.assert_same(&fs, "symlink same dir");

    // Cross directory.
    let dev = fresh();
    mount(&dev).rename("/home/user", "link", "/home/user/Photos", "moved").expect("cross dir");
    judge(&dev, "symlink cross dir", Depth::Full);
    let fs = mount(&dev);
    assert_eq!(fs.read_link("/home/user/Photos/moved").unwrap(), "hello.txt");
    assert!(!exists(&fs, "/home/user", "link"));

    // Over an existing regular file (its data extents are freed).
    let dev = fresh();
    mount(&dev).rename("/home/user", "link", "/home/user", "hello.txt").expect("over file");
    judge(&dev, "symlink over file", Depth::Full);
    let fs = mount(&dev);
    assert_eq!(fs.read_link("/home/user/hello.txt").unwrap(), "hello.txt");
    assert!(!exists(&fs, "/home/user", "link"));

    // A root-subvolume symlink and fifo.
    let dev = fresh();
    mount(&dev).rename("/root/usr/bin", "link", "/root/usr/lib", "link").expect("root symlink");
    mount(&dev).rename("/root/usr/bin", "fifo", "/root/usr/lib", "fifo2").expect("fifo");
    judge(&dev, "root symlink and fifo", Depth::Full);
    let fs = mount(&dev);
    assert_eq!(fs.read_link("/root/usr/lib/link").unwrap(), "tool");
    assert!(exists(&fs, "/root/usr/lib", "fifo2") && !exists(&fs, "/root/usr/bin", "fifo"));
}

#[test]
fn xattr_file_rename_keeps_its_xattrs_and_over_existing_drops_the_old_ones() {
    // Same directory.
    let dev = fresh();
    let (ino, n) = {
        let fs = mount(&dev);
        let ino = ino_of(&fs, "/home/user/tagged.txt");
        let n = xattr_items(&fs, "/home/user", ino);
        assert_eq!(n, 1, "fixture must carry the xattr");
        (ino, n)
    };
    mount(&dev).rename("/home/user", "tagged.txt", "/home/user", "tagged_new.txt").expect("same dir");
    judge(&dev, "xattr same dir", Depth::Full);
    {
        let fs = mount(&dev);
        assert_eq!(ino_of(&fs, "/home/user/tagged_new.txt"), ino, "rename keeps the inode");
        assert_eq!(xattr_items(&fs, "/home/user", ino), n, "xattr stays on the renamed inode");
        assert_eq!(fs.read_file("/home/user/tagged_new.txt").unwrap(), b"home xattr file\n");
    }

    // Cross directory.
    let dev = fresh();
    mount(&dev).rename("/home/user", "tagged.txt", "/home/user/Photos", "tagged.txt").expect("cross dir");
    judge(&dev, "xattr cross dir", Depth::Full);
    {
        let fs = mount(&dev);
        assert_eq!(xattr_items(&fs, "/home/user", ino_of(&fs, "/home/user/Photos/tagged.txt")), n);
    }

    // Over an existing xattr file: the victim's xattr items must be gone,
    // the mover's must stay (btrfs check flags orphaned xattr items).
    let dev = fresh();
    let (mover, victim) = {
        let fs = mount(&dev);
        let m = ino_of(&fs, "/home/user/tagged.txt");
        let v = ino_of(&fs, "/home/user/tagged_other.txt");
        assert_eq!(xattr_items(&fs, "/home/user", v), 1);
        (m, v)
    };
    mount(&dev).rename("/home/user", "tagged.txt", "/home/user", "tagged_other.txt").expect("over existing");
    judge(&dev, "xattr over existing", Depth::Full);
    {
        let fs = mount(&dev);
        assert_eq!(ino_of(&fs, "/home/user/tagged_other.txt"), mover);
        assert_eq!(xattr_items(&fs, "/home/user", mover), 1, "mover keeps its xattr");
        assert_eq!(xattr_items(&fs, "/home/user", victim), 0, "victim's xattr items are gone");
        assert!(!exists(&fs, "/home/user", "tagged.txt"));
    }

    // Root subvolume: a file with two xattrs, moved across directories, and
    // over another xattr file.
    let dev = fresh();
    let ino = {
        let fs = mount(&dev);
        let i = ino_of(&fs, "/root/usr/bin/tagged");
        assert_eq!(xattr_items(&fs, "/root/usr/bin", i), 2);
        i
    };
    mount(&dev).rename("/root/usr/bin", "tagged", "/root/usr/lib", "tagged").expect("root cross dir");
    mount(&dev).rename("/root/usr/lib", "tagged", "/root/usr/bin", "tagged2").expect("root over existing");
    judge(&dev, "root xattrs", Depth::Full);
    let fs = mount(&dev);
    assert_eq!(ino_of(&fs, "/root/usr/bin/tagged2"), ino);
    assert_eq!(xattr_items(&fs, "/root/usr/bin", ino), 2);
}

// ---------------------------------------------------------------- (c) names

fn bad_names() -> Vec<(&'static str, String)> {
    vec![
        ("empty", String::new()),
        ("dot", ".".to_string()),
        ("dotdot", "..".to_string()),
        ("slash", "a/b".to_string()),
        ("nul", "a\0b".to_string()),
        ("256 ascii bytes", "a".repeat(256)),
        // 86 x 3-byte characters = 258 bytes, only 86 characters.
        ("86 x 3-byte chars (258 bytes)", "\u{20ac}".repeat(86)),
        // 128 x 2-byte characters = 256 bytes.
        ("128 x 2-byte chars (256 bytes)", "\u{e9}".repeat(128)),
    ]
}

/// Whole-fs fingerprint that must not move on a refused `finish_file`: the
/// three trees and both directory listings.
fn guard_all(dev: &MemoryDevice) -> Untouched {
    let fs = mount(dev);
    let (root, home) = (subvol_id(&fs, "/root"), subvol_id(&fs, "/home"));
    Untouched::capture(&fs, &[5, root, home], &["/root", "/home"])
}

/// Superblock copy 0 (at 64 KiB) is the commit point: it must be unchanged.
fn superblock_bytes(dev: &MemoryDevice) -> Vec<u8> {
    let mut b = vec![0u8; 4096];
    ReadAt::read_at(&dev.clone(), 64 * 1024, &mut b).expect("read sb");
    b
}

#[test]
fn bad_names_refused_on_finish_file_create_directory_and_rename() {
    for (label, name) in bad_names() {
        let dev = fresh();
        let sb0 = superblock_bytes(&dev);
        let guard = guard_all(&dev);

        // --- finish_file: data is written first, the name is only seen here.
        {
            let mut fs = mount(&dev);
            let mut w = fs.begin_file(5000).expect("begin");
            fs.write_chunk(&mut w, &payload(5000, 7)).expect("write");
            assert_refused(fs.finish_file(w, "/home/user", &name), &format!("finish_file {label}"));
            // The batch is still usable and holds nothing from the refused file.
            fs.commit_active_batch().expect("commit after refusal");
        }
        assert!(superblock_bytes(&dev) == sb0, "{label}: finish_file refusal moved the superblock");
        guard.assert_same(&mount(&dev), &format!("finish_file {label}"));
        judge(&dev, &format!("finish_file {label}"), Depth::Full);

        // A good write afterwards succeeds and passes the judge.
        {
            let mut fs = mount(&dev);
            write_batch(&mut fs, &[("/home/user".into(), "good.bin".into(), payload(9000, 8))]);
        }
        judge(&dev, &format!("good write after {label}"), Depth::Full);
        assert_eq!(mount(&dev).read_file("/home/user/good.bin").unwrap(), payload(9000, 8));

        // --- create_file_with_data (goes through the same path).
        let dev = fresh();
        {
            let mut fs = mount(&dev);
            assert_refused(
                fs.create_file_with_data("/home/user", &name, &payload(3000, 1)),
                &format!("create_file_with_data {label}"),
            );
            fs.commit_active_batch().expect("commit");
        }
        judge(&dev, &format!("create_file_with_data {label}"), Depth::Full);
        guard_all(&dev).assert_same(&mount(&dev), "create_file_with_data");

        // --- create_directory: image byte-identical.
        let dev = fresh();
        let before = dev.snapshot();
        {
            let mut fs = mount(&dev);
            assert_refused(fs.create_directory("/home/user", &name), &format!("create_directory {label}"));
        }
        assert!(dev.snapshot() == before, "{label}: refused create_directory changed the image");
        {
            let mut fs = mount(&dev);
            fs.create_directory("/home/user", "gooddir").expect("good mkdir afterwards");
        }
        judge(&dev, &format!("mkdir after {label}"), Depth::Full);

        // --- rename destination: image byte-identical.
        let dev = fresh();
        let before = dev.snapshot();
        {
            let mut fs = mount(&dev);
            assert_refused(
                fs.rename("/home/user", "tagged.txt", "/home/user", &name),
                &format!("rename same dir {label}"),
            );
            assert_refused(
                fs.rename("/home/user", "tagged.txt", "/home/user/Photos", &name),
                &format!("rename cross dir {label}"),
            );
        }
        assert!(dev.snapshot() == before, "{label}: refused rename changed the image");
        {
            let mut fs = mount(&dev);
            fs.rename("/home/user", "tagged.txt", "/home/user", "ok.txt").expect("good rename afterwards");
        }
        judge(&dev, &format!("rename after {label}"), Depth::Full);
    }
}

/// A refused name inside a batch that already holds a good file: the good one
/// survives and commits, the bad one leaves nothing.
#[test]
fn bad_name_in_a_batch_keeps_earlier_files() {
    let dev = fresh();
    {
        let mut fs = mount(&dev);
        let mut w = fs.begin_file(4000).unwrap();
        fs.write_chunk(&mut w, &payload(4000, 1)).unwrap();
        fs.finish_file(w, "/home/user", "first.bin").unwrap();

        let mut w = fs.begin_file(4000).unwrap();
        fs.write_chunk(&mut w, &payload(4000, 2)).unwrap();
        assert_refused(fs.finish_file(w, "/home/user", "a/b"), "bad second file");

        let mut w = fs.begin_file(4000).unwrap();
        fs.write_chunk(&mut w, &payload(4000, 3)).unwrap();
        fs.finish_file(w, "/home/user", "third.bin").unwrap();
        fs.commit_active_batch().unwrap();
    }
    judge(&dev, "batch with refusal", Depth::Full);
    let fs = mount(&dev);
    assert_eq!(fs.read_file("/home/user/first.bin").unwrap(), payload(4000, 1));
    assert_eq!(fs.read_file("/home/user/third.bin").unwrap(), payload(4000, 3));
    assert!(!exists(&fs, "/home/user", "a"), "no half-created entry");
}

#[test]
fn names_of_exactly_255_bytes_are_accepted_and_pass_btrfs_check() {
    let ascii = "a".repeat(255);
    let dir255 = "d".repeat(255);
    // 127 x 2-byte characters + 1 ASCII byte = 255 bytes.
    let multi = format!("{}x", "\u{e9}".repeat(127));
    assert_eq!(multi.len(), 255);

    let dev = fresh();
    {
        let mut fs = mount(&dev);
        write_batch(
            &mut fs,
            &[
                ("/home/user".into(), ascii.clone(), payload(6000, 1)),
                ("/home/user".into(), multi.clone(), payload(6000, 2)),
            ],
        );
        fs.create_directory("/home/user", &dir255).expect("255-byte directory");
        fs.rename("/home/user", "tagged.txt", "/home/user", &"r".repeat(255)).expect("255-byte rename target");
    }
    judge(&dev, "255-byte names", Depth::Full);
    let fs = mount(&dev);
    assert_eq!(fs.read_file(&format!("/home/user/{ascii}")).unwrap(), payload(6000, 1));
    assert_eq!(fs.read_file(&format!("/home/user/{multi}")).unwrap(), payload(6000, 2));
    assert!(exists(&fs, "/home/user", &dir255));
    assert!(exists(&fs, "/home/user", &"r".repeat(255)));
    let _ = snapshot_dir; // shared helper kept imported for parity with sibling suites
}

// ---------------------------------------------------------------- (d) immutable / append-only

fn home_tree(dev: &MemoryDevice) -> u64 {
    subvol_id(&mount(dev), "/home")
}

fn flag(dev: &MemoryDevice, path: &str, flags: u64) {
    let (tree, ino) = {
        let fs = mount(dev);
        (home_tree(dev), ino_of(&fs, path))
    };
    patch_inode_flags(dev, tree, ino, flags);
}

#[test]
fn immutable_and_append_only_inodes_block_rename_and_creation() {
    for (what, bits) in [("immutable", BTRFS_INODE_IMMUTABLE), ("append-only", BTRFS_INODE_APPEND)] {
        // Source inode flagged: rename refused, image untouched.
        let dev = fresh();
        flag(&dev, "/home/user/tagged.txt", bits);
        let before = dev.snapshot();
        for (np, nn) in [("/home/user", "x"), ("/home/user/Photos", "x"), ("/home/user", "hello.txt")] {
            assert_refused(
                mount(&dev).rename("/home/user", "tagged.txt", np, nn),
                &format!("{what} source -> {np}/{nn}"),
            );
        }
        assert!(dev.snapshot() == before, "{what} source: refusal changed the image");

        // Destination inode flagged: rename-over refused.
        let dev = fresh();
        flag(&dev, "/home/user/tagged_other.txt", bits);
        let before = dev.snapshot();
        assert_refused(
            mount(&dev).rename("/home/user", "tagged.txt", "/home/user", "tagged_other.txt"),
            &format!("{what} destination"),
        );
        assert!(dev.snapshot() == before, "{what} destination: refusal changed the image");

        // Source directory flagged: nothing may leave it.
        let dev = fresh();
        flag(&dev, "/home/user/Documents", bits);
        let before = dev.snapshot();
        assert_refused(
            mount(&dev).rename("/home/user/Documents", "notes.txt", "/home/user", "notes.txt"),
            &format!("{what} source dir"),
        );
        assert!(dev.snapshot() == before, "{what} source dir: refusal changed the image");
    }

    // Immutable directory: no creation, no rename into it.
    let dev = fresh();
    flag(&dev, "/home/user/Photos", BTRFS_INODE_IMMUTABLE);
    let before = dev.snapshot();
    assert_refused(
        mount(&dev).rename("/home/user", "tagged.txt", "/home/user/Photos", "t"),
        "rename into immutable dir",
    );
    assert_refused(mount(&dev).create_directory("/home/user/Photos", "d"), "mkdir in immutable dir");
    assert_refused(
        mount(&dev).create_file_with_data("/home/user/Photos", "f", b"x"),
        "create file in immutable dir",
    );
    {
        let mut fs = mount(&dev);
        let mut w = fs.begin_file(10).unwrap();
        fs.write_chunk(&mut w, &[1u8; 10]).unwrap();
        assert_refused(fs.finish_file(w, "/home/user/Photos", "g"), "finish_file in immutable dir");
        fs.commit_active_batch().unwrap();
    }
    assert!(!exists(&mount(&dev), "/home/user/Photos", "g"));
    let _ = before;
    judge(&dev, "immutable dir refusals", Depth::Full);

    // Append-only directory: creation and rename-in are allowed.
    let dev = fresh();
    flag(&dev, "/home/user/Photos", BTRFS_INODE_APPEND);
    mount(&dev).create_directory("/home/user/Photos", "d").expect("mkdir in append-only dir");
    mount(&dev).create_file_with_data("/home/user/Photos", "f", b"data").expect("create in append-only dir");
    mount(&dev).rename("/home/user", "tagged.txt", "/home/user/Photos", "t").expect("rename into append-only dir");
    judge(&dev, "append-only dir creation", Depth::Full);

    // Control: with no flag set the same renames pass (so the refusals above
    // are about the flag).
    let dev = fresh();
    mount(&dev).rename("/home/user", "tagged.txt", "/home/user", "x").expect("control 1");
    mount(&dev).rename("/home/user/Documents", "notes.txt", "/home/user", "notes.txt").expect("control 2");
    judge(&dev, "immutable control", Depth::Full);
}
