//! Read-only btrfs consistency check (F8). See notes/recon/F8-btrfs-report.md
//! for the classification of every finding against `btrfs check`.
//!
//! The check reads the *committed* on-disk state only: it never consults the
//! node cache or the write batch overlay, and returns `Unknown{Busy}` while a
//! batch holds uncommitted changes. It needs no write feature.
//!
//! Prove-or-Unknown: a feature the checker does not model, or a read it cannot
//! judge, yields `Unknown`, never `Corrupt`.

use std::collections::{BTreeMap, HashSet};
use std::time::Instant;

use super::chunk::{Chunk, DevExtent};
use super::superblock::{
    Superblock, BTRFS_FEATURE_COMPAT_RO_FREE_SPACE_TREE, BTRFS_FEATURE_COMPAT_RO_FREE_SPACE_TREE_VALID, SUPER_OFFSETS,
    SUPER_SIZE,
};
use super::tree::{self, Key, Node};
use super::Btrfs;
use crate::device::ReadAt;
use crate::error::LuksError;
use crate::fs::health::{CancelCheck, FindingKind, Findings, HealthReport, UnknownReason};

/// `BTRFS_FEATURE_COMPAT_RO_BLOCK_GROUP_TREE`: block group items live in tree 11.
const COMPAT_RO_BLOCK_GROUP_TREE: u64 = 0x8;
const EXTENT_FLAG_TREE_BLOCK: u64 = 0x2;
const MAX_LEVEL: u8 = 8;
const DATA_RELOC_TREE: u64 = u64::MAX - 8;
const LAST_FREE_OBJECTID: u64 = u64::MAX - 256;
/// `btrfs_root_item` offsets.
const ROOT_REFS: usize = 216;
const ROOT_DROP_PROGRESS: std::ops::Range<usize> = 220..237;
const ROOT_DROP_LEVEL: usize = 237;
const ROOT_ITEM_MIN: usize = 239;

enum Stop {
    Cancelled,
    Unknown(UnknownReason),
}

/// Trees whose root leaf may not be empty (kernel tree-checker, `btrfs check`
/// lowmem): root, extent, chunk, dev, fs, data-reloc and every subvolume.
fn must_be_nonempty(tree: u64) -> bool {
    matches!(tree, 1..=5 | DATA_RELOC_TREE) || (tree::FIRST_FREE_OBJECTID..=LAST_FREE_OBJECTID).contains(&tree)
}

struct RootEntry {
    objectid: u64,
    bytenr: u64,
    level: u8,
    dead: bool,
}

/// Streaming state for the extent-tree pass.
#[derive(Default)]
struct ExtentAcc {
    prev_end: u64,
    overlaps: u64,
    overlap_sample: Option<u64>,
    fst_overlaps: u64,
    fst_sample: Option<u64>,
    fst_idx: usize,
    outside_bg: u64,
    malformed: u64,
    used: Vec<u64>,
    meta: Vec<u64>,
    bgs: Vec<(u64, u64, u64)>, // start, length, used
}

/// Streaming state for the free-space-tree pass.
#[derive(Default)]
struct FstAcc {
    ranges: Vec<(u64, u64)>,
    /// (start, end, claimed extent_count, counted runs) per bg
    infos: Vec<(u64, u64, u64, u64)>,
    malformed: u64,
}

impl FstAcc {
    fn push(&mut self, start: u64, end: u64) {
        let Some(info) = self.infos.last_mut() else {
            self.malformed += 1;
            return;
        };
        if start < info.0 || end > info.1 || end <= start {
            self.malformed += 1;
            return;
        }
        if let Some(last) = self.ranges.last_mut() {
            if last.1 == start && last.0 >= info.0 {
                last.1 = end;
                return;
            }
        }
        info.3 += 1;
        self.ranges.push((start, end));
    }
}

struct Checker<'a, D: ReadAt, C: CancelCheck> {
    fs: &'a Btrfs<D>,
    cancel: &'a C,
    f: Findings,
    visited: HashSet<u64>,
    /// Cleared by any structural failure: cross-tree accounting is then moot.
    walk_ok: bool,
}

