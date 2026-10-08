//! F3.5 read-path integrity (INV-4): a read returns the file's bytes or an
//! error, never plausible wrong bytes. Register IDs: R1r, R2r, R6r.
//!
//! Every image here is `fixtures/btrfs/plain.img` patched in memory (no disk
//! write, no feature gate) so these run in both feature sets. The patches are
//! small and each test says what shape it builds:
//!
//! * a zero-item leaf in the real fs tree (what the 2026-08-23 delete bug left
//!   on the stick),
//! * a synthetic three-leaf tree `[Lk, EMPTY, Lm]` in unused metadata space,
//!   holding the two extents of `/big.bin`, so `read_inode_data` has to cross
//!   the empty leaf,
//! * a data extent with no checksum, a flipped data byte, a csum root item
//!   that is truncated / absent, and a superblock mirror that errors.
//!
//! `tools/verify-btrfs.sh` / `btrfs inspect-internal dump-tree` confirm the
//! zero-item-leaf shape; see the run notes in the F3.5 report.

use luks_core::error::{LuksError, Result};
use luks_core::fs::btrfs::chunk::{BLOCK_GROUP_DATA, BLOCK_GROUP_METADATA};
use luks_core::fs::btrfs::crc32c::crc32c;
use luks_core::fs::btrfs::superblock::{Superblock, SUPER_OFFSETS};
use luks_core::fs::btrfs::tree::{
    Key, Node, CSUM_TREE_OBJECTID, EXTENT_DATA_KEY, HEADER_SIZE, ITEM_SIZE, KEY_PTR_SIZE,
    ROOT_ITEM_KEY,
};
use luks_core::fs::btrfs::Inode;
use luks_core::device::ReadAt;
use luks_core::fs::Btrfs;

#[cfg(feature = "dangerous-write-support")]
#[path = "../common/mod.rs"]
mod common;

const NODATASUM: u64 = 1;
const MIB: u64 = 1024 * 1024;

fn fixture() -> Vec<u8> {
    let path = format!("{}/../fixtures/btrfs/plain.img", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("missing fixture {path}: {e}"))
}

fn mount(img: &[u8]) -> Result<Btrfs<Vec<u8>>> {
    Btrfs::mount(img.to_vec())
}

fn put_u64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

/// Re-seal a tree block: csum covers bytes 32.. and sits in the first 4.
fn seal(raw: &mut [u8]) {
    let c = crc32c(&raw[32..]);
    raw[..32].fill(0);
    raw[..4].copy_from_slice(&c.to_le_bytes());
}

struct Lab {
    img: Vec<u8>,
    node: usize,
    /// (logical, physical stripes) lookups, taken from a pristine mount.
    chunks: Vec<luks_core::fs::btrfs::Chunk>,
    sb: Superblock,
    fs_root: u64,
    root_tree_root: u64,
}

impl Lab {
    fn new() -> Lab {
        let img = fixture();
        let fs = mount(&img).unwrap();
        Lab {
            node: fs.node_size() as usize,
            chunks: fs.chunk_map().chunks().to_vec(),
            sb: fs.superblock().clone(),
            fs_root: fs.fs_tree().bytenr,
            root_tree_root: fs.superblock().root,
            img,
        }
    }

    fn stripes(&self, logical: u64) -> Vec<u64> {
        let c = self
            .chunks
            .iter()
            .find(|c| logical >= c.logical && logical < c.logical + c.length)
            .expect("logical address not in a chunk");
        c.stripes.iter().map(|s| s.offset + (logical - c.logical)).collect()
    }

    fn read_raw(&self, logical: u64, len: usize) -> Vec<u8> {
        let p = self.stripes(logical)[0] as usize;
        self.img[p..p + len].to_vec()
    }

    fn write_raw(&mut self, logical: u64, bytes: &[u8]) {
        for p in self.stripes(logical) {
            self.img[p as usize..p as usize + bytes.len()].copy_from_slice(bytes);
        }
    }

    /// Patch a tree block in every stripe and re-seal its checksum.
    fn patch_node(&mut self, logical: u64, f: impl FnOnce(&mut [u8])) {
        let mut raw = self.read_raw(logical, self.node);
        f(&mut raw);
        seal(&mut raw);
        self.write_raw(logical, &raw);
    }

