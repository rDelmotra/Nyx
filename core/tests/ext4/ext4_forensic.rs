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
