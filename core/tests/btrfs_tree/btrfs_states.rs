//! B-tree state suite: drive the real write API through the tree shapes a
//! user's drive goes through, and judge the disk after every phase.
//!
//! ```text
//! cargo test -p luks_core --features dangerous-write-support --release --test btrfs_states
//! ```
//!
//! # Why this exists
//!
//! Each scenario is one real-life way a tree changes shape:
//!
//! | test | real life | tree shapes it forces |
//! |---|---|---|
//! | `s1_*` | many files in one folder, then deleting them | leaf split, interior split, root growth, empty-leaf removal, parent-key fixup, root collapse |
//! | `s2_*` | an old drive with scattered free space | free-space tree past one leaf and back |
//! | `s3_*` | copying big files, then deleting some | near-full checksum items (3-way split), csum trims, data chunk allocation |
//! | `s4_*` | one copy job into two folders, a cancelled file, rename over a file | a batch touching two directories, a block allocated and freed in one transaction, freeing overwritten data |
//! | `s5_*` | the stick is pulled during a save that reshapes a tree | every cut point of a split commit and of a collapse commit |
//!
//! # The judge
//!
//! After every phase the disk is re-mounted from its committed bytes and
//! graded three ways, because each sees something the others cannot:
//!
//! 1. `TreeValidator::validate_all`: key order, parent keys, empty nodes,
//!    levels and generations in every tree.
//! 2. `AccountingOracle`: every reachable block and file extent is in the
//!    extent tree, block-group `used` adds up, the free-space tree agrees.
//! 3. The outside tool, `btrfs check --readonly`, run directly when btrfs-progs
//!    is installed, else `tools/verify-btrfs.sh` in colima (which also mounts
//!    and scrubs). Never grade our writer with only our own reader.
//!
//! Images are `fixtures/btrfs/states-4k.img` (256 MiB, 4 KiB nodes, made by
//! `tools/gen-btrfs-fixtures.sh` with mkfs alone), loaded into memory.
//!
//! # Found by this suite on first run
//!
//! - s4: the second folder a batch wrote into got its next directory index
//!   from a read of the batch's not-yet-written fs root on the device
//!   (`batch.rs`, now `max_offset_in_tree` through `pending`). Every file
//!   failed with `AlreadyExists("duplicate key in leaf")`.
//! - s2: every delete on a fragmented free-space tree failed with "transaction
//!   did not converge": the commit loop deleted an item before inserting its
//!   replacement, emptying and re-splitting the same leaf forever
//!   (`converge_and_finalize`, now inserts first).
//! - s1: `TreeValidator` rejected a root with one child, which btrfs and the
//!   kernel allow; that check was stricter than the format and is retired.

#![cfg(feature = "dangerous-write-support")]

#[path = "../common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::atomic::Ordering;

use common::accounting::AccountingOracle;
use common::btree_validator::{AllTreesValidationReport, TreeValidator};
use common::fail_after::FailAfterNWrites;
use common::mem_device::MemoryDevice;
use luks_core::device::WriteAt;
use luks_core::fs::btrfs::Btrfs;

const FIXTURE: &str = "btrfs/states-4k.img";

// ------------------------------------------------------------------ judge

/// How hard to grade one state.
#[derive(Clone, Copy, PartialEq)]
enum Depth {
    /// Our two checkers only. For the power-cut sweep, which grades dozens of
    /// states.
    Ours,
    /// Ours plus the outside tool.
    Full,
}

/// Grade the committed state of `dev`. Panics naming `step` on any finding.
fn judge(dev: &MemoryDevice, step: &str, depth: Depth) -> AllTreesValidationReport {
    // Outside tool first: when our checkers and it disagree, its verdict is
    // the one that says whether the disk is broken.
    if depth == Depth::Full {
        outside_check(dev, step);
    }
    let fs = Btrfs::mount(dev.clone()).unwrap_or_else(|e| panic!("[{step}] remount failed: {e}"));
    let report = TreeValidator::validate_all(&fs)
        .unwrap_or_else(|e| panic!("[{step}] tree validator: {e}"));
    if let Err(e) = AccountingOracle::check(&fs) {
        panic!("[{step}] accounting: {e}");
    }
    report
}

