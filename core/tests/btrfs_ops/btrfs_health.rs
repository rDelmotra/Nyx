//! F8 btrfs health checker, graded against the kernel (`btrfs check`).
//!
//! Every verdict is compared with the oracle: kernel-made images must be
//! `Clean` and pass `btrfs check`; every injected corruption must be `Corrupt`
//! (or `Leaks` for the leak-class ones) and must also fail `btrfs check`. Each
//! injection starts from an image the checker first calls `Clean`, so a
//! checker that says `Corrupt` for everything, or a patch that does nothing,
//! cannot pass.
//!
//! Images the kernel must read live under `<repo>/target/f8-health/` (colima
//! mounts the home directory; `$TMPDIR` is invisible to it) behind `TempPath`.

#[path = "../common/temp_guard.rs"]
mod temp_guard;
use temp_guard::TempPath;

#[path = "../common/mod.rs"]
mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use luks_core::device::FileDevice;
use luks_core::fs::btrfs::superblock::{CsumType, SUPER_OFFSETS};
use luks_core::fs::btrfs::tree::{self, Key, HEADER_SIZE, ITEM_SIZE};
use luks_core::fs::health::{FindingKind, HealthReport, UnknownReason, Verdict};
use luks_core::fs::Btrfs;

const FIXTURES: &str = "fixtures/btrfs";

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn fixture(name: &str) -> PathBuf {
    repo().join(FIXTURES).join(name)
}

fn scratch(tag: &str) -> TempPath {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let dir = repo().join("target").join("f8-health");
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let n = N.fetch_add(1, Ordering::Relaxed);
    TempPath::new(dir.join(format!("{tag}-{}-{n}.img", std::process::id())))
}

fn never() -> bool {
    false
}

fn check_path(path: &Path) -> HealthReport {
    let dev = FileDevice::open(path).expect("open image read-only");
    let fs = Btrfs::mount(dev).expect("mount");
    fs.check_health(&never)
}

fn check_bytes(img: &Vec<u8>) -> HealthReport {
    let fs = Btrfs::mount(img).expect("mount");
    fs.check_health(&never)
}

/// `btrfs check --readonly` in colima. `None` only when the oracle is
/// unavailable and the operator opted out (the ledger records the skip).
fn kernel_check(path: &Path) -> Option<(bool, String)> {
    if !common::oracle::gate() {
        return None;
    }
    let out = Command::new("colima")
        .args(["ssh", "--", "sudo", "btrfs", "check", "--readonly"])
        .arg(path)
        .output()
        .expect("run btrfs check in colima");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    Some((out.status.success(), text))
}

fn assert_kernel(path: &Path, want_clean: bool, what: &str) {
    match kernel_check(path) {
        Some((ok, text)) => assert_eq!(ok, want_clean, "btrfs check disagrees for {what}:\n{text}"),
        None => eprintln!("KERNEL ORACLE SKIPPED for {what} (ALLOW_NO_ORACLE=1)"),
    }
}

fn assert_clean(r: &HealthReport, what: &str) {
    assert_eq!(r.verdict, Verdict::Clean, "{what}: {:?} {:?}", r.unknown, r.findings);
}

fn n(r: &HealthReport, k: FindingKind) -> u64 {
    r.findings.get(&k).copied().unwrap_or(0)
}

// --- kernel-made images ------------------------------------------------------

#[test]
fn kernel_made_fixtures_are_clean() {
    // plain: DUP metadata, 400-file dir (multi-leaf fs tree), FST. compress:
    // zlib/lzo/zstd, inline, hole. mixed-4k: mixed block groups, 4 KiB nodes.
    // subvol*: subvolumes and a snapshot (shared blocks). sha256-4k: non-crc
    // csum. fst-*: free-space tree with bitmaps / several leaves.
    let names = [
        "plain.img", "compress.img", "mixed-4k.img", "subvol.img", "subvol-aged.img", "subvol-shared.img",
        "nonmixed-4k.img", "sha256-4k.img", "fst-bitmap.img", "fst-multileaf.img",
    ];
    assert!(!names.is_empty());
    for name in names {
        let p = fixture(name);
        let r = check_path(&p);
        assert_clean(&r, name);
        assert_kernel(&p, true, name);
    }
}