    fn set_nr_items(&mut self, logical: u64, n: u32) {
        self.patch_node(logical, |r| r[96..100].copy_from_slice(&n.to_le_bytes()));
    }

    /// `n` consecutive unused metadata nodes at the tail of the metadata chunk.
    fn free_nodes(&self, n: usize) -> Vec<u64> {
        self.free_nodes_skip(0, n)
    }

    /// As `free_nodes`, leaving the last `skip` nodes to an earlier caller.
    fn free_nodes_skip(&self, skip: usize, n: usize) -> Vec<u64> {
        let c = self
            .chunks
            .iter()
            .find(|c| c.chunk_type & BLOCK_GROUP_METADATA != 0)
            .expect("no metadata chunk");
        let first = c.logical + c.length - ((skip + n) * self.node) as u64;
        let out: Vec<u64> = (0..n).map(|i| first + (i * self.node) as u64).collect();
        for &l in &out {
            assert!(
                self.read_raw(l, self.node).iter().all(|b| *b == 0),
                "block {l} is not free, the control for the synthetic tree is void"
            );
        }
        out
    }

    /// Build and place a node. Header fields are copied from the fs root.
    fn place(&mut self, at: u64, level: u8, leaf_items: &[(Key, &[u8])], ptrs: &[(Key, u64)]) {
        let mut raw = self.read_raw(self.fs_root, self.node);
        raw[101..].fill(0);
        put_u64(&mut raw, 48, at);
        raw[100] = level;
        if level == 0 {
            raw[96..100].copy_from_slice(&(leaf_items.len() as u32).to_le_bytes());
            let mut end = self.node - HEADER_SIZE;
            for (i, (k, d)) in leaf_items.iter().enumerate() {
                let d: &[u8] = d;
                end -= d.len();
                let at = HEADER_SIZE + i * ITEM_SIZE;
                put_u64(&mut raw, at, k.objectid);
                raw[at + 8] = k.item_type;
                put_u64(&mut raw, at + 9, k.offset);
                raw[at + 17..at + 21].copy_from_slice(&(end as u32).to_le_bytes());
                raw[at + 21..at + 25].copy_from_slice(&(d.len() as u32).to_le_bytes());
                raw[HEADER_SIZE + end..HEADER_SIZE + end + d.len()].copy_from_slice(d);
            }
        } else {
            raw[96..100].copy_from_slice(&(ptrs.len() as u32).to_le_bytes());
            for (i, (k, p)) in ptrs.iter().enumerate() {
                let at = HEADER_SIZE + i * KEY_PTR_SIZE;
                put_u64(&mut raw, at, k.objectid);
                raw[at + 8] = k.item_type;
                put_u64(&mut raw, at + 9, k.offset);
                put_u64(&mut raw, at + 17, *p);
                put_u64(&mut raw, at + 25, self.sb.generation);
            }
        }
        seal(&mut raw);
        self.write_raw(at, &raw);
    }
}

/// With `F35_DUMP_DIR` set, write the crafted image out so the kernel's
/// `btrfs check` / `dump-tree` can confirm its shape.
fn dump(name: &str, img: &[u8]) {
    if let Some(dir) = std::env::var_os("F35_DUMP_DIR") {
        let path = std::path::Path::new(&dir).join(name);
        std::fs::write(&path, img).unwrap_or_else(|e| panic!("dump {path:?}: {e}"));
    }
}

fn leaves_of(fs: &Btrfs<Vec<u8>>, root: u64) -> Vec<(u64, Node)> {
    let node = fs.read_node(root).unwrap();
    if node.is_leaf() {
        return vec![(root, node)];
    }
    let mut out = Vec::new();
    for i in 0..node.nr_items {
        out.extend(leaves_of(fs, node.key_ptr(i).unwrap().blockptr));
    }
    out
}

fn scan_all(fs: &Btrfs<Vec<u8>>, root: u64) -> (usize, Option<Key>) {
    let mut c = fs.search(root, &Key::new(0, 0, 0)).unwrap();
    let (mut n, mut last) = (0, None);
    while c.valid() {
        last = Some(c.key().unwrap());
        n += 1;
        c.advance().unwrap();
    }
    (n, last)
}

// --- R1r: cursor over a zero-item leaf --------------------------------------

