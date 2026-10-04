//! Allocating and freeing ext4 blocks and inodes.
//!
//! This is the layer under every filesystem write: before a file can exist,
//! something has to claim an inode and some blocks, and claiming them means
//! keeping **five** things consistent at once — the bitmap bit, the group's
//! free counter, the superblock's free counter, the bitmap's checksum, and the
//! group descriptor's checksum. Update four of the five and `e2fsck` reports a
//! filesystem that is quietly wrong.
//!
//! # What this deliberately does not do yet
//!
//! Groups flagged `BLOCK_UNINIT` or `INODE_UNINIT` are **skipped**. Those flags
//! mean the bitmap on disk was never written and holds whatever bytes were
//! there before, so allocating from such a group requires first *materialising*
//! its bitmap — computing which of its blocks are metadata and marking them —
//! and with `flex_bg` those metadata blocks may belong to a different group
//! entirely. That is a distinct problem with its own failure modes, so it gets
//! its own pass rather than being smuggled in here.
//!
//! The cost is real and worth stating plainly: on the 4 GiB test stick, 26 of
//! 33 groups are `BLOCK_UNINIT`, so only about 800 MiB of its 4 GiB is
//! reachable until that pass lands. Correctness first — an allocator that
//! quietly hands out blocks it believes are free, from a bitmap it never read,
//! overwrites live data.
//!
//! # Ordering
//!
//! Writes go bitmap → group descriptor → superblock, so an interruption leaves
//! the *bitmap* ahead of the counters. That direction is deliberate: a bitmap
//! bit set with a stale free count leaks a block, which `e2fsck` fixes with a
//! counter adjustment. The reverse — a counter claiming a block is taken while
//! the bitmap says free — invites the next allocation to hand out a block that
//! is already in use, which loses data.
//!
//! # Reservations and the dirty bracket (F2a)
//!
//! A *file writer* does not allocate through the paths below. It **reserves**
//! ([`reserve_blocks`](Ext4::reserve_blocks), `reserve_inode`): the planning
//! half of an allocation, recorded in memory on the [`Ext4`] and honoured by
//! every other allocator, with no device write and no counter change.
//! Abandoning a transfer is dropping the reservation: memory only, valid on a
//! dead device. `finish_file` verifies against the disk that every reserved
//! bit is still clear, then claims them inside [`Ext4::dirty_window`], which
//! marks the superblock not-clean first so that a crash inside the window is
//! visible to the write gate and to Linux, not silent.
//!
//! The immediate allocators (`alloc_blocks`, `alloc_inode`: directories and
//! `create_file`) still write before returning, exclude reservations, and are
//! bracketed by their callers.
//!
//! # Batching
//!
//! Blocks are claimed a *batch* at a time, not one at a time. The difference is
//! not a micro-optimisation: every allocation ends in a commit, and a commit is
//! three device writes — bitmap, group descriptor, superblock — none of which
//! is block-aligned, so each costs a read-modify-write on top. Measured through
//! the SCSI layer that came to **7.0 commands per 4 KiB block**, which for a
//! 50 MB file is over 85,000 round trips: on the phone's USB host that is the
//! five-minute hang that motivated this, spent entirely in bookkeeping while
//! the data itself needs a few hundred commands.
//!
//! [`alloc_blocks`](Ext4::alloc_blocks) reads each group's bitmap **once**,
//! claims every block it needs from it in memory, and commits **once per group
//! touched**, with a single superblock write closing the whole batch. A 50 MB
//! file fits in one group, so its bookkeeping is three writes rather than
//! 36,000. This is the same conclusion ext4 itself reached: ext3 allocated per
//! block at `write_begin` time, and ext4 replaced it with delayed allocation
//! plus a multi-block allocator for these exact reasons.
//!
//! The work is split into a **plan** phase that only reads, and a **commit**
//! phase that only writes. Nothing reaches the device until every block is
//! decided, so running out of space now costs a discarded `Vec` rather than an
//! unwind of already-committed bitmaps. The ordering above is unchanged —
//! bitmap before counters, per group — so an interruption still leaks blocks
//! and still cannot double-allocate. There are simply far fewer moments at
//! which it can be interrupted.

use crate::device::WriteAt;
use crate::error::{LuksError, Result};
use crate::fs::ext4::csum::Seed;
use crate::fs::ext4::{Ext4, GroupDesc, Superblock};

/// Which of a group's two bitmaps is being addressed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bitmap {
    Block,
    Inode,
}

/// A contiguous run of physical blocks, as handed out by
/// [`alloc_blocks`](Ext4::alloc_blocks).
///
/// Deliberately without a logical offset: where a run sits inside a *file* is
/// the file layer's business, and the allocator has no opinion about it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Extent {
    pub physical: u64,
    pub len: u64,
}

/// Why a multi-step operation stopped, and what it left behind.
///
/// `consistent` is the one fact the dirty bracket ([`Ext4::dirty_window`])
/// needs: `true` means the on-disk metadata is self-consistent (nothing was
/// written, or every step that was written has been undone and the undo
/// landed), so the filesystem may be marked clean again. `false` means
/// something may be half done, and the filesystem is **left marked dirty** on
/// disk, which is both the truth and what makes the write gate refuse it until
/// `e2fsck` has run.
///
/// The `From<LuksError>` conversion is deliberately the pessimistic one, so a
/// stray `?` inside a window can never mark a possibly-broken filesystem
/// clean. Optimism has to be spelled out.
#[derive(Debug)]
pub(super) struct Aborted {
    /// The primary failure. It is always what the caller receives, unchanged,
    /// so `fences_write_session()` and every other classification of it
    /// survive.
    pub err: LuksError,
    pub consistent: bool,
    /// A failure of the *cleanup* that followed `err`. Never substituted for
    /// it; kept on the instance for [`Ext4::take_cleanup_failure`].
    pub cleanup: Option<LuksError>,
}

impl From<LuksError> for Aborted {
    fn from(err: LuksError) -> Self {
        Aborted {
            err,
            consistent: false,
            cleanup: None,
        }
    }
}

impl Aborted {
    /// The failing step provably wrote nothing and nothing before it needs
    /// undoing.
    pub(super) fn consistent(err: LuksError) -> Self {
        Aborted {
            err,
            consistent: true,
            cleanup: None,
        }
    }

    /// An error from a **single-write** step. A refusal this crate generated
    /// (`AlreadyExists`, an unsupported feature) means the step wrote nothing;
    /// a transport error means nobody knows. This is the same split
    /// [`LuksError::fences_write_session`] already draws. Do not use it for a
    /// step that writes more than once: a refusal after the first write is
    /// partial completion (see [`Aborted::nothing_written_if`]).
    pub(super) fn step(err: LuksError) -> Self {
        let consistent = !err.fences_write_session();
        Aborted {
            err,
            consistent,
            cleanup: None,
        }
    }