/// mkfs + populate in the colima VM, so the kernel is the author.
const GEN: &str = r#"
set -euo pipefail
IMG="$1"; FST="$2"; MNT="$(mktemp -d)"
trap 'umount "$MNT" 2>/dev/null || true; rmdir "$MNT" 2>/dev/null || true' EXIT
truncate -s 600M "$IMG"
if [ "$FST" = fst ]; then mkfs.btrfs -q -f -R free-space-tree "$IMG"; OPTS=loop; else mkfs.btrfs -q -f -R ^free-space-tree "$IMG"; OPTS=loop,nospace_cache; fi
mount -o "$OPTS,compress-force=zstd" "$IMG" "$MNT"
python3 -c "
import sys
open(sys.argv[1]+'/zstd.txt','w').write(''.join('line %06d the quick brown fox\n'%i for i in range(20000)))
" "$MNT"
umount "$MNT"
mount -o "$OPTS" "$IMG" "$MNT"
python3 - "$MNT" <<'PY'
import os, sys
m = sys.argv[1]
open(m + '/tiny', 'w').write('inline extent\n')
for d in range(30):
    os.makedirs('%s/d%02d/sub' % (m, d))
    for i in range(100):
        open('%s/d%02d/f%03d' % (m, d, i), 'w').write('x%d\n' % i)           # inline
        open('%s/d%02d/sub/g%03d' % (m, d, i), 'wb').write(os.urandom(8192))  # out of line
# fragmented file: two files appended in lockstep with fsync, then one removed
a = open(m + '/frag-a', 'wb'); b = open(m + '/frag-b', 'wb')
for i in range(600):
    a.write(os.urandom(4096)); a.flush(); os.fsync(a.fileno())
    b.write(os.urandom(4096)); b.flush(); os.fsync(b.fileno())
a.close(); b.close(); os.remove(m + '/frag-b')
for d in range(0, 30, 2):
    for i in range(0, 100, 2):
        os.remove('%s/d%02d/sub/g%03d' % (m, d, i))
PY
btrfs subvolume create "$MNT/sv" >/dev/null
echo in-subvol > "$MNT/sv/file"
btrfs subvolume snapshot "$MNT" "$MNT/snap" >/dev/null
echo after-snap > "$MNT/sv/file2"
sync
"#;

#[test]
fn kernel_generated_images_are_clean_with_and_without_free_space_tree() {
    for mode in ["fst", "nofst"] {
        let img = scratch(mode);
        if !common::oracle::gate() {
            eprintln!("KERNEL ORACLE SKIPPED: cannot generate {mode} image");
            return;
        }
        let st = Command::new("colima")
            .args(["ssh", "--", "sudo", "bash", "-c", GEN, "gen"])
            .arg(&*img)
            .arg(mode)
            .status()
            .expect("run generator in colima");
        assert!(st.success(), "generator failed for {mode}");
        let r = check_path(&img);
        assert_clean(&r, mode);
        println!("F8 btrfs elapsed kernel-made 600 MiB {mode}: {:?}", r.elapsed);
        assert_kernel(&img, true, mode);
        // Non-vacuity: the image must have a multi-level fs tree and the right FST state.
        let fs = Btrfs::mount(FileDevice::open(&img).expect("open")).expect("mount");
        let fs_root = fs.read_node(fs.fs_tree().bytenr).expect("fs root");
        assert!(fs_root.level >= 1, "{mode}: fs tree has a single leaf; fixture too small");
        let has_fst = fs.find_tree_root(tree::FREE_SPACE_TREE_OBJECTID).expect("lookup").is_some();
        assert_eq!(has_fst, mode == "fst", "{mode}: free-space tree presence");
    }
}

// --- injected corruption -----------------------------------------------------

