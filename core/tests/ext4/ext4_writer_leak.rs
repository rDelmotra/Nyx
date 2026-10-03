//! F0.1 / F0.2: an ext4 file transfer must never leave the image dirty, graded
//! by the kernel's `e2fsck -fn`, never by our own reader.
//!
//! * W1/W2 (E5): `begin_file` used to persist the inode + every block bitmap
//!   before any data landed, and the writer has no `Drop`. Since F2a the
//!   reservation is memory-only, so dropping the writer leaves the disk as it
//!   was.
//! * W1/W8/W9/D4, split by where the device dies: `f0_2a` before
//!   `finish_file` (must be e2fsck-clean and read clean), `f0_2b` inside its
//!   publish window (must read not-clean, and e2fsck may report only
//!   leak-class problems).
//!
//! ```text
//! cargo test --features dangerous-write-support --test ext4_writer_leak -- --include-ignored
//! ```
#![cfg(feature = "dangerous-write-support")]

#[path = "../common/mod.rs"]
mod common;

use common::fail_after::FailAfterNWrites;
use luks_core::device::{FileDevice, ReadAt, WriteAt};
use luks_core::fs::ext4::Ext4;
use luks_core::fs::FileType;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::Ordering;

const FIXTURE: &str = "big-4k.img";
const BLOCKS: usize = 64;
const BS: usize = 4096;

fn scratch(tag: &str) -> PathBuf {
    let src = format!("{}/../fixtures/ext4/{FIXTURE}", env!("CARGO_MANIFEST_DIR"));
    let dst = std::env::temp_dir().join(format!("ext4-leak-{tag}-{}.img", std::process::id()));
    std::fs::copy(src, &dst).expect("copy fixture");
    dst
}

fn dev(path: &PathBuf) -> FileDevice {
    let len = std::fs::metadata(path).expect("stat scratch").len();
    FileDevice::open_writable(path.to_str().expect("utf8 path"), len).expect("open writable")
}

/// Kernel verdict: `Ok(())` if e2fsck is clean, else its full output.
#[track_caller]
fn grade(img: &PathBuf) -> Result<(), String> {
    assert!(common::oracle::gate(), "oracle required: this test cannot pass without e2fsck");
    let script = format!("{}/../tools/verify-ext4.sh", env!("CARGO_MANIFEST_DIR"));
    let out = Command::new("bash").arg(&script).arg(img).output().expect("run verify-ext4.sh");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    if out.status.success() { Ok(()) } else { Err(text) }
}

fn payload() -> Vec<u8> {
    (0..BLOCKS * BS).map(|i| ((i * 31 + 7) % 251) as u8).collect()
}

/// begin_file(BLOCKS) -> write ~half in two chunks; yields the open writer.
/// (`FileWriter` is not publicly nameable, hence a macro.)
macro_rules! half_transfer {
    ($fs:expr) => {{
        let data = payload();
        let mut w = $fs.begin_file(data.len() as u64).expect("begin_file");
        let half = data.len() / 2;
        $fs.write_chunk(&mut w, &data[..half / 2]).expect("chunk 1");
        $fs.write_chunk(&mut w, &data[half / 2..half]).expect("chunk 2");
        w
    }};
}

/// F0.1 positive control: abandon_file after a half transfer is clean today.
#[test]
fn f0_1_control_abandon_after_half_transfer_is_clean() {
    let img = scratch("abandon");
    {
        let mut fs = Ext4::mount(dev(&img)).expect("mount");
        let w = half_transfer!(fs);
        fs.abandon_file(w);
        fs.flush().expect("flush");
    }
    if let Err(text) = grade(&img) {
        panic!("positive control FAILED (abandon_file leaves e2fsck dirty):\n{text}");
    }
}

/// F0.1 (W1/W2, E5): drop the writer with no finish/abandon. The reservation
/// is memory-only, so the image stays clean.
#[test]
fn f0_1_w1_w2_dropped_writer_leaves_image_clean() {
    let img = scratch("drop");
    {
        let mut fs = Ext4::mount(dev(&img)).expect("mount");
        let w = half_transfer!(fs);
        drop(w);
        drop(fs);
    }
    if let Err(text) = grade(&img) {
        let e5 = text.contains("Block bitmap differences") && text.contains("Inode bitmap differences");
        assert!(e5, "red for the WRONG reason, expected the E5 signature (block + inode bitmap differences):\n{text}");
        panic!("E5 signature reproduced (block bitmap + inode bitmap differences):\n{text}");
    }
}

