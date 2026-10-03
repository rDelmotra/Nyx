//! F3 write-side regression tests (register: B4, B5, B7, N7, D2-retry, P-3).
//! Each is graded with the kernel oracle (`btrfs check` via verify-btrfs.sh).
//!
//! ```text
//! cargo test -p luks_core --features dangerous-write-support --test btrfs_f3_commit_semantics
//! ```
#![cfg(feature = "dangerous-write-support")]

#[path = "../common/mod.rs"]
mod common;

use common::fail_after::FailAfterNWrites;
use luks_core::device::FileDevice;
use luks_core::error::{LuksError, Result};
use luks_core::fs::btrfs::Btrfs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

fn scratch(tag: &str) -> PathBuf {
    let src = format!("{}/../fixtures/btrfs/plain.img", env!("CARGO_MANIFEST_DIR"));
    let dst = std::env::temp_dir().join(format!("btrfs-f3-{tag}-{}.img", std::process::id()));
    std::fs::copy(src, &dst).expect("copy fixture");
    dst
}

fn dev(path: &PathBuf) -> FileDevice {
    let len = std::fs::metadata(path).expect("stat").len();
    FileDevice::open_writable(path.to_str().expect("utf8"), len).expect("open writable")
}

macro_rules! small_file {
    ($fs:expr, $name:expr) => {{
        let mut w = $fs.begin_file(100).expect("begin_file");
        $fs.write_chunk(&mut w, &[0x5Au8; 100]).expect("write_chunk");
        $fs.finish_file(w, "/", $name)
    }};
}

/// Test images are ~140 MiB sparse copies; leaving them in $TMPDIR filled the host disk.
fn cleanup(img: &PathBuf) {
    std::fs::remove_file(img).expect("remove scratch image");
}

/// Kernel verdict: must be clean AND must have reached its mount step.
fn assert_oracle_clean(img: &PathBuf, what: &str) -> String {
    let (ok, stdout, stderr) = common::oracle::verify_btrfs(img);
    assert!(
        stdout.contains("mounted read-only, root contains"),
        "{what}: oracle did not reach its mount step:\n{stdout}\n{stderr}"
    );
    assert!(ok, "{what}: btrfs check/mount/scrub not clean:\n{stdout}\n{stderr}");
    stdout
}

// ---------------------------------------------------------------------------
// N7: abandon_file's commit error arm must not drop earlier acknowledged files.
// ---------------------------------------------------------------------------
#[test]
fn n7_abandon_commit_failure_keeps_acked_files() {
    const A: &str = "A_acked.bin";
    let img = scratch("n7");
    let retry = {
        let mut fs = Btrfs::mount(FailAfterNWrites::new(dev(&img), usize::MAX)).expect("mount");
        small_file!(fs, A).expect("A acked");
        let w = fs.begin_file(100).expect("begin_file");
        fs.device_mut().budget.store(0, Ordering::SeqCst);
        let r = fs.abandon_file(w);
        assert!(r.is_err(), "setup: abandon's commit must hit the dead device");
        fs.device_mut().budget.store(usize::MAX, Ordering::SeqCst);
        fs.commit_active_batch()
    };
    assert!(retry.is_ok(), "retry commit after recovery must succeed: {retry:?}");
    let listing = assert_oracle_clean(&img, "n7");
    assert!(listing.contains(A), "N7: acked {A} lost after failed abandon commit:\n{listing}");
    cleanup(&img);
}

