//! Read-only ext4 consistency check (F8). Needs no write feature.
//!
//! An advisory signal; `e2fsck` stays the truth. It reads the superblock and
//! the descriptor table **from the device**, not from the mount's in-memory
//! copy, builds the set of blocks every in-use inode and every piece of fs
//! metadata owns, and compares that with the on-disk bitmaps.
//!
//! # Classification
//!
//! * `Corrupt`: a block owned twice, an owned block the bitmap calls free, a
//!   dirent to an unused inode, inode / extent-block / directory-block
//!   checksum failures, malformed trees or dirents, errors recorded in the
//!   superblock.
//! * `Leaks`: bitmap bits with no owner, wrong counters, an in-use inode
//!   with no name, a bitmap or descriptor checksum mismatch (written without
//!   a flush between them), or a dirty flag.
//! * `Unknown{Unsupported}`: any feature or shape the checker has not been
//!   graded against. Never red.
//!
//! An inode is *in use* by e2fsck's rule: `i_links_count > 0`, or an inode
//! below `s_first_ino`. The bitmap bit decides only whether the bitmap agrees.
//!
//! No name, path or block number leaves this module except as a capped debug
//! sample on [`Findings`].

use std::collections::HashMap;
use std::time::Instant;

use super::csum::Seed;
use super::{
    GroupDesc, Superblock, BG_BLOCK_UNINIT, BG_INODE_UNINIT, EXTENT_MAGIC, INCOMPAT_64BIT, INCOMPAT_CSUM_SEED,
    INCOMPAT_EXTENTS, INCOMPAT_FILETYPE, INODE_FL_EXTENTS, INODE_FL_INLINE_DATA, RO_COMPAT_GDT_CSUM,
    RO_COMPAT_METADATA_CSUM, STATE_CLEANLY_UNMOUNTED, STATE_ERRORS_SEEN,
};
use crate::device::ReadAt;
use crate::fs::health::{CancelCheck, FindingKind, Findings, HealthReport, UnknownReason};
use crate::fs::ext4::Ext4;

/// Owned-set budget (A4): above this the check returns `Unknown{TooLarge}`.
const MAX_OWNED_BYTES: u64 = 128 * 1024 * 1024;

// Feature bits the checker has been graded against with kernel-made images.
// Anything else set in the superblock is `Unsupported`.
const COMPAT_HAS_JOURNAL: u32 = 0x0004;
const COMPAT_EXT_ATTR: u32 = 0x0008;
const COMPAT_RESIZE_INODE: u32 = 0x0010;
const COMPAT_DIR_INDEX: u32 = 0x0020;
const COMPAT_ORPHAN_FILE: u32 = 0x1000;
const KNOWN_COMPAT: u32 = COMPAT_HAS_JOURNAL | COMPAT_EXT_ATTR | COMPAT_RESIZE_INODE | COMPAT_DIR_INDEX | COMPAT_ORPHAN_FILE;
const INCOMPAT_FLEX_BG: u32 = 0x0200;
const KNOWN_INCOMPAT: u32 =
    INCOMPAT_FILETYPE | INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_FLEX_BG | INCOMPAT_CSUM_SEED;
const RO_SPARSE_SUPER: u32 = 0x0001;
const RO_LARGE_FILE: u32 = 0x0002;
const RO_HUGE_FILE: u32 = 0x0008;
const RO_DIR_NLINK: u32 = 0x0020;
const RO_EXTRA_ISIZE: u32 = 0x0040;
const KNOWN_RO_COMPAT: u32 = RO_SPARSE_SUPER
    | RO_LARGE_FILE
    | RO_HUGE_FILE
    | RO_COMPAT_GDT_CSUM
    | RO_DIR_NLINK
    | RO_EXTRA_ISIZE
    | RO_COMPAT_METADATA_CSUM;

const INODE_FL_INDEX: u32 = 0x1000;
const EA_MAGIC: u32 = 0xEA02_0000;
const MAX_EXTENT_DEPTH: u16 = 5;
/// Directory blocks read per device call.
const DIR_READ_BLOCKS: u64 = 32;
/// Inodes read per device call.
const ITABLE_CHUNK_INODES: u64 = 1024;
const DOT_BIT: u32 = 1 << 31;

/// What a check counted, for tests and vacuity controls. Not part of the report.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HealthStats {
    pub groups_scanned: u64,
    /// Inodes in use by e2fsck's rule (links > 0, or below `s_first_ino`).
    pub inodes_in_use: u64,
    /// Blocks in the owned set at the end of the scan.
    pub owned_blocks: u64,
    pub max_extent_depth: u16,
    /// Block-mapped (non-extent) inodes walked, and the deepest indirection seen (1 to 3).
    pub indirect_inodes: u64,
    pub max_indirect_level: u8,
    /// Extended-attribute blocks owned by more than one inode.
    pub shared_ea_blocks: u64,
    pub dirs_scanned: u64,
    /// Directory blocks that were htree index blocks (root or interior node).
    pub dx_blocks: u64,
}

