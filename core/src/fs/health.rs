//! Read-only consistency check shared by the ext4 and btrfs checkers.
//!
//! An advisory signal: the kernel's own checkers (`e2fsck`, `btrfs check`)
//! stay the truth. A report carries kinds and counts only, never a path, a
//! name or a block number, so it can cross into Java and the UI as is.

use std::collections::BTreeMap;
use std::time::Duration;

/// The light the app shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    /// Every check passed.
    Clean,
    /// Only leak-class findings: space or inodes marked used with no owner,
    /// counters off, an intact file with no name, or a dirty flag. No data is
    /// at risk; e2fsck fixes it.
    Leaks,
    /// Something that can lose or mix data.
    Corrupt,
    /// Not judged: see [`UnknownReason`].
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownReason {
    Cancelled,
    /// A feature or on-disk shape the checker has not been proven against.
    Unsupported,
    /// The volume is too large for the checker's memory budget.
    TooLarge,
    /// Uncommitted writes are pending (btrfs batch open).
    Busy,
    /// An I/O error while reading.
    Io,
    /// The checker panicked. Always a bug in us.
    Internal,
}

/// What a check found. Each kind maps to exactly one severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingKind {
    // Leak class.
    MarkedDirty,
    LeakedBlocks,
    LeakedInodes,
    CounterMismatch,
    UnattachedInode,
    BitmapChecksum,
    GroupDescChecksum,
    LeakedExtentItems,
    /// btrfs: a superblock mirror that fails its checksum or magic while the
    /// primary is valid (torn mirror write, never-written mirror).
    MirrorInvalid,
    /// Fewer links than names point at the inode. Reads are fine, but deleting
    /// one name would free data another name still uses.
    LinkCountLow,
    // Corrupt class.
    BlockInUseButFree,
    InodeInUseButFree,
    DoubleOwnedBlock,
    DirentToFreeInode,
    InodeChecksum,
    ExtentChecksum,
    DirChecksum,
    BadStructure,
    MarkedErrors,
    EmptyLeaf,
    BlockGroupUsedMismatch,
    FreeSpaceOverlapsExtent,
    OverlappingExtents,
    OverlappingDevExtents,
    MirrorMismatch,
    UnownedTreeBlock,
}

impl FindingKind {
    pub fn is_corrupt(self) -> bool {
        use FindingKind::*;
        !matches!(
            self,
            MarkedDirty
                | LeakedBlocks
                | LeakedInodes
                | CounterMismatch
                | UnattachedInode
                | BitmapChecksum
                | GroupDescChecksum
                | LeakedExtentItems
                | MirrorInvalid
                | LinkCountLow
        )
    }

    /// Whether writing on top of this finding is safe: it only wastes space or
    /// skews a counter the writer does not trust. Everything else (including
    /// the rest of the leak class) refuses writes until a repair.
    pub fn write_safe(self) -> bool {
        use FindingKind::*;
        matches!(self, LeakedBlocks | LeakedInodes | CounterMismatch | MirrorInvalid)
    }