/// A pristine `plain.img` in memory with helpers to rewrite sealed tree blocks.
struct Lab {
    img: Vec<u8>,
    chunks: Vec<luks_core::fs::btrfs::Chunk>,
    node_size: usize,
    csum: CsumType,
}

impl Lab {
    fn new() -> Lab {
        let path = fixture("plain.img");
        let img = std::fs::read(&path).expect("read plain.img");
        let l = Lab::from(img);
        assert_clean(&check_bytes(&l.img), "pristine plain.img");
        l
    }

    fn from(img: Vec<u8>) -> Lab {
        let (chunks, node_size, csum) = {
            let fs = Btrfs::mount(&img).expect("mount");
            (fs.chunk_map().chunks().to_vec(), fs.node_size() as usize, fs.superblock().csum_type)
        };
        Lab { img, chunks, node_size, csum }
    }

    fn fs(&self) -> Btrfs<&Vec<u8>> {
        Btrfs::mount(&self.img).expect("mount")
    }

    fn physical(&self, logical: u64) -> Vec<usize> {
        let c = self
            .chunks
            .iter()
            .find(|c| logical >= c.logical && logical < c.logical + c.length)
            .expect("logical address in a chunk");
        c.stripes.iter().map(|s| (s.offset + logical - c.logical) as usize).collect()
    }

    /// Rewrite one tree block on every stripe and re-seal its checksum.
    fn patch_block(&mut self, logical: u64, f: impl FnOnce(&mut [u8])) {
        let at = self.physical(logical);
        let mut raw = self.img[at[0]..at[0] + self.node_size].to_vec();
        f(&mut raw);
        let c = self.csum.calculate(&raw[32..]);
        raw[..32].copy_from_slice(&c);
        for p in at {
            self.img[p..p + self.node_size].copy_from_slice(&raw);
        }
    }

    /// Leaf holding exactly `key`, and the item index.
    fn locate(&self, tree_id: u64, key: &Key) -> (u64, usize) {
        let fs = self.fs();
        let mut at = fs.tree_root(tree_id).expect("tree root").bytenr;
        loop {
            let node = fs.read_node(at).expect("read node");
            if node.is_leaf() {
                let i = node.lower_bound(key).expect("search");
                assert!(i < node.nr_items && node.key(i).expect("key") == *key, "key not found");
                return (at, i);
            }
            at = node.key_ptr(node.child_for(key).expect("child")).expect("ptr").blockptr;
        }
    }

    fn items(&self, tree_id: u64, ty: u8) -> Vec<(Key, Vec<u8>)> {
        let fs = self.fs();
        let root = fs.tree_root(tree_id).expect("tree root").bytenr;
        let mut v = Vec::new();
        fs.walk_tree(root, &mut |k, d| {
            if k.item_type == ty {
                v.push((k, d.to_vec()));
            }
            Ok(())
        })
        .expect("walk");
        v
    }

    fn write_image(&self, tag: &str) -> TempPath {
        let p = scratch(tag);
        std::fs::write(&*p, &self.img).expect("write image");
        p
    }
}

fn item_data_at(raw: &[u8], i: usize) -> usize {
    let d = HEADER_SIZE + i * ITEM_SIZE;
    HEADER_SIZE + u32::from_le_bytes(raw[d + 17..d + 21].try_into().expect("4 bytes")) as usize
}

fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}
fn get64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// Run the injected image through both judges.
fn grade(lab: &Lab, tag: &str, verdict: Verdict, kind: FindingKind) -> HealthReport {
    let r = check_bytes(&lab.img);
    assert_eq!(r.verdict, verdict, "{tag}: {:?} {:?}", r.unknown, r.findings);
    assert!(n(&r, kind) > 0, "{tag}: expected {kind:?}, got {:?}", r.findings);
    let p = lab.write_image(tag);
    assert_kernel(&p, false, tag);
    r
}