impl<D: ReadAt> Btrfs<D> {
    /// Read-only consistency check of the committed on-disk state.
    pub fn check_health(&self, cancel: &impl CancelCheck) -> HealthReport {
        let start = Instant::now();
        let mut ck = Checker { fs: self, cancel, f: Findings::default(), visited: HashSet::new(), walk_ok: true };
        if self.has_uncommitted_state() {
            return ck.f.unknown(UnknownReason::Busy, start.elapsed());
        }
        match ck.run() {
            Ok(()) => ck.f.finish(start.elapsed()),
            Err(Stop::Cancelled) => ck.f.unknown(UnknownReason::Cancelled, start.elapsed()),
            Err(Stop::Unknown(r)) => ck.f.unknown(r, start.elapsed()),
        }
    }

    fn has_uncommitted_state(&self) -> bool {
        #[cfg(feature = "dangerous-write-support")]
        {
            self.commit_pending || self.active_batch.as_ref().is_some_and(|b| b.has_uncommitted_files())
        }
        #[cfg(not(feature = "dangerous-write-support"))]
        {
            false
        }
    }
}

fn u64at(b: &[u8], o: usize) -> u64 {
    let mut v = [0u8; 8];
    v.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(v)
}
fn u32at(b: &[u8], o: usize) -> u32 {
    let mut v = [0u8; 4];
    v.copy_from_slice(&b[o..o + 4]);
    u32::from_le_bytes(v)
}

