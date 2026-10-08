//! Power-cut and batch safety inside the `home` subvolume of a Fedora-shaped
//! disk.
//!
//! ```text
//! cargo test -p luks_core --features dangerous-write-support --release \
//!     --test btrfs_subvol_power_cut -- --nocapture
//! # outside checker from a specific btrfs-progs:
//! NYX_BTRFS=/path/to/btrfs cargo test ... --test btrfs_subvol_power_cut
//! ```
//!
//! # Purpose
//!
//! `fixtures/btrfs/fedora-4k.img` mirrors the real SSD: subvolumes `root`
//! (id 256) and `home` (id 257), zstd. The app writes only inside
//! `/home/user`, and the USB cable can be pulled at ANY moment. This suite
//! ports the B-tree state scenarios (S4 batches, S5 power-cut sweeps) from the
//! top-level tree into the `home` subvolume and proves, for every state:
//!
//! - the disk mounts and passes our tree validator and accounting oracle
//!   (and `btrfs check --readonly` at [`Depth::Full`] points);
//! - the `root` subvolume tree and the top-level tree are byte-for-byte
//!   untouched (`Untouched`), as is every file of `/root`;
//! - every pre-existing `/home/user` file is unchanged (these tests never
//!   delete or modify originals);
//! - the new data is either absent, a whole file, or (for batches) the old set
//!   plus a prefix of the new files. Never a partial file, never anything else.
//!
//! | test | what it proves |
//! |---|---|
//! | `h4_*` | one job into two new folders and an existing one, a cancelled file, rename across folders and over a file, all in `home` |
//! | `h4b_*` | a batch that starts in `/home/user` and then targets `/root/etc` is refused and leaves `root` alone |
//! | `h5_power_cut_during_home_tree_split_commit_*` | cut at every point of a batch that splits leaves of a tall `home` tree |
//! | `h5_power_cut_during_home_deletes_*` | cut at every point of deletes that remove leaves |
//! | `h5_power_cut_during_big_file_write_in_home` | cut during an 8 MiB file (data, csum, extent, free-space updates) |
//!
//! A batch commits every few files, so a cut leaves "old set + a prefix of the
//! new files".
//!
//! If the fixture is missing the tests fail with the command that makes it:
//! `tools/gen-btrfs-fixtures.sh <outdir> fedora-4k`.

#![cfg(feature = "dangerous-write-support")]

#[path = "../common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::path::Path;

use common::fail_after::FailAfterNWrites;
use common::mem_device::MemoryDevice;
use common::states::{
    count_writes, cut_after, cut_points, files_print, fresh_from, judge, long_name, mount, payload,
    snapshot_dir, subvol_id, tree_print, write_batch, Depth, Untouched,
};
use luks_core::fs::btrfs::Btrfs;

const FIXTURE: &str = "btrfs/fedora-4k.img";
const HOME: &str = "/home/user";

type Files = BTreeMap<String, Vec<u8>>;
type CutFs = Btrfs<FailAfterNWrites<MemoryDevice>>;

fn fresh() -> MemoryDevice {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("fixtures").join(FIXTURE);
    assert!(
        p.exists(),
        "fixture {} is missing; build it with `tools/gen-btrfs-fixtures.sh <outdir> fedora-4k` \
         and copy it to fixtures/btrfs/",
        p.display()
    );
    fresh_from(FIXTURE)
}

/// What must survive every step: the protected trees and the original
/// `/home/user` contents.
struct Frame {
    untouched: Untouched,
    originals: Files,
}

impl Frame {
    fn capture(fs: &Btrfs<MemoryDevice>) -> Self {
        let root_id = subvol_id(fs, "/root");
        assert_ne!(root_id, subvol_id(fs, "/home"));
        let originals = files_print(fs, HOME);
        assert!(originals.contains_key("hello.txt") && originals.contains_key("Documents/notes.txt"));
        Frame { untouched: Untouched::capture(fs, &[5, root_id], &["/root"]), originals }
    }