#[test]
fn empty_non_root_leaf_is_corrupt() {
    let mut lab = Lab::new();
    let fs = lab.fs();
    let root = fs.read_node(fs.fs_tree().bytenr).expect("fs root");
    assert!(root.level >= 1, "plain.img fs tree must have an interior root");
    let leaf = root.key_ptr(1).expect("second child").blockptr;
    drop(fs);
    // The v0.2.0 delete bug's shape: nr_items = 0 under a correct checksum.
    lab.patch_block(leaf, |raw| raw[96..100].fill(0));
    grade(&lab, "empty-leaf", Verdict::Corrupt, FindingKind::EmptyLeaf);
}

fn data_bg(lab: &Lab) -> (Key, u64) {
    let (k, d) = lab
        .items(tree::EXTENT_TREE_OBJECTID, tree::BLOCK_GROUP_ITEM_KEY)
        .into_iter()
        .find(|(_, d)| get64(d, 16) & 1 != 0 && get64(d, 0) >= 1 << 20)
        .expect("a used DATA block group");
    (k, get64(&d, 0))
}

fn patch_bg_used(lab: &mut Lab, new_used: impl FnOnce(u64) -> u64) {
    let (key, used) = data_bg(lab);
    let (leaf, i) = lab.locate(tree::EXTENT_TREE_OBJECTID, &key);
    lab.patch_block(leaf, |raw| {
        let at = item_data_at(raw, i);
        put64(raw, at, new_used(used));
    });
}

#[test]
fn block_group_used_below_extents_is_corrupt() {
    let mut lab = Lab::new();
    patch_bg_used(&mut lab, |u| u - 16384);
    grade(&lab, "bg-used-low", Verdict::Corrupt, FindingKind::BlockGroupUsedMismatch);
}

#[test]
fn block_group_used_above_extents_is_a_leak_not_corruption() {
    let mut lab = Lab::new();
    patch_bg_used(&mut lab, |u| u + 16384);
    // btrfs check rejects it, but nothing can be lost: the space just stays
    // accounted as used. Amber, and the kernel still calls it an error.
    grade(&lab, "bg-used-high", Verdict::Leaks, FindingKind::LeakedBlocks);
}

fn extent_starts(lab: &Lab) -> Vec<(u64, u64)> {
    let mut v: Vec<(u64, u64)> = lab
        .items(tree::EXTENT_TREE_OBJECTID, tree::EXTENT_ITEM_KEY)
        .into_iter()
        .map(|(k, _)| (k.objectid, k.offset))
        .collect();
    v.extend(
        lab.items(tree::EXTENT_TREE_OBJECTID, tree::METADATA_ITEM_KEY)
            .into_iter()
            .map(|(k, _)| (k.objectid, lab.node_size as u64)),
    );
    v.sort();
    v
}

#[test]
fn free_space_range_overlapping_a_live_extent_is_corrupt() {
    let mut lab = Lab::new();
    let starts: std::collections::HashSet<u64> = extent_starts(&lab).into_iter().map(|(s, _)| s).collect();
    let free = lab.items(tree::FREE_SPACE_TREE_OBJECTID, tree::FREE_SPACE_EXTENT_KEY);
    // The extent must sit inside the same block group, not at the next one's start.
    let bg_starts: std::collections::HashSet<u64> = lab.chunks.iter().map(|c| c.logical).collect();
    let (key, _) = free
        .iter()
        .find(|(k, _)| {
            let end = k.objectid + k.offset;
            starts.contains(&end) && !bg_starts.contains(&end)
        })
        .expect("a free range that ends where a live extent starts");
    let (leaf, i) = lab.locate(tree::FREE_SPACE_TREE_OBJECTID, key);
    let grown = key.offset + 4096;
    // Key offset is bytes 9..17 of the item descriptor: the free range now
    // covers the first sector of the live extent after it.
    lab.patch_block(leaf, |raw| put64(raw, HEADER_SIZE + i * ITEM_SIZE + 9, grown));
    grade(&lab, "fst-overlap", Verdict::Corrupt, FindingKind::FreeSpaceOverlapsExtent);
}