impl<D: ReadAt, C: CancelCheck> Checker<'_, D, C> {
    fn bad(&mut self, sample: Option<u64>) {
        self.walk_ok = false;
        self.f.add(FindingKind::BadStructure, 1, sample);
    }

    /// A read or parse error: structure-class errors are findings, everything
    /// else stops the check as Unknown.
    fn judge(&mut self, e: LuksError, at: u64) -> Result<(), Stop> {
        match e {
            LuksError::FsChecksumMismatch(_)
            | LuksError::CorruptFs(_)
            | LuksError::Truncated { .. }
            | LuksError::OutOfBounds => {
                self.bad(Some(at));
                Ok(())
            }
            LuksError::UnsupportedFsFeature(_) => Err(Stop::Unknown(UnknownReason::Unsupported)),
            _ => Err(Stop::Unknown(UnknownReason::Io)),
        }
    }

    /// Read one tree block straight from disk (no cache, no batch overlay).
    fn read_block(&mut self, bytenr: u64) -> Result<Option<Node>, Stop> {
        if self.cancel.cancelled() {
            return Err(Stop::Cancelled);
        }
        let sb = self.fs.superblock();
        let mut raw = vec![0u8; sb.node_size as usize];
        let res = self
            .fs
            .read_logical(bytenr, &mut raw)
            .and_then(|()| Node::parse(raw, bytenr, sb.csum_type, &sb.metadata_uuid));
        match res {
            Ok(n) => Ok(Some(n)),
            Err(e) => {
                self.judge(e, bytenr)?;
                Ok(None)
            }
        }
    }

    fn run(&mut self) -> Result<(), Stop> {
        let sb = self.fs.superblock().clone();
        if sb.compat_ro_flags & COMPAT_RO_BLOCK_GROUP_TREE != 0 {
            return Err(Stop::Unknown(UnknownReason::Unsupported));
        }
        self.check_super_mirrors()?;

        // Chunk tree.
        let mut chunks: Vec<Chunk> = Vec::new();
        let mut chunk_errs: Vec<LuksError> = Vec::new();
        self.walk(sb.chunk_root, Some(sb.chunk_root_level), tree::CHUNK_TREE_OBJECTID, &mut |k, d| {
            if k.item_type == tree::CHUNK_ITEM_KEY {
                match Chunk::parse(k.offset, d) {
                    Ok(c) => chunks.push(c),
                    Err(e) => chunk_errs.push(e),
                }
            }
        })?;
        for e in chunk_errs {
            self.judge(e, 0)?;
        }
        if chunks.is_empty() {
            self.bad(None);
        }
        let mut overlapping_chunks = 0;
        for w in chunks.windows(2) {
            if w[0].logical + w[0].length > w[1].logical {
                overlapping_chunks += 1;
            }
        }
        if overlapping_chunks > 0 {
            self.bad(None);
        }

        // Root tree: collect every ROOT_ITEM.
        let mut roots: Vec<RootEntry> = Vec::new();
        let mut short_roots = 0u64;
        self.walk(sb.root, Some(sb.root_level), tree::ROOT_TREE_OBJECTID, &mut |k, d| {
            if k.item_type != tree::ROOT_ITEM_KEY {
                return;
            }
            if d.len() < ROOT_ITEM_MIN {
                short_roots += 1;
                return;
            }
            let dead = u32at(d, ROOT_REFS) == 0
                || d[ROOT_DROP_PROGRESS].iter().any(|&b| b != 0)
                || d[ROOT_DROP_LEVEL] != 0;
            roots.push(RootEntry { objectid: k.objectid, bytenr: u64at(d, 176), level: d[238], dead });
        })?;
        for _ in 0..short_roots {
            self.bad(None);
        }
        let has_dead = roots.iter().any(|r| r.dead);
        let find = |id: u64| roots.iter().find(|r| r.objectid == id && !r.dead);

        // Dev tree.
        let mut devexts: Vec<DevExtent> = Vec::new();
        let mut dev_errs: Vec<LuksError> = Vec::new();
        match find(tree::DEV_TREE_OBJECTID) {
            Some(r) => {
                let (b, l) = (r.bytenr, r.level);
                self.walk(b, Some(l), tree::DEV_TREE_OBJECTID, &mut |k, d| {
                    if k.item_type == tree::DEV_EXTENT_KEY {
                        match DevExtent::parse(&k, d) {
                            Ok(e) => devexts.push(e),
                            Err(e) => dev_errs.push(e),
                        }
                    }
                })?;
            }
            None => self.bad(None),
        }
        for e in dev_errs {
            self.judge(e, 0)?;
        }

        // Free-space tree (valid only: the kernel rebuilds an invalid one).
        let fst_present = sb.compat_ro_flags & BTRFS_FEATURE_COMPAT_RO_FREE_SPACE_TREE != 0;
        let fst_valid = fst_present && sb.compat_ro_flags & BTRFS_FEATURE_COMPAT_RO_FREE_SPACE_TREE_VALID != 0;
        let mut fst = FstAcc::default();
        if fst_present {
            match find(tree::FREE_SPACE_TREE_OBJECTID) {
                Some(r) => {
                    let (b, l) = (r.bytenr, r.level);
                    let sector = sb.sector_size as u64;
                    self.walk(b, Some(l), tree::FREE_SPACE_TREE_OBJECTID, &mut |k, d| {
                        collect_fst(&mut fst, sector, k, d)
                    })?;
                }
                None => self.bad(None),
            }
        }

        // Every other tree, structurally.
        for r in roots.iter().filter(|r| {
            !r.dead
                && !matches!(
                    r.objectid,
                    tree::EXTENT_TREE_OBJECTID | tree::DEV_TREE_OBJECTID | tree::FREE_SPACE_TREE_OBJECTID
                )
        }) {
            self.walk(r.bytenr, Some(r.level), r.objectid, &mut |_, _| {})?;
        }

        // Extent tree: sums, overlaps, ownership.
        let mut acc = ExtentAcc { used: vec![0; chunks.len()], ..Default::default() };
        let nodesize = sb.node_size as u64;
        match find(tree::EXTENT_TREE_OBJECTID) {
            Some(r) => {
                let (b, l) = (r.bytenr, r.level);
                let fst_ranges: &[(u64, u64)] = if fst_valid { &fst.ranges } else { &[] };
                let chunks_ref = &chunks;
                self.walk(b, Some(l), tree::EXTENT_TREE_OBJECTID, &mut |k, d| {
                    extent_item(&mut acc, chunks_ref, fst_ranges, nodesize, k, d)
                })?;
            }
            None => self.bad(None),
        }

        if !self.walk_ok {
            // A tree could not be read in full; sums over partial data would
            // only add noise to a verdict that is already Corrupt.
            return Ok(());
        }

        self.f.add(FindingKind::OverlappingExtents, acc.overlaps, acc.overlap_sample);
        self.f.add(FindingKind::FreeSpaceOverlapsExtent, acc.fst_overlaps, acc.fst_sample);
        if acc.malformed + acc.outside_bg + fst.malformed > 0 {
            self.f.add(FindingKind::BadStructure, acc.malformed + acc.outside_bg + fst.malformed, None);
        }
        if fst_valid {
            for info in &fst.infos {
                if info.2 != info.3 {
                    self.f.add(FindingKind::CounterMismatch, 1, Some(info.0));
                }
            }
        }
        self.check_block_groups(&chunks, &acc, sb.bytes_used);
        self.check_ownership(&acc, has_dead);
        self.check_dev_extents(&chunks, &mut devexts);
        Ok(())
    }

    // --- superblock mirrors -------------------------------------------------

    fn check_super_mirrors(&mut self) -> Result<(), Stop> {
        let len = self.fs.device().len();
        let mut primary: Option<Superblock> = None;
        for (i, &off) in SUPER_OFFSETS.iter().enumerate() {
            match len {
                Some(l) if off + SUPER_SIZE as u64 > l => continue,
                None if i > 0 => continue,
                _ => {}
            }
            let mut buf = vec![0u8; SUPER_SIZE];
            if let Err(e) = self.fs.device().read_at(off, &mut buf) {
                if matches!(e, LuksError::OutOfBounds | LuksError::Truncated { .. }) && i > 0 {
                    continue;
                }
                return Err(Stop::Unknown(UnknownReason::Io));
            }
            match Superblock::parse(&buf, off) {
                Ok(s) if i == 0 => primary = Some(s),
                Ok(s) => {
                    if let Some(p) = &primary {
                        let differs = s.fsid != p.fsid
                            || s.generation > p.generation
                            || (s.generation == p.generation
                                && (s.root != p.root
                                    || s.chunk_root != p.chunk_root
                                    || s.log_root != p.log_root
                                    || s.root_level != p.root_level
                                    || s.chunk_root_level != p.chunk_root_level
                                    || s.total_bytes != p.total_bytes));
                        if differs {
                            self.f.add(FindingKind::MirrorMismatch, 1, Some(off));
                        }
                    }
                }
                Err(LuksError::UnsupportedFsFeature(_)) => return Err(Stop::Unknown(UnknownReason::Unsupported)),
                Err(
                    LuksError::NotBtrfs(_) | LuksError::FsChecksumMismatch(_) | LuksError::CorruptFs(_),
                ) => {
                    let kind = if i == 0 { FindingKind::MirrorMismatch } else { FindingKind::MirrorInvalid };
                    self.f.add(kind, 1, Some(off));
                }
                Err(_) => return Err(Stop::Unknown(UnknownReason::Io)),
            }
        }
        Ok(())
    }

    // --- tree walk ----------------------------------------------------------

    fn walk(
        &mut self,
        root: u64,
        root_level: Option<u8>,
        tree_id: u64,
        visit: &mut dyn FnMut(Key, &[u8]),
    ) -> Result<(), Stop> {
        if self.visited.contains(&root) {
            return Ok(());
        }
        let Some(node) = self.read_block(root)? else { return Ok(()) };
        if node.level > MAX_LEVEL || root_level.is_some_and(|l| l != node.level) {
            self.bad(Some(root));
            return Ok(());
        }
        let mut last = None;
        self.descend(root, node, None, true, tree_id, &mut last, visit)
    }

    #[allow(clippy::too_many_arguments)]
    fn descend(
        &mut self,
        bytenr: u64,
        node: Node,
        parent_key: Option<Key>,
        is_root: bool,
        tree_id: u64,
        last: &mut Option<Key>,
        visit: &mut dyn FnMut(Key, &[u8]),
    ) -> Result<(), Stop> {
        self.visited.insert(bytenr);
        if node.nr_items == 0 {
            if node.is_leaf() && is_root && !must_be_nonempty(tree_id) {
                return Ok(());
            }
            if node.is_leaf() {
                self.walk_ok = false;
                self.f.add(FindingKind::EmptyLeaf, 1, Some(bytenr));
            } else {
                self.bad(Some(bytenr));
            }
            return Ok(());
        }
        let first = match node.key(0) {
            Ok(k) => k,
            Err(e) => return self.judge(e, bytenr),
        };
        if parent_key.is_some_and(|p| p != first) {
            self.bad(Some(bytenr));
        }
        if node.is_leaf() {
            for i in 0..node.nr_items {
                let (k, d) = match (node.key(i), node.item_data(i)) {
                    (Ok(k), Ok(d)) => (k, d),
                    (Err(e), _) | (_, Err(e)) => return self.judge(e, bytenr),
                };
                if last.is_some_and(|l| l >= k) {
                    self.bad(Some(bytenr));
                    return Ok(());
                }
                *last = Some(k);
                visit(k, d);
            }
            return Ok(());
        }
        for i in 0..node.nr_items {
            let ptr = match node.key_ptr(i) {
                Ok(p) => p,
                Err(e) => return self.judge(e, bytenr),
            };
            if i > 0 && node.key(i - 1).is_ok_and(|p| p >= ptr.key) {
                self.bad(Some(bytenr));
                return Ok(());
            }
            if self.visited.contains(&ptr.blockptr) {
                // Shared with another tree (snapshot) or a cycle; the level
                // bound and this set both guarantee termination.
                *last = None;
                continue;
            }
            let Some(child) = self.read_block(ptr.blockptr)? else { continue };
            if child.level + 1 != node.level || child.generation != ptr.generation {
                self.bad(Some(ptr.blockptr));
                continue;
            }
            self.descend(ptr.blockptr, child, Some(ptr.key), false, tree_id, last, visit)?;
        }
        Ok(())
    }

    // --- cross-tree accounting ----------------------------------------------

    fn check_block_groups(&mut self, chunks: &[Chunk], acc: &ExtentAcc, sb_bytes_used: u64) {
        let (mut ci, mut bi) = (0usize, 0usize);
        let mut total_used = 0u64;
        while ci < chunks.len() || bi < acc.bgs.len() {
            let c = chunks.get(ci).map(|c| (c.logical, c.length));
            let b = acc.bgs.get(bi).map(|b| (b.0, b.1));
            match (c, b) {
                (Some(c), Some(b)) if c == b => {
                    let used = acc.bgs[bi].2;
                    total_used = total_used.saturating_add(used);
                    let actual = acc.used[ci];
                    if used < actual {
                        // Reclaim of an "empty" bg, or a wrong free-space
                        // picture: data can be lost.
                        self.f.add(FindingKind::BlockGroupUsedMismatch, 1, Some(c.0));
                    } else if used > actual {
                        // Space accounted but unowned: a leak, nothing is lost.
                        self.f.add(FindingKind::LeakedBlocks, (used - actual).div_ceil(4096), Some(c.0));
                    }
                    ci += 1;
                    bi += 1;
                }
                (Some(c), Some(b)) if c.0 <= b.0 => {
                    self.bad(Some(c.0));
                    ci += 1;
                }
                (Some(_), None) => {
                    self.bad(None);
                    ci += 1;
                }
                _ => {
                    self.bad(None);
                    bi += 1;
                }
            }
        }
        if total_used != sb_bytes_used {
            self.f.add(FindingKind::CounterMismatch, 1, None);
        }
    }

    fn check_ownership(&mut self, acc: &ExtentAcc, has_dead: bool) {
        let meta: HashSet<u64> = acc.meta.iter().copied().collect();
        let mut unowned = 0;
        let mut first = None;
        for &b in &self.visited {
            if !meta.contains(&b) {
                unowned += 1;
                first.get_or_insert(b);
            }
        }
        self.f.add(FindingKind::UnownedTreeBlock, unowned, first);
        if !has_dead {
            let mut leaked = 0;
            let mut sample = None;
            for &m in &acc.meta {
                if !self.visited.contains(&m) {
                    leaked += 1;
                    sample.get_or_insert(m);
                }
            }
            self.f.add(FindingKind::LeakedExtentItems, leaked, sample);
        }
    }

    fn check_dev_extents(&mut self, chunks: &[Chunk], devexts: &mut [DevExtent]) {
        // Key order is (devid, offset); sweep for overlap per device.
        let mut overlaps = 0;
        let mut sample = None;
        for w in devexts.windows(2) {
            if w[0].devid == w[1].devid && w[0].physical_offset + w[0].length > w[1].physical_offset {
                overlaps += 1;
                sample.get_or_insert(w[1].physical_offset);
            }
        }
        self.f.add(FindingKind::OverlappingDevExtents, overlaps, sample);

        // Every chunk stripe has exactly its dev extent, and nothing else exists.
        let mut by_pos: BTreeMap<(u64, u64), (u64, u64)> = BTreeMap::new();
        for e in devexts.iter() {
            by_pos.insert((e.devid, e.physical_offset), (e.chunk_offset, e.length));
        }
        let mut matched = 0usize;
        let mut missing = 0u64;
        for c in chunks {
            for s in &c.stripes {
                match by_pos.get(&(s.devid, s.offset)) {
                    Some(&(co, len)) if co == c.logical && len == c.length => matched += 1,
                    _ => missing += 1,
                }
            }
        }
        let extra = by_pos.len().saturating_sub(matched) as u64;
        if missing + extra > 0 {
            self.f.add(FindingKind::BadStructure, missing + extra, None);
        }
    }
}