/// `btrfs check --readonly` on a dump of `dev`.
fn outside_check(dev: &MemoryDevice, step: &str) {
    let path = std::env::temp_dir().join(format!(
        "nyx-states-{}-{}.img",
        std::process::id(),
        step.replace(|c: char| !c.is_ascii_alphanumeric(), "_")
    ));
    dev.dump_to_file(&path).expect("dump image");
    let local = Command::new("btrfs").args(["check", "--readonly"]).arg(&path).output();
    let verdict = match local {
        Ok(out) => {
            let ok = out.status.success();
            (ok, format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
        }
        // No btrfs-progs on this host (macOS): the colima script, with its
        // own ALLOW_NO_ORACLE gate.
        Err(_) => {
            let (ok, out, err) = common::oracle::verify_btrfs(&path);
            (ok, format!("{out}{err}"))
        }
    };
    let _ = std::fs::remove_file(&path);
    assert!(verdict.0, "[{step}] btrfs check rejected the image:\n{}", verdict.1);
}

// ---------------------------------------------------------------- helpers

fn fresh() -> MemoryDevice {
    MemoryDevice::from_fixture(FIXTURE)
}

fn mount(dev: &MemoryDevice) -> Btrfs<MemoryDevice> {
    Btrfs::mount(dev.clone()).expect("mount")
}

/// Deterministic file content, different per `seed`.
fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Write `files` through the batch path the app uses, then commit.
fn write_batch<D: WriteAt>(fs: &mut Btrfs<D>, files: &[(String, String, Vec<u8>)]) {
    for (dir, name, data) in files {
        let mut w = fs.begin_file(data.len() as u64).unwrap_or_else(|e| panic!("begin {name}: {e}"));
        for piece in data.chunks(256 * 1024) {
            fs.write_chunk(&mut w, piece).unwrap_or_else(|e| panic!("write {name}: {e}"));
        }
        fs.finish_file(w, dir, name).unwrap_or_else(|e| panic!("finish {name}: {e}"));
    }
    fs.commit_active_batch().expect("commit batch");
}

/// A long name makes each directory entry big, so few fit in a 4 KiB leaf
/// and the tree grows tall after a few thousand files.
fn long_name(i: usize) -> String {
    format!("{i:05}-{}", "n".repeat(150))
}

/// Every regular file under `dir` (one level) with its contents.
fn snapshot_dir(fs: &Btrfs<MemoryDevice>, dir: &str) -> BTreeMap<String, Vec<u8>> {
    fs.list_dir(dir)
        .expect("list dir")
        .into_iter()
        .filter(|e| !e.name.starts_with('.'))
        .map(|e| {
            let p = format!("{}/{}", dir.trim_end_matches('/'), e.name);
            let data = fs.read_file(&p).unwrap_or_else(|err| panic!("read {p}: {err}"));
            (e.name, data)
        })
        .collect()
}

// ------------------------------------------------------------------ S1

/// Grow the fs tree to three levels, empty it from the left and then the
/// right, and use it again.
///
/// Deleting the lowest names first empties the leftmost leaves, which is the
/// path that removes nodes and must push a new lowest key up to every parent.
/// Deleting the rest from the top end empties the right side.
///
/// The engine never merges half-empty nodes (btrfs does not require it), so a
/// few survivors whose directory entries hash all over the key space keep the
/// tree tall. That is legal. What must hold: empty nodes are gone (the
/// validator rejects any), the node count falls with the items, and once the
/// folder is empty everything is back in one leaf.
#[test]
fn s1_fs_tree_grows_to_three_levels_and_shrinks_back() {
    const N: usize = 2000;
    let dev = fresh();
    let mut fs = mount(&dev);
    fs.create_directory("/", "d").expect("mkdir");

    for chunk in (0..N).collect::<Vec<_>>().chunks(250) {
        let files: Vec<_> = chunk
            .iter()
            .map(|&i| ("/d".to_string(), long_name(i), payload(64 + i % 200, i as u64)))
            .collect();
        write_batch(&mut fs, &files);
    }
    drop(fs);
    let grown = judge(&dev, "s1 grown", Depth::Full);
    assert!(
        grown.fs_tree.tree_height >= 2,
        "2000 long-named files must give a 3-level fs tree, got height {}",
        grown.fs_tree.tree_height
    );

    // Left side first.
    let mut fs = mount(&dev);
    for i in 0..N / 2 {
        fs.delete_file(&format!("/d/{}", long_name(i))).expect("delete low");
    }
    drop(fs);
    let half = judge(&dev, "s1 left half deleted", Depth::Full);
    assert!(
        half.fs_tree.total_nodes < grown.fs_tree.total_nodes,
        "deleting half the files must free nodes ({} -> {})",
        grown.fs_tree.total_nodes,
        half.fs_tree.total_nodes
    );
    let fs = mount(&dev);
    let left = snapshot_dir(&fs, "/d");
    assert_eq!(left.len(), N / 2, "right half must survive");
    for i in (N / 2..N).step_by(97) {
        assert_eq!(left[&long_name(i)], payload(64 + i % 200, i as u64), "survivor {i} content");
    }
    drop(fs);

    // Then the right side, highest names first, leaving 10.
    let mut fs = mount(&dev);
    for i in (N / 2 + 10..N).rev() {
        fs.delete_file(&format!("/d/{}", long_name(i))).expect("delete high");
    }
    drop(fs);
    let sparse = judge(&dev, "s1 ten left", Depth::Full);
    assert!(
        sparse.fs_tree.total_nodes < half.fs_tree.total_nodes,
        "node count must keep falling ({} -> {})",
        half.fs_tree.total_nodes,
        sparse.fs_tree.total_nodes
    );

    // Last ten: the folder is empty, a handful of items remain in the tree.
    let mut fs = mount(&dev);
    for i in N / 2..N / 2 + 10 {
        fs.delete_file(&format!("/d/{}", long_name(i))).expect("delete last");
    }
    drop(fs);
    let empty = judge(&dev, "s1 emptied", Depth::Full);
    // One mutation collapses the root by one level, like the kernel, so a
    // single-child chain above the last leaf is legal. All the items must
    // be back in one leaf, every emptied node freed.
    assert_eq!(
        empty.fs_tree.total_leaves, 1,
        "an empty folder fits in one leaf (height {}, nodes {})",
        empty.fs_tree.tree_height, empty.fs_tree.total_nodes
    );
    assert_eq!(
        empty.fs_tree.total_nodes,
        empty.fs_tree.tree_height as usize + 1,
        "only a single-child chain may sit above that leaf"
    );
    assert!(empty.fs_tree.tree_height < grown.fs_tree.tree_height, "the root must have collapsed");

    // The collapsed tree must still take new work.
    let mut fs = mount(&dev);
    let files: Vec<_> = (0..50)
        .map(|i| ("/d".to_string(), format!("again-{i}"), payload(3000, 7000 + i)))
        .collect();
    write_batch(&mut fs, &files);
    drop(fs);
    judge(&dev, "s1 regrown", Depth::Full);

    let fs = mount(&dev);
    let got = snapshot_dir(&fs, "/d");
    assert_eq!(got.len(), 50);
    for i in 0..50u64 {
        assert_eq!(got[&format!("again-{i}")], payload(3000, 7000 + i), "new file {i} content");
    }
}

// ------------------------------------------------------------------ S2

/// Scatter free space until the free-space tree outgrows one leaf, then let
/// it coalesce back.
///
/// 1500 one-block files sit side by side; deleting every other one leaves
/// ~750 separate holes, one free-space item each (~158 fit in a 4 KiB leaf).
/// Deleting the rest merges them back into a few large extents.
///
/// Found with this test: every one of the ~750 deletes on the fragmented
/// tree failed with "transaction did not converge". The commit loop split a
/// free-space leaf, emptied it the next round, and split it again forever
/// (see the comments in `converge_and_finalize`).
#[test]
fn s2_free_space_tree_grows_and_shrinks() {
    const N: u64 = 1500;
    let dev = fresh();
    let mut fs = mount(&dev);
    fs.create_directory("/", "f").expect("mkdir");
    for chunk in (0..N).collect::<Vec<_>>().chunks(250) {
        let files: Vec<_> = chunk
            .iter()
            .map(|&i| ("/f".to_string(), format!("{i:04}"), payload(4096, i)))
            .collect();
        write_batch(&mut fs, &files);
    }
    drop(fs);
    let before = judge(&dev, "s2 filled", Depth::Full);
    let fst_before = before.free_space_tree.as_ref().expect("fixture has a free-space tree");

    let mut fs = mount(&dev);
    for i in (0..N).step_by(2) {
        fs.delete_file(&format!("/f/{i:04}")).expect("delete even");
    }
    drop(fs);
    let holes = judge(&dev, "s2 fragmented", Depth::Full);
    let fst_holes = holes.free_space_tree.as_ref().unwrap();
    assert!(
        fst_holes.tree_height >= 1 && fst_holes.total_items > fst_before.total_items + 500,
        "750 holes must push the free-space tree past one leaf (height {}, items {} -> {})",
        fst_holes.tree_height,
        fst_before.total_items,
        fst_holes.total_items
    );

    let fs = mount(&dev);
    for i in (1..N).step_by(400) {
        assert_eq!(fs.read_file(&format!("/f/{i:04}")).expect("read"), payload(4096, i), "file {i}");
    }
    drop(fs);

    let mut fs = mount(&dev);
    for i in (1..N).step_by(2) {
        fs.delete_file(&format!("/f/{i:04}")).expect("delete odd");
    }
    drop(fs);
    let merged = judge(&dev, "s2 coalesced", Depth::Full);
    let fst_merged = merged.free_space_tree.as_ref().unwrap();
    assert!(
        fst_merged.total_items <= fst_before.total_items + 2,
        "free space must merge back ({} items before, {} after)",
        fst_before.total_items,
        fst_merged.total_items
    );
    assert!(
        fst_merged.total_leaves < fst_holes.total_leaves,
        "emptied free-space leaves must be freed ({} -> {})",
        fst_holes.total_leaves,
        fst_merged.total_leaves
    );
}

// ------------------------------------------------------------------ S3

/// Big files: near-full checksum items, new data chunks, then deleting some.
///
/// One 4 KiB csum leaf holds about 990 sectors (~4 MiB) of crc32c, so a
/// 24 MiB file is split across several near-maximum items, which is what
/// forces the three-way leaf split. 96 MiB also outgrows the data chunk
/// mkfs made, so new chunks are allocated mid-batch.
#[test]
fn s3_big_files_fill_and_trim_the_checksum_tree() {
    const MIB: usize = 1024 * 1024;
    let dev = fresh();
    let chunks_before = mount(&dev).chunk_map().len();

    let mut fs = mount(&dev);
    let files: Vec<_> = (0..4u64)
        .map(|i| ("/".to_string(), format!("big{i}"), payload(24 * MIB + 4096 * i as usize, 100 + i)))
        .collect();
    write_batch(&mut fs, &files);
    let chunks_after = fs.chunk_map().len();
    drop(fs);
    let full = judge(&dev, "s3 written", Depth::Full);
    let csum = full.csum_tree.as_ref().expect("csum tree");
    assert!(csum.tree_height >= 1, "~100 MiB of checksums must split the csum tree");
    assert!(chunks_after > chunks_before, "96 MiB must allocate new data chunks");

    let mut fs = mount(&dev);
    fs.delete_file("/big1").expect("delete big1");
    fs.delete_file("/big2").expect("delete big2");
    drop(fs);
    let trimmed = judge(&dev, "s3 two deleted", Depth::Full);
    assert!(
        trimmed.csum_tree.as_ref().unwrap().total_items < csum.total_items,
        "deleting files must remove their checksums"
    );

    let fs = mount(&dev);
    assert_eq!(fs.read_file("/big0").expect("read big0"), files[0].2);
    assert_eq!(fs.read_file("/big3").expect("read big3"), files[3].2);
}

// ------------------------------------------------------------------ S4

/// Batch shapes the app produces: one job writing into two folders, a file
/// cancelled half-way, and renames, including one over an existing file.
#[test]
fn s4_batch_into_two_folders_cancel_and_rename_over() {
    let dev = fresh();
    let mut fs = mount(&dev);
    fs.create_directory("/", "x").expect("mkdir x");
    fs.create_directory("/", "y").expect("mkdir y");

    // Seed both folders so their next directory index is not the first one.
    let seed: Vec<_> = (0..5u64)
        .flat_map(|i| {
            [
                ("/x".to_string(), format!("seed{i}"), payload(5000, 10 + i)),
                ("/y".to_string(), format!("seed{i}"), payload(5000, 20 + i)),
            ]
        })
        .collect();
    write_batch(&mut fs, &seed);

    // One batch alternating between the folders, with a cancelled file in
    // the middle: its blocks are reserved, then given back, before commit.
    let mut expected_x = BTreeMap::new();
    let mut expected_y = BTreeMap::new();
    for i in 0..60u64 {
        let (dir, map) = if i % 2 == 0 { ("/x", &mut expected_x) } else { ("/y", &mut expected_y) };
        let name = format!("f{i}");
        let data = payload(5000 + i as usize * 37, 1000 + i);
        let mut w = fs.begin_file(data.len() as u64).expect("begin");
        fs.write_chunk(&mut w, &data).expect("write");
        fs.finish_file(w, dir, &name).expect("finish");
        map.insert(name, data);
        if i == 30 {
            let mut w = fs.begin_file(64 * 1024).expect("begin cancelled");
            fs.write_chunk(&mut w, &[7u8; 32 * 1024]).expect("write cancelled");
            fs.abandon_file(w).expect("abandon");
        }
    }
    fs.commit_active_batch().expect("commit");
    drop(fs);
    judge(&dev, "s4 two-folder batch", Depth::Full);

    let fs = mount(&dev);
    let got_x = snapshot_dir(&fs, "/x");
    let got_y = snapshot_dir(&fs, "/y");
    for (name, data) in &expected_x {
        assert_eq!(got_x.get(name), Some(data), "/x/{name}");
    }
    for (name, data) in &expected_y {
        assert_eq!(got_y.get(name), Some(data), "/y/{name}");
    }
    assert_eq!(got_x.len(), 35);
    assert_eq!(got_y.len(), 35);
    drop(fs);

    // Rename across folders, then over an existing file (frees its data).
    let mut fs = mount(&dev);
    fs.rename("/x", "f0", "/y", "moved").expect("rename across");
    fs.rename("/x", "f2", "/y", "f1").expect("rename over existing");
    drop(fs);
    judge(&dev, "s4 renamed", Depth::Full);

    let fs = mount(&dev);
    assert_eq!(fs.read_file("/y/moved").expect("read"), expected_x["f0"]);
    assert_eq!(fs.read_file("/y/f1").expect("read"), expected_x["f2"], "overwritten by f2");
    assert!(fs.read_file("/x/f0").is_err() && fs.read_file("/x/f2").is_err(), "sources gone");
}

// ------------------------------------------------------------------ S5

/// Number of `write_at` calls `op` makes on a copy of `base`.
type CutOp<'a> = &'a dyn Fn(&mut Btrfs<FailAfterNWrites<MemoryDevice>>) -> luks_core::error::Result<()>;

fn count_writes(base: &[u8], op: CutOp) -> usize {
    let dev = FailAfterNWrites::new(MemoryDevice::from_bytes(base.to_vec()), usize::MAX);
    let mut fs = Btrfs::mount(dev).expect("mount");
    op(&mut fs).expect("uncut run must succeed");
    fs.device().attempted.load(Ordering::SeqCst)
}

/// Cut points worth trying: all of them when few, otherwise an even spread
/// plus every one of the last 12 (the superblock and flush end of a commit).
fn cut_points(total: usize) -> Vec<usize> {
    if total <= 60 {
        return (0..total).collect();
    }
    let mut pts: Vec<usize> = (0..40).map(|k| k * total / 40).collect();
    pts.extend(total.saturating_sub(12)..total);
    pts.sort_unstable();
    pts.dedup();
    pts
}

/// Run `op` on a copy of `base` with the device vanishing after `n` writes,
/// then hand back what is on the device.
fn cut_after(base: &[u8], n: usize, op: CutOp) -> MemoryDevice {
    let mem = MemoryDevice::from_bytes(base.to_vec());
    let mut fs = Btrfs::mount(FailAfterNWrites::new(mem.clone(), n)).expect("mount");
    // The op fails wherever the device vanished, or finishes if `n` is past
    // its last write. Either is fine: only what is on the device is judged.
    let _ = op(&mut fs);
    mem
}

/// A base image with 600 long-named files in /d: the fs tree has many leaves
/// and an interior level, so the next writes split and the next deletes
/// remove leaves.
fn s5_base() -> (Vec<u8>, BTreeMap<String, Vec<u8>>) {
    let dev = fresh();
    let mut fs = mount(&dev);
    fs.create_directory("/", "d").expect("mkdir");
    let files: Vec<_> = (0..600).map(|i| ("/d".to_string(), long_name(i), payload(100, i as u64))).collect();
    write_batch(&mut fs, &files);
    let state = snapshot_dir(&fs, "/d");
    drop(fs);
    let r = judge(&dev, "s5 base", Depth::Ours);
    assert!(r.fs_tree.tree_height >= 1);
    (dev.snapshot(), state)
}

/// Power cut during a batch whose commits split leaves.
///
/// The batch commits every few files, so a cut may land after some commits:
/// the folder must hold the old files plus a prefix of the new ones, in
/// order, never a file half-added, and the disk must be clean.
#[test]
fn s5_power_cut_during_split_commit_is_all_or_nothing() {
    let (base, old) = s5_base();
    let new_files: Vec<_> = (600..640).map(|i| ("/d".to_string(), long_name(i), payload(100, i as u64))).collect();
    let op = |fs: &mut Btrfs<FailAfterNWrites<MemoryDevice>>| -> luks_core::error::Result<()> {
        for (dir, name, data) in &new_files {
            let mut w = fs.begin_file(data.len() as u64)?;
            fs.write_chunk(&mut w, data)?;
            fs.finish_file(w, dir, name)?;
        }
        fs.commit_active_batch()
    };

    let total = count_writes(&base, &op);
    assert!(total > 10, "the batch must take several writes, got {total}");
    let pts = cut_points(total + 1);
    let outside = [pts[0], pts[pts.len() / 2], *pts.last().unwrap()];
    let (mut saw_none, mut saw_all) = (false, false);
    for &n in &pts {
        let dev = cut_after(&base, n, &op);
        let step = format!("s5 split cut after {n}/{total}");
        judge(&dev, &step, if outside.contains(&n) { Depth::Full } else { Depth::Ours });
        let got = snapshot_dir(&mount(&dev), "/d");
        let landed = new_files.iter().take_while(|(_, name, _)| got.contains_key(name)).count();
        let mut expect = old.clone();
        for (_, name, data) in &new_files[..landed] {
            expect.insert(name.clone(), data.clone());
        }
        assert!(got == expect, "[{step}] folder is not old + the first {landed} new files ({} files)", got.len());
        saw_none |= landed == 0;
        saw_all |= landed == new_files.len();
    }
    assert!(saw_none && saw_all, "the sweep must cover both ends");
}

/// Power cut while deleting files one by one, which empties and removes
/// leaves: whatever survives is a suffix of the deletion order, the files
/// left read back intact, and the disk is clean.
#[test]
fn s5_power_cut_during_deletes_leaves_a_clean_prefix() {
    let (base, old) = s5_base();
    // Lowest names first: these empty the leftmost leaves.
    let doomed: Vec<String> = (0..40).map(long_name).collect();
    let op = |fs: &mut Btrfs<FailAfterNWrites<MemoryDevice>>| -> luks_core::error::Result<()> {
        for name in &doomed {
            fs.delete_file(&format!("/d/{name}"))?;
        }
        Ok(())
    };

    let total = count_writes(&base, &op);
    let pts = cut_points(total + 1);
    let outside = [pts[pts.len() / 3], *pts.last().unwrap()];
    for &n in &pts {
        let dev = cut_after(&base, n, &op);
        let step = format!("s5 delete cut after {n}/{total}");
        judge(&dev, &step, if outside.contains(&n) { Depth::Full } else { Depth::Ours });
        let got = snapshot_dir(&mount(&dev), "/d");
        let gone = doomed.iter().take_while(|name| !got.contains_key(*name)).count();
        assert!(
            doomed[gone..].iter().all(|name| got.contains_key(name)),
            "[{step}] deletions are not a prefix of the order"
        );
        assert_eq!(got.len(), old.len() - gone, "[{step}] unexpected files gone");
        for (name, data) in &got {
            assert_eq!(&old[name], data, "[{step}] {name} changed");
        }
    }
}