#[test]
fn overlapping_extent_items_are_corrupt() {
    let mut lab = Lab::new();
    let data: Vec<(Key, Vec<u8>)> = lab.items(tree::EXTENT_TREE_OBJECTID, tree::EXTENT_ITEM_KEY);
    let key = data
        .windows(2)
        .find(|w| w[0].0.objectid + w[0].0.offset == w[1].0.objectid)
        .map(|w| w[0].0)
        .expect("two adjacent data extents");
    let (leaf, i) = lab.locate(tree::EXTENT_TREE_OBJECTID, &key);
    lab.patch_block(leaf, |raw| put64(raw, HEADER_SIZE + i * ITEM_SIZE + 9, key.offset + 4096));
    grade(&lab, "extent-overlap", Verdict::Corrupt, FindingKind::OverlappingExtents);
}

#[test]
fn overlapping_dev_extents_are_corrupt() {
    let mut lab = Lab::new();
    let devs = lab.items(tree::DEV_TREE_OBJECTID, tree::DEV_EXTENT_KEY);
    assert!(devs.len() >= 2);
    let (key, _) = &devs[0];
    let reach = devs[1].0.offset - key.offset + 4096;
    let (leaf, i) = lab.locate(tree::DEV_TREE_OBJECTID, key);
    lab.patch_block(leaf, |raw| put64(raw, item_data_at(raw, i) + 24, reach));
    grade(&lab, "dev-overlap", Verdict::Corrupt, FindingKind::OverlappingDevExtents);
}

#[test]
fn superblock_mirror_newer_than_primary_is_corrupt() {
    let mut lab = Lab::new();
    let at = SUPER_OFFSETS[1] as usize;
    let gen = get64(&lab.img, at + 0x48);
    put64(&mut lab.img, at + 0x48, gen + 1);
    let c = lab.csum.calculate(&lab.img[at + 32..at + 4096]);
    lab.img[at..at + 32].copy_from_slice(&c);
    let r = check_bytes(&lab.img);
    assert_eq!((r.verdict, n(&r, FindingKind::MirrorMismatch)), (Verdict::Corrupt, 1), "{:?}", r.findings);
    // Full kernel grade (check, mount, scrub): scrub is what inspects mirrors.
    let p = lab.write_image("mirror-newer");
    let (ok, out, err) = common::oracle::verify_btrfs(&p);
    assert!(!ok, "kernel accepted a mirror newer than the primary:\n{out}\n{err}");
}

#[test]
fn torn_superblock_mirror_is_a_leak_and_an_older_valid_one_is_clean() {
    let mut lab = Lab::new();
    let at = SUPER_OFFSETS[1] as usize;
    // Older and valid (what a commit torn after the primary write leaves).
    let gen = get64(&lab.img, at + 0x48);
    put64(&mut lab.img, at + 0x48, gen - 1);
    let c = lab.csum.calculate(&lab.img[at + 32..at + 4096]);
    lab.img[at..at + 32].copy_from_slice(&c);
    assert_clean(&check_bytes(&lab.img), "older valid mirror");
    // Torn: checksum no longer matches.
    lab.img[at + 100] ^= 0xff;
    let r = check_bytes(&lab.img);
    assert_eq!((r.verdict, n(&r, FindingKind::MirrorInvalid)), (Verdict::Leaks, 1), "{:?}", r.findings);
    let p = lab.write_image("mirror-torn");
    let (ok, out, err) = common::oracle::verify_btrfs(&p);
    assert!(!ok, "kernel scrub accepted a torn mirror:\n{out}\n{err}");
}