fn collect_fst(acc: &mut FstAcc, sector: u64, k: Key, d: &[u8]) {
    match k.item_type {
        tree::FREE_SPACE_INFO_KEY => {
            if d.len() < 8 || k.offset == 0 {
                acc.malformed += 1;
                return;
            }
            acc.infos.push((k.objectid, k.objectid.saturating_add(k.offset), u32at(d, 0) as u64, 0));
        }
        tree::FREE_SPACE_EXTENT_KEY => acc.push(k.objectid, k.objectid.saturating_add(k.offset)),
        tree::FREE_SPACE_BITMAP_KEY => {
            let bits = k.offset / sector.max(1);
            if (d.len() as u64) * 8 < bits {
                acc.malformed += 1;
                return;
            }
            let mut run_start: Option<u64> = None;
            for i in 0..bits {
                let set = d[(i / 8) as usize] >> (i % 8) & 1 == 1;
                match (set, run_start) {
                    (true, None) => run_start = Some(i),
                    (false, Some(s)) => {
                        acc.push(k.objectid + s * sector, k.objectid + i * sector);
                        run_start = None;
                    }
                    _ => {}
                }
            }
            if let Some(s) = run_start {
                acc.push(k.objectid + s * sector, k.objectid + bits * sector);
            }
        }
        _ => {}
    }
}