// ---------------------------------------------------------------------------
// D2/N14/B3 retry safety: fail the commit after N device writes, recover the
// device, commit again. Every acknowledged file must be present and the kernel
// must grade the image clean. N sweeps: before any write, mid tree-blocks,
// just before the superblock, after SB mirror 0, and at the final flush.
// ---------------------------------------------------------------------------
#[test]
fn d2_retry_after_partial_commit_is_clean_and_complete() {
    const FILES: [&str; 3] = ["a0.bin", "a1.bin", "a2.bin"];
    // Dry run: how many writes does this commit issue?
    let k = {
        let img = scratch("d2k");
        let mut fs = Btrfs::mount(FailAfterNWrites::new(dev(&img), usize::MAX)).expect("mount");
        for f in FILES {
            small_file!(fs, f).expect("ack");
        }
        fs.device_mut().attempted.store(0, Ordering::SeqCst);
        fs.commit_active_batch().expect("dry-run commit");
        fs.device().attempted.load(Ordering::SeqCst)
    };
    assert!(k >= 6, "vacuity: the commit must issue several writes, got {k}");
    let mut points = vec![0, 1, k / 2, k - 4, k - 3, k - 2, k - 1, k];
    points.sort_unstable();
    points.dedup();
    for n in points {
        let img = scratch(&format!("d2n{n}"));
        let mut fs = Btrfs::mount(FailAfterNWrites::new(dev(&img), usize::MAX)).expect("mount");
        for f in FILES {
            small_file!(fs, f).expect("ack");
        }
        fs.device_mut().budget.store(n, Ordering::SeqCst);
        let first = fs.commit_active_batch();
        assert!(first.is_err(), "n={n}/{k}: commit with {n} writes allowed must fail");
        fs.device_mut().budget.store(usize::MAX, Ordering::SeqCst);
        fs.commit_active_batch()
            .unwrap_or_else(|e| panic!("n={n}/{k}: retry commit failed: {e}"));
        drop(fs);
        let listing = assert_oracle_clean(&img, &format!("n={n}/{k}"));
        for f in FILES {
            assert!(listing.contains(f), "n={n}/{k}: acked {f} missing after retry:\n{listing}");
        }
        cleanup(&img);
    }
}

// ---------------------------------------------------------------------------
// B4: the in-memory chunk map must not learn a chunk whose allocation commit
// failed.
// ---------------------------------------------------------------------------
#[test]
fn b4_failed_chunk_commit_leaves_chunk_map_unchanged() {
    let img = scratch("b4");
    let mut fs = Btrfs::mount(FailAfterNWrites::new(dev(&img), usize::MAX)).expect("mount");
    let before = fs.chunk_map().len();
    fs.device_mut().budget.store(0, Ordering::SeqCst);
    assert!(fs.allocate_data_chunk().is_err(), "setup: commit must fail");
    assert_eq!(fs.chunk_map().len(), before, "B4: phantom chunk left in the chunk map");
    // And it still works after recovery, with a clean image.
    fs.device_mut().budget.store(usize::MAX, Ordering::SeqCst);
    fs.allocate_data_chunk().expect("retry allocation");
    assert_eq!(fs.chunk_map().len(), before + 1);
    drop(fs);
    assert_oracle_clean(&img, "b4");
    cleanup(&img);
}

// ---------------------------------------------------------------------------
// B5: superblock dev_item.bytes_used must track the chunk tree's DEV_ITEM.
// ---------------------------------------------------------------------------
#[test]
fn b5_superblock_dev_item_bytes_used_matches_chunk_tree() {
    use std::io::{Read, Seek, SeekFrom};
    let img = scratch("b5");
    let (alloc_len, chunk_tree_used) = {
        let mut fs = Btrfs::mount(dev(&img)).expect("mount");
        let logical = fs.allocate_data_chunk().expect("allocate");
        let len = fs.chunk_map().chunks().iter().find(|c| c.logical == logical).unwrap().length;
        let mut used = None;
        let root = fs.superblock().chunk_root;
        fs.walk_tree(root, &mut |k, d| {
            // DEV_ITEM key type 216; bytes_used at +0x10.
            if k.item_type == 216 && d.len() >= 24 {
                used = Some(u64::from_le_bytes(d[0x10..0x18].try_into().unwrap()));
            }
            Ok(())
        })
        .expect("walk chunk tree");
        (len, used.expect("dev item in chunk tree"))
    };
    let mut f = std::fs::File::open(&img).unwrap();
    f.seek(SeekFrom::Start(0x1_0000 + 0xd9)).unwrap();
    let mut b = [0u8; 8];
    f.read_exact(&mut b).unwrap();
    let sb_used = u64::from_le_bytes(b);
    assert!(alloc_len > 0);
    assert_eq!(
        sb_used, chunk_tree_used,
        "B5: superblock dev_item.bytes_used {sb_used} != chunk tree DEV_ITEM {chunk_tree_used}"
    );
    assert_oracle_clean(&img, "b5");
    cleanup(&img);
}