/// Picks the middle leaf of the plain.img fs tree and empties it.
fn lab_with_empty_middle_leaf() -> (Lab, usize, usize, Vec<(u64, Node)>) {
    let mut lab = Lab::new();
    let fs = mount(&lab.img).unwrap();
    let leaves = leaves_of(&fs, lab.fs_root);
    assert!(leaves.len() >= 3, "vacuity: the fs tree needs >= 3 leaves, has {}", leaves.len());
    let (before, _) = scan_all(&fs, lab.fs_root);
    let mid = leaves.len() / 2;
    lab.set_nr_items(leaves[mid].0, 0);
    dump("empty-middle-leaf.img", &lab.img);
    (lab, mid, before, leaves)
}

#[test]
fn a_forward_scan_continues_past_a_zero_item_leaf() {
    let (lab, mid, before, leaves) = lab_with_empty_middle_leaf();
    let fs = mount(&lab.img).unwrap();
    let (n, last) = scan_all(&fs, lab.fs_root);
    let lost = leaves[mid].1.nr_items;
    assert!(lost > 0);
    assert_eq!(n, before - lost, "scan must visit every item outside the empty leaf");
    let want_last = leaves.last().unwrap().1.key(leaves.last().unwrap().1.nr_items - 1).unwrap();
    assert_eq!(last, Some(want_last), "the scan stopped before the last leaf");
}

#[test]
fn a_backward_step_skips_a_zero_item_leaf_instead_of_erroring() {
    // The WEIRD_INCIDENT shape: search_le lands after an empty leaf and
    // retreat must cross it.
    let (lab, mid, _, leaves) = lab_with_empty_middle_leaf();
    let fs = mount(&lab.img).unwrap();
    let prev = &leaves[mid - 1].1;
    let want = prev.key(prev.nr_items - 1).unwrap();
    // A key inside the emptied leaf's range, below the next leaf's first key.
    let mid_first = leaves[mid].1.key(0).unwrap();
    let next_first = leaves[mid + 1].1.key(0).unwrap();
    assert!(mid_first < next_first);
    let c = fs.search_le(lab.fs_root, &mid_first).expect("search_le across an empty leaf");
    assert!(c.valid());
    assert_eq!(c.key().unwrap(), want);
}

#[test]
fn retreating_past_the_first_item_stays_invalid() {
    // L3c-O2: the second retreat used to land on an interior key.
    let lab = Lab::new();
    let fs = mount(&lab.img).unwrap();
    assert!(leaves_of(&fs, lab.fs_root).len() >= 2, "vacuity: need a multi-leaf tree");
    let mut c = fs.search_le(lab.fs_root, &Key::new(0, 0, 0)).unwrap();
    assert!(!c.valid());
    for _ in 0..3 {
        c.retreat().unwrap();
        assert!(!c.valid(), "a cursor parked before the start must stay invalid");
        assert!(c.key().is_err());
    }
    // And a forward step from there reaches the first item.
    c.advance().unwrap();
    assert!(c.valid());
    assert_eq!(c.key().unwrap(), fs.search(lab.fs_root, &Key::new(0, 0, 0)).unwrap().key().unwrap());
}

#[test]
fn a_search_through_an_empty_interior_node_is_corruption_not_an_empty_tree() {
    let mut lab = Lab::new();
    let fs = mount(&lab.img).unwrap();
    assert!(!fs.read_node(lab.fs_root).unwrap().is_leaf(), "vacuity: fs root must be interior");
    drop(fs);
    lab.set_nr_items(lab.fs_root, 0);
    let fs = mount(&lab.img).unwrap();
    match fs.search(lab.fs_root, &Key::new(256, 1, 0)) {
        Err(LuksError::CorruptFs(_)) => {}
        Err(e) => panic!("wrong error: {e}"),
        Ok(c) => panic!("silent cursor, valid={}", c.valid()),
    }
}

#[test]
fn a_genuinely_empty_leaf_root_is_still_an_empty_tree() {
    // Control: the CorruptFs above is for interior nodes only.
    let mut lab = Lab::new();
    let at = lab.free_nodes(1)[0];
    lab.place(at, 0, &[], &[]);
    let fs = mount(&lab.img).unwrap();
    let c = fs.search(at, &Key::new(0, 0, 0)).unwrap();
    assert!(!c.valid());
}

