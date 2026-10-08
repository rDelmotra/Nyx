//! B-tree state suite inside a subvolume: the shapes of `btrfs_states` S1-S3,
//! driven in the `home` subvolume of a Fedora-shaped image, with the `root`
//! subvolume and the top-level tree proven untouched.
//!
//! ```text
//! cd core
//! cargo test --release --features dangerous-write-support \
//!     --test btrfs_subvol_states -- --nocapture
//! # a newer, stricter checker:
//! NYX_BTRFS=/path/to/btrfs cargo test --release ... --test btrfs_subvol_states
//! ```
//!
//! # Purpose
//!
//! The image `fixtures/btrfs/fedora-4k.img` mirrors a real Fedora SSD: the
//! subvolumes `root` and `home`, and the app only ever writes inside
//! `/home/user`. The claim under test: writing there never corrupts the disk
//! and never touches `root`. `btrfs_states` grows and shrinks trees only in the
//! top-level tree 5; here the same pressure lands on the `home` tree, whose
//! root item lives in the root tree, so the subvolume bookkeeping (root item
//! update, generation, extent back-references) is exercised at height 2.
//!
//! # What every test checks after every step
//!
//! The disk is re-mounted from its committed bytes, then:
//!
//! 1. `Untouched::assert_same`: tree 5, the `root` subvolume tree (every node,
//!    byte for byte) and the listing of `/root` (every file's contents) equal
//!    what they were before the first write.
//! 2. Every original `/home/user` file (hello.txt, Documents/notes.txt,
//!    Photos/img1.bin, .config/app.conf) reads back identical.
//! 3. `judge`: tree validator, accounting oracle and, at `Depth::Full`,
//!    `btrfs check --readonly` (`NYX_BTRFS` overrides the binary). The start,
//!    the peak and the end of each test are always `Full`.
//!
//! | test | what it proves |
//! |---|---|
//! | `h1_*` | home tree reaches 3 levels, empties from both ends, collapses, takes new files |
//! | `h2_*` | the free-space tree outgrows a leaf while writing in home, and merges back |
//! | `h3_*` | ~96 MiB written in home splits the checksum tree; trimming it keeps survivors exact |
//!
//! The fixture is made by `tools/gen-btrfs-fixtures.sh fedora-4k`. If it is
//! missing the tests fail (this is a safety suite, it never skips).

#![cfg(feature = "dangerous-write-support")]

#[path = "../common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::path::Path;

use common::btree_validator::{AllTreesValidationReport, TreeValidationReport};
use common::mem_device::MemoryDevice;
use common::states::{
    files_print, fresh_from, judge, long_name, mount, payload, snapshot_dir, subvol_id, write_batch,
    Depth, Untouched, DIR_MARK,
};

const FIXTURE: &str = "btrfs/fedora-4k.img";
const MIB: usize = 1024 * 1024;

/// A fresh Fedora-shaped image plus everything needed to grade it.
struct Frame {
    dev: MemoryDevice,
    root_id: u64,
    home_id: u64,
    untouched: Untouched,
    /// Original `/home/user` files: relative path -> contents.
    originals: BTreeMap<String, Vec<u8>>,
}

impl Frame {
    fn new() -> Self {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures").join(FIXTURE);
        assert!(
            path.exists(),
            "fixture {} is missing: run `tools/gen-btrfs-fixtures.sh fedora-4k`",
            path.display()
        );
        let dev = fresh_from(FIXTURE);
        let fs = mount(&dev);
        let root_id = subvol_id(&fs, "/root");
        let home_id = subvol_id(&fs, "/home");
        assert_ne!(root_id, home_id, "root and home must be different subvolumes");
        assert!(root_id != 5 && home_id != 5, "both must be real subvolumes, not the top level");
        let untouched = Untouched::capture(&fs, &[5, root_id], &["/root"]);
        let originals: BTreeMap<_, _> = files_print(&fs, "/home/user")
            .into_iter()
            .filter(|(k, v)| !(k.ends_with('/') && v.as_slice() == DIR_MARK))
            .collect();
        for k in ["hello.txt", "Documents/notes.txt", "Photos/img1.bin", ".config/app.conf"] {
            assert!(originals.contains_key(k), "fixture lacks /home/user/{k}");
        }
        Self { dev, root_id, home_id, untouched, originals }
    }

