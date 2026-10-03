//! Device wrapper shared by the crash-safety and transfer-integrity tests.

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};

use luks_core::device::{FileDevice, ReadAt, WriteAt};
use luks_core::error::{LuksError, Result};

// ---------------------------------------------------------------------
// FailAfterNWrites: a device wrapper that lets exactly `budget` successful
// `write_at` calls through, then fails every subsequent device operation
// (writes *and* flushes — a vanished device answers nothing, not just
// writes).
// ---------------------------------------------------------------------

/// Wraps a real, file-backed [`FileDevice`] and simulates the device
/// disappearing (USB detach, power loss) after `budget` successful
/// `write_at` calls.
///
/// `write_at`/`flush` are `&self` in this crate's `WriteAt` trait — the
/// budget therefore has to be interior-mutable. A plain `AtomicUsize` is
/// used rather than `Cell` so the wrapper stays `Sync`-agnostic-safe; the
/// test harness is single-threaded, but nothing about the trait promises
/// that.
pub struct FailAfterNWrites {
    pub inner: FileDevice,
    /// Number of `write_at` calls still allowed to succeed.
    pub budget: AtomicUsize,
    /// Total `write_at` calls observed, successful or not — used by the
    /// probe test and by assertions that want to know how far the commit
    /// actually got.
    pub attempted: AtomicUsize,
}

impl FailAfterNWrites {
    pub fn new(inner: FileDevice, budget: usize) -> Self {
        Self {
            inner,
            budget: AtomicUsize::new(budget),
            attempted: AtomicUsize::new(0),
        }
    }

    pub fn simulated_failure() -> LuksError {
        LuksError::Io {
            path: "simulated-power-loss".to_string(),
            source: std::io::Error::other(
                "device vanished mid-commit (simulated by FailAfterNWrites)",
            ),
        }
    }
}

impl ReadAt for FailAfterNWrites {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_at(offset, buf)
    }

    fn len(&self) -> Option<u64> {
        self.inner.len()
    }
}

impl WriteAt for FailAfterNWrites {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        self.attempted.fetch_add(1, Ordering::SeqCst);
        // Decrement-if-nonzero. Every call after the budget hits zero fails,
        // including this one.
        let prev = self
            .budget
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |b| {
                if b == 0 {
                    None
                } else {
                    Some(b - 1)
                }
            });
        if prev.is_err() {
            return Err(Self::simulated_failure());
        }
        self.inner.write_at(offset, buf)
    }

    fn flush(&self) -> Result<()> {
        // A vanished device answers a flush the same way it answers a write:
        // not at all. Once the budget is exhausted the device is gone, full
        // stop, so `flush()` fails too rather than quietly succeeding on a
        // device that no longer exists.
        if self.budget.load(Ordering::SeqCst) == 0 {
            return Err(Self::simulated_failure());
        }
        self.inner.flush()
    }
}