    /// Stable snake_case name used in the JNI JSON.
    pub fn name(self) -> &'static str {
        use FindingKind::*;
        match self {
            MarkedDirty => "marked_dirty",
            LeakedBlocks => "leaked_blocks",
            LeakedInodes => "leaked_inodes",
            CounterMismatch => "counter_mismatch",
            UnattachedInode => "unattached_inode",
            BitmapChecksum => "bitmap_checksum",
            GroupDescChecksum => "group_desc_checksum",
            LeakedExtentItems => "leaked_extent_items",
            LinkCountLow => "link_count_low",
            MirrorInvalid => "mirror_invalid",
            BlockInUseButFree => "block_in_use_but_free",
            InodeInUseButFree => "inode_in_use_but_free",
            DoubleOwnedBlock => "double_owned_block",
            DirentToFreeInode => "dirent_to_free_inode",
            InodeChecksum => "inode_checksum",
            ExtentChecksum => "extent_checksum",
            DirChecksum => "dir_checksum",
            BadStructure => "bad_structure",
            MarkedErrors => "marked_errors",
            EmptyLeaf => "empty_leaf",
            BlockGroupUsedMismatch => "block_group_used_mismatch",
            FreeSpaceOverlapsExtent => "free_space_overlaps_extent",
            OverlappingExtents => "overlapping_extents",
            OverlappingDevExtents => "overlapping_dev_extents",
            MirrorMismatch => "mirror_mismatch",
            UnownedTreeBlock => "unowned_tree_block",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthReport {
    pub verdict: Verdict,
    pub unknown: Option<UnknownReason>,
    /// Count per kind (blocks, inodes or items, whichever the kind counts).
    pub findings: BTreeMap<FindingKind, u64>,
    pub elapsed: Duration,
    /// Up to [`MAX_SAMPLES`] block or inode numbers per kind, for tests and
    /// debugging only. Never serialised across JNI.
    pub samples: BTreeMap<FindingKind, Vec<u64>>,
}

pub const MAX_SAMPLES: usize = 8;

/// Accumulates findings during a check and derives the verdict.
#[derive(Debug, Default)]
pub struct Findings {
    counts: BTreeMap<FindingKind, u64>,
    samples: BTreeMap<FindingKind, Vec<u64>>,
}

impl Findings {
    pub fn add(&mut self, kind: FindingKind, count: u64, sample: Option<u64>) {
        if count == 0 {
            return;
        }
        *self.counts.entry(kind).or_insert(0) += count;
        if let Some(s) = sample {
            let v = self.samples.entry(kind).or_default();
            if v.len() < MAX_SAMPLES {
                v.push(s);
            }
        }
    }

    pub fn finish(self, elapsed: Duration) -> HealthReport {
        let verdict = if self.counts.keys().any(|k| k.is_corrupt()) {
            Verdict::Corrupt
        } else if self.counts.is_empty() {
            Verdict::Clean
        } else {
            Verdict::Leaks
        };
        HealthReport { verdict, unknown: None, findings: self.counts, elapsed, samples: self.samples }
    }

    /// The check could not judge. Findings so far are kept for debugging but
    /// do not decide the verdict.
    pub fn unknown(self, reason: UnknownReason, elapsed: Duration) -> HealthReport {
        HealthReport {
            verdict: Verdict::Unknown,
            unknown: Some(reason),
            findings: self.counts,
            elapsed,
            samples: self.samples,
        }
    }
}

/// Polled by the checkers between units of work (an ext4 group, a batch of
/// btrfs nodes). Returning true stops the check with `Unknown{Cancelled}`.
pub trait CancelCheck {
    fn cancelled(&self) -> bool;
}

impl<F: Fn() -> bool> CancelCheck for F {
    fn cancelled(&self) -> bool {
        self()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_follows_the_worst_finding() {
        let mut f = Findings::default();
        assert_eq!(Findings::default().finish(Duration::ZERO).verdict, Verdict::Clean);
        f.add(FindingKind::LeakedBlocks, 1869, Some(122959));
        f.add(FindingKind::LeakedInodes, 1, Some(37));
        assert_eq!(f.finish(Duration::ZERO).verdict, Verdict::Leaks);
        let mut f = Findings::default();
        f.add(FindingKind::LeakedBlocks, 1, None);
        f.add(FindingKind::DoubleOwnedBlock, 1, None);
        assert_eq!(f.finish(Duration::ZERO).verdict, Verdict::Corrupt);
    }

    #[test]
    fn only_space_and_counter_leaks_are_write_safe() {
        assert!(FindingKind::LeakedBlocks.write_safe());
        assert!(!FindingKind::LinkCountLow.write_safe());
        assert!(!FindingKind::LinkCountLow.is_corrupt());
        assert!(!FindingKind::MarkedDirty.write_safe());
        assert!(!FindingKind::DoubleOwnedBlock.write_safe());
    }
}
