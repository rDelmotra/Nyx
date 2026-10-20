//! F9: the ext4 write paths leave numeric forensic events (inode numbers,
//! byte counts, stage + code on failure) and nothing else.
//!
//! ```text
//! cargo test --features dangerous-write-support --test ext4_forensic
//! ```
#![cfg(feature = "dangerous-write-support")]

#[path = "../common/temp_guard.rs"]
mod temp_guard;
use temp_guard::TempStr;

#[path = "../common/fail_after.rs"]
mod fail_after;
use fail_after::FailAfterNWrites;

use luks_core::device::FileDevice;
use luks_core::forensic::{drain_notable, Ext4Event, ForensicEvent, ForensicRecord};
use luks_core::fs::ext4::Ext4;
use luks_core::fs::FileType;
use std::sync::Mutex;

/// The notable queue is process-global; tests in this binary run in parallel.
static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    match TEST_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn scratch(test: &str) -> TempStr {
    let src = format!("{}/../fixtures/ext4/big-4k.img", env!("CARGO_MANIFEST_DIR"));
    let dst = std::env::temp_dir().join(format!("ext4-forensic-{test}.img"));
    std::fs::copy(&src, &dst).expect("copy fixture");
    TempStr::new(dst.to_string_lossy().into_owned())
}

fn open(path: &str) -> Ext4<FileDevice> {
    let len = std::fs::metadata(path).expect("stat scratch").len();
    Ext4::mount(FileDevice::open_writable(path, len).expect("open writable")).expect("mount")
}

fn ext4_events(recs: Vec<ForensicRecord>) -> Vec<Ext4Event> {
    recs.into_iter()
        .filter_map(|r| match r.event {
            ForensicEvent::Ext4(e) => Some(e),
            _ => None,
        })
        .collect()
}

fn position(evs: &[Ext4Event], want: &Ext4Event) -> usize {
    evs.iter()
        .position(|e| e == want)
        .unwrap_or_else(|| panic!("missing {want:?} in {evs:?}"))
}

#[test]
fn begin_write_finish_records_sizes_and_the_real_inode() {
    let _g = lock();
    let img = scratch("finish");
    let mut fs = open(&img);
    drain_notable();

    let data = vec![0xA5u8; 10_000];
    let mut w = fs.begin_file(data.len() as u64).expect("begin");
    fs.write_chunk(&mut w, &data).expect("write");
    let ino = fs.finish_file(w, 2, "a.bin", FileType::Regular).expect("finish");

    let (recs, dropped) = drain_notable();
    assert_eq!(dropped, 0);
    let evs = ext4_events(recs);
    let begin = position(&evs, &Ext4Event::BeginFile { size: 10_000 });
    let finish = position(&evs, &Ext4Event::FinishFile { ino, bytes: 10_000 });
    let dirty = position(&evs, &Ext4Event::MarkDirty);
    let clean = position(&evs, &Ext4Event::MarkClean);
    assert!(begin < dirty && dirty < clean, "begin, then dirty, then clean: {evs:?}");
    assert!(clean <= finish, "FinishFile is recorded once the file is published: {evs:?}");
    assert!(evs.contains(&Ext4Event::Flush { ok: true }), "flush is recorded: {evs:?}");
    assert!(
        !evs.iter().any(|e| matches!(e, Ext4Event::Error { .. })),
        "no error on the happy path: {evs:?}"
    );
}

#[test]
fn streaming_begin_and_abandon_record_zero_size_and_bytes_written() {
    let _g = lock();
    let img = scratch("abandon");
    let mut fs = open(&img);
    drain_notable();

    let mut w = fs.begin_file_streaming().expect("begin streaming");
    fs.write_chunk(&mut w, &[1u8; 5000]).expect("write");
    fs.abandon_file(w);

    let evs = ext4_events(drain_notable().0);
    position(&evs, &Ext4Event::BeginFile { size: 0 });
    position(&evs, &Ext4Event::AbandonFile { bytes: 5000 });
    assert!(!evs.contains(&Ext4Event::MarkDirty), "abandon writes nothing: {evs:?}");
}

