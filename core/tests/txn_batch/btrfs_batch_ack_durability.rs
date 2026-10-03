//! F0.4 (D2) and F0.5 (B1): btrfs batch durability and generation discipline.
//!
//! ```text
//! cargo test --features dangerous-write-support --test btrfs_batch_ack_durability
//! ```
//!
//! Both files are tiny (one block) and finished in one live `Batch`; the 16th
//! `finish_file` trips `should_commit` (accumulated_files >= 16), which is the
//! same `commit_and_rearm` path a >2 s tree import hits.
#![cfg(feature = "dangerous-write-support")]

#[path = "../common/mod.rs"]
mod common;

use common::fail_after::FailAfterNWrites;
use luks_core::device::FileDevice;
use luks_core::fs::btrfs::Btrfs;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

const SB_MIRROR0: u64 = 0x1_0000;
const SB_GENERATION_OFF: u64 = 72;

fn scratch(tag: &str) -> PathBuf {
    let src = format!("{}/../fixtures/btrfs/plain.img", env!("CARGO_MANIFEST_DIR"));
    let dst = std::env::temp_dir().join(format!("btrfs-ack-{tag}-{}.img", std::process::id()));
    std::fs::copy(src, &dst).expect("copy fixture");
    dst
}

fn dev(path: &PathBuf) -> FileDevice {
    let len = std::fs::metadata(path).expect("stat").len();
    FileDevice::open_writable(path.to_str().expect("utf8"), len).expect("open writable")
}

/// Raw on-disk generation of superblock mirror 0 (not parsed by our reader).
fn disk_generation(path: &PathBuf) -> u64 {
    use std::io::{Seek, SeekFrom};
    let mut f = std::fs::File::open(path).expect("open image");
    f.seek(SeekFrom::Start(SB_MIRROR0 + SB_GENERATION_OFF)).expect("seek");
    let mut b = [0u8; 8];
    f.read_exact(&mut b).expect("read generation");
    u64::from_le_bytes(b)
}

/// Write one small file through the streaming-writer API; returns finish_file's result.
macro_rules! small_file {
    ($fs:expr, $name:expr) => {{
        let mut w = $fs.begin_file(100).expect("begin_file");
        $fs.write_chunk(&mut w, &[0x5Au8; 100]).expect("write_chunk");
        $fs.finish_file(w, "/", $name)
    }};
}

/// F0.5 (B1): `Batch.generation` is not bumped on re-arm, so two commits from one
/// live batch write the same superblock generation with different roots. Correct:
/// every commit's generation is strictly greater than the previous one.
#[test]
fn f0_5_b1_two_commits_from_one_batch_have_increasing_generations() {
    let img = scratch("b1");
    let mut fs = Btrfs::mount(dev(&img)).expect("mount");
    let g0 = disk_generation(&img);
    let mut gens = vec![g0];
    for i in 0..32 {
        small_file!(fs, &format!("f{i:02}")).expect("finish_file");
        if (i + 1) % 16 == 0 {
            // The 16th and 32nd finish tripped should_commit and committed.
            gens.push(disk_generation(&img));
        }
    }
    drop(fs);
    assert_eq!(gens.len(), 3, "setup: expected two commits from the live batch");
    assert!(
        gens[0] < gens[1] && gens[1] < gens[2],
        "superblock generations must strictly increase across commits, got {gens:?}"
    );
}

/// F0.4 (D2): file A was acknowledged Ok but is only in the live batch. A later
/// commit fails. After the device recovers and the caller flushes, a remount
/// must show A, or the API must have reported that A was lost (here: the
/// follow-up `commit_active_batch` errors). Today the batch is dropped on the
/// failed commit, so A vanishes and every call afterwards says Ok.
#[test]
fn f0_4_d2_acked_file_survives_or_loss_is_reported_after_commit_failure() {
    const A: &str = "A_acked.bin";
    let img = scratch("d2");
    let retry = {
        let mut fs = Btrfs::mount(FailAfterNWrites::new(dev(&img), usize::MAX)).expect("mount");
        small_file!(fs, A).expect("A must be acknowledged Ok");
        for i in 1..15 {
            small_file!(fs, &format!("f{i:02}")).expect("filler finish_file");
        }
        // The 16th file's data lands, then the device dies before its commit.
        let mut w = fs.begin_file(100).expect("begin_file");
        fs.write_chunk(&mut w, &[0x5Au8; 100]).expect("write_chunk");
        fs.device_mut().budget.store(0, Ordering::SeqCst);
        let r = fs.finish_file(w, "/", "trigger");
        assert!(r.is_err(), "setup: the 16th finish_file must hit the failing commit");
        // Device recovers; the caller flushes whatever is still pending.
        fs.device_mut().budget.store(usize::MAX, Ordering::SeqCst);
        fs.commit_active_batch()
    };

    let (_, stdout, stderr) = common::oracle::verify_btrfs(&img);
    assert!(
        stdout.contains("mounted read-only, root contains"),
        "oracle did not reach its mount step:\n{stdout}\n{stderr}"
    );
    let a_exists = stdout.contains(A);
    assert!(
        a_exists || retry.is_err(),
        "D2: {A} was acknowledged Ok, is absent after remount, and nothing reported the loss \
         (commit_active_batch -> {retry:?}).\nkernel listing:\n{stdout}"
    );
}