    /// Re-mount the committed bytes and check the protected state.
    fn check(&self, dev: &MemoryDevice, step: &str) {
        let fs = mount(dev);
        self.untouched.assert_same(&fs, step);
        let now = files_print(&fs, HOME);
        for (k, v) in &self.originals {
            // Directories (DIR_MARK) are compared too: they must still exist.
            match now.get(k) {
                None => panic!("[{step}] original {HOME}/{k} disappeared"),
                Some(w) => assert!(w == v, "[{step}] original {HOME}/{k} changed"),
            }
        }
    }
}


trait FsPut {
    fn put(&mut self, dir: &str, name: &str, data: &[u8]) -> luks_core::error::Result<()>;
}

impl<D: luks_core::device::WriteAt> FsPut for Btrfs<D> {
    fn put(&mut self, dir: &str, name: &str, data: &[u8]) -> luks_core::error::Result<()> {
        let mut w = self.begin_file(data.len() as u64)?;
        for piece in data.chunks(256 * 1024) {
            self.write_chunk(&mut w, piece)?;
        }
        self.finish_file(w, dir, name)?;
        Ok(())
    }
}

// ------------------------------------------------------------------ H4

/// S4 inside `home`: one job into two new folders and an existing original
/// folder, a cancelled file, rename across folders and over an existing file.
#[test]
fn h4_home_batch_into_two_folders_cancel_and_rename_over() {
    let dev = fresh();
    let frame = Frame::capture(&mount(&dev));
    let mut fs = mount(&dev);
    fs.create_directory(HOME, "a").expect("mkdir a");
    fs.create_directory(HOME, "b").expect("mkdir b");
    let (da, db, dd) = (format!("{HOME}/a"), format!("{HOME}/b"), format!("{HOME}/Documents"));

    let seed: Vec<_> = (0..5u64)
        .flat_map(|i| {
            [
                (da.clone(), format!("seed{i}"), payload(5000, 10 + i)),
                (db.clone(), format!("seed{i}"), payload(5000, 20 + i)),
            ]
        })
        .collect();
    write_batch(&mut fs, &seed);

    let (mut exp_a, mut exp_b, mut exp_d) = (BTreeMap::new(), BTreeMap::new(), BTreeMap::new());
    for i in 0..60u64 {
        let (dir, map) = match i % 3 {
            0 => (&da, &mut exp_a),
            1 => (&db, &mut exp_b),
            _ => (&dd, &mut exp_d),
        };
        let name = if i % 3 == 2 { format!("added{i}.txt") } else { format!("f{i}") };
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
    judge(&dev, "h4 batch", Depth::Full);
    frame.check(&dev, "h4 batch");

    let fs = mount(&dev);
    let (got_a, got_b) = (snapshot_dir(&fs, &da), snapshot_dir(&fs, &db));
    for (name, data) in &exp_a {
        assert_eq!(got_a.get(name), Some(data), "h4 {da}/{name}");
    }
    for (name, data) in &exp_b {
        assert_eq!(got_b.get(name), Some(data), "h4 {db}/{name}");
    }
    assert_eq!(got_a.len(), 5 + exp_a.len(), "h4 /a: seeds + new, no cancelled file");
    assert_eq!(got_b.len(), 5 + exp_b.len(), "h4 /b: seeds + new, no cancelled file");
    for (name, data) in &exp_d {
        assert_eq!(fs.read_file(&format!("{dd}/{name}")).expect("read added"), *data, "h4 {dd}/{name}");
    }
    // Documents holds exactly the original entries plus the added files.
    let docs = files_print(&fs, &dd);
    assert_eq!(docs.len(), 1 + exp_d.len(), "h4 Documents: notes.txt + added files, got {:?}", docs.keys());
    drop(fs);

    // Rename across folders, then over an existing file (frees its data).
    let mut fs = mount(&dev);
    fs.rename(&da, "f0", &db, "moved").expect("rename across");
    fs.rename(&da, "f3", &db, "f1").expect("rename over existing");
    drop(fs);
    judge(&dev, "h4 renamed", Depth::Full);
    frame.check(&dev, "h4 renamed");

    let fs = mount(&dev);
    assert_eq!(fs.read_file(&format!("{db}/moved")).expect("read"), exp_a["f0"]);
    assert_eq!(fs.read_file(&format!("{db}/f1")).expect("read"), exp_a["f3"], "f1 overwritten by f3");
    assert!(
        fs.read_file(&format!("{da}/f0")).is_err() && fs.read_file(&format!("{da}/f3")).is_err(),
        "rename sources gone"
    );
}

/// A batch that starts in `/home/user` and then names a file in `/root/etc`
/// is refused (one batch commits one subvolume tree). The refused file leaves
/// no trace; the file already in the batch is still pending and lands when the
/// batch is committed; `root` never changes.
#[test]
fn h4b_batch_spanning_root_and_home_is_refused() {
    let dev = fresh();
    let frame = Frame::capture(&mount(&dev));
    let first = payload(9000, 1);
    let second = payload(9000, 2);

    // Variant 1: refusal, then commit the pending home file.
    let mut fs = mount(&dev);
    fs.put(HOME, "first.bin", &first).expect("home file into batch");
    let err = fs.put("/root/etc", "second.bin", &second).expect_err("cross-subvolume file must be refused");
    let msg = err.to_string();
    assert!(msg.contains("multi-subvolume"), "unexpected refusal: {msg}");
    // The refusal did not poison the batch: the home file is still there to commit.
    fs.commit_active_batch().expect("commit pending home file after refusal");
    drop(fs);
    judge(&dev, "h4b refused then committed", Depth::Full);
    frame.check(&dev, "h4b refused then committed");
    let fs = mount(&dev);
    assert_eq!(fs.read_file(&format!("{HOME}/first.bin")).expect("first lands"), first);
    assert!(fs.read_file("/root/etc/second.bin").is_err(), "refused file must not exist");
    drop(fs);

    // Variant 2: refusal, then the batch is abandoned (teardown without commit).
    let dev2 = fresh();
    let mut fs = mount(&dev2);
    fs.put(HOME, "first.bin", &first).expect("home file into batch");
    fs.put("/root/etc", "second.bin", &second).expect_err("refused");
    assert!(fs.discard_active_batch(), "batch was open");
    drop(fs);
    judge(&dev2, "h4b refused then discarded", Depth::Full);
    frame.check(&dev2, "h4b refused then discarded");
    let fs = mount(&dev2);
    assert!(fs.read_file(&format!("{HOME}/first.bin")).is_err(), "uncommitted file must not appear");
    assert!(fs.read_file("/root/etc/second.bin").is_err());
}

// ------------------------------------------------------------------ H5

/// fedora-4k plus 600 long-named files in `/home/user/d`: the home tree has
/// many leaves and an interior level.
fn h5_base() -> (Vec<u8>, Files, Frame) {
    let dev = fresh();
    let frame = Frame::capture(&mount(&dev));
    let mut fs = mount(&dev);
    fs.create_directory(HOME, "d").expect("mkdir d");
    let dir = format!("{HOME}/d");
    let files: Vec<_> = (0..600).map(|i| (dir.clone(), long_name(i), payload(100, i as u64))).collect();
    write_batch(&mut fs, &files);
    let state = snapshot_dir(&fs, &dir);
    drop(fs);
    judge(&dev, "h5 base", Depth::Full);
    // `judge`'s report describes tree 5; the height that matters is home's.
    let home = tree_print(&mount(&dev), subvol_id(&mount(&dev), "/home"));
    assert!(home.level >= 1, "home tree must have an interior level, got level {}", home.level);
    println!("h5 base: home tree level {}, {} nodes", home.level, home.nodes.len());
    frame.check(&dev, "h5 base");
    (dev.snapshot(), state, frame)
}

/// The base's `/home/user/d` (created by this suite) is an addition to the
/// originals; the frame's `originals` are from the pristine image.
#[test]
fn h5_power_cut_during_home_tree_split_commit_is_all_or_nothing() {
    let (base, old, frame) = h5_base();
    let dir = format!("{HOME}/d");
    let new_files: Vec<_> = (600..640).map(|i| (dir.clone(), long_name(i), payload(100, i as u64))).collect();
    let op = |fs: &mut CutFs| -> luks_core::error::Result<()> {
        for (d, name, data) in &new_files {
            let mut w = fs.begin_file(data.len() as u64)?;
            fs.write_chunk(&mut w, data)?;
            fs.finish_file(w, d, name)?;
        }
        fs.commit_active_batch()
    };

    let total = count_writes(&base, &op);
    assert!(total > 10, "the batch must take several writes, got {total}");
    let pts = cut_points(total + 1);
    let outside = [pts[0], pts[pts.len() / 2], *pts.last().unwrap()];
    println!("h5a: {total} writes, {} cut points", pts.len());
    let (mut saw_none, mut saw_all) = (false, false);
    for &n in &pts {
        let dev = cut_after(&base, n, &op);
        let step = format!("h5a split cut after {n}/{total}");
        judge(&dev, &step, if outside.contains(&n) { Depth::Full } else { Depth::Ours });
        frame.check(&dev, &step);
        let got = snapshot_dir(&mount(&dev), &dir);
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

#[test]
fn h5_power_cut_during_home_deletes_leaves_a_clean_prefix() {
    let (base, old, frame) = h5_base();
    let doomed: Vec<String> = (0..40).map(long_name).collect();
    let op = |fs: &mut CutFs| -> luks_core::error::Result<()> {
        for name in &doomed {
            fs.delete_file(&format!("{HOME}/d/{name}"))?;
        }
        Ok(())
    };

    let total = count_writes(&base, &op);
    let pts = cut_points(total + 1);
    let outside = [pts[0], pts[pts.len() / 3], *pts.last().unwrap()];
    println!("h5b: {total} writes, {} cut points", pts.len());
    for &n in &pts {
        let dev = cut_after(&base, n, &op);
        let step = format!("h5b delete cut after {n}/{total}");
        judge(&dev, &step, if outside.contains(&n) { Depth::Full } else { Depth::Ours });
        frame.check(&dev, &step);
        let got = snapshot_dir(&mount(&dev), &format!("{HOME}/d"));
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

#[test]
fn h5_power_cut_during_big_file_write_in_home() {
    const MIB: usize = 1024 * 1024;
    let dev = fresh();
    let frame = Frame::capture(&mount(&dev));
    let base = dev.snapshot();
    let big = payload(8 * MIB, 77);
    let op = |fs: &mut CutFs| -> luks_core::error::Result<()> {
        fs.put(HOME, "big.bin", &big)?;
        fs.commit_active_batch()
    };

    let total = count_writes(&base, &op);
    assert!(total > 10, "an 8 MiB file must take several writes, got {total}");
    let pts = cut_points(total + 1);
    let outside = [pts[0], pts[pts.len() / 2], *pts.last().unwrap()];
    println!("h5c: {total} writes, {} cut points", pts.len());
    let (mut saw_absent, mut saw_whole) = (false, false);
    for &n in &pts {
        let dev = cut_after(&base, n, &op);
        let step = format!("h5c big file cut after {n}/{total}");
        judge(&dev, &step, if outside.contains(&n) { Depth::Full } else { Depth::Ours });
        frame.check(&dev, &step);
        let fs = mount(&dev);
        let listed = fs.list_dir(HOME).expect("list").iter().any(|e| e.name == "big.bin");
        if listed {
            let data = fs.read_file(&format!("{HOME}/big.bin")).unwrap_or_else(|e| panic!("[{step}] read big.bin: {e}"));
            assert!(data == big, "[{step}] big.bin is visible but not byte-exact ({} bytes)", data.len());
            saw_whole = true;
        } else {
            saw_absent = true;
        }
    }
    assert!(saw_absent && saw_whole, "the sweep must cover both ends");
}