// ---------------------------------------------------------------------------
// B7: a missing/unreadable csum tree or free-space tree must fail the write,
// never be skipped. The root tree's ROOT_ITEM for the tree is re-keyed (7 -> 8,
// 10 -> 11; order-preserving, node checksum recomputed) so the tree "does not
// exist" as far as `tree_root` is concerned while the superblock still
// advertises it.
// ---------------------------------------------------------------------------
fn rekey_root_item(img: &PathBuf, from: u64, to: u64) {
    use std::io::{Read, Seek, SeekFrom, Write};
    let (root, node_size, stripes) = {
        let fs = Btrfs::mount(dev(img)).expect("mount");
        let sb = fs.superblock();
        assert_eq!(sb.root_level, 0, "setup: root tree must be a single leaf");
        (sb.root, sb.node_size as usize, fs.chunk_map().map_all_stripes(sb.root).expect("map"))
    };
    assert!(!stripes.is_empty());
    let mut f = std::fs::OpenOptions::new().read(true).write(true).open(img).unwrap();
    let mut done = 0;
    for phys in stripes {
        let mut buf = vec![0u8; node_size];
        f.seek(SeekFrom::Start(phys)).unwrap();
        f.read_exact(&mut buf).unwrap();
        let nritems = u32::from_le_bytes(buf[96..100].try_into().unwrap()) as usize;
        let mut hit = false;
        for i in 0..nritems {
            let at = 101 + 25 * i;
            let oid = u64::from_le_bytes(buf[at..at + 8].try_into().unwrap());
            if oid == from && buf[at + 8] == 132 {
                buf[at..at + 8].copy_from_slice(&to.to_le_bytes());
                hit = true;
            }
        }
        assert!(hit, "setup: ROOT_ITEM for objectid {from} not in root leaf");
        let crc = luks_core::fs::btrfs::crc32c::crc32c(&buf[32..]);
        buf[..32].fill(0);
        buf[..4].copy_from_slice(&crc.to_le_bytes());
        f.seek(SeekFrom::Start(phys)).unwrap();
        f.write_all(&buf).unwrap();
        done += 1;
    }
    assert!(done >= 1);
}

#[test]
fn b7_missing_csum_tree_fails_the_write_instead_of_skipping_csums() {
    let img = scratch("b7csum");
    rekey_root_item(&img, 7, 8);
    let mut fs = Btrfs::mount(dev(&img)).expect("mount with csum root hidden");
    let r = small_file!(fs, "nocsum.bin");
    assert!(
        r.is_err(),
        "B7: finish_file reported Ok after skipping the csum-tree insert (data written without checksums)"
    );
    drop(fs);
    cleanup(&img);
}

#[test]
fn b7_missing_free_space_tree_fails_the_commit_instead_of_skipping_reconcile() {
    let img = scratch("b7fst");
    {
        let fs = Btrfs::mount(dev(&img)).expect("mount");
        assert_ne!(fs.superblock().compat_ro_flags & 1, 0, "setup: fixture must advertise the FST");
    }
    rekey_root_item(&img, 10, 11);
    let mut fs = Btrfs::mount(dev(&img)).expect("mount with FST root hidden");
    let r = small_file!(fs, "nofst.bin").and_then(|_| fs.commit_active_batch().map(|_| 0));
    assert!(
        r.is_err(),
        "B7: commit reported Ok after skipping free-space-tree reconciliation while compat_ro still advertises the FST"
    );
    drop(fs);
    cleanup(&img);
}

