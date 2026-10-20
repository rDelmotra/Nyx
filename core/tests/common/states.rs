//! Shared judge and helpers for suites that drive the real btrfs write API
//! and grade the committed disk afterwards.
//!
//! Requires `dangerous-write-support` (declared that way in `common/mod.rs`).
//! `common` is compiled separately into every test binary, so unused items
//! are normal; `common/mod.rs` carries `#![allow(dead_code)]`.
//!
//! # The judge
//!
//! [`judge`] re-mounts the disk from its committed bytes and grades it:
//!
//! 1. `TreeValidator::validate_all` (key order, parent keys, levels, ...).
//! 2. `AccountingOracle` (extent tree, block groups, free-space tree).
//! 3. With [`Depth::Full`], the outside tool `btrfs check --readonly`.
//!
//! # Environment
//!
//! - `NYX_BTRFS`: path of the `btrfs` binary used by [`outside_check`].
//!   Default: `btrfs` from `PATH`. If the binary cannot be started, the
//!   check falls back to `tools/verify-btrfs.sh` (colima), which has its own
//!   `ALLOW_NO_ORACLE` gate (see `common::oracle`).

use std::collections::BTreeMap;
use std::process::Command;
use std::sync::atomic::Ordering;

use super::accounting::AccountingOracle;
use super::btree_validator::{AllTreesValidationReport, TreeValidator};
use super::fail_after::FailAfterNWrites;
use super::mem_device::MemoryDevice;
use luks_core::device::{ReadAt, WriteAt};
use luks_core::fs::btrfs::Btrfs;

/// How hard to grade one state.
#[derive(Clone, Copy, PartialEq)]
pub enum Depth {
    /// Our two checkers only. For the power-cut sweep, which grades dozens of
    /// states.
    Ours,
    /// Ours plus the outside tool.
    Full,
}

