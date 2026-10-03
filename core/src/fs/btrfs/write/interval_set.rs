//! Disjoint sorted interval set for double-allocation tracking.
//!
//! Maintains a collection of non-overlapping, half-open ranges `[start, end)`
//! sorted by start address. Detects overlaps in $O(\log K)$ time.

#[derive(Debug, Clone, Default)]
pub struct IntervalSet {
    intervals: Vec<(u64, u64)>, // sorted strictly by start: interval[i].1 < interval[i+1].0
    /// Every range exactly as inserted (start -> end), NOT merged. `intervals`
    /// merges touching inserts, which loses the boundaries a release must
    /// match; `remove_exact` is checked against this map, so releasing a
    /// sub-range, a range spanning two inserts, or the same range twice is
    /// detected (W16).
    parts: std::collections::BTreeMap<u64, u64>,
}

/// Equality is over the covered address ranges only; how they were inserted is
/// bookkeeping for `remove_exact`.
impl PartialEq for IntervalSet {
    fn eq(&self, other: &Self) -> bool {
        self.intervals == other.intervals
    }
}
impl Eq for IntervalSet {}

impl IntervalSet {
    /// Create an empty interval set.
    pub fn new() -> Self {
        Self {
            intervals: Vec::new(),
            parts: std::collections::BTreeMap::new(),
        }
    }

    /// Check if `[start, start + length)` overlaps with any existing interval in the set.
    pub fn overlaps(&self, start: u64, length: u64) -> bool {
        if length == 0 {
            return false;
        }
        let end = match start.checked_add(length) {
            Some(e) => e,
            None => return true,
        };

        // Binary search for insertion point
        let idx = self.intervals.partition_point(|&(s, _)| s < end);
        if idx > 0 {
            let (_, prev_end) = self.intervals[idx - 1];
            if prev_end > start {
                return true;
            }
        }
        false
    }

    /// Insert `[start, start + length)` into the set.
    /// Returns `true` if inserted successfully without overlap,
    /// or `false` if an overlap was detected (double allocation).
    pub fn insert(&mut self, start: u64, length: u64) -> bool {
        if length == 0 {
            return true;
        }
        let end = match start.checked_add(length) {
            Some(e) => e,
            None => return false,
        };

        let idx = self.intervals.partition_point(|&(s, _)| s < end);
        if idx > 0 {
            let (_, prev_end) = self.intervals[idx - 1];
            if prev_end > start {
                return false; // Overlap detected!
            }
        }

        // Insert and merge with adjacent intervals if touching
        let mut new_start = start;
        let mut new_end = end;
        let mut remove_start = idx;
        let mut remove_end = idx;

        // Check merge with previous interval if touching
        if idx > 0 && self.intervals[idx - 1].1 == start {
            new_start = self.intervals[idx - 1].0;
            remove_start = idx - 1;
        }

        // Check merge with next interval if touching
        if idx < self.intervals.len() && self.intervals[idx].0 == end {
            new_end = self.intervals[idx].1;
            remove_end = idx + 1;
        }

        self.intervals
            .splice(remove_start..remove_end, std::iter::once((new_start, new_end)));
        self.parts.insert(start, end);
        true
    }

    /// Remove an interval `[start, start + length)` from the set (e.g. during rollback).
    pub fn remove(&mut self, start: u64, length: u64) {
        if length == 0 {
            return;
        }
        let end = match start.checked_add(length) {
            Some(e) => e,
            None => return,
        };

        // O(log n + k) in both structures: binary-search the affected window
        // and splice, never rebuild (a rebuild per release made releasing N
        // runs O(N^2): 375 s for 32k runs, measured 2026-10-04).
        let lo = self.intervals.partition_point(|&(_, e)| e <= start);
        let hi = self.intervals.partition_point(|&(s, _)| s < end);
        if lo < hi {
            let mut pieces = Vec::with_capacity(2);
            let (first_s, _) = self.intervals[lo];
            let (_, last_e) = self.intervals[hi - 1];
            if first_s < start {
                pieces.push((first_s, start));
            }
            if last_e > end {
                pieces.push((end, last_e));
            }
            self.intervals.splice(lo..hi, pieces);
        }

        // `parts` is disjoint and sorted, so ends are sorted too: walk back from
        // the last part starting before `end` while it still reaches past `start`.
        let hit: Vec<(u64, u64)> = self
            .parts
            .range(..end)
            .rev()
            .take_while(|&(_, &e)| e > start)
            .map(|(&s, &e)| (s, e))
            .collect();
        for (s, e) in hit {
            self.parts.remove(&s);
            if s < start {
                self.parts.insert(s, start);
            }
            if e > end {
                self.parts.insert(end, e);
            }
        }
    }

