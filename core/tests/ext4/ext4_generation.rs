//! F4 B2: every inode the ext4 driver allocates carries a fresh, non-zero
//! `i_generation`, so (ino, generation) identifies a file across delete and
//! reuse of the inode number.
//!
//! Graded by the kernel (`e2fsck -fn`), never by our own reader alone: the
//! generation is folded into the inode checksum, the extent-leaf checksum and
//! the directory-block checksum, so a writer that stamps it in one place and
//! checksums with another value is caught as a checksum error.
//!
//! ```text
//! cargo test -p luks_core --features dangerous-write-support --test ext4_generation
//! ```
#![cfg(feature = "dangerous-write-support")]

#[path = "../common/temp_guard.rs"]
mod temp_guard;
use temp_guard::TempPath;

#[path = "../common/mod.rs"]
mod common;

use luks_core::device::FileDevice;
use luks_core::fs::ext4::Ext4;
use luks_core::fs::FileType;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::Command;

const ROOT: u64 = 2;
const BS: usize = 4096;
/// With and without `metadata_csum` (the generation feeds the checksums).
const FIXTURES: [&str; 2] = ["big-4k.img", "csum-uuid-4k.img"];

fn scratch(fixture: &str, tag: &str) -> TempPath {
    let src = format!("{}/../fixtures/ext4/{fixture}", env!("CARGO_MANIFEST_DIR"));
    let dst = std::env::temp_dir().join(format!("ext4-gen-{tag}-{}-{fixture}", std::process::id()));
    std::fs::copy(src, &dst).expect("copy fixture");
    TempPath::new(dst)
}

fn open(path: &PathBuf) -> Ext4<FileDevice> {
    let len = std::fs::metadata(path).expect("stat scratch").len();
    Ext4::mount(FileDevice::open_writable(path.to_str().expect("utf8"), len).expect("open writable"))
        .expect("mount")
}

#[track_caller]
fn assert_kernel_clean(img: &PathBuf, what: &str) {
    assert!(common::oracle::gate(), "oracle required: this test cannot pass without e2fsck");
    let script = format!("{}/../tools/verify-ext4.sh", env!("CARGO_MANIFEST_DIR"));
    let out = Command::new("bash").arg(&script).arg(img).output().expect("run verify-ext4.sh");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{what}: e2fsck is not clean:\n{text}");
}

fn generation_of(fs: &Ext4<FileDevice>, path: &str) -> u32 {
    fs.resolve(path).unwrap_or_else(|e| panic!("resolve {path}: {e}")).generation
}

/// Every allocation path stamps a non-zero, pairwise-distinct generation and
/// the kernel accepts the result. The paths: `create_file` (immediate),
/// `begin_file`/`finish_file` (deferred, sized), `begin_file_streaming`, a
/// fragmented file (extent leaf block, whose checksum folds the generation in)
/// and `create_directory` (dirent block checksum folds the generation in).
#[test]
fn every_allocated_inode_gets_a_fresh_generation_and_e2fsck_accepts_it() {
    for fixture in FIXTURES {
        let img = scratch(fixture, "alloc");
        let mut fs = open(&img);

        fs.create_file(ROOT, "immediate", &vec![1u8; BS], FileType::Regular).expect("create_file");

        let mut w = fs.begin_file(2 * BS as u64).expect("begin_file");
        fs.write_chunk(&mut w, &vec![2u8; 2 * BS]).expect("chunk");
        fs.finish_file(w, ROOT, "sized", FileType::Regular).expect("finish sized");

        let mut w = fs.begin_file_streaming().expect("begin streaming");
        fs.write_chunk(&mut w, &vec![3u8; BS]).expect("chunk");
        fs.finish_file(w, ROOT, "streamed", FileType::Regular).expect("finish streamed");

        fs.create_directory(ROOT as u32, "dir").expect("mkdir");
        // A file inside the new directory forces a checksummed write into the
        // directory block whose seed contains the directory's generation.
        let dir = fs.resolve("/dir").expect("resolve dir").number;
        fs.create_file(dir, "inner", &vec![4u8; BS], FileType::Regular).expect("create inner");

        let paths = ["/immediate", "/sized", "/streamed", "/dir", "/dir/inner"];
        let gens: Vec<u32> = paths.iter().map(|p| generation_of(&fs, p)).collect();
        assert_eq!(gens.len(), paths.len(), "vacuity");
        for (p, g) in paths.iter().zip(&gens) {
            assert_ne!(*g, 0, "{fixture}: {p} was allocated with i_generation 0");
        }
        let distinct: BTreeSet<u32> = gens.iter().copied().collect();
        assert_eq!(distinct.len(), gens.len(), "{fixture}: generations collide: {gens:?}");
        drop(fs);
        assert_kernel_clean(&img, &format!("{fixture}: fresh generations"));
    }
}

/// The point of the field: delete a file and create another that reuses the
/// inode number; the pair (ino, generation) must differ.
#[test]
fn a_reused_inode_number_gets_a_different_generation() {
    let img = scratch("big-4k.img", "reuse");
    let mut fs = open(&img);
    let first = fs.create_file(ROOT, "a", &vec![9u8; BS], FileType::Regular).expect("create a");
    let g1 = generation_of(&fs, "/a");
    fs.delete_file("/a").expect("delete a");
    let second = fs.create_file(ROOT, "c", &vec![8u8; BS], FileType::Regular).expect("create c");
    assert_eq!(first, second, "vacuity: the allocator must reuse the freed inode number");
    let g2 = generation_of(&fs, "/c");
    assert_ne!(g1, g2, "same inode number, same generation: identity cannot tell them apart");
    drop(fs);
    assert_kernel_clean(&img, "reused inode");
}