    /// For a multi-write step: consistent only if `err` is one of the
    /// refusals that this step raises *before* its first write.
    pub(super) fn nothing_written_if(err: LuksError, before_first_write: impl Fn(&LuksError) -> bool) -> Self {
        let consistent = before_first_write(&err);
        Aborted {
            err,
            consistent,
            cleanup: None,
        }
    }

    /// Undo after `err`. A transport/fencing primary means the device is gone:
    /// no rollback is attempted and the fs stays dirty, which is correct.
    /// Otherwise run `undo`; if it lands the fs is consistent and `err`
    /// stands, and if it fails `err` still stands and the undo failure rides
    /// along in `cleanup`.
    pub(super) fn after_undo(err: LuksError, undo: impl FnOnce() -> Result<()>) -> Self {
        if err.fences_write_session() {
            return Aborted::from(err);
        }
        match undo() {
            Ok(()) => Aborted::consistent(err),
            Err(c) => Aborted {
                err,
                consistent: false,
                cleanup: Some(c),
            },
        }
    }
}

/// One block group's share of a block plan: the bitmap as it will be written,
/// and the runs it yields.
struct GroupPlan {
    group: usize,
    bitmap: Vec<u8>,
    extents: Vec<Extent>,
    taken: u64,
}

/// A group's bitmap with every claim already applied, ready to write.
pub(super) struct BlockClaim {
    group: usize,
    bitmap: Vec<u8>,
    taken: u32,
}

/// The inode half of a publish: which bit, in which group, in which bitmap.
pub(super) struct InodeClaim {
    ino: u64,
    group: usize,
    bit: usize,
    bitmap: Vec<u8>,
}

impl<D: WriteAt> Ext4<D> {
    // --- bitmap primitives -------------------------------------------------

    /// Where a bitmap lives and how many bytes of it the checksum covers.
    ///
    /// The length is not the block size. The kernel checksums exactly
    /// `clusters_per_group / 8` (or `inodes_per_group / 8`) bytes, and using
    /// the whole block instead produces a checksum that never matches.
    ///
    /// The bitmap's block number is an on-disk pointer (it came out of the
    /// group descriptor), so like every such pointer it is bounds-proven
    /// before anything reads through it — or writes, which is the case that
    /// matters: a corrupt descriptor must not aim a bitmap write at an
    /// arbitrary offset.
    fn bitmap_extent(&self, group: usize, which: Bitmap) -> Result<(u64, usize)> {
        let g = &self.groups[group];
        match which {
            Bitmap::Block => Ok((
                self.metadata_block_offset(g.block_bitmap)?,
                (self.sb.clusters_per_group as usize).div_ceil(8),
            )),
            Bitmap::Inode => Ok((
                self.metadata_block_offset(g.inode_bitmap)?,
                (self.sb.inodes_per_group as usize).div_ceil(8),
            )),
        }
    }

    fn read_bitmap(&self, group: usize, which: Bitmap) -> Result<Vec<u8>> {
        let (offset, _len) = self.bitmap_extent(group, which)?;

        let is_uninit = match which {
            Bitmap::Block => self.groups[group].block_bitmap_uninit(),
            Bitmap::Inode => self.groups[group].inode_bitmap_uninit(),
        };
        if is_uninit {
            return self.materialize_bitmap(group, which, self.sb.block_size as usize);
        }

        let mut buf = vec![0u8; self.sb.block_size as usize];
        self.device.read_at(offset, &mut buf)?;
        Ok(buf)
    }

    fn materialize_bitmap(&self, group: usize, which: Bitmap, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let sb = &self.sb;

        let set_bit = |buf: &mut [u8], bit: usize| {
            if bit / 8 < buf.len() {
                buf[bit / 8] |= 1 << (bit % 8);
            }
        };

        match which {
            Bitmap::Inode => {
                let first_inode = (group as u32 * sb.inodes_per_group) + 1;
                let valid_inodes = if first_inode > sb.inodes_count {
                    0
                } else if first_inode + sb.inodes_per_group - 1 > sb.inodes_count {
                    sb.inodes_count - first_inode + 1
                } else {
                    sb.inodes_per_group
                };
                for i in valid_inodes as usize..buf.len() * 8 {
                    set_bit(&mut buf, i);
                }
            }
            Bitmap::Block => {
                let first_block = sb.group_first_block(group);
                let mut valid_blocks = sb.blocks_per_group;
                if first_block + valid_blocks as u64 > sb.blocks_count {
                    valid_blocks = (sb.blocks_count.saturating_sub(first_block)) as u32;
                }
                for b in valid_blocks as usize..buf.len() * 8 {
                    set_bit(&mut buf, b);
                }

                if sb.group_has_super(group) {
                    let mut meta_blocks = 1 + sb.reserved_gdt_blocks as u32;
                    let desc_bytes = self.groups.len() as u64 * sb.group_desc_size() as u64;
                    let desc_blocks = desc_bytes.div_ceil(sb.block_size as u64);
                    meta_blocks += desc_blocks as u32;

                    for i in 0..meta_blocks {
                        set_bit(&mut buf, i as usize);
                    }
                }

                let group_last_block = first_block + valid_blocks as u64 - 1;
                for g in &self.groups {
                    let bb = g.block_bitmap;
                    if bb >= first_block && bb <= group_last_block {
                        set_bit(&mut buf, (bb - first_block) as usize);
                    }
                    let ib = g.inode_bitmap;
                    if ib >= first_block && ib <= group_last_block {
                        set_bit(&mut buf, (ib - first_block) as usize);
                    }
                    let it_start = g.inode_table;
                    let it_blocks = (sb.inodes_per_group as u64 * sb.inode_size as u64).div_ceil(sb.block_size as u64);
                    let it_end = it_start + it_blocks - 1;

                    let overlap_start = it_start.max(first_block);
                    let overlap_end = it_end.min(group_last_block);
                    if overlap_start <= overlap_end {
                        for b in overlap_start..=overlap_end {
                            set_bit(&mut buf, (b - first_block) as usize);
                        }
                    }
                }
            }
        }
        Ok(buf)
    }

    /// Write a bitmap back and record its new checksum in the group
    /// descriptor. The descriptor itself is not flushed here — the caller
    /// adjusts the counters first, so that it is written exactly once.
    fn write_bitmap(&mut self, group: usize, which: Bitmap, bm: &[u8], seed: &Seed) -> Result<()> {
        let (offset, csum_len) = self.bitmap_extent(group, which)?;
        self.device.write_at(offset, bm)?;

        if let Some(csum) = seed.bitmap(&bm[..csum_len]) {
            let g = &mut self.groups[group];
            match which {
                Bitmap::Block => g.block_bitmap_csum = csum,
                Bitmap::Inode => g.inode_bitmap_csum = csum,
            }
        }
        Ok(())
    }

    // --- descriptor and superblock -----------------------------------------