fn extent_item(acc: &mut ExtentAcc, chunks: &[Chunk], fst: &[(u64, u64)], nodesize: u64, k: Key, d: &[u8]) {
    let (start, len, is_meta) = match k.item_type {
        tree::EXTENT_ITEM_KEY => {
            if d.len() < 24 {
                acc.malformed += 1;
                return;
            }
            (k.objectid, k.offset, u64at(d, 16) & EXTENT_FLAG_TREE_BLOCK != 0)
        }
        tree::METADATA_ITEM_KEY => {
            if d.len() < 24 {
                acc.malformed += 1;
                return;
            }
            (k.objectid, nodesize, true)
        }
        tree::BLOCK_GROUP_ITEM_KEY => {
            if d.len() < 24 {
                acc.malformed += 1;
            } else {
                acc.bgs.push((k.objectid, k.offset, u64at(d, 0)));
            }
            return;
        }
        _ => return,
    };
    let end = start.saturating_add(len);
    if start < acc.prev_end {
        acc.overlaps += 1;
        acc.overlap_sample.get_or_insert(start);
    }
    acc.prev_end = acc.prev_end.max(end);
    while acc.fst_idx < fst.len() && fst[acc.fst_idx].1 <= start {
        acc.fst_idx += 1;
    }
    if acc.fst_idx < fst.len() && fst[acc.fst_idx].0 < end {
        acc.fst_overlaps += 1;
        acc.fst_sample.get_or_insert(start);
    }
    let ci = match chunks.binary_search_by_key(&start, |c| c.logical) {
        Ok(i) => Some(i),
        Err(0) => None,
        Err(i) => Some(i - 1),
    };
    match ci.filter(|&i| end <= chunks[i].logical + chunks[i].length) {
        Some(i) => acc.used[i] += len,
        None => acc.outside_bg += 1,
    }
    if is_meta {
        acc.meta.push(start);
    }
}