/// A file with more than four runs gets an extent leaf block whose checksum
/// folds in the inode's generation. The leaf is written before the inode
/// record, so both must use the same value; e2fsck on a checksummed fs grades
/// that.
#[test]
fn a_fragmented_file_with_an_extent_leaf_checksums_against_its_generation() {
    let img = scratch("csum-uuid-4k.img", "leaf");
    let mut fs = open(&img);
    let mut held = Vec::new();
    for _ in 0..64 {
        held.push(fs.alloc_block(0).expect("checkerboard alloc"));
    }
    for (i, b) in held.iter().enumerate() {
        if i % 2 == 0 {
            fs.free_block(*b).expect("free every other block");
        }
    }
    let free_before = fs.superblock().free_blocks_count;
    let content = vec![0xAAu8; 30 * BS];
    fs.create_file(ROOT, "frag", &content, FileType::Regular).expect("fragmented create");
    fs.flush().expect("flush");
    assert_eq!(
        fs.superblock().free_blocks_count,
        free_before - 31,
        "vacuity: 30 data blocks + 1 extent leaf means the leaf path ran"
    );
    assert_ne!(generation_of(&fs, "/frag"), 0);
    // Give back the checkerboard so the only complaint source is the leaf.
    for (i, b) in held.iter().enumerate() {
        if i % 2 == 1 {
            fs.free_block(*b).expect("free the rest");
        }
    }
    fs.flush().expect("flush");
    drop(fs);
    assert_kernel_clean(&img, "extent leaf checksum with generation");
}

/// Mixed images: `big-4k.img` was built by mke2fs, so its files are
/// generation 0 ("written before this driver stamped generations"). New inodes
/// from our driver sit beside them, in the *same directory*, and a directory
/// rewrite (new entry, new subdirectory, parent link-count patch) happens in a
/// directory that holds old inodes. e2fsck must stay clean, and the old files
/// must still open and read through the identity path: a directory rewrite
/// touches only the directory's blocks, never an old file's identity.
#[test]
fn old_generation_zero_inodes_survive_a_directory_rewrite_and_read_by_identity() {
    use luks_core::fs::{MountedFs, OpenFile};

    let img = scratch("big-4k.img", "mixed");
    let old_paths = ["/docs/readme.md", "/docs/nested/deep.txt", "/hello.txt", "/big.bin"];

    // Identities pinned BEFORE the rewrite, on a read-only mount.
    let pinned: Vec<(&str, OpenFile, Vec<u8>)> = {
        let fs = MountedFs::mount(FileDevice::open(img.to_str().expect("utf8")).expect("open")).expect("mount");
        old_paths
            .iter()
            .map(|p| {
                let f = fs.open(p).unwrap_or_else(|e| panic!("open {p}: {e}"));
                let bytes = fs.read_file(p).unwrap_or_else(|e| panic!("read {p}: {e}"));
                (*p, f, bytes)
            })
            .collect()
    };
    assert_eq!(pinned.len(), old_paths.len(), "vacuity");
    {
        let fs = open(&img);
        for p in old_paths {
            assert_eq!(generation_of(&fs, p), 0, "vacuity: {p} must be an old generation-0 inode");
        }
    }

    // New inodes from our driver, in the directory that holds old ones, plus
    // a new subdirectory (rewrites the parent's dirent block and link count).
    {
        let mut fs = open(&img);
        let docs = fs.resolve("/docs").expect("docs").number;
        fs.create_file(docs, "new-a.txt", &vec![0x41u8; 5000], FileType::Regular).expect("new-a");
        fs.create_directory(docs as u32, "newdir").expect("mkdir in old dir");
        let newdir = fs.resolve("/docs/newdir").expect("newdir").number;
        fs.create_file(newdir, "inner.txt", b"inner", FileType::Regular).expect("inner");
        fs.create_file(ROOT, "new-root.txt", &vec![0x42u8; 100], FileType::Regular).expect("new-root");
        for p in ["/docs/new-a.txt", "/docs/newdir", "/docs/newdir/inner.txt", "/new-root.txt"] {
            assert_ne!(generation_of(&fs, p), 0, "new inode {p} must carry a generation");
        }
        for p in old_paths {
            assert_eq!(generation_of(&fs, p), 0, "old inode {p} must be untouched");
        }
    }

    assert_kernel_clean(&img, "old and new generations mixed, directory rewritten");

    // The old files still read through the identity path, with the bytes they had.
    let fs = MountedFs::mount(FileDevice::open(img.to_str().expect("utf8")).expect("open")).expect("remount");
    for (p, pinned_file, before) in &pinned {
        let mut buf = vec![0u8; before.len()];
        let n = fs
            .read_open(pinned_file, 0, &mut buf)
            .unwrap_or_else(|e| panic!("{p}: a pinned old file must still read after a directory rewrite: {e}"));
        assert_eq!(n, before.len(), "{p}");
        assert_eq!(&buf, before, "{p}: bytes changed");
    }
    // And the new file reads through a fresh handle.
    let f = fs.open("/docs/new-a.txt").expect("open new");
    let mut buf = vec![0u8; 5000];
    assert_eq!(fs.read_open(&f, 0, &mut buf).expect("read new"), 5000);
    assert!(buf.iter().all(|&b| b == 0x41));
}