    /// Remount and grade the committed disk; returns the validator report.
    fn check(&self, step: &str, depth: Depth) -> AllTreesValidationReport {
        let fs = mount(&self.dev);
        self.untouched.assert_same(&fs, step);
        for (rel, want) in &self.originals {
            let got = fs
                .read_file(&format!("/home/user/{rel}"))
                .unwrap_or_else(|e| panic!("[{step}] original /home/user/{rel} unreadable: {e}"));
            assert!(&got == want, "[{step}] original /home/user/{rel} changed ({} -> {} bytes)", want.len(), got.len());
        }
        drop(fs);
        judge(&self.dev, step, depth)
    }

    fn home<'a>(&self, r: &'a AllTreesValidationReport) -> &'a TreeValidationReport {
        &r.subvolume_trees
            .iter()
            .find(|(id, _)| *id == self.home_id)
            .unwrap_or_else(|| panic!("validator report has no tree {}", self.home_id))
            .1
    }
}

// ------------------------------------------------------------------ H1

/// Grow the `home` tree to three levels, empty `/home/user/many` from the left
/// and then the right, and use it again (mirror of S1).
///
/// As in S1 the engine never merges half-empty nodes, so a few survivors keep
/// the tree tall for a while; what must hold is that nodes fall with the items,
/// the root collapses once the folder is empty, and the originals (which stay)
/// keep the tree at a few leaves at most.
#[test]
fn h1_home_tree_grows_to_three_levels_and_shrinks_back() {
    const N: usize = 2000;
    let f = Frame::new();
    let start = f.check("h1 start", Depth::Full);
    let start_home = f.home(&start).clone();
    eprintln!("h1 start: home height {} nodes {}", start_home.tree_height, start_home.total_nodes);

    let mut fs = mount(&f.dev);
    fs.create_directory("/home/user", "many").expect("mkdir /home/user/many");
    for chunk in (0..N).collect::<Vec<_>>().chunks(250) {
        let files: Vec<_> = chunk
            .iter()
            .map(|&i| ("/home/user/many".to_string(), long_name(i), payload(64 + i % 200, i as u64)))
            .collect();
        write_batch(&mut fs, &files);
    }
    drop(fs);
    let grown = f.check("h1 grown", Depth::Full);
    let g = f.home(&grown).clone();
    eprintln!("h1 grown: home height {} nodes {} items {}", g.tree_height, g.total_nodes, g.total_items);
    assert!(g.tree_height >= 2, "h1 grown: 2000 long-named files must give a 3-level home tree, got height {}", g.tree_height);

    let mut fs = mount(&f.dev);
    for i in 0..N / 2 {
        fs.delete_file(&format!("/home/user/many/{}", long_name(i))).expect("delete low");
    }
    drop(fs);
    let half = f.check("h1 left half deleted", Depth::Full);
    let h = f.home(&half).clone();
    eprintln!("h1 half: home height {} nodes {} items {}", h.tree_height, h.total_nodes, h.total_items);
    assert!(
        h.total_nodes < g.total_nodes,
        "h1 left half deleted: home nodes must fall ({} -> {})",
        g.total_nodes,
        h.total_nodes
    );
    let fs = mount(&f.dev);
    let left = snapshot_dir(&fs, "/home/user/many");
    assert_eq!(left.len(), N / 2, "h1 left half deleted: right half must survive");
    for i in (N / 2..N).step_by(97) {
        assert_eq!(left[&long_name(i)], payload(64 + i % 200, i as u64), "h1 left half deleted: survivor {i}");
    }
    drop(fs);

    let mut fs = mount(&f.dev);
    for i in (N / 2 + 10..N).rev() {
        fs.delete_file(&format!("/home/user/many/{}", long_name(i))).expect("delete high");
    }
    drop(fs);
    let sparse = f.check("h1 ten left", Depth::Full);
    let s = f.home(&sparse).clone();
    eprintln!("h1 sparse: home height {} nodes {} items {}", s.tree_height, s.total_nodes, s.total_items);
    assert!(
        s.total_nodes < h.total_nodes,
        "h1 ten left: home nodes must keep falling ({} -> {})",
        h.total_nodes,
        s.total_nodes
    );

    let mut fs = mount(&f.dev);
    for i in N / 2..N / 2 + 10 {
        fs.delete_file(&format!("/home/user/many/{}", long_name(i))).expect("delete last");
    }
    drop(fs);
    let empty = f.check("h1 emptied", Depth::Full);
    let e = f.home(&empty).clone();
    eprintln!("h1 empty: home height {} nodes {} leaves {} items {}", e.tree_height, e.total_nodes, e.total_leaves, e.total_items);
    assert!(e.tree_height < g.tree_height, "h1 emptied: the root must have collapsed ({} -> {})", g.tree_height, e.tree_height);
    assert!(
        e.total_leaves <= start_home.total_leaves + 1,
        "h1 emptied: home tree must be back to about its start size (leaves {} at start, {} now)",
        start_home.total_leaves,
        e.total_leaves
    );
    assert!(
        e.total_nodes <= start_home.total_nodes + 2,
        "h1 emptied: home nodes {} at start, {} now",
        start_home.total_nodes,
        e.total_nodes
    );

    let mut fs = mount(&f.dev);
    let files: Vec<_> = (0..50)
        .map(|i| ("/home/user/many".to_string(), format!("again-{i}"), payload(3000, 7000 + i)))
        .collect();
    write_batch(&mut fs, &files);
    drop(fs);
    f.check("h1 regrown", Depth::Full);
    let fs = mount(&f.dev);
    let got = snapshot_dir(&fs, "/home/user/many");
    assert_eq!(got.len(), 50, "h1 regrown: file count");
    for i in 0..50u64 {
        assert_eq!(got[&format!("again-{i}")], payload(3000, 7000 + i), "h1 regrown: new file {i}");
    }
}