/// Grade the committed state of `dev`. Panics naming `step` on any finding.
pub fn judge(dev: &MemoryDevice, step: &str, depth: Depth) -> AllTreesValidationReport {
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

/// `btrfs check --readonly` on a dump of `dev`. The binary is `$NYX_BTRFS`
/// if set, else `btrfs` from `PATH`.
pub fn outside_check(dev: &MemoryDevice, step: &str) {
    let path = std::env::temp_dir().join(format!(
        "nyx-states-{}-{}.img",
        std::process::id(),
        step.replace(|c: char| !c.is_ascii_alphanumeric(), "_")
    ));
    dev.dump_to_file(&path).expect("dump image");
    let bin = std::env::var_os("NYX_BTRFS").unwrap_or_else(|| "btrfs".into());
    let local = Command::new(bin).args(["check", "--readonly"]).arg(&path).output();
    let verdict = match local {
        Ok(out) => {
            let ok = out.status.success();
            (ok, format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
        }
        // No btrfs-progs on this host (macOS): the colima script, with its
        // own ALLOW_NO_ORACLE gate.
        Err(_) => {
            let (ok, out, err) = super::oracle::verify_btrfs(&path);
            (ok, format!("{out}{err}"))
        }
    };
    let _ = std::fs::remove_file(&path);
    assert!(verdict.0, "[{step}] btrfs check rejected the image:\n{}", verdict.1);
}

// ---------------------------------------------------------------- helpers

/// A fresh in-memory copy of `fixtures/<fixture>`, e.g. `"btrfs/states-4k.img"`.
pub fn fresh_from(fixture: &str) -> MemoryDevice {
    MemoryDevice::from_fixture(fixture)
}

pub fn mount(dev: &MemoryDevice) -> Btrfs<MemoryDevice> {
    Btrfs::mount(dev.clone()).expect("mount")
}

/// Deterministic file content, different per `seed`.
pub fn payload(len: usize, seed: u64) -> Vec<u8> {
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

/// Write `files` (`(dir, name, data)`) through the batch path the app uses,
/// then commit.
pub fn write_batch<D: WriteAt>(fs: &mut Btrfs<D>, files: &[(String, String, Vec<u8>)]) {
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
pub fn long_name(i: usize) -> String {
    format!("{i:05}-{}", "n".repeat(150))
}

/// Every entry directly under `dir` (one level, names starting with `.`
/// skipped) with its contents.
pub fn snapshot_dir(fs: &Btrfs<MemoryDevice>, dir: &str) -> BTreeMap<String, Vec<u8>> {
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

// ------------------------------------------------------------- power cuts

/// An operation to run against a device that may vanish mid-way.
pub type CutOp<'a> = &'a dyn Fn(&mut Btrfs<FailAfterNWrites<MemoryDevice>>) -> luks_core::error::Result<()>;

/// Number of `write_at` calls `op` makes on a copy of `base`.
pub fn count_writes(base: &[u8], op: CutOp) -> usize {
    let dev = FailAfterNWrites::new(MemoryDevice::from_bytes(base.to_vec()), usize::MAX);
    let mut fs = Btrfs::mount(dev).expect("mount");
    op(&mut fs).expect("uncut run must succeed");
    fs.device().attempted.load(Ordering::SeqCst)
}

/// Cut points worth trying: all of them when few, otherwise an even spread
/// plus every one of the last 12 (the superblock and flush end of a commit).
pub fn cut_points(total: usize) -> Vec<usize> {
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
pub fn cut_after(base: &[u8], n: usize, op: CutOp) -> MemoryDevice {
    let mem = MemoryDevice::from_bytes(base.to_vec());
    let mut fs = Btrfs::mount(FailAfterNWrites::new(mem.clone(), n)).expect("mount");
    // The op fails wherever the device vanished, or finishes if `n` is past
    // its last write. Either is fine: only what is on the device is judged.
    let _ = op(&mut fs);
    mem
}

// -------------------------------------------------------- "untouched" judge
//
// For a subvolume safety suite: after writing somewhere, prove that other
// trees and directories are byte-for-byte what they were.

/// A tree's identity and the raw bytes of every node reachable from its root.
///
/// Raw node bytes (`Node::raw()`, the full node including header, checksum,
/// generation and every item) are public, so this is the strongest possible
/// comparison: any change to any reachable node, however small, differs. A
/// copy-on-write rewrite of even one leaf also moves its bytenr and every
/// ancestor, so it shows up as a changed root and a changed node set.
///
/// Memory: one node_size buffer per reachable node. Fine for fixtures; for a
/// very large tree prefer fixtures of modest size.
#[derive(Clone)]
pub struct TreePrint {
    pub objectid: u64,
    pub root_bytenr: u64,
    pub generation: u64,
    pub level: u8,
    /// bytenr -> raw node bytes, in address order.
    pub nodes: BTreeMap<u64, Vec<u8>>,
}

/// Capture tree `objectid` of the mounted fs (5 = top level, 256+ = subvolume).
/// Panics if the tree does not exist or is unreadable.
pub fn tree_print<D: ReadAt>(fs: &Btrfs<D>, objectid: u64) -> TreePrint {
    let root = if objectid == 5 {
        fs.fs_tree()
    } else {
        fs.find_tree_root(objectid)
            .unwrap_or_else(|e| panic!("tree {objectid}: root lookup failed: {e}"))
            .unwrap_or_else(|| panic!("tree {objectid}: no such tree"))
    };
    let mut nodes = BTreeMap::new();
    collect_nodes(fs, objectid, root.bytenr, &mut nodes);
    TreePrint {
        objectid,
        root_bytenr: root.bytenr,
        generation: root.generation,
        level: root.level,
        nodes,
    }
}

fn collect_nodes<D: ReadAt>(fs: &Btrfs<D>, objectid: u64, bytenr: u64, out: &mut BTreeMap<u64, Vec<u8>>) {
    let node = fs
        .read_node(bytenr)
        .unwrap_or_else(|e| panic!("tree {objectid}: read node @{bytenr}: {e}"));
    out.insert(bytenr, node.raw().to_vec());
    if !node.is_leaf() {
        for i in 0..node.nr_items {
            let ptr = node.key_ptr(i).unwrap_or_else(|e| panic!("tree {objectid}: key_ptr {i} @{bytenr}: {e}"));
            // A tree is a tree, not a DAG, except for shared (snapshot)
            // subtrees; dedupe so those are read once.
            if !out.contains_key(&ptr.blockptr) {
                collect_nodes(fs, objectid, ptr.blockptr, out);
            }
        }
    }
}

/// Panic, naming the first difference, unless `after` is identical to
/// `before` (root bytenr/generation/level, node set, and every node byte).
#[track_caller]
pub fn assert_tree_untouched(before: &TreePrint, after: &TreePrint, what: &str) {
    let id = before.objectid;
    assert_eq!(before.objectid, after.objectid, "[{what}] comparing different trees");
    if before.root_bytenr != after.root_bytenr {
        panic!("[{what}] tree {id} root moved: bytenr {} -> {}", before.root_bytenr, after.root_bytenr);
    }
    if before.generation != after.generation {
        panic!("[{what}] tree {id} root generation changed: {} -> {}", before.generation, after.generation);
    }
    if before.level != after.level {
        panic!("[{what}] tree {id} root level changed: {} -> {}", before.level, after.level);
    }
    // First difference in address order, across both node sets.
    let mut b = before.nodes.iter().peekable();
    let mut a = after.nodes.iter().peekable();
    loop {
        match (b.peek(), a.peek()) {
            (None, None) => return,
            (Some((bn, _)), None) => panic!("[{what}] tree {id} node @{bn} no longer reachable"),
            (None, Some((an, _))) => panic!("[{what}] tree {id} gained node @{an}"),
            (Some((bn, bd)), Some((an, ad))) => {
                if bn < an {
                    panic!("[{what}] tree {id} node @{bn} no longer reachable");
                }
                if an < bn {
                    panic!("[{what}] tree {id} gained node @{an}");
                }
                if bd != ad {
                    // (`ReadAt` is in scope and gives Vec a `len`, so go via slices.)
                    let (bs, as_): (&[u8], &[u8]) = (bd.as_slice(), ad.as_slice());
                    let off = (0..bs.len().min(as_.len())).find(|&i| bs[i] != as_[i]).unwrap_or(0);
                    panic!("[{what}] tree {id} node @{bn} bytes changed (first difference at offset {off})");
                }
                b.next();
                a.next();
            }
        }
    }
}

/// Marker value recorded for a directory in [`files_print`] (key ends in `/`).
pub const DIR_MARK: &[u8] = b"";
/// Marker value for a subvolume boundary (key ends in `/`); not descended into,
/// track that tree with [`tree_print`] instead.
pub const SUBVOL_MARK: &[u8] = b"<subvolume>";

/// Every entry under `dir`, recursively. Key = path relative to `dir`.
/// Regular files: name -> contents. Directories: `name/` -> [`DIR_MARK`].
/// Symlinks: `name@` -> target bytes. Subvolume boundaries: `name/` ->
/// [`SUBVOL_MARK`], not descended. Other special files: `name?` -> empty.
pub fn files_print<D: ReadAt>(fs: &Btrfs<D>, dir: &str) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    walk_files(fs, dir.trim_end_matches('/'), "", &mut out);
    out
}

fn walk_files<D: ReadAt>(fs: &Btrfs<D>, abs: &str, rel: &str, out: &mut BTreeMap<String, Vec<u8>>) {
    let listing_path = if abs.is_empty() { "/" } else { abs };
    let entries = fs.list_dir(listing_path).unwrap_or_else(|e| panic!("list {listing_path}: {e}"));
    for e in entries {
        if e.name == "." || e.name == ".." {
            continue;
        }
        let a = format!("{abs}/{}", e.name);
        let r = format!("{rel}{}", e.name);
        if e.is_subvolume {
            out.insert(format!("{r}/"), SUBVOL_MARK.to_vec());
        } else if e.file_type.is_dir() {
            out.insert(format!("{r}/"), DIR_MARK.to_vec());
            walk_files(fs, &a, &format!("{r}/"), out);
        } else if e.file_type.is_file() {
            let data = fs.read_file(&a).unwrap_or_else(|err| panic!("read {a}: {err}"));
            out.insert(r, data);
        } else if e.file_type == luks_core::fs::FileType::Symlink {
            let t = fs.read_link(&a).unwrap_or_else(|err| panic!("readlink {a}: {err}"));
            out.insert(format!("{r}@"), t.into_bytes());
        } else {
            out.insert(format!("{r}?"), Vec::new());
        }
    }
}

/// Panic, naming the first differing path, unless the two listings match.
#[track_caller]
pub fn assert_files_untouched(
    before: &BTreeMap<String, Vec<u8>>,
    after: &BTreeMap<String, Vec<u8>>,
    what: &str,
) {
    for (k, v) in before {
        match after.get(k) {
            None => panic!("[{what}] {k} disappeared"),
            Some(w) if w != v => {
                panic!("[{what}] {k} changed ({} -> {} bytes)", v.len(), w.len())
            }
            Some(_) => {}
        }
    }
    if let Some(k) = after.keys().find(|k| !before.contains_key(*k)) {
        panic!("[{what}] {k} appeared");
    }
}

/// Tree id of the subvolume that `path` names (e.g. `"/root"`), resolved from
/// the top level. Panics if the path does not exist or is not the root of a
/// subvolume (a plain directory would silently resolve to tree 5).
pub fn subvol_id<D: ReadAt>(fs: &Btrfs<D>, path: &str) -> u64 {
    let found = fs
        .resolve_no_follow(fs.fs_tree(), path)
        .unwrap_or_else(|e| panic!("resolve {path}: {e}"));
    assert!(
        found.tree.objectid != 5 && found.inode.objectid == found.tree.root_dirid,
        "{path} is not the root of a subvolume (tree {})",
        found.tree.objectid
    );
    found.tree.objectid
}

/// A set of trees and directories captured together, to be re-checked after
/// work that must not have touched them.
pub struct Untouched {
    pub trees: Vec<TreePrint>,
    /// `(dir, listing)` as captured by [`files_print`].
    pub files: Vec<(String, BTreeMap<String, Vec<u8>>)>,
}

impl Untouched {
    /// Capture trees by objectid (5 = top level) and directories by path.
    pub fn capture<D: ReadAt>(fs: &Btrfs<D>, tree_ids: &[u64], dirs: &[&str]) -> Self {
        Self {
            trees: tree_ids.iter().map(|&id| tree_print(fs, id)).collect(),
            files: dirs.iter().map(|d| (d.to_string(), files_print(fs, d))).collect(),
        }
    }

    /// Re-capture on `fs` (re-mount first so it shows the committed disk) and
    /// panic naming `step` and the first difference.
    #[track_caller]
    pub fn assert_same<D: ReadAt>(&self, fs: &Btrfs<D>, step: &str) {
        for before in &self.trees {
            let after = tree_print(fs, before.objectid);
            assert_tree_untouched(before, &after, step);
        }
        for (dir, before) in &self.files {
            let after = files_print(fs, dir);
            assert_files_untouched(before, &after, &format!("{step}: {dir}"));
        }
    }
}