    /// Serialise one group descriptor from the in-memory copy, checksum it and
    /// write it.
    ///
    /// The existing bytes are read first and patched rather than built from
    /// scratch: a descriptor has fields this crate does not model (the exclude
    /// bitmap, reserved space), and rebuilding it would zero them.
    fn write_group_desc(&mut self, group: usize, seed: &Seed) -> Result<()> {
        let size = self.sb.group_desc_size();
        let offset = self.sb.group_desc_offset() + (group * size) as u64;

        let mut d = vec![0u8; size];
        self.device.read_at(offset, &mut d)?;

        let g = self.groups[group].clone();
        let wide = size >= 64;

        put16(&mut d, 0x0C, g.free_blocks_count as u16);
        put16(&mut d, 0x0E, g.free_inodes_count as u16);
        put16(&mut d, 0x10, g.used_dirs_count as u16);
        put16(&mut d, 0x12, g.flags);
        put16(&mut d, 0x18, g.block_bitmap_csum as u16);
        put16(&mut d, 0x1A, g.inode_bitmap_csum as u16);
        put16(&mut d, 0x1C, g.itable_unused as u16);
        if wide {
            put16(&mut d, 0x2C, (g.free_blocks_count >> 16) as u16);
            put16(&mut d, 0x2E, (g.free_inodes_count >> 16) as u16);
            put16(&mut d, 0x30, (g.used_dirs_count >> 16) as u16);
            put16(&mut d, 0x32, (g.itable_unused >> 16) as u16);
            put16(&mut d, 0x38, (g.block_bitmap_csum >> 16) as u16);
            put16(&mut d, 0x3A, (g.inode_bitmap_csum >> 16) as u16);
        }

        // The checksum covers the field's own bytes as zeros, so it must be
        // computed after every other field is in place.
        if let Some(csum) = seed.group_desc(group as u64, &d) {
            put16(&mut d, 0x1E, csum);
            self.groups[group].checksum = csum;
        }

        self.device.write_at(offset, &d)?;
        Ok(())
    }

    /// Write the superblock's free counters and re-checksum it.
    ///
    /// Backup superblocks are left alone. That matches what the kernel does on
    /// every ordinary write — backups are refreshed by `e2fsck` and `tune2fs`,
    /// not on the fly — so touching them here would be a deviation, not extra
    /// safety.
    fn write_superblock(&mut self, seed: &Seed) -> Result<()> {
        let mut sb = vec![0u8; 1024];
        self.device.read_at(1024, &mut sb)?;

        put32(&mut sb, 12, self.sb.free_blocks_count as u32);
        put32(&mut sb, 16, self.sb.free_inodes_count);
        if self.sb.is_64bit() {
            // 0x158, not 0x154 — 0x154 is s_r_blocks_count_hi, and writing the
            // free count there both loses the update and clobbers the reserved
            // count. Invisible below 16 TiB, where every hi word is zero.
            put32(
                &mut sb,
                super::SB_FREE_BLOCKS_COUNT_HI,
                (self.sb.free_blocks_count >> 32) as u32,
            );
        }

        if let Some(csum) = seed.superblock(&sb) {
            put32(&mut sb, 0x3FC, csum);
        }

        self.device.write_at(1024, &sb)?;
        Ok(())
    }

    /// Push a group's bitmap, descriptor and the superblock in the safe order.
    fn commit(&mut self, group: usize, which: Bitmap, bm: &[u8], seed: &Seed) -> Result<()> {
        match which {
            Bitmap::Block => self.groups[group].flags &= !crate::fs::ext4::BG_BLOCK_UNINIT,
            Bitmap::Inode => self.groups[group].flags &= !crate::fs::ext4::BG_INODE_UNINIT,
        }
        self.write_bitmap(group, which, bm, seed)?;
        self.write_group_desc(group, seed)?;
        self.write_superblock(seed)
    }

    // --- blocks ------------------------------------------------------------

    /// Set, in `view`, every bit this instance has promised to an open writer
    /// in `group`. `view` is a *search* copy: it decides what may be handed
    /// out, and is never written back.
    fn overlay_reserved_blocks(&self, group: usize, view: &mut [u8]) {
        let per = self.sb.blocks_per_group as u64;
        let start = self.sb.first_data_block as u64 + group as u64 * per;
        let end = start + per;
        for &(s, e) in self.reserved_blocks.intervals() {
            if e <= start || s >= end {
                continue;
            }
            for block in s.max(start)..e.min(end) {
                set_bit(view, (block - start) as usize);
            }
        }
    }

    /// Blocks promised to open writers inside `group`.
    fn reserved_in_group(&self, group: usize) -> u64 {
        let per = self.sb.blocks_per_group as u64;
        let start = self.sb.first_data_block as u64 + group as u64 * per;
        self.reserved_blocks.covered_within(start, start + per)
    }

    /// The planning half of an allocation: decide which blocks, touching
    /// nothing. Reads each group's bitmap once.
    ///
    /// Blocks reserved for an open writer count as used, and a group's
    /// allowance is its descriptor's free count **minus** what is reserved in
    /// it, so a reservation can never be starved by an immediate allocation
    /// and a publish can never find the counter already spent.
    ///
    /// A group is never asked for more than that allowance. Trusting the
    /// bitmap past it would hand out blocks while the counter went its own
    /// way, and the counter is what `e2fsck` checks.
    ///
    /// The memory this holds is one bitmap per group *touched* — 4 KiB each on
    /// the test stick. A 50 MB file touches one; a 4 GiB file touches 33, so
    /// 132 KiB. Worth stating, not worth engineering around at these sizes.
    fn plan_blocks(&self, goal: u64, count: u64) -> Result<(Vec<GroupPlan>, Vec<Extent>)> {
        let per = self.sb.blocks_per_group as u64;
        let first = self.sb.first_data_block as u64;
        let ngroups = self.groups.len();

        let start = if goal >= first {
            (((goal - first) / per) as usize).min(ngroups.saturating_sub(1))
        } else {
            0
        };

        let mut plan: Vec<GroupPlan> = Vec::new();
        let mut out: Vec<Extent> = Vec::new();
        let mut remaining = count;

        for n in 0..ngroups {
            if remaining == 0 {
                break;
            }
            let g = (start + n) % ngroups;
            if self.groups[g].free_blocks_count == 0 {
                continue;
            }
            let spare = (self.groups[g].free_blocks_count as u64).saturating_sub(self.reserved_in_group(g));
            if spare == 0 {
                continue;
            }

            let mut bm = self.read_bitmap(g, Bitmap::Block)?;
            let mut view = bm.clone();
            self.overlay_reserved_blocks(g, &mut view);
            let limit = self.blocks_in_group(g);
            let allowance = spare.min(remaining);

            let mut taken = 0u64;
            let mut cursor = 0usize;
            let mut extents: Vec<Extent> = Vec::new();
            while taken < allowance {
                // The descriptor may claim free blocks the bitmap does not
                // have. Stopping here is the safe reading: trusting the
                // counter would hand out a block that is already in use.
                let Some(bit) = find_free_bit_from(&view, cursor, limit) else {
                    break;
                };
                set_bit(&mut bm, bit);
                cursor = bit + 1;

                let block = first + g as u64 * per + bit as u64;
                match extents.last_mut() {
                    Some(e) if e.physical + e.len == block => e.len += 1,
                    _ => extents.push(Extent {
                        physical: block,
                        len: 1,
                    }),
                }
                match out.last_mut() {
                    Some(e) if e.physical + e.len == block => e.len += 1,
                    _ => out.push(Extent {
                        physical: block,
                        len: 1,
                    }),
                }
                taken += 1;
                remaining -= 1;
            }

            if taken > 0 {
                plan.push(GroupPlan {
                    group: g,
                    bitmap: bm,
                    extents,
                    taken,
                });
            }
        }

        if remaining > 0 {
            return Err(LuksError::FilesystemFull);
        }
        Ok((plan, out))
    }