// ------------------------------------------------------------------ H2

/// Scatter free space while writing in `/home/user/small` until the free-space
/// tree outgrows one leaf, then let it coalesce back (mirror of S2).
#[test]
fn h2_free_space_tree_grows_and_shrinks_while_writing_home() {
    const N: u64 = 1500;
    let f = Frame::new();
    f.check("h2 start", Depth::Full);

    let mut fs = mount(&f.dev);
    fs.create_directory("/home/user", "small").expect("mkdir small");
    for chunk in (0..N).collect::<Vec<_>>().chunks(250) {
        let files: Vec<_> = chunk
            .iter()
            .map(|&i| ("/home/user/small".to_string(), format!("{i:04}"), payload(4096, i)))
            .collect();
        write_batch(&mut fs, &files);
    }
    drop(fs);
    let before = f.check("h2 filled", Depth::Full);
    let fst_before = before.free_space_tree.as_ref().expect("fixture has a free-space tree").clone();

    let mut fs = mount(&f.dev);
    for i in (0..N).step_by(2) {
        fs.delete_file(&format!("/home/user/small/{i:04}")).expect("delete even");
    }
    drop(fs);
    let holes = f.check("h2 fragmented", Depth::Full);
    let fst_holes = holes.free_space_tree.as_ref().unwrap().clone();
    eprintln!(
        "h2: fst height {} items {} leaves {} -> fragmented height {} items {} leaves {}",
        fst_before.tree_height, fst_before.total_items, fst_before.total_leaves,
        fst_holes.tree_height, fst_holes.total_items, fst_holes.total_leaves
    );
    assert!(
        fst_holes.tree_height >= 1 && fst_holes.total_items > fst_before.total_items + 500,
        "h2 fragmented: 750 holes must push the free-space tree past one leaf (height {}, items {} -> {})",
        fst_holes.tree_height,
        fst_before.total_items,
        fst_holes.total_items
    );

    let fs = mount(&f.dev);
    for i in (1..N).step_by(400) {
        assert_eq!(
            fs.read_file(&format!("/home/user/small/{i:04}")).expect("read"),
            payload(4096, i),
            "h2 fragmented: file {i}"
        );
    }
    drop(fs);

    let mut fs = mount(&f.dev);
    for i in (1..N).step_by(2) {
        fs.delete_file(&format!("/home/user/small/{i:04}")).expect("delete odd");
    }
    drop(fs);
    let merged = f.check("h2 coalesced", Depth::Full);
    let fst_merged = merged.free_space_tree.as_ref().unwrap();
    eprintln!("h2: coalesced items {} leaves {}", fst_merged.total_items, fst_merged.total_leaves);
    assert!(
        fst_merged.total_items <= fst_before.total_items + 2,
        "h2 coalesced: free space must merge back ({} items before, {} after)",
        fst_before.total_items,
        fst_merged.total_items
    );
    assert!(
        fst_merged.total_leaves < fst_holes.total_leaves,
        "h2 coalesced: emptied free-space leaves must be freed ({} -> {})",
        fst_holes.total_leaves,
        fst_merged.total_leaves
    );
}