#[test]
fn create_mkdir_rename_delete_record_parent_and_ino() {
    let _g = lock();
    let img = scratch("tree");
    let mut fs = open(&img);
    drain_notable();

    let dir = u64::from(fs.create_directory(2, "d").expect("mkdir"));
    let ino = fs.create_file(dir, "f.txt", b"hello", FileType::Regular).expect("create");
    fs.rename_file(dir as u32, "f.txt", "g.txt").expect("rename");
    fs.delete_file("/d/g.txt").expect("delete");

    let evs = ext4_events(drain_notable().0);
    position(&evs, &Ext4Event::Mkdir { parent: 2, ino: dir });
    position(&evs, &Ext4Event::CreateFile { parent: dir, ino });
    position(&evs, &Ext4Event::Rename { from_parent: dir, to_parent: dir, ino });
    position(&evs, &Ext4Event::DeleteFile { parent: dir, ino });
    assert!(
        !evs.iter().any(|e| matches!(e, Ext4Event::Error { .. })),
        "no error on the happy path: {evs:?}"
    );
}

#[test]
fn failure_records_stage_and_code_but_no_name() {
    let _g = lock();
    let img = scratch("error");
    let mut fs = open(&img);
    fs.create_file(2, "dup.txt", b"x", FileType::Regular).expect("first create");
    drain_notable();

    let err = fs.create_file(2, "dup.txt", b"y", FileType::Regular).expect_err("duplicate must fail");
    let code = err.forensic_code();
    assert_eq!(code, 15, "AlreadyExists is wire code 15");

    let evs = ext4_events(drain_notable().0);
    position(&evs, &Ext4Event::Error { stage: "create_file", code });
    assert!(
        !evs.iter().any(|e| matches!(e, Ext4Event::CreateFile { .. })),
        "a refused create must not record success: {evs:?}"
    );
    // The rendered line holds no trace of the name.
    let json = luks_core::forensic::ring_json();
    assert!(!json.contains("dup"), "name leaked into ring json");
}

fn open_failing(path: &str) -> Ext4<FailAfterNWrites> {
    let len = std::fs::metadata(path).expect("stat scratch").len();
    let dev = FileDevice::open_writable(path, len).expect("open writable");
    Ext4::mount(FailAfterNWrites::new(dev, usize::MAX)).expect("mount")
}

fn set_budget(fs: &mut Ext4<FailAfterNWrites>, n: usize) {
    fs.device_mut().budget.store(n, std::sync::atomic::Ordering::SeqCst);
}

/// S5: a device that dies mid-window leaves the disk dirty. The log must say
/// so explicitly (LeftDirty), not make the reader infer it from a missing
/// MarkClean, and must carry the suppressed rollback's code and the resync
/// outcome.
#[test]
fn mid_window_device_failure_records_left_dirty_and_no_mark_clean() {
    let _g = lock();
    let img = scratch("leftdirty");
    let mut fs = open_failing(&img);
    let mut w = fs.begin_file(10_000).expect("begin");
    fs.write_chunk(&mut w, &[7u8; 10_000]).expect("write");
    drain_notable();

    // Let the dirty mark and a little more through, then the device vanishes.
    set_budget(&mut fs, 3);
    let err = fs.finish_file(w, 2, "x.bin", FileType::Regular).expect_err("device died mid-window");
    let cleanup = fs.take_cleanup_failure().map(|e| e.forensic_code());

    let evs = ext4_events(drain_notable().0);
    let dirty = position(&evs, &Ext4Event::MarkDirty);
    let left = evs
        .iter()
        .position(|e| matches!(e, Ext4Event::LeftDirty { .. }))
        .unwrap_or_else(|| panic!("no LeftDirty in {evs:?}"));
    let error = position(&evs, &Ext4Event::Error { stage: "finish_file", code: err.forensic_code() });
    assert!(dirty < left && left < error, "MarkDirty, LeftDirty, then Error: {evs:?}");
    assert_eq!(evs[left], Ext4Event::LeftDirty { cleanup_code: cleanup, resync_ok: true }, "{evs:?}");
    assert!(!evs.contains(&Ext4Event::MarkClean), "the fs is NOT clean: {evs:?}");
}

/// S5: a USB failure while streaming data is an error return of a public
/// entry point and must be recorded with its stage.
#[test]
fn write_chunk_failure_records_its_stage() {
    let _g = lock();
    let img = scratch("chunkerr");
    let mut fs = open_failing(&img);
    let mut w = fs.begin_file_streaming().expect("begin");
    drain_notable();

    set_budget(&mut fs, 0);
    let err = fs.write_chunk(&mut w, &[1u8; 20_000]).expect_err("device refuses data writes");
    let evs = ext4_events(drain_notable().0);
    position(&evs, &Ext4Event::Error { stage: "write_chunk", code: err.forensic_code() });
}