/// Full begin -> chunks -> finish scenario. On any error the writer (if still
/// held) is abandoned, as the real caller does. Returns how many device writes
/// had been attempted when `finish_file` was called, or `None` if the scenario
/// never got that far.
fn scenario<D: ReadAt + WriteAt>(
    fs: &mut Ext4<D>,
    attempted: impl Fn(&mut Ext4<D>) -> usize,
) -> Option<usize> {
    let data = payload();
    let mut w = match fs.begin_file(data.len() as u64) {
        Ok(w) => w,
        Err(_) => return None,
    };
    for chunk in data.chunks(BS * 20) {
        if fs.write_chunk(&mut w, chunk).is_err() {
            fs.abandon_file(w);
            return None;
        }
    }
    let before_finish = attempted(fs);
    // finish_file consumes the writer and cleans up after itself on error.
    if fs.finish_file(w, 2, "swept.bin", FileType::Regular).is_ok() {
        // The final flush is the caller's; its failure is not a finish failure.
        if let Err(e) = fs.flush() {
            panic!("final flush failed on a device that never failed a write: {e}");
        }
    }
    Some(before_finish)
}

fn attempted_of(fs: &mut Ext4<FailAfterNWrites>) -> usize {
    fs.device_mut().attempted.load(Ordering::SeqCst)
}

const STATE_OFFSET: u64 = 1024 + 0x3A;

/// The raw `s_state` of an image (bit 0 = cleanly unmounted), read from the
/// file, not through the driver.
fn disk_state(img: &PathBuf) -> u16 {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(img).expect("open image");
    let mut b = [0u8; 2];
    f.read_exact_at(&mut b, STATE_OFFSET).expect("read s_state");
    u16::from_le_bytes(b)
}

/// e2fsck lines that are leak-class: a block or inode the bitmaps claim
/// wrongly *in use* (a `-` difference: the bitmap says used, nothing owns it),
/// counters that disagree, and the published inode `ino` unattached. None of
/// them can show a user another file's data, cross-link two files, or corrupt
/// a directory. Returns every line of `text` that is NOT one of those.
///
/// Stricter than "mentions a bitmap": any `+` difference (something IS using a
/// block or inode the bitmap calls free, so the next allocation would
/// double-allocate it) is rejected, and an unattached/ref-count line is
/// accepted only for `ino`, the inode this scenario publishes.
fn non_leak_lines(text: &str, ino: u64) -> Vec<String> {
    const OK_PREFIX: [&str; 5] = [
        "Pass ",
        "e2fsck ",
        "Free blocks count wrong",
        "Free inodes count wrong",
        "Connect to /lost+found",
    ];
    let own = |l: &str, lead: &str| {
        l.strip_prefix(lead)
            .is_some_and(|rest| rest.split(|c: char| !c.is_ascii_digit()).next() == Some(&ino.to_string()))
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| !OK_PREFIX.iter().any(|p| l.starts_with(p)))
        .filter(|l| {
            let bitmap = l.starts_with("Block bitmap differences") || l.starts_with("Inode bitmap differences");
            !(bitmap && !l.contains('+'))
        })
        .filter(|l| {
            !(own(l, "Unattached inode ")
                || own(l, "Unattached zero-length inode ")
                || (l.contains(" ref count is") && own(l, "Inode ")))
        })
        .filter(|l| !l.starts_with("Fix? no"))
        .filter(|l| !l.contains("WARNING: Filesystem still has errors"))
        .filter(|l| !l.contains(" files (") && !l.contains("VERDICT"))
        // The "-n" answers and the ssh wrapper's own exit report, not findings.
        .filter(|l| *l != "IGNORED." && !(l.contains("level=fatal") && l.contains("exit status")))
        .map(String::from)
        .collect()
}

/// Vacuity control for the classifier: it must flag the damage the dirty
/// window is NOT allowed to produce.
#[test]
fn f0_2_classifier_flags_non_leak_damage() {
    let leak = "Pass 5: Checking group summary information\n\
                Block bitmap differences:  -(1970--2033)\n\
                Inode bitmap differences:  -22\n\
                Free blocks count wrong for group #0 (1, counted=2).\n\
                Inode 22 ref count is 1, should be 0.\n\
                Unattached inode 22\n";
    assert!(non_leak_lines(leak, 22).is_empty(), "{:?}", non_leak_lines(leak, 22));
    for bad in [
        "Running additional passes to resolve blocks claimed by more than one inode...",
        "Multiply-claimed block(s) in inode 12: 1970",
        "Inode 22 has illegal block(s).  Clear? no",
        "Entry 'x' in / (2) has deleted/unused inode 22.  Clear? no",
        "Directory inode 2, block #0, offset 0: directory corrupted",
        // `+` differences: something uses what the bitmap calls free.
        "Block bitmap differences:  +(1970--2033)",
        "Block bitmap differences:  -5 +(1970--2033)",
        "Inode bitmap differences:  +22",
        // Another inode's unattached / ref-count line is not ours to excuse.
        "Unattached inode 99",
        "Unattached zero-length inode 99",
        "Inode 99 ref count is 1, should be 0.",
        "Unattached inode 220",
    ] {
        assert!(!non_leak_lines(bad, 22).is_empty(), "classifier missed: {bad}");
    }
}