    /// Promise `count` free blocks to a writer, preferring blocks near `goal`.
    ///
    /// **No device I/O, and no counter changes.** The blocks are free on disk
    /// and stay free on disk until the writer publishes; what changes is that
    /// every allocator in this instance now treats them as taken. Dropping the
    /// reservation ([`release_blocks`](Self::release_blocks)) is therefore
    /// memory-only and cannot fail, which is what makes abandoning a transfer
    /// valid on a dead device.
    pub(super) fn reserve_blocks(&mut self, goal: u64, count: u64) -> Result<Vec<Extent>> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let (_plan, out) = self.plan_blocks(goal, count)?;
        for e in &out {
            if !self.reserved_blocks.insert(e.physical, e.len) {
                return Err(LuksError::CorruptFs("a block was reserved twice"));
            }
        }
        Ok(out)
    }

    /// Return reserved blocks to the pool. Memory only; infallible.
    pub(super) fn release_blocks(&mut self, extents: &[Extent]) {
        for e in extents {
            self.reserved_blocks.remove(e.physical, e.len);
        }
    }

    /// Claim `count` free blocks, preferring blocks near `goal`, and return
    /// them as contiguous runs.
    ///
    /// Locality is not cosmetic: a file whose blocks are scattered across the
    /// disk costs one SCSI round trip per fragment, and on this transport a
    /// round trip measured at 1.15 ms. Starting the search in `goal`'s own
    /// group is what keeps a file's extents contiguous.
    ///
    /// This is the *immediate* path: the bitmap and counters are written
    /// before it returns. File writers do not use it; they reserve
    /// ([`reserve_blocks`](Self::reserve_blocks)) and publish at finish. It
    /// still honours their reservations.
    ///
    /// On a mid-commit failure the blocks already written are lost to this
    /// signature; callers that must free them use
    /// [`alloc_blocks_tracked`](Self::alloc_blocks_tracked).
    pub fn alloc_blocks(&mut self, goal: u64, count: u64) -> Result<Vec<Extent>> {
        self.alloc_blocks_tracked(goal, count).map_err(|(e, _)| e)
    }

    /// [`alloc_blocks`](Self::alloc_blocks), but a commit failure also returns
    /// every run whose bitmap write landed, so the caller can free them
    /// instead of leaking them (W9).
    ///
    /// # Two phases
    ///
    /// The planning half only *reads*; not one byte reaches the device until
    /// the whole request is satisfied, so the out-of-space path returns having
    /// changed nothing. Then bitmap -> descriptor per group, superblock once.
    /// The in-memory counters are authoritative: freeing the returned runs
    /// writes them back out, so memory and disk converge.
    pub(super) fn alloc_blocks_tracked(
        &mut self,
        goal: u64,
        count: u64,
    ) -> std::result::Result<Vec<Extent>, (LuksError, Vec<Extent>)> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let seed = Seed::for_writing(&self.sb).map_err(|e| (e, Vec::new()))?;
        let (plan, out) = self.plan_blocks(goal, count).map_err(|e| (e, Vec::new()))?;

        // --- commit: bitmap -> descriptor, per group; superblock once ------
        //
        // An I/O error partway through leaves the groups already written
        // holding blocks nothing references — a leak, which `e2fsck` repairs
        // with a counter adjustment. The direction that loses data, a counter
        // ahead of its bitmap, is still impossible.
        let mut committed: Vec<Extent> = Vec::new();
        for gp in &plan {
            self.groups[gp.group].free_blocks_count -= gp.taken as u32;
            self.groups[gp.group].flags &= !crate::fs::ext4::BG_BLOCK_UNINIT;
            // Saturating: the superblock counter can already be stale-low on a
            // filesystem that saw an unclean unmount. Wrapping it to 2^64-1
            // and re-checksumming would *sign* the nonsense; sticking at 0
            // leaves an off-by-some counter, which is exactly what e2fsck
            // knows how to fix.
            let sb_before = self.sb.free_blocks_count;
            self.sb.free_blocks_count = sb_before.saturating_sub(gp.taken);
            if let Err(e) = self.write_bitmap(gp.group, Bitmap::Block, &gp.bitmap, &seed) {
                // The bitmap did not land, so this group's blocks are NOT
                // committed (freeing them would be a double free) and memory
                // goes back to what the disk still says.
                self.groups[gp.group].free_blocks_count += gp.taken as u32;
                self.sb.free_blocks_count = sb_before;
                return Err((e, committed));
            }
            // Committed only once its bitmap write has landed.
            committed.extend_from_slice(&gp.extents);
            self.write_group_desc(gp.group, &seed)
                .map_err(|e| (e, committed.clone()))?;
        }
        self.write_superblock(&seed).map_err(|e| (e, committed.clone()))?;

        Ok(out)
    }

    /// Verify, against the disk, that every reserved block is still free, and
    /// return each group's bitmap with the claim applied. **Reads only.**
    ///
    /// This is the assert-clear half of a publish. A reserved block is free on
    /// disk by construction; if its bit is set, somebody else claimed it
    /// behind this instance's back (another mount, an external tool, a
    /// corrupt bitmap) and writing on would double-allocate. That is refused
    /// as `CorruptFs`, with nothing written.
    pub(super) fn verify_block_claim(&self, extents: &[Extent]) -> Result<Vec<BlockClaim>> {
        let per = self.sb.blocks_per_group as u64;
        let first = self.sb.first_data_block as u64;
        let mut claims: Vec<BlockClaim> = Vec::new();

        for e in extents {
            for block in e.physical..e.physical + e.len {
                if block < first || block >= self.sb.blocks_count {
                    return Err(LuksError::CorruptFs("a reserved block lies outside the filesystem"));
                }
                let g = ((block - first) / per) as usize;
                let bit = ((block - first) % per) as usize;
                let slot = match claims.iter().position(|c| c.group == g) {
                    Some(i) => i,
                    None => {
                        let bitmap = self.read_bitmap(g, Bitmap::Block)?;
                        claims.push(BlockClaim { group: g, bitmap, taken: 0 });
                        claims.len() - 1
                    }
                };
                let c = &mut claims[slot];
                if test_bit(&c.bitmap, bit) {
                    return Err(LuksError::CorruptFs(
                        "a reserved block was claimed on disk by someone else",
                    ));
                }
                set_bit(&mut c.bitmap, bit);
                c.taken += 1;
            }
        }
        for c in &claims {
            // Not enough free blocks left in the descriptor to cover the claim
            // means the counter was changed under us.
            if self.groups[c.group].free_blocks_count < c.taken {
                return Err(LuksError::CorruptFs(
                    "a group's free count no longer covers its reserved blocks",
                ));
            }
        }
        Ok(claims)
    }

    /// Write a verified block claim: bitmap -> descriptor, per group. The
    /// superblock counter is written once by the caller
    /// ([`write_superblock_counters`](Self::write_superblock_counters)).
    pub(super) fn write_block_claim(&mut self, claims: &[BlockClaim], seed: &Seed) -> Result<()> {
        for c in claims {
            self.groups[c.group].free_blocks_count -= c.taken;
            self.groups[c.group].flags &= !crate::fs::ext4::BG_BLOCK_UNINIT;
            self.sb.free_blocks_count = self.sb.free_blocks_count.saturating_sub(c.taken as u64);
            self.write_bitmap(c.group, Bitmap::Block, &c.bitmap, seed)?;
            self.write_group_desc(c.group, seed)?;
        }
        Ok(())
    }

    /// Write the superblock's free counters from memory.
    pub(super) fn write_superblock_counters(&mut self, seed: &Seed) -> Result<()> {
        self.write_superblock(seed)
    }

    /// Claim one free block, preferring one near `goal`.
    pub fn alloc_block(&mut self, goal: u64) -> Result<u64> {
        let runs = self.alloc_blocks(goal, 1)?;
        runs.first()
            .map(|e| e.physical)
            .ok_or(LuksError::FilesystemFull)
    }

    /// Release a batch of previously allocated runs.
    ///
    /// Freeing a block that is already free is refused rather than silently
    /// accepted — it means the caller has lost track, and double-freeing is how
    /// two files come to share a block. Because the batch is planned in memory
    /// first, that check also catches a batch that names the same block twice,
    /// which the per-block version could not see.
    ///
    /// Like [`alloc_blocks`](Self::alloc_blocks), nothing is written until the
    /// whole batch validates.
    pub fn free_blocks(&mut self, extents: &[Extent]) -> Result<()> {
        if extents.is_empty() {
            return Ok(());
        }
        let seed = Seed::for_writing(&self.sb)?;
        let plan = self.plan_free_blocks(extents)?;
        for (g, bm, freed) in &plan {
            self.groups[*g].free_blocks_count += *freed;
            self.groups[*g].flags &= !crate::fs::ext4::BG_BLOCK_UNINIT;
            self.sb.free_blocks_count += *freed as u64;
            self.write_bitmap(*g, Bitmap::Block, bm, &seed)?;
            self.write_group_desc(*g, &seed)?;
        }
        self.write_superblock(&seed)
    }

    /// Everything [`free_blocks`](Self::free_blocks) can refuse, with nothing
    /// written: the write gate, every bound, every double free (also across
    /// the batch), and every bitmap read. A caller that is about to do
    /// something irreversible first (delete removes the name) runs this, so a
    /// refusal costs nothing.
    pub(super) fn check_free_blocks(&self, extents: &[Extent]) -> Result<()> {
        Seed::for_writing(&self.sb)?;
        self.plan_free_blocks(extents).map(|_| ())
    }

    /// The dirty bitmaps (group, bitmap, bits cleared) a free would write.
    fn plan_free_blocks(&self, extents: &[Extent]) -> Result<Vec<(usize, Vec<u8>, u32)>> {
        let per = self.sb.blocks_per_group as u64;
        let first = self.sb.first_data_block as u64;

        // group index, dirty bitmap, how many bits were cleared in it
        let mut plan: Vec<(usize, Vec<u8>, u32)> = Vec::new();

        for e in extents {
            for block in e.physical..e.physical + e.len {
                if block < first || block >= self.sb.blocks_count {
                    return Err(LuksError::CorruptFs("free of a block outside the filesystem"));
                }
                let g = ((block - first) / per) as usize;
                let bit = ((block - first) % per) as usize;

                let slot = match plan.iter().position(|(gi, _, _)| *gi == g) {
                    Some(i) => i,
                    None => {
                        let bm = self.read_bitmap(g, Bitmap::Block)?;
                        plan.push((g, bm, 0));
                        plan.len() - 1
                    }
                };

                if !test_bit(&plan[slot].1, bit) {
                    return Err(LuksError::CorruptFs("double free of a block"));
                }
                clear_bit(&mut plan[slot].1, bit);
                plan[slot].2 += 1;
            }
        }
        Ok(plan)
    }

    /// Release a single block previously allocated.
    pub fn free_block(&mut self, block: u64) -> Result<()> {
        self.free_blocks(&[Extent {
            physical: block,
            len: 1,
        }])
    }

    /// How many blocks this group actually contains. The last group is usually
    /// short, and its bitmap's trailing bits are set so they can never be
    /// allocated — but relying on that rather than on the real count would put
    /// the search one bad bitmap away from returning a block past the end.
    fn blocks_in_group(&self, group: usize) -> usize {
        let per = self.sb.blocks_per_group as u64;
        let first = self.sb.first_data_block as u64;
        let start = first + group as u64 * per;
        (self.sb.blocks_count - start).min(per) as usize
    }

    // --- inodes ------------------------------------------------------------

    /// Inodes promised to open writers inside `group`.
    fn reserved_inodes_in_group(&self, group: usize) -> usize {
        let per = self.sb.inodes_per_group as u64;
        let lo = group as u64 * per + 1;
        self.reserved_inodes.range(lo..lo + per).count()
    }

    /// Decide which inode to hand out, touching nothing: its group, its bit,
    /// and the group's bitmap exactly as it is on disk. Inodes reserved for an
    /// open writer count as used, and a group's allowance is its free count
    /// minus what is reserved in it.
    fn pick_inode(&self) -> Result<(usize, usize, Vec<u8>)> {
        let per = self.sb.inodes_per_group as usize;
        let first_ino = self.sb.first_ino as u64;

        for g in 0..self.groups.len() {
            let spare = (self.groups[g].free_inodes_count as usize)
                .saturating_sub(self.reserved_inodes_in_group(g));
            if spare == 0 {
                continue;
            }

            let bm = self.read_bitmap(g, Bitmap::Inode)?;
            let mut view = bm.clone();
            for &r in self.reserved_inodes.range(g as u64 * per as u64 + 1..(g as u64 + 1) * per as u64 + 1) {
                set_bit(&mut view, (r - 1) as usize - g * per);
            }

            let mut cursor = 0usize;
            while let Some(bit) = find_free_bit_from(&view, cursor, per) {
                let ino = (g * per + bit) as u64 + 1;
                // Inodes below s_first_ino are reserved — 2 is the root, 8 is
                // the journal. Handing one out corrupts the filesystem's own
                // metadata.
                if ino >= first_ino {
                    return Ok((g, bit, bm));
                }
                cursor = bit + 1;
            }
        }
        Err(LuksError::NoFreeInodes)
    }

    /// Promise one free inode to a writer. No device I/O; see
    /// [`reserve_blocks`](Self::reserve_blocks).
    pub(super) fn reserve_inode(&mut self) -> Result<u64> {
        let (g, bit, _bm) = self.pick_inode()?;
        let ino = (g * self.sb.inodes_per_group as usize + bit) as u64 + 1;
        if !self.reserved_inodes.insert(ino) {
            return Err(LuksError::CorruptFs("an inode was reserved twice"));
        }
        Ok(ino)
    }

    /// Return a reserved inode to the pool. Memory only; infallible.
    pub(super) fn release_inode(&mut self, ino: u64) {
        self.reserved_inodes.remove(&ino);
    }

    /// Apply a claim on inode bit `bit` of group `g` to the in-memory
    /// counters. Shared by the immediate and the deferred paths so the two
    /// cannot drift.
    fn account_inode_claim(&mut self, g: usize, bit: usize, is_dir: bool) {
        let per = self.sb.inodes_per_group as usize;
        self.groups[g].free_inodes_count -= 1;
        // Saturating for the same reason as the block counter above.
        self.sb.free_inodes_count = self.sb.free_inodes_count.saturating_sub(1);
        if is_dir {
            self.groups[g].used_dirs_count += 1;
        }

        // `itable_unused` counts never-used inodes at the *tail* of the
        // table, so it must shrink to exclude the one just taken. Leaving
        // it too large tells e2fsck it may skip an inode that is now live.
        let used_through = bit + 1;
        let unused = per.saturating_sub(used_through) as u32;
        if self.groups[g].itable_unused > unused {
            self.groups[g].itable_unused = unused;
        }
    }

    /// Claim one free inode.
    ///
    /// `is_dir` is not bookkeeping trivia: `bg_used_dirs_count` feeds the
    /// allocator's directory-spreading heuristic, and `e2fsck` checks it.
    ///
    /// The immediate path (directories, `create_file`): written before it
    /// returns. Honours reservations held for open file writers.
    pub fn alloc_inode(&mut self, is_dir: bool) -> Result<u64> {
        let seed = Seed::for_writing(&self.sb)?;
        let per = self.sb.inodes_per_group as usize;
        let (g, bit, mut bm) = self.pick_inode()?;
        let ino = (g * per + bit) as u64 + 1;

        set_bit(&mut bm, bit);
        self.account_inode_claim(g, bit, is_dir);

        // Erase the record before the bitmap claims the slot — the mirror
        // of the argument in `free_inode`, and for the same reason.
        //
        // The record is not written for real until `write_inode_record`,
        // which on this path runs only after every data block has crossed
        // the cable: minutes, on USB, with a file of any size. For that
        // whole window the bitmap says "in use" while the record still
        // describes whatever occupied the slot before — typically a
        // deleted file, with its extents still naming blocks. Interrupted
        // there, `e2fsck` finds a live-looking inode pointing at blocks
        // that are now somebody else's, and the file reads back as a
        // deleted file's plaintext under a new name. On a volume that
        // exists for confidentiality that is the failure that matters.
        //
        // Zeroed first, the same interruption leaves "in use, record
        // zeroed", which `e2fsck` resolves by marking the inode free. A
        // repair complaint instead of a disclosure. The reverse order
        // would reintroduce the window it is meant to close, so the write
        // goes before the commit, not after.
        let blank = vec![0u8; self.sb.inode_size as usize];
        let offset = self.inode_offset(ino)?;
        self.device.write_at(offset, &blank)?;

        self.commit(g, Bitmap::Inode, &bm, &seed)?;
        Ok(ino)
    }

    /// Verify, against the disk, that a reserved inode is still free, and
    /// return its group's bitmap with the claim applied. **Reads only.** The
    /// inode twin of [`verify_block_claim`](Self::verify_block_claim).
    pub(super) fn verify_inode_claim(&self, ino: u64) -> Result<InodeClaim> {
        let per = self.sb.inodes_per_group as u64;
        if ino < self.sb.first_ino as u64 || ino > self.sb.inodes_count as u64 {
            return Err(LuksError::BadInode(ino));
        }
        let group = ((ino - 1) / per) as usize;
        let bit = ((ino - 1) % per) as usize;
        if group >= self.groups.len() {
            return Err(LuksError::BadInode(ino));
        }
        let mut bitmap = self.read_bitmap(group, Bitmap::Inode)?;
        if test_bit(&bitmap, bit) {
            return Err(LuksError::CorruptFs(
                "a reserved inode was claimed on disk by someone else",
            ));
        }
        if self.groups[group].free_inodes_count == 0 {
            return Err(LuksError::CorruptFs(
                "a group's free inode count no longer covers its reserved inode",
            ));
        }
        set_bit(&mut bitmap, bit);
        Ok(InodeClaim { ino, group, bit, bitmap })
    }

    /// Write a verified inode claim: zero the record, then bitmap, then
    /// descriptor. The superblock counter is the caller's, written once for
    /// the whole publish.
    ///
    /// **Zero-before-claim is preserved**, and the argument is stronger than
    /// on the immediate path. Here the slot is only *ever* claimed inside a
    /// publish, milliseconds before the real record is written, not minutes
    /// (the old begin-time claim spanned the whole transfer). Within that
    /// window the order is still: the record is erased first, while the
    /// bitmap still says free, and only then does the bitmap claim the slot.
    /// An interruption at any point therefore leaves either "free, record
    /// zeroed" (harmless) or "in use, record zeroed" (`e2fsck` frees it) —
    /// never "in use, record describing a previous file's blocks".
    pub(super) fn write_inode_claim(&mut self, claim: &InodeClaim, seed: &Seed) -> Result<()> {
        let blank = vec![0u8; self.sb.inode_size as usize];
        let offset = self.inode_offset(claim.ino)?;
        self.device.write_at(offset, &blank)?;

        self.account_inode_claim(claim.group, claim.bit, false);
        self.groups[claim.group].flags &= !crate::fs::ext4::BG_INODE_UNINIT;
        self.write_bitmap(claim.group, Bitmap::Inode, &claim.bitmap, seed)?;
        self.write_group_desc(claim.group, seed)
    }

    // --- the dirty bracket -------------------------------------------------

    /// Set or clear the "cleanly unmounted" bit in the on-disk superblock,
    /// re-checksum it, write it and flush.
    ///
    /// Reads the superblock from the disk and patches `s_state` only, so it
    /// does not disturb the counters (those are `write_superblock`'s) and does
    /// not touch the in-memory `Superblock`: the in-memory copy keeps
    /// describing the filesystem as it was gated at mount, which is what lets
    /// the checksum seed and the write gate keep working inside the window.
    fn write_fs_state(&mut self, clean: bool) -> Result<()> {
        use crate::forensic::{record_ext4, Ext4Event};
        let r = self.write_fs_state_inner(clean);
        match &r {
            Ok(()) => record_ext4(if clean { Ext4Event::MarkClean } else { Ext4Event::MarkDirty }),
            Err(e) => record_ext4(Ext4Event::Error {
                stage: if clean { "mark_clean" } else { "mark_dirty" },
                code: e.forensic_code(),
            }),
        }
        r
    }

    fn write_fs_state_inner(&mut self, clean: bool) -> Result<()> {
        let seed = Seed::for_writing(&self.sb)?;
        let mut sb = vec![0u8; 1024];
        self.device.read_at(1024, &mut sb)?;
        let state = u16::from_le_bytes([sb[0x3A], sb[0x3B]]);
        let state = if clean {
            state | super::STATE_CLEANLY_UNMOUNTED
        } else {
            state & !super::STATE_CLEANLY_UNMOUNTED
        };
        put16(&mut sb, 0x3A, state);
        if let Some(csum) = seed.superblock(&sb) {
            put32(&mut sb, 0x3FC, csum);
        }
        self.device.write_at(1024, &sb)?;
        self.device.flush()
    }

    /// Re-read the superblock and descriptors from the device, so memory never
    /// runs ahead of the disk after a failed multi-step write (L2-O10).
    fn reload_metadata(&mut self) -> Result<()> {
        let mut raw = [0u8; 1024];
        self.device.read_at(1024, &mut raw)?;
        let sb = Superblock::parse(&raw)?;
        let groups = Self::read_group_descriptors(&self.device, &sb)?;
        self.sb = sb;
        self.groups = groups;
        Ok(())
    }

    /// Run `f` between "the filesystem is not clean" and "it is clean again".
    ///
    /// Marks the superblock not-cleanly-unmounted (and flushes) before `f`
    /// writes anything; if `f` succeeds, or fails having left the metadata
    /// consistent ([`Aborted::consistent`]), marks it clean again. Otherwise
    /// the disk is left **dirty**: the truth about a half-done operation, and
    /// the thing that makes the write gate refuse the filesystem until a
    /// Linux `e2fsck` has looked at it.
    ///
    /// After an inconsistent failure the in-memory counters are reloaded from
    /// disk. If that reload also fails the in-memory state is marked unclean so
    /// the gate refuses this session too, instead of writing on top of
    /// counters that may have run ahead of the disk.
    pub(super) fn dirty_window<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> std::result::Result<T, Aborted>,
    ) -> Result<T> {
        self.write_fs_state(false)?;
        match f(self) {
            Ok(v) => {
                self.write_fs_state(true)?;
                Ok(v)
            }
            Err(a) if a.consistent => {
                self.write_fs_state(true)?;
                Err(a.err)
            }
            Err(a) => {
                let cleanup_code = a.cleanup.as_ref().map(|e| e.forensic_code());
                self.cleanup_failure = a.cleanup;
                let resync_ok = self.resync_after_failure();
                crate::forensic::record_ext4(crate::forensic::Ext4Event::LeftDirty { cleanup_code, resync_ok });
                Err(a.err)
            }
        }
    }

    /// The failure of the last rollback that itself failed, if any. The
    /// operation's own (primary) error is always what is returned; this is
    /// the suppressed secondary.
    pub fn take_cleanup_failure(&mut self) -> Option<LuksError> {
        self.cleanup_failure.take()
    }

    /// Bring memory back in line with the disk after a failed window; on
    /// failure to do even that, make the session refuse further writes.
    ///
    /// Returns whether memory could be reloaded from disk.
    pub(super) fn resync_after_failure(&mut self) -> bool {
        if self.reload_metadata().is_err() {
            self.sb.state &= !super::STATE_CLEANLY_UNMOUNTED;
            false
        } else {
            true
        }
    }

    /// Everything [`free_inode`](Self::free_inode) can refuse before it
    /// writes: range, group, uninit, double free, record offset. Returns the
    /// group, the bit and the bitmap as read.
    fn plan_free_inode(&self, ino: u64) -> Result<(usize, usize, Vec<u8>)> {
        let per = self.sb.inodes_per_group as u64;

        if ino < self.sb.first_ino as u64 || ino > self.sb.inodes_count as u64 {
            return Err(LuksError::BadInode(ino));
        }
        let g = ((ino - 1) / per) as usize;
        let bit = ((ino - 1) % per) as usize;

        // `s_inodes_count` is not proof that the group exists: `groups` is
        // sized from the block count, and nothing on disk guarantees the two
        // agree. A superblock claiming more inodes than its groups can hold
        // would otherwise index past the end and panic — in a library whose
        // whole input is attacker- or failure-controlled metadata.
        // `inode_offset` has always got this right; this did not.
        if g >= self.groups.len() {
            return Err(LuksError::BadInode(ino));
        }

        // Same refusal as `free_block`'s: an uninitialised group's bitmap was
        // never written, so reading it yields noise — and if a noise bit
        // happened to be set, the "freed" bitmap written back would be noise
        // *with a valid checksum*. No inode from such a group can have been
        // allocated by this crate, so the request is itself evidence of a
        // caller that has lost track.
        if self.groups[g].inode_bitmap_uninit() {
            return Err(LuksError::CorruptFs(
                "free of an inode in an uninitialised group",
            ));
        }

        let bm = self.read_bitmap(g, Bitmap::Inode)?;
        if !test_bit(&bm, bit) {
            return Err(LuksError::CorruptFs("double free of an inode"));
        }
        // The record write below needs a valid offset; find out now.
        self.inode_offset(ino)?;
        Ok((g, bit, bm))

    }

    /// [`plan_free_inode`](Self::plan_free_inode) for callers that only want
    /// the verdict, with nothing written (see [`check_free_blocks`](Self::check_free_blocks)).
    pub(super) fn check_free_inode(&self, ino: u64) -> Result<()> {
        Seed::for_writing(&self.sb)?;
        self.plan_free_inode(ino).map(|_| ())
    }

    /// Release an inode. `was_dir` must match what it was allocated as, or the
    /// directory count drifts.
    ///
    /// `itable_unused` is intentionally *not* restored. It is a lower bound on
    /// how much of the inode table has ever been touched, and growing it again
    /// would tell `e2fsck` to skip inodes beyond this one that may still be in
    /// use. The kernel leaves it alone for the same reason.
    pub fn free_inode(&mut self, ino: u64, was_dir: bool) -> Result<()> {
        let seed = Seed::for_writing(&self.sb)?;
        let (g, bit, mut bm) = self.plan_free_inode(ino)?;

        // Clearing the bitmap bit does not, on its own, make an inode free.
        // `e2fsck` walks the inode *table* independently of the bitmap, and a
        // record still carrying a mode and a link count is reported as an
        // unattached inode however the bitmap reads — which is precisely what
        // a rolled-back `create_file` used to leave behind, with the freed
        // block showing up as a bitmap difference alongside it.
        //
        // Erased before the bitmap is committed, not after. Neither order has
        // a clean crash window, so this is the lesser of two: interrupted
        // here, the slot reads as "in use, record zeroed", which `e2fsck`
        // resolves by marking it free — it is losing an inode that was
        // already being discarded. The other order leaves a live-looking file
        // in a slot the bitmap has already offered to the next allocation.
        //
        // This is deliberately *not* conditional on how the inode was written.
        // The slot may hold a stale record from an earlier deletion by some
        // other implementation, and an allocation that rolls back must not
        // resurrect it.
        let blank = vec![0u8; self.sb.inode_size as usize];
        let offset = self.inode_offset(ino)?;
        self.device.write_at(offset, &blank)?;

        clear_bit(&mut bm, bit);
        self.groups[g].free_inodes_count += 1;
        self.sb.free_inodes_count += 1;
        if was_dir && self.groups[g].used_dirs_count > 0 {
            self.groups[g].used_dirs_count -= 1;
        }

        self.commit(g, Bitmap::Inode, &bm, &seed)
    }

    /// Flush the underlying device. A USB bridge acknowledges a write once it
    /// reaches its own buffer, so without this a "completed" allocation can
    /// still be lost to a pulled cable.
    pub fn flush(&self) -> Result<()> {
        let r = self.device.flush();
        crate::forensic::record_ext4(crate::forensic::Ext4Event::Flush { ok: r.is_ok() });
        r
    }

    /// The in-memory descriptor for one group, for tests and diagnostics.
    pub fn group(&self, index: usize) -> Option<&GroupDesc> {
        self.groups.get(index)
    }
}