// --- synthetic tree for read_inode_data -------------------------------------

struct Big {
    inode: Inode,
    ext0: Vec<u8>,
    ext1: Vec<u8>,
    bytes: Vec<u8>,
}

fn big(lab: &Lab) -> Big {
    let fs = mount(&lab.img).unwrap();
    let found = fs.resolve_no_follow(fs.fs_tree(), "/big.bin").unwrap();
    let id = found.inode.objectid;
    let item = |off| fs.find_item(found.tree.bytenr, &Key::new(id, EXTENT_DATA_KEY, off)).unwrap().expect("extent item");
    Big {
        ext0: item(0),
        ext1: item(MIB),
        bytes: fs.read_file("/big.bin").unwrap(),
        inode: found.inode,
    }
}

/// Root of `[Lk, E, Lm]` (or `[Lk, Lm]` without E) holding big.bin's extents.
fn synth_tree(lab: &mut Lab, b: &Big, with_empty: bool) -> u64 {
    let id = b.inode.objectid;
    let n = lab.free_nodes(4);
    let (root, lk, e, lm) = (n[0], n[1], n[2], n[3]);
    lab.place(lk, 0, &[(Key::new(id, EXTENT_DATA_KEY, 0), &b.ext0)], &[]);
    lab.place(e, 0, &[], &[]);
    lab.place(lm, 0, &[(Key::new(id, EXTENT_DATA_KEY, MIB), &b.ext1)], &[]);
    let mut ptrs = vec![(Key::new(id, EXTENT_DATA_KEY, 0), lk)];
    if with_empty {
        ptrs.push((Key::new(id, EXTENT_DATA_KEY, 1), e));
    }
    ptrs.push((Key::new(id, EXTENT_DATA_KEY, MIB), lm));
    lab.place(root, 1, &[], &ptrs);
    root
}

#[test]
fn read_inode_data_crosses_a_zero_item_leaf() {
    let mut lab = Lab::new();
    let b = big(&lab);
    let ctl = synth_tree(&mut lab, &b, false);
    let root = synth_tree_again(&mut lab, &b);
    let fs = mount(&lab.img).unwrap();
    let mut out = vec![0u8; b.bytes.len()];
    // Control: the same two leaves with no empty leaf between them.
    let n = fs.read_inode_data(ctl, &b.inode, 0, &mut out).unwrap();
    assert_eq!((n, &out[..]), (b.bytes.len(), &b.bytes[..]), "control read");
    // Under test: Lk, EMPTY, Lm.
    let mut out = vec![0u8; b.bytes.len()];
    let n = fs.read_inode_data(root, &b.inode, 0, &mut out).expect("read across empty leaf");
    assert_eq!(n, b.bytes.len());
    assert!(out == b.bytes, "bytes after the empty leaf were not the file's (zero-filled?)");
}

/// Second synthetic tree, with the empty leaf, in its own free blocks.
fn synth_tree_again(lab: &mut Lab, b: &Big) -> u64 {
    // free_nodes() hands out the tail of the chunk; shift the second tree down.
    let id = b.inode.objectid;
    let all = lab.free_nodes_skip(4, 4);
    let (root, lk, e, lm) = (all[0], all[1], all[2], all[3]);
    lab.place(lk, 0, &[(Key::new(id, EXTENT_DATA_KEY, 0), &b.ext0)], &[]);
    lab.place(e, 0, &[], &[]);
    lab.place(lm, 0, &[(Key::new(id, EXTENT_DATA_KEY, MIB), &b.ext1)], &[]);
    lab.place(
        root,
        1,
        &[],
        &[
            (Key::new(id, EXTENT_DATA_KEY, 0), lk),
            (Key::new(id, EXTENT_DATA_KEY, 1), e),
            (Key::new(id, EXTENT_DATA_KEY, MIB), lm),
        ],
    );
    root
}

// --- O12: no zero-fill for bytes that were not read -------------------------