/// Why the scan stopped before the end.
enum Stop {
    Cancelled,
    Unsupported,
    TooLarge,
    Io,
    /// A red finding was recorded that makes going on meaningless.
    Abort,
}

impl From<crate::error::LuksError> for Stop {
    fn from(_: crate::error::LuksError) -> Self {
        Stop::Io
    }
}

type Res<T> = Result<T, Stop>;

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// One in-use inode, kept sorted by number for dirent lookups.
struct Rec {
    ino: u32,
    links: u16,
    refs: u16,
    /// References from entries other than `.` and `..`.
    named: u16,
    is_dir: bool,
    /// Owned by the superblock or reserved: no dirent expected.
    special: bool,
}

struct Ext {
    start: u64,
    len: u64,
    uninit: bool,
}

struct Checker<'a, D: ReadAt> {
    dev: &'a D,
    cancel: &'a dyn CancelCheck,
    sb: Superblock,
    raw_sb: Vec<u8>,
    seed: Seed,
    f: Findings,
    stats: HealthStats,
    bs: usize,
    fdb: u64,
    /// Blocks the bitmaps address: `blocks_count - first_data_block`.
    nbits: u64,
    groups: Vec<GroupDesc>,
    gdt_raw: Vec<u8>,
    uninit_ok: bool,
    block_uninit: Vec<bool>,
    group_ok: Vec<bool>,
    /// Bit `b - first_data_block` set: some owner holds block `b`.
    owned: Vec<u8>,
    recs: Vec<Rec>,
    targets: Vec<u32>,
    /// Extended-attribute blocks: (owners seen, `h_refcount`).
    ea: HashMap<u64, (u32, u32)>,
    special_inos: Vec<u32>,
    sum_free_inodes: u64,
}

impl<D: ReadAt> Ext4<D> {
    /// Check the filesystem read-only. Reads the device directly; the
    /// in-memory superblock and descriptors are not consulted.
    pub fn check_health(&self, cancel: &impl CancelCheck) -> HealthReport {
        self.check_health_stats(cancel).0
    }

    /// [`check_health`](Self::check_health) plus what the scan counted.
    pub fn check_health_stats(&self, cancel: &impl CancelCheck) -> (HealthReport, HealthStats) {
        let start = Instant::now();
        let (f, stats, res) = match Checker::new(&self.device, cancel) {
            Ok(mut c) => {
                let res = c.run();
                (c.f, c.stats, res)
            }
            Err((f, res)) => (f, HealthStats::default(), Err(res)),
        };
        let elapsed = start.elapsed();
        let report = match res {
            Ok(()) | Err(Stop::Abort) => f.finish(elapsed),
            Err(Stop::Cancelled) => f.unknown(UnknownReason::Cancelled, elapsed),
            Err(Stop::Unsupported) => f.unknown(UnknownReason::Unsupported, elapsed),
            Err(Stop::TooLarge) => f.unknown(UnknownReason::TooLarge, elapsed),
            Err(Stop::Io) => f.unknown(UnknownReason::Io, elapsed),
        };
        (report, stats)
    }
}