#[test]
fn cancel_and_no_panic_on_bit_flips() {
    let lab = Lab::new();
    let fs = lab.fs();
    let r = fs.check_health(&|| true);
    assert_eq!((r.verdict, r.unknown), (Verdict::Unknown, Some(UnknownReason::Cancelled)));
    let polls = std::cell::Cell::new(0u32);
    let r = fs.check_health(&|| {
        polls.set(polls.get() + 1);
        polls.get() > 5
    });
    assert_eq!(r.unknown, Some(UnknownReason::Cancelled), "cancel must land mid-walk");
    drop(fs);

    // Hostile bytes: flip single bits across the metadata chunk. No panic, and
    // a flip in a used block must never leave the verdict Clean.
    let meta = lab
        .chunks
        .iter()
        .find(|c| c.chunk_type & luks_core::fs::btrfs::chunk::BLOCK_GROUP_METADATA != 0)
        .expect("metadata chunk");
    let base = meta.stripes[0].offset as usize;
    let mut img = lab.img.clone();
    let (mut mounted, mut dirty) = (0, 0);
    let mut x = 0x9e3779b97f4a7c15u64;
    for _ in 0..300 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let at = base + (x as usize % (2 << 20));
        let bit = 1u8 << (x >> 40 & 7);
        img[at] ^= bit;
        if let Ok(fs) = Btrfs::mount(&img) {
            mounted += 1;
            if fs.check_health(&never).verdict != Verdict::Clean {
                dirty += 1;
            }
        }
        img[at] ^= bit;
    }
    assert!(mounted > 100 && dirty > 20, "vacuous: mounted {mounted}, flagged {dirty}");
}

#[test]
fn elapsed_on_a_four_gib_image() {
    let p = fixture("fst-aged.img");
    assert!(std::fs::metadata(&p).expect("stat").len() >= 4 << 30);
    let r = check_path(&p);
    assert_clean(&r, "fst-aged.img");
    println!("F8 btrfs elapsed fst-aged.img (4 GiB): {:?}", r.elapsed);
}

// --- our own writes ----------------------------------------------------------

#[cfg(feature = "dangerous-write-support")]
mod ours {
    use super::*;

    fn open_rw(p: &Path) -> Btrfs<FileDevice> {
        let len = std::fs::metadata(p).expect("stat").len();
        Btrfs::mount(FileDevice::open_writable(p, len).expect("open rw")).expect("mount rw")
    }

    #[test]
    fn images_after_writes_deletes_renames_and_commits_are_clean() {
        let img = scratch("ours");
        std::fs::copy(fixture("plain.img"), &*img).expect("copy fixture");
        let mut fs = open_rw(&img);
        fs.create_directory("/", "newdir").expect("mkdir");
        for i in 0..40 {
            let data = vec![i as u8; 4096 * (1 + i % 7)];
            fs.create_file_with_data("/newdir", &format!("f{i:02}"), &data).expect("create");
        }
        for i in (0..40).step_by(3) {
            fs.delete_file(&format!("/newdir/f{i:02}")).expect("delete");
        }
        fs.rename("/newdir", "f01", "/", "moved-f01").expect("rename");
        fs.delete_file("/hello.txt").expect("delete original");
        fs.commit_active_batch().expect("commit");
        let r = fs.check_health(&never);
        assert_clean(&r, "after our writes (live handle)");
        drop(fs);
        assert_clean(&check_path(&img), "after our writes (remount)");
        let (ok, out, err) = common::oracle::verify_btrfs(&img);
        assert!(ok, "kernel rejects the image:\n{out}\n{err}");
    }

    #[test]
    fn open_batch_is_busy_then_clean_after_commit() {
        let img = scratch("busy");
        std::fs::copy(fixture("plain.img"), &*img).expect("copy fixture");
        let mut fs = open_rw(&img);
        let mut w = fs.begin_file(5000).expect("begin");
        fs.write_chunk(&mut w, &[7u8; 5000]).expect("write");
        fs.finish_file(w, "/", "pending.bin").expect("finish");
        assert!(fs.active_batch().is_some_and(|b| b.has_uncommitted_files()), "batch must be open");
        let r = fs.check_health(&never);
        assert_eq!((r.verdict, r.unknown), (Verdict::Unknown, Some(UnknownReason::Busy)));
        fs.commit_active_batch().expect("commit");
        assert_clean(&fs.check_health(&never), "after commit");
    }
}