    /// Check if `[start, start + length)` is wholly enclosed within a single interval in the set.
    pub fn contains_range(&self, start: u64, length: u64) -> bool {
        if length == 0 {
            return true;
        }
        let end = match start.checked_add(length) {
            Some(e) => e,
            None => return false,
        };

        // Binary search for the rightmost interval whose start <= start
        let idx = self.intervals.partition_point(|&(s, _)| s <= start);
        if idx > 0 {
            let (prev_start, prev_end) = self.intervals[idx - 1];
            if prev_start <= start && prev_end >= end {
                return true;
            }
        }
        false
    }

    /// Remove `[start, start + length)`, which must be EXACTLY a range that was
    /// passed to `insert` and not yet removed.
    /// Returns `false` (and changes nothing) for a never-inserted range, a
    /// strict sub-range, a range spanning several inserts, or a second release
    /// of the same range. (Touching inserts merge in `intervals()`, so
    /// coverage alone cannot tell these apart; `parts` can.)
    pub fn remove_exact(&mut self, start: u64, length: u64) -> bool {
        if length == 0 {
            return true;
        }
        let Some(end) = start.checked_add(length) else {
            return false;
        };
        if self.parts.get(&start) != Some(&end) {
            return false;
        }
        self.remove(start, length);
        true
    }

    /// Number of disjoint intervals in the set.
    pub fn len(&self) -> usize {
        self.intervals.len()
    }

    /// Whether the interval set is empty.
    pub fn is_empty(&self) -> bool {
        self.intervals.is_empty()
    }

    /// How many units of `[start, end)` are covered by the set.
    pub fn covered_within(&self, start: u64, end: u64) -> u64 {
        if start >= end {
            return 0;
        }
        let first = self.intervals.partition_point(|&(_, e)| e <= start);
        self.intervals[first..]
            .iter()
            .take_while(|&&(s, _)| s < end)
            .map(|&(s, e)| e.min(end) - s.max(start))
            .sum()
    }

    /// Clear all intervals in the set.
    pub fn clear(&mut self) {
        self.intervals.clear();
        self.parts.clear();
    }

