//! F0.1 / F0.2: an ext4 file transfer must never leave the image dirty, graded
//! by the kernel's `e2fsck -fn`, never by our own reader.
//!
//! * W1/W2 (E5): `begin_file` persists the inode + every block bitmap before
//!   any data lands, and the writer has no `Drop`. Dropping it mid-transfer
//!   leaves the reservation on disk: "Block bitmap differences" and "Inode
//!   bitmap differences", the signature of the E5 stick image.
//! * W1/W8/W9/D4: the same, swept over a device that dies after N writes.
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

/// F0.1 (W1/W2, E5): drop the writer with no finish/abandon. Correct behaviour
/// is a clean image; today the reservation leaks.
#[test]
#[ignore = "KNOWN-RED W1/W2: fixed by F2 (ext4 reservation not persisted at begin / owned by native layer)"]
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
/// held) is abandoned, as the real caller does.
fn scenario<D: ReadAt + WriteAt>(fs: &mut Ext4<D>) {
    let data = payload();
    let mut w = match fs.begin_file(data.len() as u64) {
        Ok(w) => w,
        Err(_) => return,
    };
    for chunk in data.chunks(BS * 20) {
        if fs.write_chunk(&mut w, chunk).is_err() {
            fs.abandon_file(w);
            return;
        }
    }
    // finish_file consumes the writer and cleans up after itself on error.
    if fs.finish_file(w, 2, "swept.bin", FileType::Regular).is_ok() {
        let _ = fs.flush();
    }
}

/// F0.2 (W1, W8, W9, D4): the device dies after N writes, for every N across
/// begin -> chunks -> finish (+ abandon after a mid-chunk failure). Every
/// resulting image must be clean.
#[test]
#[ignore = "KNOWN-RED W1/W8/W9/D4: fixed by F2 (ext4 transactional boundary + rollback that reports)"]
fn f0_2_fail_after_n_writes_sweep_image_always_clean() {
    let total = {
        let img = scratch("count");
        let counting = FailAfterNWrites::new(dev(&img), usize::MAX);
        let mut fs = Ext4::mount(counting).expect("mount");
        scenario(&mut fs);
        let n = fs.device_mut().attempted.load(Ordering::SeqCst);
        assert!(n > 5, "scenario made suspiciously few writes: {n}");
        n
    };
    let mut dirty = Vec::new();
    for n in 0..=total {
        let img = scratch(&format!("n{n}"));
        {
            let mut fs = Ext4::mount(FailAfterNWrites::new(dev(&img), n)).expect("mount");
            scenario(&mut fs);
        }
        if let Err(text) = grade(&img) {
            let first = text.lines().find(|l| l.contains("differences") || l.contains("Free")).unwrap_or("(see e2fsck)");
            dirty.push(format!("N={n}/{total}: {first}"));
        }
    }
    assert!(dirty.is_empty(), "{} of {} budgets left e2fsck dirty:\n{}", dirty.len(), total + 1, dirty.join("\n"));
}