fn clear_no_holes(lab: &mut Lab) {
    for off in SUPER_OFFSETS.iter().take(2) {
        let o = *off as usize;
        let mut raw = lab.img[o..o + 4096].to_vec();
        let f = u64::from_le_bytes(raw[0xbc..0xc4].try_into().unwrap()) & !0x200;
        put_u64(&mut raw, 0xbc, f);
        let c = crc32c(&raw[32..]);
        raw[..32].fill(0);
        raw[..4].copy_from_slice(&c.to_le_bytes());
        lab.img[o..o + 4096].copy_from_slice(&raw);
    }
}

fn single_extent_tree(lab: &mut Lab, b: &Big) -> u64 {
    let id = b.inode.objectid;
    let at = lab.free_nodes(1)[0];
    lab.place(at, 0, &[(Key::new(id, EXTENT_DATA_KEY, 0), &b.ext0)], &[]);
    at
}

#[test]
fn a_tail_gap_is_a_hole_only_where_the_layout_allows_one() {
    let mut lab = Lab::new();
    let b = big(&lab);
    let root = single_extent_tree(&mut lab, &b);
    let mut out = vec![0xAAu8; 2 * MIB as usize];

    // NO_HOLES (set on plain.img): the second MiB has no item, a legal hole.
    let fs = mount(&lab.img).unwrap();
    assert!(fs.superblock().has_no_holes(), "vacuity");
    let n = fs.read_inode_data(root, &b.inode, 0, &mut out).unwrap();
    assert_eq!(n, 2 * MIB as usize);
    assert_eq!(&out[..MIB as usize], &b.bytes[..MIB as usize]);
    assert!(out[MIB as usize..].iter().all(|x| *x == 0), "a proven hole reads as zeros");

    // Without NO_HOLES every hole needs an item, so the same layout is damage.
    clear_no_holes(&mut lab);
    dump("no-nohole-flag.img", &lab.img);
    let fs = mount(&lab.img).unwrap();
    assert!(!fs.superblock().has_no_holes(), "vacuity: flag cleared");
    let err = fs.read_inode_data(root, &b.inode, 0, &mut out);
    assert!(matches!(err, Err(LuksError::CorruptFs(_))), "got {err:?}");
}

#[test]
fn a_mid_file_gap_without_no_holes_is_corruption() {
    let mut lab = Lab::new();
    let b = big(&lab);
    let id = b.inode.objectid;
    let at = lab.free_nodes(1)[0];
    // extent 0 at 0 and extent 1 moved to 3 MiB: a 2 MiB gap in the middle.
    lab.place(
        at,
        0,
        &[
            (Key::new(id, EXTENT_DATA_KEY, 0), &b.ext0),
            (Key::new(id, EXTENT_DATA_KEY, 3 * MIB), &b.ext1),
        ],
        &[],
    );
    let mut inode = b.inode.clone();
    inode.size = 4 * MIB;
    let mut out = vec![0u8; 4 * MIB as usize];
    let fs = mount(&lab.img).unwrap();
    // Control: NO_HOLES, the gap is a hole and the read succeeds.
    assert_eq!(fs.read_inode_data(at, &inode, 0, &mut out).unwrap(), 4 * MIB as usize);
    clear_no_holes(&mut lab);
    let fs = mount(&lab.img).unwrap();
    let err = fs.read_inode_data(at, &inode, 0, &mut out);
    assert!(matches!(err, Err(LuksError::CorruptFs(_))), "got {err:?}");
}

// --- R2r / O5 / O6: data checksums -------------------------------------------

/// A one-sector regular extent at the tail of the data chunk, which no
/// checksum item covers, plus an inode over it.
fn uncovered_extent(lab: &mut Lab, b: &Big, flags: u64) -> (u64, Inode, u64) {
    let c = lab.chunks.iter().find(|c| c.chunk_type & BLOCK_GROUP_DATA != 0).unwrap();
    let addr = c.logical + c.length - 4096;
    let mut item = b.ext0.clone();
    put_u64(&mut item, 8, 4096); // ram_bytes
    put_u64(&mut item, 21, addr); // disk_bytenr
    put_u64(&mut item, 29, 4096); // disk_num_bytes
    put_u64(&mut item, 37, 0); // offset
    put_u64(&mut item, 45, 4096); // num_bytes
    let id = b.inode.objectid;
    let at = lab.free_nodes(1)[0];
    lab.place(at, 0, &[(Key::new(id, EXTENT_DATA_KEY, 0), &item)], &[]);
    let mut inode = b.inode.clone();
    inode.size = 4096;
    inode.flags = flags;
    (at, inode, addr)
}

