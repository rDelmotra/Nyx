//! Drop guards for temp images that tests create under `std::env::temp_dir()`.
//!
//! A manual `remove_file` at the end of a test never runs when the test
//! panics, and KNOWN-RED and sweep tests panic by design. That leaked ~170 GiB
//! of images into `$TMPDIR`. These guards delete the file in `Drop`, which
//! runs during unwinding too.
//!
//! Escape hatch for debugging a failing image: set `LUKS_KEEP_SCRATCH=1`
//! (or call `preserve()`), and the file is left on disk.
//!
//! Standalone on purpose (no deps), so any test crate, including `luks_jni`'s,
//! can include it with `#[path = ".../temp_guard.rs"] mod temp_guard;`.

#![allow(dead_code)]

use std::ffi::OsStr;
use std::fmt;
use std::ops::Deref;
use std::path::{Path, PathBuf};

fn keep_env() -> bool {
    ["LUKS_KEEP_SCRATCH", "LUKS_KEEP_FAILED_SCRATCH"]
        .iter()
        .any(|k| std::env::var(k).map(|v| v == "1").unwrap_or(false))
}

fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Owns a temp file; deletes it on drop (including panic unwind).
/// Derefs to `PathBuf`, so `&guard` works wherever `&Path`/`&PathBuf` does.
#[derive(Debug)]
pub struct TempPath {
    path: PathBuf,
    keep: bool,
}

impl TempPath {
    /// Take ownership of `path`. The file need not exist yet.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), keep: keep_env() }
    }

    /// Leave the file on disk when this guard drops.
    pub fn preserve(&mut self) {
        self.keep = true;
    }
}

impl Deref for TempPath {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.path
    }
}

impl AsRef<Path> for TempPath {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<OsStr> for TempPath {
    fn as_ref(&self) -> &OsStr {
        self.path.as_os_str()
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        if !self.keep {
            remove(&self.path);
        }
    }
}

/// Same guard for helpers that hand paths around as `String`.
/// Derefs to `String`, so `&guard` coerces to `&str`.
#[derive(Debug)]
pub struct TempStr {
    path: String,
    keep: bool,
}

impl TempStr {
    pub fn new(path: impl Into<String>) -> Self {
        Self { path: path.into(), keep: keep_env() }
    }

    pub fn preserve(&mut self) {
        self.keep = true;
    }
}

impl Deref for TempStr {
    type Target = String;
    fn deref(&self) -> &String {
        &self.path
    }
}

impl AsRef<Path> for TempStr {
    fn as_ref(&self) -> &Path {
        Path::new(&self.path)
    }
}

impl AsRef<str> for TempStr {
    fn as_ref(&self) -> &str {
        &self.path
    }
}

impl fmt::Display for TempStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.path)
    }
}

impl AsRef<OsStr> for TempStr {
    fn as_ref(&self) -> &OsStr {
        OsStr::new(&self.path)
    }
}

impl Drop for TempStr {
    fn drop(&mut self) {
        if !self.keep {
            remove(Path::new(&self.path));
        }
    }
}