    /// Borrow the sorted disjoint intervals as `[(start, end)]`.
    pub fn intervals(&self) -> &[(u64, u64)] {
        &self.intervals
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interval_set_covered_within() {
        let mut set = IntervalSet::new();
        assert!(set.insert(10, 10)); // [10, 20)
        assert!(set.insert(30, 5)); // [30, 35)
        assert_eq!(set.covered_within(0, 100), 15);
        assert_eq!(set.covered_within(15, 32), 5 + 2);
        assert_eq!(set.covered_within(20, 30), 0);
        assert_eq!(set.covered_within(34, 34), 0);
        assert_eq!(set.covered_within(0, 11), 1);
    }

    #[test]
    fn test_interval_set_insert_and_merge() {
        let mut set = IntervalSet::new();
        assert!(set.insert(100, 50)); // [100, 150)
        assert_eq!(set.intervals(), &[(100, 150)]);

        assert!(set.insert(200, 50)); // [200, 250)
        assert_eq!(set.intervals(), &[(100, 150), (200, 250)]);

        // Merge adjacent touching interval [150, 200)
        assert!(set.insert(150, 50));
        assert_eq!(set.intervals(), &[(100, 250)]);
    }

    #[test]
    fn test_interval_set_detects_overlap() {
        let mut set = IntervalSet::new();
        assert!(set.insert(1000, 500)); // [1000, 1500)

        // Exact match overlap
        assert!(!set.insert(1000, 500));

        // Partial overlap left
        assert!(!set.insert(800, 300)); // [800, 1100) overlaps with [1000, 1500)

        // Partial overlap right
        assert!(!set.insert(1400, 200)); // [1400, 1600) overlaps with [1000, 1500)

        // Enclosed overlap
        assert!(!set.insert(1100, 100)); // [1100, 1200)

        // Enclosing overlap
        assert!(!set.insert(500, 2000)); // [500, 2500)

        // Disjoint does not overlap
        assert!(set.insert(500, 400)); // [500, 900)
        assert!(set.insert(1600, 400)); // [1600, 2000)
        assert_eq!(set.intervals(), &[(500, 900), (1000, 1500), (1600, 2000)]);
    }

    #[test]
    fn test_interval_set_remove() {
        let mut set = IntervalSet::new();
        set.insert(100, 200); // [100, 300)
        set.remove(150, 50); // remove [150, 200)
        assert_eq!(set.intervals(), &[(100, 150), (200, 300)]);
    }

    #[test]
    fn test_interval_set_contains_range() {
        let mut set = IntervalSet::new();
        assert!(set.contains_range(100, 0)); // length 0 is trivially contained
        assert!(!set.contains_range(100, 50)); // empty set

        set.insert(1000, 500); // [1000, 1500)
        assert!(set.contains_range(1000, 500)); // exact match
        assert!(set.contains_range(1000, 100)); // subrange at start
        assert!(set.contains_range(1200, 100)); // subrange in middle
        assert!(set.contains_range(1400, 100)); // subrange at end
        assert!(set.contains_range(1000, 0)); // zero length

        // Outside / partial
        assert!(!set.contains_range(900, 200)); // starts before
        assert!(!set.contains_range(1400, 200)); // extends past end
        assert!(!set.contains_range(800, 1000)); // spans past both sides
        assert!(!set.contains_range(2000, 100)); // disjoint
        assert!(!set.contains_range(u64::MAX - 10, 20)); // overflow
    }

    #[test]
    fn test_interval_set_remove_exact() {
        let mut set = IntervalSet::new();
        set.insert(100, 100); // [100, 200)
        set.insert(300, 100); // [300, 400)

        // Non-existent, partial or overlapping ranges return false and mutate nothing.
        assert!(!set.remove_exact(50, 100));
        assert!(!set.remove_exact(150, 100));
        assert!(!set.remove_exact(200, 100));
        assert_eq!(set.intervals(), &[(100, 200), (300, 400)]);

        // W16: a strict sub-range of an inserted range is NOT an exact release.
        assert!(!set.remove_exact(120, 30));
        assert_eq!(set.intervals(), &[(100, 200), (300, 400)]);

        // Exact full-range removal succeeds, once.
        assert!(set.remove_exact(300, 100));
        assert!(!set.remove_exact(300, 100), "double release must be detected");
        assert_eq!(set.intervals(), &[(100, 200)]);
    }

    /// W16: adjacent inserts merge in `intervals()`, but each inserted range
    /// stays individually releasable exactly once, and a range spanning two
    /// inserts (or a sub-range of one) is rejected.
    #[test]
    fn test_interval_set_remove_exact_across_merged_ranges() {
        let mut set = IntervalSet::new();
        assert!(set.insert(100, 50));
        assert!(set.insert(150, 50));
        assert_eq!(set.intervals(), &[(100, 200)]);
        assert!(!set.remove_exact(100, 100), "spans two inserted ranges");
        assert!(!set.remove_exact(120, 10), "sub-range of an inserted range");
        assert!(set.remove_exact(100, 50));
        assert!(!set.remove_exact(100, 50), "double release of the first range");
        assert_eq!(set.intervals(), &[(150, 200)]);
        assert!(set.remove_exact(150, 50));
        assert!(set.is_empty());
    }

    /// H2: releasing N runs one by one must not be O(N^2). 32k adjacent runs is
    /// a 4 GiB stream written in 128 KiB steps; `abandon_file` releases each.
    #[test]
    fn test_interval_set_release_32k_runs_is_fast() {
        const N: u64 = 32 * 1024;
        const LEN: u64 = 128 * 1024;
        let mut set = IntervalSet::new();
        for i in 0..N {
            assert!(set.insert(i * LEN, LEN));
        }
        assert_eq!(set.intervals(), &[(0, N * LEN)], "adjacent inserts merge");
        let t = std::time::Instant::now();
        for i in 0..N {
            assert!(set.remove_exact(i * LEN, LEN), "run {i}");
        }
        let el = t.elapsed();
        eprintln!("release of {N} runs took {el:?}");
        assert!(set.is_empty());
        assert!(el < std::time::Duration::from_millis(500), "release took {el:?}");
    }

    #[test]
    fn test_interval_set_remove_splits_parts_and_intervals() {
        let mut set = IntervalSet::new();
        set.insert(100, 100);
        set.insert(200, 100); // merged [100,300), parts [100,200) [200,300)
        set.remove(150, 100); // [150,250) out
        assert_eq!(set.intervals(), &[(100, 150), (250, 300)]);
        assert!(set.remove_exact(100, 50), "left remainder is now exactly [100,150)");
        assert!(set.remove_exact(250, 50));
        assert!(set.is_empty());
    }
}