#[test]
fn a_regular_extent_with_no_checksum_is_an_error_unless_the_inode_is_nodatasum() {
    let mut lab = Lab::new();
    let b = big(&lab);
    let (root, inode, addr) = uncovered_extent(&mut lab, &b, 0);
    let fs = mount(&lab.img).unwrap();

    // Vacuity: the csum tree really has nothing for this sector.
    let mut sector = vec![0u8; 4096];
    fs.read_logical(addr, &mut sector).unwrap();
    assert_eq!(
        fs.verify_data(addr, &sector).unwrap(),
        luks_core::fs::btrfs::Verified::Partial { unchecked_sectors: 1 }
    );

    let mut out = vec![0u8; 4096];
    let err = fs.read_inode_data(root, &inode, 0, &mut out);
    assert!(err.is_err(), "unchecked data was returned as if it were verified: {err:?}");

    // NODATASUM is the one case where "no checksum" is the file's own contract.
    let mut nodatasum = inode.clone();
    nodatasum.flags = NODATASUM;
    assert_eq!(fs.read_inode_data(root, &nodatasum, 0, &mut out).unwrap(), 4096);
}

#[test]
fn a_flipped_data_byte_is_a_checksum_error() {
    // Control for the hard-error half of R2r (already true; keep it true).
    let mut lab = Lab::new();
    let b = big(&lab);
    let fs = mount(&lab.img).unwrap();
    let found = fs.resolve_no_follow(fs.fs_tree(), "/big.bin").unwrap();
    let e = fs.file_extents(found.tree.bytenr, found.inode.objectid).unwrap();
    let phys = lab.stripes(e[1].disk_bytenr)[0] as usize + 100;
    drop(fs);
    lab.img[phys] ^= 0x01;
    let fs = mount(&lab.img).unwrap();
    let mut out = vec![0u8; b.bytes.len()];
    let err = fs.read_inode_data(fs.resolve_no_follow(fs.fs_tree(), "/big.bin").unwrap().tree.bytenr, &b.inode, 0, &mut out);
    assert!(matches!(err, Err(LuksError::FsChecksumMismatch(_))), "got {err:?}");
}

/// Index of the csum tree's ROOT_ITEM in the root tree's leaf.
fn csum_root_item(lab: &Lab) -> usize {
    let fs = mount(&lab.img).unwrap();
    let node = fs.read_node(lab.root_tree_root).unwrap();
    assert!(node.is_leaf(), "vacuity: plain.img root tree is one leaf");
    (0..node.nr_items)
        .find(|i| {
            let k = node.key(*i).unwrap();
            k.objectid == CSUM_TREE_OBJECTID && k.item_type == ROOT_ITEM_KEY
        })
        .expect("csum root item")
}

#[test]
fn a_csum_root_lookup_failure_is_not_silently_no_checksums() {
    // O6: a root item that cannot be parsed used to become `None` through
    // `.ok()`, and every later read ran unverified.
    let mut lab = Lab::new();
    let i = csum_root_item(&lab);
    let size_at = HEADER_SIZE + i * ITEM_SIZE + 21;
    lab.patch_node(lab.root_tree_root, |r| r[size_at..size_at + 4].copy_from_slice(&100u32.to_le_bytes()));
    match mount(&lab.img) {
        Err(LuksError::CorruptFs(_)) => {}
        Err(e) => panic!("wrong error {e}"),
        Ok(fs) => panic!("mounted with csum tree = {:?}", fs.csum_tree()),
    }
}