impl<'a, D: ReadAt> Checker<'a, D> {
    /// Read and validate the superblock and descriptor table.
    fn new(dev: &'a D, cancel: &'a dyn CancelCheck) -> Result<Self, (Findings, Stop)> {
        let mut f = Findings::default();
        let fail = |f: Findings, s: Stop| Err((f, s));

        let mut raw_sb = vec![0u8; 1024];
        if dev.read_at(1024, &mut raw_sb).is_err() {
            return fail(f, Stop::Io);
        }
        let sb = match Superblock::parse(&raw_sb) {
            Ok(sb) => sb,
            // Mounted a moment ago, unreadable now: not something to judge.
            Err(_) => return fail(f, Stop::Unsupported),
        };
        if sb.feature_compat & !KNOWN_COMPAT != 0
            || sb.feature_incompat & !KNOWN_INCOMPAT != 0
            || sb.feature_ro_compat & !KNOWN_RO_COMPAT != 0
        {
            return fail(f, Stop::Unsupported);
        }
        let bs = sb.block_size as usize;
        let fdb = sb.first_data_block as u64;
        let bpg = sb.blocks_per_group as u64;
        let ipg = sb.inodes_per_group as u64;
        let isz = sb.inode_size as u64;
        // Geometry that would make the arithmetic below meaningless.
        let geometry_ok = sb.blocks_count > fdb
            && bpg % 8 == 0
            && bpg <= 8 * bs as u64
            && sb.clusters_per_group == sb.blocks_per_group
            && ipg <= 8 * bs as u64
            && isz <= bs as u64
            && (sb.inode_size as usize) >= 128;
        if !geometry_ok {
            f.add(FindingKind::BadStructure, 1, None);
            return fail(f, Stop::Abort);
        }
        let group_count = sb.group_count();
        if sb.inodes_count as u64 != group_count * ipg || sb.inodes_count >= DOT_BIT {
            f.add(FindingKind::BadStructure, 1, None);
            return fail(f, Stop::Abort);
        }
        let nbits = sb.blocks_count - fdb;
        if nbits.div_ceil(8) > MAX_OWNED_BYTES {
            return fail(f, Stop::TooLarge);
        }
        let ds = sb.group_desc_size() as u64;
        let gdt_bytes = group_count * ds;
        if sb.group_desc_offset() + gdt_bytes > sb.blocks_count * bs as u64 {
            f.add(FindingKind::BadStructure, 1, None);
            return fail(f, Stop::Abort);
        }
        let mut gdt_raw = vec![0u8; gdt_bytes as usize];
        if dev.read_at(sb.group_desc_offset(), &mut gdt_raw).is_err() {
            return fail(f, Stop::Io);
        }
        let groups: Vec<GroupDesc> = (0..group_count as usize)
            .map(|g| GroupDesc::parse(&gdt_raw[g * ds as usize..(g + 1) * ds as usize]))
            .collect();
        let uninit_ok = sb.feature_ro_compat & (RO_COMPAT_GDT_CSUM | RO_COMPAT_METADATA_CSUM) != 0;
        let block_uninit: Vec<bool> = groups.iter().map(|g| uninit_ok && g.block_bitmap_uninit()).collect();
        if !uninit_ok && groups.iter().any(|g| g.flags & (BG_BLOCK_UNINIT | BG_INODE_UNINIT) != 0) {
            return fail(f, Stop::Unsupported);
        }
        let mut special_inos = vec![le32(&raw_sb, 0xE0), le32(&raw_sb, 0x240), le32(&raw_sb, 0x244), le32(&raw_sb, 0x26C)];
        if sb.feature_compat & COMPAT_ORPHAN_FILE != 0 {
            special_inos.push(le32(&raw_sb, 0x280));
        }
        let seed = Seed::of(&sb);
        Ok(Checker {
            dev,
            cancel,
            raw_sb,
            seed,
            f,
            stats: HealthStats::default(),
            bs,
            fdb,
            nbits,
            gdt_raw,
            uninit_ok,
            block_uninit,
            group_ok: vec![true; groups.len()],
            owned: vec![0u8; nbits.div_ceil(8) as usize],
            groups,
            sb,
            recs: Vec::new(),
            targets: Vec::new(),
            ea: HashMap::new(),
            special_inos,
            sum_free_inodes: 0,
        })
    }

    fn poll(&self) -> Res<()> {
        if self.cancel.cancelled() {
            Err(Stop::Cancelled)
        } else {
            Ok(())
        }
    }

    fn read(&self, offset: u64, len: usize) -> Res<Vec<u8>> {
        let mut buf = vec![0u8; len];
        self.dev.read_at(offset, &mut buf)?;
        Ok(buf)
    }

    fn read_block(&self, block: u64) -> Res<Vec<u8>> {
        self.read(block * self.bs as u64, self.bs)
    }

    fn block_in_fs(&self, block: u64) -> bool {
        block >= self.fdb && block < self.sb.blocks_count
    }

    fn run(&mut self) -> Res<()> {
        self.check_superblock();
        let groups = self.groups.len();

        // Pass A: descriptors, fs metadata ownership, inode scan.
        for g in 0..groups {
            self.poll()?;
            self.scan_group(g)?;
            self.stats.groups_scanned += 1;
        }
        self.resolve_dirents();
        self.check_ea_refcounts();

        // Pass B: block bitmaps against the owned set, counters.
        let mut free_blocks = 0u64;
        let free_inodes = self.sum_free_inodes;
        for g in 0..groups {
            self.poll()?;
            free_blocks += self.compare_block_bitmap(g)?;
        }
        if self.sb.free_blocks_count != free_blocks {
            self.f.add(FindingKind::CounterMismatch, 1, None);
        }
        if self.sb.free_inodes_count as u64 != free_inodes {
            self.f.add(FindingKind::CounterMismatch, 1, None);
        }
        self.stats.owned_blocks = self.owned.iter().map(|b| b.count_ones() as u64).sum();
        Ok(())
    }
}

fn popcount_bits(bm: &[u8], nbits: u64) -> u64 {
    let full = (nbits / 8) as usize;
    let mut n: u64 = bm[..full].iter().map(|b| b.count_ones() as u64).sum();
    let rem = (nbits % 8) as u32;
    if rem != 0 {
        n += (bm[full] & ((1u8 << rem) - 1)).count_ones() as u64;
    }
    n
}