// --- bit twiddling ---------------------------------------------------------
//
// ext4 bitmaps are little-endian *bit* order: bit 0 of byte 0 is the first
// block of the group. Using a big-endian convention here still round-trips
// perfectly in our own tests while disagreeing with the kernel on every byte.

fn test_bit(bm: &[u8], bit: usize) -> bool {
    bm[bit / 8] & (1 << (bit % 8)) != 0
}

fn set_bit(bm: &mut [u8], bit: usize) {
    bm[bit / 8] |= 1 << (bit % 8);
}

fn clear_bit(bm: &mut [u8], bit: usize) {
    bm[bit / 8] &= !(1 << (bit % 8));
}

/// Index of the first zero bit below `limit`, or `None`.
#[cfg(test)]
fn find_free_bit(bm: &[u8], limit: usize) -> Option<usize> {
    find_free_bit_from(bm, 0, limit)
}

/// Index of the first zero bit at or after `from` and below `limit`.
///
/// Resuming from a cursor is what makes claiming N blocks from one bitmap an
/// O(bitmap) scan rather than N scans from byte zero: the caller has just set
/// every bit it found below `from`, so re-examining them can only find them
/// taken. Bits below `from` inside the first byte are masked to 1 for exactly
/// that reason — they are taken *for this search*, whatever they are on disk.
fn find_free_bit_from(bm: &[u8], from: usize, limit: usize) -> Option<usize> {
    if from >= limit {
        return None;
    }
    let mut i = from / 8;
    let mut byte = *bm.get(i)? | (((1u16 << (from % 8)) - 1) as u8);
    loop {
        if byte != 0xFF {
            let index = i * 8 + byte.trailing_ones() as usize;
            return if index >= limit { None } else { Some(index) };
        }
        i += 1;
        byte = *bm.get(i)?;
    }
}

fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_free_bit_is_found_in_little_endian_order() {
        // 0b0000_0101 — bits 0 and 2 taken, so bit 1 is the first free one.
        // A big-endian reading would answer 5, and would be self-consistent.
        assert_eq!(find_free_bit(&[0b0000_0101], 8), Some(1));
        assert_eq!(find_free_bit(&[0xFF, 0b0000_0001], 16), Some(9));
        assert_eq!(find_free_bit(&[0xFF, 0xFF], 16), None);
    }

    #[test]
    fn the_limit_is_respected() {
        // A short final group: bits past the end must never be handed out even
        // though the bitmap byte says they are free.
        assert_eq!(find_free_bit(&[0b0000_1111], 4), None);
        assert_eq!(find_free_bit(&[0b0000_0111], 4), Some(3));
    }

    #[test]
    fn the_cursor_resumes_without_re_finding_what_it_already_took() {
        // Claiming N blocks from one bitmap walks it once with a cursor rather
        // than restarting at byte zero N times. Bits below the cursor are
        // masked to "taken" for the search — get that mask wrong and the batch
        // hands out the same block repeatedly, each one passing every
        // single-block test.
        let bm = [0b0000_0000u8, 0b0000_0000];

        assert_eq!(find_free_bit_from(&bm, 0, 16), Some(0));
        assert_eq!(find_free_bit_from(&bm, 1, 16), Some(1));
        assert_eq!(find_free_bit_from(&bm, 7, 16), Some(7));
        // Crossing a byte boundary: the mask applies to the first byte only.
        assert_eq!(find_free_bit_from(&bm, 8, 16), Some(8));

        // A cursor past a fully taken byte must skip it, not stall on it.
        let bm = [0xFF, 0b0000_0010];
        assert_eq!(find_free_bit_from(&bm, 0, 16), Some(8));
        assert_eq!(find_free_bit_from(&bm, 9, 16), Some(10));

        // The limit still binds when resuming, not only from zero.
        assert_eq!(find_free_bit_from(&[0u8], 4, 6), Some(4));
        assert_eq!(find_free_bit_from(&[0b0011_1111u8], 4, 6), None);

        // A cursor at or past the limit has nothing left to find.
        assert_eq!(find_free_bit_from(&[0u8], 8, 8), None);
    }

    #[test]
    fn set_and_clear_are_inverses() {
        let mut bm = [0u8; 4];
        set_bit(&mut bm, 13);
        assert!(test_bit(&bm, 13));
        assert!(!test_bit(&bm, 12));
        clear_bit(&mut bm, 13);
        assert_eq!(bm, [0u8; 4]);
    }
}