fn counted_total(tag: &str) -> (usize, usize, u64) {
    let img = scratch(tag);
    let counting = FailAfterNWrites::new(dev(&img), usize::MAX);
    let mut fs = Ext4::mount(counting).expect("mount");
    let before_finish = scenario(&mut fs, attempted_of).expect("the unlimited scenario must reach finish");
    let n = fs.device_mut().attempted.load(Ordering::SeqCst);
    assert!(n > 5, "scenario made suspiciously few writes: {n}");
    assert!(before_finish > 0 && before_finish < n, "finish must add writes: {before_finish} of {n}");
    // The inode the scenario publishes, found once from the finished image.
    let ino = fs.resolve("/swept.bin").expect("the unlimited run publishes swept.bin").number;
    (before_finish, n, ino)
}

/// F0.2a (W1, W8, W9, D4): the device dies after N writes, for every N up to
/// the moment `finish_file` is called (begin, every data chunk, abandon after a
/// mid-chunk failure). Nothing about the file is on disk yet, so every image
/// must be clean to the kernel **and** read as cleanly unmounted.
#[test]
fn f0_2a_fail_before_publish_image_always_clean() {
    let (before_finish, total, _ino) = counted_total("count-a");
    let mut dirty = Vec::new();
    for n in 0..=before_finish {
        let img = scratch(&format!("a{n}"));
        {
            let mut fs = Ext4::mount(FailAfterNWrites::new(dev(&img), n)).expect("mount");
            scenario(&mut fs, attempted_of);
        }
        if let Err(text) = grade(&img) {
            let first = text.lines().find(|l| l.contains("differences") || l.contains("Free")).unwrap_or("(see e2fsck)");
            dirty.push(format!("N={n}/{total}: {first}"));
        }
        if disk_state(&img) & 1 == 0 {
            dirty.push(format!("N={n}/{total}: marked not-clean before publish started"));
        }
    }
    assert!(dirty.is_empty(), "{} of {} pre-publish budgets left the image dirty:\n{}", dirty.len(), before_finish + 1, dirty.join("\n"));
}

/// F0.2b: the device dies inside `finish_file`'s publish window. The image may
/// need `e2fsck`, but it must SAY so (not cleanly unmounted), and what e2fsck
/// finds must be leak-class only: never cross-linked blocks, another file's
/// data, or a damaged directory. Prints one line per boundary.
#[test]
fn f0_2b_fail_inside_publish_is_flagged_and_leak_class_only() {
    let (before_finish, total, ino) = counted_total("count-b");
    let mut violations = Vec::new();
    let mut flagged = 0;
    let mut clean_after_all = false;
    for n in before_finish + 1..=total {
        let img = scratch(&format!("b{n}"));
        {
            let mut fs = Ext4::mount(FailAfterNWrites::new(dev(&img), n)).expect("mount");
            scenario(&mut fs, attempted_of);
        }
        let marked_clean = disk_state(&img) & 1 == 1;
        match grade(&img) {
            Ok(()) => {
                println!("f0_2b N={n}/{total}: e2fsck clean, marked_clean={marked_clean}");
                if n == total {
                    clean_after_all = marked_clean;
                }
            }
            Err(text) => {
                let bad = non_leak_lines(&text, ino);
                println!(
                    "f0_2b N={n}/{total}: e2fsck dirty, marked_clean={marked_clean}, non-leak lines={}",
                    bad.len()
                );
                if marked_clean {
                    violations.push(format!("N={n}/{total}: e2fsck dirty but the image reads clean:\n{text}"));
                }
                if !bad.is_empty() {
                    violations.push(format!("N={n}/{total}: not leak-class: {bad:?}"));
                }
                if !marked_clean {
                    flagged += 1;
                }
            }
        }
    }
    assert!(clean_after_all, "the complete run must end clean to e2fsck and marked clean");
    assert!(flagged > 0, "vacuity: some publish boundary must leave a flagged, dirty image");
    assert!(violations.is_empty(), "{} boundaries violate the publish-window contract:\n{}", violations.len(), violations.join("\n"));
}