#[test]
fn a_genuinely_absent_csum_tree_reads_only_nodatasum_files() {
    let mut lab = Lab::new();
    let b = big(&lab);
    let i = csum_root_item(&lab);
    let key_at = HEADER_SIZE + i * ITEM_SIZE;
    // Rename the csum root item to objectid 8; assert it still sorts.
    {
        let fs = mount(&lab.img).unwrap();
        let node = fs.read_node(lab.root_tree_root).unwrap();
        assert!(node.key(i + 1).unwrap().objectid > 8, "vacuity: renaming keeps the leaf sorted");
    }
    lab.patch_node(lab.root_tree_root, |r| put_u64(r, key_at, 8));
    dump("csum-root-renamed.img", &lab.img);
    let root = single_extent_tree(&mut lab, &b);
    let fs = mount(&lab.img).expect("an absent csum tree is a legal mount");
    assert!(fs.csum_tree().is_none());
    let mut inode = b.inode.clone();
    inode.size = MIB;
    let mut out = vec![0u8; MIB as usize];
    assert!(fs.read_inode_data(root, &inode, 0, &mut out).is_err(), "no csum tree, datasum inode");
    inode.flags = NODATASUM;
    assert_eq!(fs.read_inode_data(root, &inode, 0, &mut out).unwrap(), MIB as usize);
    assert_eq!(out, b.bytes[..MIB as usize]);
}

// --- O9: superblock mirrors --------------------------------------------------

struct FailAt {
    inner: Vec<u8>,
    bad: u64,
}
impl ReadAt for FailAt {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset == self.bad {
            return Err(LuksError::Io {
                path: "test".into(),
                source: std::io::Error::other("injected mirror read failure"),
            });
        }
        self.inner.read_at(offset, buf)
    }
    fn len(&self) -> Option<u64> {
        ReadAt::len(&self.inner)
    }
}

/// plain.img with mirror 1 (64 MiB) one generation newer than mirror 0.
fn newer_mirror() -> (Vec<u8>, u64) {
    let mut img = fixture();
    let o = SUPER_OFFSETS[1] as usize;
    let mut raw = img[o..o + 4096].to_vec();
    let gen = u64::from_le_bytes(raw[0x48..0x50].try_into().unwrap()) + 1;
    put_u64(&mut raw, 0x48, gen);
    let c = crc32c(&raw[32..]);
    raw[..32].fill(0);
    raw[..4].copy_from_slice(&c.to_le_bytes());
    img[o..o + 4096].copy_from_slice(&raw);
    (img, gen)
}

#[test]
fn an_unreadable_superblock_mirror_is_not_silently_skipped() {
    let (img, gen) = newer_mirror();
    // Control: readable, the newer mirror wins.
    assert_eq!(Superblock::find(&img).unwrap().generation, gen);
    // A read error on a mirror that is inside the device must surface, since
    // that copy may be the newest one.
    let dev = FailAt { inner: img.clone(), bad: SUPER_OFFSETS[1] };
    match Superblock::find(&dev) {
        Err(LuksError::Io { .. }) => {}
        Err(e) => panic!("wrong error {e}"),
        Ok(sb) => panic!("mounted generation {} while the newer mirror was unreadable", sb.generation),
    }
}

#[test]
fn a_mirror_beyond_the_device_is_still_skipped() {
    let (mut img, _) = newer_mirror();
    img.truncate(SUPER_OFFSETS[1] as usize); // mirror 1 does not exist
    let sb = Superblock::find(&img).expect("out-of-range mirrors are not errors");
    assert_eq!(sb.bytenr, SUPER_OFFSETS[0]);
}

// --- R4r: a file written in the open batch reads back verified ---------------

#[cfg(feature = "dangerous-write-support")]
mod uncommitted {
    use super::common;
    use luks_core::fs::Btrfs;

    #[test]
    fn a_file_in_the_open_batch_reads_back_with_full_checksum_coverage() {
        let dev = common::mem_device::MemoryDevice::from_fixture("btrfs/plain.img");
        let mut fs = Btrfs::mount(dev).unwrap();
        let payload: Vec<u8> = (0..65536u32).map(|i| (i * 37 + 11) as u8).collect();
        let mut w = fs.begin_file(payload.len() as u64).unwrap();
        fs.write_chunk(&mut w, &payload).unwrap();
        fs.finish_file(w, "/", "batch.bin").unwrap(); // not committed
        let found = fs.resolve_no_follow(fs.fs_tree(), "/batch.bin").unwrap();
        let mut out = vec![0u8; payload.len()];
        let (n, cov) = fs
            .read_inode_data_covered(found.tree.bytenr, &found.inode, 0, &mut out)
            .expect("uncommitted read");
        assert_eq!(n, payload.len());
        assert_eq!(out, payload);
        assert_eq!(cov.verified_bytes, payload.len() as u64, "coverage {cov:?}");
        assert_eq!(cov.unverified_bytes, 0);
    }
}