// ------------------------------------------------------------------ H3

/// Big files in `/home/user/big`: near-full checksum items, new data chunks,
/// then deleting some (mirror of S3).
#[test]
fn h3_big_files_in_home_fill_and_trim_the_checksum_tree() {
    let f = Frame::new();
    let chunks_before = mount(&f.dev).chunk_map().len();
    let start = f.check("h3 start", Depth::Full);
    let csum_start = start.csum_tree.as_ref().map(|c| c.total_items).unwrap_or(0);

    let mut fs = mount(&f.dev);
    fs.create_directory("/home/user", "big").expect("mkdir big");
    let files: Vec<_> = (0..4u64)
        .map(|i| ("/home/user/big".to_string(), format!("big{i}"), payload(24 * MIB + 4096 * i as usize, 100 + i)))
        .collect();
    write_batch(&mut fs, &files);
    let chunks_after = fs.chunk_map().len();
    drop(fs);
    let full = f.check("h3 written", Depth::Full);
    let csum = full.csum_tree.as_ref().expect("csum tree");
    eprintln!(
        "h3: csum items {} -> {} height {} leaves {}; chunks {} -> {}",
        csum_start, csum.total_items, csum.tree_height, csum.total_leaves, chunks_before, chunks_after
    );
    assert!(csum.tree_height >= 1, "h3 written: ~96 MiB of checksums must split the csum tree");
    assert!(chunks_after > chunks_before, "h3 written: 96 MiB must allocate new data chunks");

    let mut fs = mount(&f.dev);
    fs.delete_file("/home/user/big/big1").expect("delete big1");
    fs.delete_file("/home/user/big/big2").expect("delete big2");
    drop(fs);
    let trimmed = f.check("h3 two deleted", Depth::Full);
    assert!(
        trimmed.csum_tree.as_ref().unwrap().total_items < csum.total_items,
        "h3 two deleted: deleting files must remove their checksums"
    );

    let fs = mount(&f.dev);
    assert!(fs.read_file("/home/user/big/big0").expect("read big0") == files[0].2, "h3 two deleted: big0 changed");
    assert!(fs.read_file("/home/user/big/big3").expect("read big3") == files[3].2, "h3 two deleted: big3 changed");
    assert!(fs.read_file("/home/user/big/big1").is_err(), "h3 two deleted: big1 must be gone");
    let _ = (f.root_id, f.home_id);
}