/// `rec_len` of the directory entry at `off`, decoded for 64 KiB blocks.
fn dirent_rec_len(block: &[u8], off: usize) -> usize {
    let raw = le16(block, off + 4) as usize;
    if block.len() >= 65536 {
        if raw == 0 || raw == 65535 {
            65536
        } else {
            (raw & 65532) | ((raw & 3) << 16)
        }
    } else {
        raw
    }
}

/// `get_dx_countlimit`: where the htree count/limit pair sits, if this block
/// is an htree root or interior node.
fn dx_count_offset(block: &[u8]) -> Option<usize> {
    let bs = block.len();
    let rl = dirent_rec_len(block, 0);
    if rl == bs {
        return Some(8);
    }
    if rl == 12 && bs >= 44 {
        if dirent_rec_len(block, 12) != bs - 12 {
            return None;
        }
        // dx_root_info: reserved_zero u32, hash_version u8, info_length u8.
        if le32(block, 24) != 0 || block[29] != 8 {
            return None;
        }
        return Some(32);
    }
    None
}

impl<D: ReadAt> Checker<'_, D> {
    fn check_superblock(&mut self) {
        if self.seed.enabled() && self.seed.superblock(&self.raw_sb) != Some(le32(&self.raw_sb, 0x3FC)) {
            self.f.add(FindingKind::BadStructure, 1, None);
        }
        let state = self.sb.state;
        if state & STATE_ERRORS_SEEN != 0 {
            self.f.add(FindingKind::MarkedErrors, 1, None);
        }
        if state & STATE_CLEANLY_UNMOUNTED == 0 || self.sb.last_orphan != 0 {
            self.f.add(FindingKind::MarkedDirty, 1, None);
        }
    }

    fn csum_matches(&self, computed: u32, stored: u32) -> bool {
        if self.sb.group_desc_size() >= 64 {
            computed == stored
        } else {
            computed & 0xFFFF == stored & 0xFFFF
        }
    }

    /// Mark `[start, start+len)` owned. Returns how many blocks were already
    /// owned. `file` marks a file-data or file-tree claim, which must not
    /// land in a BLOCK_UNINIT group.
    fn claim(&mut self, start: u64, len: u64, file: bool) -> u64 {
        if len == 0 {
            return 0;
        }
        let end = match start.checked_add(len) {
            Some(e) if start >= self.fdb && e <= self.sb.blocks_count => e,
            _ => {
                self.f.add(FindingKind::BadStructure, 1, Some(start));
                return 0;
            }
        };
        let (a, b) = (start - self.fdb, end - self.fdb);
        let (mut overlap, mut first_overlap) = (0u64, None);
        let mut i = a;
        while i < b {
            if i % 8 == 0 && i + 8 <= b && self.owned[(i / 8) as usize] == 0 {
                self.owned[(i / 8) as usize] = 0xFF;
                i += 8;
                continue;
            }
            let (byte, mask) = ((i / 8) as usize, 1u8 << (i % 8));
            if self.owned[byte] & mask != 0 {
                overlap += 1;
                first_overlap.get_or_insert(i + self.fdb);
            } else {
                self.owned[byte] |= mask;
            }
            i += 1;
        }
        if overlap > 0 {
            self.f.add(FindingKind::DoubleOwnedBlock, overlap, first_overlap);
        }
        if file {
            let bpg = self.sb.blocks_per_group as u64;
            for g in (a / bpg)..=((b - 1) / bpg) {
                if self.block_uninit.get(g as usize).copied().unwrap_or(false) {
                    let n = b.min((g + 1) * bpg) - a.max(g * bpg);
                    self.f.add(FindingKind::BlockInUseButFree, n, Some(start));
                }
            }
        }
        overlap
    }

    /// An extended-attribute block. May be shared by up to `h_refcount` inodes.
    fn claim_ea(&mut self, blk: u64) -> Res<()> {
        if let Some(e) = self.ea.get_mut(&blk) {
            e.0 += 1;
            return Ok(());
        }
        if !self.block_in_fs(blk) {
            self.f.add(FindingKind::BadStructure, 1, Some(blk));
            return Ok(());
        }
        let data = self.read_block(blk)?;
        let refcount = if le32(&data, 0) == EA_MAGIC {
            le32(&data, 4)
        } else {
            self.f.add(FindingKind::BadStructure, 1, Some(blk));
            1
        };
        self.claim(blk, 1, true);
        self.ea.insert(blk, (1, refcount));
        Ok(())
    }

    fn check_ea_refcounts(&mut self) {
        let mut under = 0u64;
        for (&blk, &(owners, refcount)) in &self.ea {
            if owners > 1 {
                self.stats.shared_ea_blocks += 1;
            }
            if owners > refcount {
                self.f.add(FindingKind::DoubleOwnedBlock, (owners - refcount) as u64, Some(blk));
            } else if owners < refcount {
                under += 1;
            }
        }
        self.f.add(FindingKind::CounterMismatch, under, None);
    }

    /// Descriptor, fs-metadata ownership, inode bitmap and inode table of one group.
    fn scan_group(&mut self, g: usize) -> Res<()> {
        let gd = self.groups[g].clone();
        let ds = self.sb.group_desc_size();
        let (ipg, isz) = (self.sb.inodes_per_group as u64, self.sb.inode_size as u64);
        let bs = self.bs as u64;

        if self.seed.enabled() {
            let raw = &self.gdt_raw[g * ds..(g + 1) * ds];
            if self.seed.group_desc(g as u64, raw) != Some(gd.checksum) {
                self.f.add(FindingKind::GroupDescChecksum, 1, None);
            }
        }

        // Superblock copy, descriptor table and reserved GDT blocks.
        if self.sb.group_has_super(g) {
            let desc_blocks = (self.groups.len() as u64 * ds as u64).div_ceil(bs);
            let n = 1 + desc_blocks + self.sb.reserved_gdt_blocks as u64;
            self.claim(self.sb.group_first_block(g), n, false);
        }

        let it_blocks = (ipg * isz).div_ceil(bs);
        let pointers_ok = self.block_in_fs(gd.block_bitmap)
            && self.block_in_fs(gd.inode_bitmap)
            && self.block_in_fs(gd.inode_table)
            && gd.inode_table + it_blocks <= self.sb.blocks_count
            && gd.itable_unused as u64 <= ipg;
        if !pointers_ok {
            self.f.add(FindingKind::BadStructure, 1, None);
            self.group_ok[g] = false;
            return Ok(());
        }
        self.claim(gd.block_bitmap, 1, false);
        self.claim(gd.inode_bitmap, 1, false);
        self.claim(gd.inode_table, it_blocks, false);

        let inode_uninit = self.uninit_ok && gd.inode_bitmap_uninit();
        let ibm_len = ipg.div_ceil(8) as usize;
        let ibm = if inode_uninit {
            vec![0u8; ibm_len]
        } else {
            let b = self.read_block(gd.inode_bitmap)?;
            if let Some(c) = self.seed.bitmap(&b[..ibm_len]) {
                if !self.csum_matches(c, gd.inode_bitmap_csum) {
                    self.f.add(FindingKind::BitmapChecksum, 1, None);
                }
            }
            b
        };
        let scan_end = if inode_uninit {
            0
        } else if self.uninit_ok {
            ipg - gd.itable_unused as u64
        } else {
            ipg
        };
        let first_ino = self.sb.first_ino as u64;
        let mut idx = 0u64;
        while idx < scan_end {
            let n = ITABLE_CHUNK_INODES.min(scan_end - idx);
            let buf = self.read(gd.inode_table * bs + idx * isz, (n * isz) as usize)?;
            for k in 0..n {
                let i = idx + k;
                let ino = g as u64 * ipg + i + 1;
                let raw = &buf[(k * isz) as usize..((k + 1) * isz) as usize];
                let used = le16(raw, 0x1A) > 0 || ino < first_ino;
                let bit = ibm[(i / 8) as usize] >> (i % 8) & 1 == 1;
                if used && !bit {
                    self.f.add(FindingKind::InodeInUseButFree, 1, Some(ino));
                } else if !used && bit {
                    self.f.add(FindingKind::LeakedInodes, 1, Some(ino));
                }
                if used {
                    self.process_inode(ino, raw)?;
                }
            }
            idx += n;
            self.poll()?;
        }
        // Bits set in a tail the descriptor says was never used.
        for i in scan_end..ipg {
            if ibm[(i / 8) as usize] >> (i % 8) & 1 == 1 {
                self.f.add(FindingKind::LeakedInodes, 1, Some(g as u64 * ipg + i + 1));
            }
        }
        let free = ipg - popcount_bits(&ibm, ipg);
        if gd.free_inodes_count as u64 != free {
            self.f.add(FindingKind::CounterMismatch, 1, None);
        }
        self.sum_free_inodes += free;
        Ok(())
    }

    fn process_inode(&mut self, ino: u64, raw: &[u8]) -> Res<()> {
        if self.seed.enabled() && raw.iter().any(|&b| b != 0) {
            if let Some(computed) = self.seed.inode(ino, raw) {
                let computed = if self.seed.inode_has_csum_hi(raw) { computed } else { computed & 0xFFFF };
                if computed != self.seed.stored_inode_csum(raw) {
                    self.f.add(FindingKind::InodeChecksum, 1, Some(ino));
                }
            }
        }
        let mode = le16(raw, 0);
        let ty = mode & 0xF000;
        let links = le16(raw, 0x1A);
        let flags = u32::from_le_bytes([raw[0x20], raw[0x21], raw[0x22], raw[0x23]]);
        let generation = le32(raw, 0x64);
        let special = ino < self.sb.first_ino as u64
            || self.special_inos.iter().any(|&s| s != 0 && s as u64 == ino);
        self.stats.inodes_in_use += 1;
        self.recs.push(Rec { ino: ino as u32, links, refs: 0, named: 0, is_dir: ty == 0x4000, special });

        let wide = self.sb.feature_incompat & INCOMPAT_64BIT != 0;
        let acl = le32(raw, 0x68) as u64 | if wide { (le16(raw, 0x76) as u64) << 32 } else { 0 };
        if acl != 0 {
            self.claim_ea(acl)?;
        }

        if flags & INODE_FL_INLINE_DATA != 0 {
            return Err(Stop::Unsupported);
        }
        let iblock = &raw[0x28..0x64];
        let blocks512 = le32(raw, 0x1C) as u64 | ((le16(raw, 0x74) as u64) << 32);
        let size = le32(raw, 4) as u64 | if ty == 0x8000 { (le32(raw, 0x6C) as u64) << 32 } else { 0 };
        if flags & INODE_FL_EXTENTS != 0 {
            let mut exts = Vec::new();
            let is_dir = ty == 0x4000;
            self.walk_extents(ino, generation, iblock, None, is_dir, &mut exts, true)?;
            if is_dir {
                self.scan_dir(ino, generation, flags & INODE_FL_INDEX != 0, &exts)?;
            }
            return Ok(());
        }
        let ea_sectors = if acl != 0 { self.bs as u64 / 512 } else { 0 };
        let zero_map = iblock.iter().all(|&b| b == 0);
        match ty {
            // i_block holds device numbers.
            0x2000 | 0x6000 | 0x1000 | 0xC000 => {}
            // Fast symlink: the target text lives in i_block.
            0xA000 if size < 60 && blocks512 == ea_sectors => {}
            _ if ino == 7 && self.sb.feature_compat & COMPAT_RESIZE_INODE != 0 && !zero_map => {
                // Only the double-indirect block is the inode's own; the
                // reserved GDT blocks it lists are fs metadata, claimed once.
                if iblock[..52].iter().any(|&b| b != 0) || iblock[56..].iter().any(|&b| b != 0) {
                    return Err(Stop::Unsupported);
                }
                self.claim(le32(iblock, 52) as u64, 1, false);
            }
            _ if zero_map && size == 0 => {}
            // Block-mapped (ext2/ext3 style, or an e2fsck-made lost+found on
            // an extents filesystem): direct, indirect, double and triple.
            0x8000 | 0x4000 | 0xA000 => {
                let is_dir = ty == 0x4000;
                let mut exts = Vec::new();
                self.stats.indirect_inodes += 1;
                self.walk_indirect(ino, iblock, is_dir, &mut exts)?;
                if is_dir {
                    self.scan_dir(ino, generation, flags & INODE_FL_INDEX != 0, &exts)?;
                }
            }
            // A mode nothing here has been graded against.
            _ => return Err(Stop::Unsupported),
        }
        Ok(())
    }

    fn leaf_ptr(&mut self, p: u64, collect: bool, out: &mut Vec<Ext>) {
        if p == 0 {
            return;
        }
        if !self.block_in_fs(p) {
            self.f.add(FindingKind::BadStructure, 1, Some(p));
            return;
        }
        self.claim(p, 1, true);
        if collect {
            match out.last_mut() {
                Some(l) if l.start + l.len == p => l.len += 1,
                _ => out.push(Ext { start: p, len: 1, uninit: false }),
            }
        }
    }

    fn walk_indirect(&mut self, ino: u64, iblock: &[u8], collect: bool, out: &mut Vec<Ext>) -> Res<()> {
        for i in 0..12 {
            self.leaf_ptr(le32(iblock, i * 4) as u64, collect, out);
        }
        for (slot, level) in [(12usize, 1u8), (13, 2), (14, 3)] {
            let p = le32(iblock, slot * 4) as u64;
            if p != 0 {
                self.walk_ind_block(ino, p, level, collect, out)?;
            }
        }
        Ok(())
    }

    fn walk_ind_block(&mut self, ino: u64, p: u64, level: u8, collect: bool, out: &mut Vec<Ext>) -> Res<()> {
        if !self.block_in_fs(p) {
            self.f.add(FindingKind::BadStructure, 1, Some(ino));
            return Ok(());
        }
        // Already owned: not descended into, which also stops a loop.
        if self.claim(p, 1, true) > 0 {
            return Ok(());
        }
        self.stats.max_indirect_level = self.stats.max_indirect_level.max(level);
        self.poll()?;
        let blk = self.read_block(p)?;
        for k in 0..self.bs / 4 {
            let q = le32(&blk, k * 4) as u64;
            if q == 0 {
                continue;
            }
            if level == 1 {
                self.leaf_ptr(q, collect, out);
            } else {
                self.walk_ind_block(ino, q, level - 1, collect, out)?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_extents(
        &mut self,
        ino: u64,
        generation: u32,
        node: &[u8],
        expect_depth: Option<u16>,
        collect: bool,
        out: &mut Vec<Ext>,
        root: bool,
    ) -> Res<()> {
        let (entries, max, depth) = (le16(node, 2) as usize, le16(node, 4) as usize, le16(node, 6));
        let sane = le16(node, 0) == EXTENT_MAGIC
            && expect_depth.is_none_or(|d| d == depth)
            && depth <= MAX_EXTENT_DEPTH
            && 12 + max * 12 <= node.len()
            && entries <= max;
        if !sane {
            self.f.add(FindingKind::BadStructure, 1, Some(ino));
            return Ok(());
        }
        self.stats.max_extent_depth = self.stats.max_extent_depth.max(depth);
        if !root && self.seed.enabled() {
            let tail = 12 + max * 12;
            if tail + 4 > node.len() {
                self.f.add(FindingKind::BadStructure, 1, Some(ino));
                return Ok(());
            }
            if self.seed.extent_block(ino, generation, &node[..tail + 4]) != Some(le32(node, tail)) {
                self.f.add(FindingKind::ExtentChecksum, 1, Some(ino));
            }
        }
        if depth == 0 {
            let mut prev_end = 0u64;
            for i in 0..entries {
                let e = &node[12 + i * 12..24 + i * 12];
                let lblock = le32(e, 0) as u64;
                let raw_len = le16(e, 4);
                let (len, uninit) = if raw_len > 32768 { ((raw_len - 32768) as u64, true) } else { (raw_len as u64, false) };
                let start = le32(e, 8) as u64 | ((le16(e, 6) as u64) << 32);
                if len == 0 || lblock < prev_end {
                    self.f.add(FindingKind::BadStructure, 1, Some(ino));
                    continue;
                }
                prev_end = lblock + len;
                self.claim(start, len, true);
                if collect {
                    out.push(Ext { start, len, uninit });
                }
            }
        } else {
            for i in 0..entries {
                let e = &node[12 + i * 12..24 + i * 12];
                let child = le32(e, 4) as u64 | ((le16(e, 8) as u64) << 32);
                if !self.block_in_fs(child) {
                    self.f.add(FindingKind::BadStructure, 1, Some(ino));
                    continue;
                }
                // A block already owned is not descended into: this is also
                // what stops a hostile loop.
                if self.claim(child, 1, true) > 0 {
                    continue;
                }
                let blk = self.read_block(child)?;
                self.walk_extents(ino, generation, &blk, Some(depth - 1), collect, out, false)?;
            }
        }
        Ok(())
    }

    fn scan_dir(&mut self, ino: u64, generation: u32, indexed: bool, exts: &[Ext]) -> Res<()> {
        self.stats.dirs_scanned += 1;
        let bs = self.bs;
        for e in exts.iter().filter(|e| !e.uninit) {
            let mut b = 0u64;
            while b < e.len {
                let n = DIR_READ_BLOCKS.min(e.len - b);
                let first = e.start + b;
                if !self.block_in_fs(first) || first + n > self.sb.blocks_count {
                    break; // already reported by the claim
                }
                let buf = self.read(first * bs as u64, n as usize * bs)?;
                for blk in buf.chunks_exact(bs) {
                    self.dir_block(ino, generation, indexed, blk);
                }
                b += n;
                self.poll()?;
            }
        }
        Ok(())
    }

    fn dir_block(&mut self, ino: u64, generation: u32, indexed: bool, block: &[u8]) {
        let bs = block.len();
        let dx = if indexed { dx_count_offset(block) } else { None };
        if let Some(co) = dx {
            self.stats.dx_blocks += 1;
            if self.seed.enabled() {
                let (limit, count) = (le16(block, co) as usize, le16(block, co + 2) as usize);
                if co + limit * 8 > bs - 8 || count > limit {
                    self.f.add(FindingKind::BadStructure, 1, Some(ino));
                    return;
                }
                let (size, tail) = (co + count * 8, co + limit * 8);
                let reserved = [block[tail], block[tail + 1], block[tail + 2], block[tail + 3]];
                if self.seed.dx_block(ino, generation, &block[..size], reserved) != Some(le32(block, tail + 4)) {
                    self.f.add(FindingKind::DirChecksum, 1, Some(ino));
                }
            }
            if dirent_rec_len(block, 0) == bs {
                return; // interior node: no entries
            }
        } else if self.seed.enabled() {
            let t = bs - 12;
            let has_tail = le32(block, t) == 0 && le16(block, t + 4) == 12 && block[t + 6] == 0 && block[t + 7] == 0xDE;
            if !has_tail || self.seed.dir_block(ino, generation, block) != Some(le32(block, t + 8)) {
                self.f.add(FindingKind::DirChecksum, 1, Some(ino));
            }
        }
        let mut off = 0usize;
        while off < bs {
            if off + 8 > bs {
                self.f.add(FindingKind::BadStructure, 1, Some(ino));
                return;
            }
            let target = le32(block, off);
            let rl = dirent_rec_len(block, off);
            let name_len = block[off + 6] as usize;
            if rl < 8 || rl % 4 != 0 || off + rl > bs || 8 + name_len > rl {
                self.f.add(FindingKind::BadStructure, 1, Some(ino));
                return;
            }
            if target != 0 {
                if target > self.sb.inodes_count {
                    self.f.add(FindingKind::BadStructure, 1, Some(ino));
                } else {
                    let name = &block[off + 8..off + 8 + name_len];
                    let dot = name == b"." || name == b"..";
                    self.targets.push(if dot { target | DOT_BIT } else { target });
                }
            }
            off += rl;
        }
    }

    /// Match every collected dirent with an in-use inode, then judge names
    /// and link counts.
    fn resolve_dirents(&mut self) {
        let targets = std::mem::take(&mut self.targets);
        let mut dangling = 0u64;
        let mut sample = None;
        for t in targets {
            let ino = t & !DOT_BIT;
            match self.recs.binary_search_by_key(&ino, |r| r.ino) {
                Ok(i) => {
                    let r = &mut self.recs[i];
                    r.refs = r.refs.saturating_add(1);
                    if t & DOT_BIT == 0 {
                        r.named = r.named.saturating_add(1);
                    }
                }
                Err(_) => {
                    dangling += 1;
                    sample.get_or_insert(ino as u64);
                }
            }
        }
        self.f.add(FindingKind::DirentToFreeInode, dangling, sample);
        for r in &self.recs {
            if r.special && r.ino != 2 {
                continue;
            }
            if r.named == 0 && r.ino != 2 {
                self.f.add(FindingKind::UnattachedInode, 1, Some(r.ino as u64));
            } else if !(r.is_dir && r.links == 1) && r.links < r.refs {
                self.f.add(FindingKind::LinkCountLow, 1, Some(r.ino as u64));
            } else if !(r.is_dir && r.links == 1) && r.links != r.refs {
                self.f.add(FindingKind::CounterMismatch, 1, Some(r.ino as u64));
            }
        }
    }

    /// One group's block bitmap against the owned set. Returns the group's
    /// free-block count as the bitmap states it.
    fn compare_block_bitmap(&mut self, g: usize) -> Res<u64> {
        let gd = self.groups[g].clone();
        if !self.group_ok[g] {
            return Ok(gd.free_blocks_count as u64);
        }
        let bpg = self.sb.blocks_per_group as u64;
        let valid = bpg.min(self.nbits - g as u64 * bpg);
        let base = (g as u64 * bpg / 8) as usize;
        let nbytes = valid.div_ceil(8) as usize;
        let free;
        if self.block_uninit[g] {
            // The kernel computes this bitmap from the layout: exactly the
            // fs metadata. File blocks here were reported when claimed.
            free = valid - popcount_bits(&self.owned[base..base + nbytes], valid);
        } else {
            let bm = self.read_block(gd.block_bitmap)?;
            if let Some(c) = self.seed.bitmap(&bm[..(bpg / 8) as usize]) {
                if !self.csum_matches(c, gd.block_bitmap_csum) {
                    self.f.add(FindingKind::BitmapChecksum, 1, None);
                }
            }
            let (mut plus, mut minus) = (0u64, 0u64);
            let (mut plus_at, mut minus_at) = (None, None);
            for i in 0..nbytes {
                let mask = if (i as u64 + 1) * 8 <= valid { 0xFF } else { (1u8 << (valid - i as u64 * 8)) - 1 };
                let (o, b) = (self.owned[base + i] & mask, bm[i] & mask);
                let at = |bits: u8| self.fdb + g as u64 * bpg + i as u64 * 8 + bits.trailing_zeros() as u64;
                if o & !b != 0 {
                    plus += (o & !b).count_ones() as u64;
                    plus_at.get_or_insert(at(o & !b));
                }
                if b & !o != 0 {
                    minus += (b & !o).count_ones() as u64;
                    minus_at.get_or_insert(at(b & !o));
                }
            }
            self.f.add(FindingKind::BlockInUseButFree, plus, plus_at);
            self.f.add(FindingKind::LeakedBlocks, minus, minus_at);
            if valid < bpg && (valid..bpg).any(|bit| bm[(bit / 8) as usize] >> (bit % 8) & 1 == 0) {
                self.f.add(FindingKind::CounterMismatch, 1, None);
            }
            free = valid - popcount_bits(&bm, valid);
        }
        if gd.free_blocks_count as u64 != free {
            self.f.add(FindingKind::CounterMismatch, 1, None);
        }
        Ok(free)
    }
}