// ---------------------------------------------------------------------------
// P-3: physically adjacent streaming extents are merged.
// ---------------------------------------------------------------------------
#[test]
fn p3_streaming_extents_are_merged() {
    let img = scratch("p3");
    let ino = {
        let mut fs = Btrfs::mount(dev(&img)).expect("mount");
        let mut w = fs.begin_file_streaming().expect("begin streaming");
        for i in 0..8u8 {
            fs.write_chunk(&mut w, &vec![i; 128 * 1024]).expect("chunk");
        }
        let ino = fs.finish_file(w, "/", "stream.bin").expect("finish");
        fs.commit_active_batch().expect("commit");
        let ex = fs.file_extents(fs.fs_tree().bytenr, ino).expect("extents");
        let total: u64 = ex.iter().map(|e| e.num_bytes).sum();
        assert_eq!(total, 1024 * 1024);
        assert!(ex.len() <= 2, "P-3: 8 adjacent chunks produced {} extent items", ex.len());
        ino
    };
    let _ = ino;
    let listing = assert_oracle_clean(&img, "p3");
    assert!(listing.contains("stream.bin"));
    cleanup(&img);
}

// ---------------------------------------------------------------------------
// B1 (graded): 40 files through one live batch (two threshold commits plus the
// final one) must leave a kernel-clean image, and the re-armed batch must
// stamp sb.generation + 1.
// ---------------------------------------------------------------------------
#[test]
fn b1_rearmed_batch_stamps_next_generation_and_image_is_clean() {
    let img = scratch("b1g");
    {
        let mut fs = Btrfs::mount(dev(&img)).expect("mount");
        for i in 0..40 {
            small_file!(fs, &format!("g{i:02}")).expect("finish_file");
            if let Some(b) = fs.active_batch() {
                assert_eq!(
                    b.generation,
                    fs.superblock().generation + 1,
                    "after file {i}: live batch must stamp sb.generation + 1"
                );
            }
        }
        fs.commit_active_batch().expect("final commit");
    }
    assert_oracle_clean(&img, "b1g");
    // The oracle's listing is `head -40`; check presence with our own reader.
    let fs = Btrfs::mount(dev(&img)).expect("remount");
    for i in 0..40 {
        assert_eq!(fs.read_file(&format!("/g{i:02}")).expect("file present").len(), 100);
    }
    drop(fs);
    cleanup(&img);
}

// ---------------------------------------------------------------------------
// P-8 mechanism (characterisation, not a fix): `finish_file` returning Ok does
// not touch the medium. A short import (< 2 s, < 16 files) sits only in the
// batch; dropping the volume then loses it. 2026-09-08 log: import #1 (1.34 s,
// commit=0.02 s) vanished at the lock one second later; import #2 (3.95 s,
// crossed the 2 s `should_commit` threshold, commit=1.50 s) persisted. The
// driver exposes the state (`has_uncommitted_files`); the commit policy that
// closes it is F5.
// ---------------------------------------------------------------------------
#[test]
fn p8_ok_finish_is_not_durable_until_committed() {
    let img = scratch("p8");
    let before = {
        let fs = Btrfs::mount(dev(&img)).expect("mount");
        fs.superblock().generation
    };
    {
        let mut fs = Btrfs::mount(dev(&img)).expect("mount");
        small_file!(fs, "p8.bin").expect("finish_file");
        assert!(fs.active_batch().expect("batch").has_uncommitted_files());
        assert_eq!(fs.superblock().generation, before, "nothing committed yet");
        // Dropping `fs` here is what lock/teardown did.
    }
    let fs = Btrfs::mount(dev(&img)).expect("remount");
    assert_eq!(fs.superblock().generation, before, "uncommitted file must not be on disk");
    assert!(fs.read_file("/p8.bin").is_err(), "file must be absent after a drop without commit");
    drop(fs);
    cleanup(&img);
}
