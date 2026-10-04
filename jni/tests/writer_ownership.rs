//! F4 Part A red tests: the volume owns every open writer (INV-1), release
//! never depends on the fence, close reports the final commit, and a poisoned
//! device lock fences (T3).
//!
//! ```text
//! cargo test -p luks_jni --features dangerous-write-support --test writer_ownership
//! ```
//!
//! Disk state is graded by the kernel oracle after the volume is dropped; our
//! own reader only confirms presence of a file the kernel already called clean.
#![cfg(feature = "dangerous-write-support")]

#[path = "../../core/tests/common/temp_guard.rs"]
mod temp_guard;
use temp_guard::TempStr;

#[path = "../../core/tests/common/oracle.rs"]
mod oracle;

use luks_core::device::{FileDevice, ReadAt, WriteAt};
use luks_core::error::{LuksError, Result};
use luks_jni::bridge::{
    close_volume, close_writer_handle, code, drop_volume, error_code, into_raw, volume_ref, writer_ref, DeviceHandle,
    Payload, VolumeHandle, WriterError, WriterHandle,
};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

const PASSWORD: &[u8] = b"test";

fn scratch(fixture: &str, test: &str) -> TempStr {
    let src = format!("{}/../fixtures/disks/{fixture}", env!("CARGO_MANIFEST_DIR"));
    let dst = std::env::temp_dir().join(format!("luks-jni-own-{test}.img"));
    std::fs::copy(src, &dst).expect("copy fixture");
    TempStr::new(dst.to_string_lossy().into_owned())
}

/// A `FileDevice` with switches: fail writes, panic on read, count writes.
struct Probe {
    inner: FileDevice,
    fail_writes: Arc<AtomicBool>,
    panic_reads: Arc<AtomicBool>,
    writes: Arc<AtomicUsize>,
    attempts: Arc<AtomicUsize>,
    block_writes: Arc<AtomicBool>,
    blocked: Arc<AtomicUsize>,
}

#[derive(Clone)]
struct Switches {
    fail_writes: Arc<AtomicBool>,
    panic_reads: Arc<AtomicBool>,
    /// Successful writes.
    writes: Arc<AtomicUsize>,
    /// Every write_at call, successful or not.
    attempts: Arc<AtomicUsize>,
    /// While set, write_at parks (a slow drive).
    block_writes: Arc<AtomicBool>,
    /// Number of write_at calls that have parked.
    blocked: Arc<AtomicUsize>,
}

impl ReadAt for Probe {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        if self.panic_reads.load(Ordering::SeqCst) {
            panic!("probe: injected panic inside a device call");
        }
        self.inner.read_at(offset, buf)
    }
    fn len(&self) -> Option<u64> {
        self.inner.len()
    }
}

impl WriteAt for Probe {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.block_writes.load(Ordering::SeqCst) {
            self.blocked.fetch_add(1, Ordering::SeqCst);
            while self.block_writes.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(LuksError::UsbTransfer("probe: injected write failure".into()));
        }
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.write_at(offset, buf)
    }
    fn flush(&self) -> Result<()> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(LuksError::UsbTransfer("probe: injected flush failure".into()));
        }
        self.inner.flush()
    }
}

fn open_probe(path: &str) -> (DeviceHandle, Switches) {
    let len = std::fs::metadata(path).expect("stat").len();
    let sw = Switches {
        fail_writes: Arc::new(AtomicBool::new(false)),
        panic_reads: Arc::new(AtomicBool::new(false)),
        writes: Arc::new(AtomicUsize::new(0)),
        attempts: Arc::new(AtomicUsize::new(0)),
        block_writes: Arc::new(AtomicBool::new(false)),
        blocked: Arc::new(AtomicUsize::new(0)),
    };
    let dev = Probe {
        inner: FileDevice::open_writable(path, len).expect("open writable"),
        fail_writes: sw.fail_writes.clone(),
        panic_reads: sw.panic_reads.clone(),
        writes: sw.writes.clone(),
        attempts: sw.attempts.clone(),
        block_writes: sw.block_writes.clone(),
        blocked: sw.blocked.clone(),
    };
    let handle = DeviceHandle::new(dev, 512, len / 512, "TEST".into(), "PROBE".into()).expect("scan");
    (handle, sw)
}

fn unlock_handle(handle: &DeviceHandle) -> VolumeHandle {
    let offset = handle
        .table
        .luks_partitions()
        .next()
        .expect("a LUKS partition")
        .offset_bytes();
    handle.unlock(offset, PASSWORD).expect("unlock")
}

fn unlock(path: &str) -> (VolumeHandle, DeviceHandle, Switches) {
    let (dev, sw) = open_probe(path);
    let vol = unlock_handle(&dev);
    (vol, dev, sw)
}

fn assert_kernel_clean(path: &str, what: &str) {
    assert!(
        oracle::gate(),
        "oracle unreachable (ALLOW_NO_ORACLE set?): a graded test cannot pass without it"
    );
    let script = format!("{}/../tools/verify-image.sh", env!("CARGO_MANIFEST_DIR"));
    let out = Command::new("bash").arg(&script).arg(path).arg("test").output().expect("run verify-image");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(
        out.status.success() && text.contains("VERDICT: clean"),
        "{what}: the kernel found the image unclean:\n{text}"
    );
}

/// A5: closing a volume that has an open btrfs writer AND an acknowledged but
/// uncommitted file publishes the file. Today drop discards the batch.
#[test]
fn a5_drop_volume_with_open_btrfs_writer_commits_acked_file() {
    let path = scratch("gpt-luks-btrfs.img", "a5-commit");
    {
        let (vol, _dev, _sw) = unlock(&path);
        let vol_h = into_raw(Payload::Volume(vol));
        {
            let v = volume_ref(vol_h).expect("volume");
            let w = v.begin_file_streaming().expect("begin 1");
            v.write_file_chunk(w, b"acked payload").expect("chunk");
            v.finish_file(w, "/", "acked.bin").expect("finish: acknowledged, not yet committed");
            let open = v.begin_file(1 << 16).expect("begin 2");
            v.write_file_chunk(open, b"still open").expect("chunk 2");
            // Java abandons the writer without closing it: only the id exists.
            assert_eq!(v.open_writer_count(), 1, "vacuity: one writer left open");
            let _ = open;
        }
        drop_volume(vol_h);
    }
    assert_kernel_clean(&path, "close with open btrfs writer");
    let (vol, _dev, _sw) = unlock(&path);
    let got = vol.read_file("/acked.bin", 1024).expect("the acknowledged file must be on the medium after close");
    assert_eq!(got, b"acked payload");
}

/// A5 control: a fenced close must NOT commit (the batch is untrustworthy).
#[test]
fn a5_fenced_close_does_not_commit_btrfs() {
    let path = scratch("gpt-luks-btrfs.img", "a5-fenced");
    {
        let (vol, _dev, _sw) = unlock(&path);
        let w = vol.begin_file_streaming().expect("begin");
        vol.write_file_chunk(w, b"will be discarded").expect("chunk");
        vol.finish_file(w, "/", "fenced.bin").expect("finish");
        vol.fence_writes(LuksError::ScsiProtocol("injected fence"));
        assert!(vol.is_write_fenced(), "vacuity: fenced");
    }
    assert_kernel_clean(&path, "fenced close");
    let (vol, _dev, _sw) = unlock(&path);
    assert!(vol.read_file("/fenced.bin", 1024).is_err(), "a fenced session must not publish its batch");
}

/// A6/T3: a panic inside a device call poisons the device mutex; the volume's
/// write session must be fenced, not silently recovered. Reads keep working.
#[test]
fn a6_poisoned_device_lock_fences_writes() {
    let path = scratch("gpt-luks.img", "a6");
    let (vol, dev, sw) = unlock(&path);
    sw.panic_reads.store(true, Ordering::SeqCst);
    let mut buf = [0u8; 16];
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        use luks_core::device::ReadAt;
        dev.dev.read_at(0, &mut buf)
    }));
    assert!(r.is_err(), "vacuity: the probe must have panicked inside the device lock");
    sw.panic_reads.store(false, Ordering::SeqCst);

    let err = vol.write_file("/", "after.txt", b"x").expect_err("write after a poisoned device lock");
    assert_eq!(error_code(&err), code::WRITE_SESSION_FENCED, "got {err:?}");
    assert!(vol.is_write_fenced());
    vol.read_file("/proof.txt", 1024).expect("reads survive a fence");
}

fn begin_with(vol: &VolumeHandle, chunk: &[u8]) -> u64 {
    let w = vol.begin_file(1 << 16).expect("begin");
    vol.write_file_chunk(w, chunk).expect("chunk");
    w
}

/// A2/A3 on ext4: a fenced session still releases the writer on finish and abandon.
#[test]
fn a3_finish_on_fenced_session_releases_writer_ext4() {
    let path = scratch("gpt-luks.img", "a3-ext4");
    let (vol, _dev, _sw) = unlock(&path);
    let w = begin_with(&vol, b"partial");
    assert_eq!(vol.open_writer_count(), 1, "vacuity: one open writer");
    vol.fence_writes(LuksError::ScsiProtocol("injected fence"));
    let err = vol.finish_file(w, "/", "x.bin").expect_err("fenced finish must fail");
    assert!(matches!(err, WriterError::Luks(LuksError::WriteSessionFenced(_))), "got {err:?}");
    assert_eq!(vol.open_writer_count(), 0, "the failed finish must not leave the writer registered");
}

/// A3 on btrfs, plus: no WriterBusy afterwards (has_open_writer cleared) once
/// the session is usable again on a fresh volume is not possible (fence is
/// final), so assert the registry and that abandon of the same id is BadHandle.
#[test]
fn a3_finish_on_fenced_session_releases_writer_btrfs() {
    let path = scratch("gpt-luks-btrfs.img", "a3-btrfs");
    let (vol, _dev, _sw) = unlock(&path);
    let w = begin_with(&vol, b"partial");
    vol.fence_writes(LuksError::ScsiProtocol("injected fence"));
    let err = vol.finish_file(w, "/", "x.bin").expect_err("fenced finish must fail");
    assert!(matches!(err, WriterError::Luks(LuksError::WriteSessionFenced(_))), "got {err:?}");
    assert_eq!(vol.open_writer_count(), 0);
    assert!(matches!(vol.abandon_file(w), Err(WriterError::BadHandle(_))), "the id is spent");
}

/// A2: abandon on a fenced session releases (and succeeds), both filesystems.
#[test]
fn a2_abandon_on_fenced_session_releases_writer() {
    for (fixture, name) in [("gpt-luks.img", "a2-ext4"), ("gpt-luks-btrfs.img", "a2-btrfs")] {
        let path = scratch(fixture, name);
        let (vol, _dev, _sw) = unlock(&path);
        let w = begin_with(&vol, b"partial");
        vol.fence_writes(LuksError::ScsiProtocol("injected fence"));
        vol.abandon_file(w).unwrap_or_else(|e| panic!("{name}: abandon on a fenced session: {e}"));
        assert_eq!(vol.open_writer_count(), 0, "{name}");
    }
}

/// A5: explicit close releases every open writer (ext4 and btrfs) and refuses
/// new ones; a second close is a no-op.
#[test]
fn a5_close_releases_open_writers_and_refuses_new_ones() {
    for (fixture, name) in [("gpt-luks.img", "a5-ext4"), ("gpt-luks-btrfs.img", "a5b-btrfs")] {
        let path = scratch(fixture, name);
        {
            let (vol, _dev, _sw) = unlock(&path);
            let _w = begin_with(&vol, b"partial");
            assert_eq!(vol.open_writer_count(), 1, "{name}: vacuity");
            vol.close().unwrap_or_else(|e| panic!("{name}: close: {e}"));
            assert_eq!(vol.open_writer_count(), 0, "{name}");
            assert!(vol.begin_file(16).is_err(), "{name}: a closed volume must not hand out writers");
            vol.close().unwrap_or_else(|e| panic!("{name}: second close must be a no-op: {e}"));
        }
        assert_kernel_clean(&path, name);
    }
}

/// A5/M1: the final commit's failure is reported by close(), and a transport
/// failure fences the session, which discards the batch: nothing can publish
/// the unconfirmed file later, and the image stays clean.
#[test]
fn a5_close_reports_commit_failure_and_fences() {
    let path = scratch("gpt-luks-btrfs.img", "a5-fail");
    {
        let (vol, _dev, sw) = unlock(&path);
        let w = vol.begin_file_streaming().expect("begin");
        vol.write_file_chunk(w, b"lost with the failed close").expect("chunk");
        vol.finish_file(w, "/", "lost.bin").expect("finish (acknowledged, not committed)");
        sw.fail_writes.store(true, Ordering::SeqCst);
        let err = vol.close().expect_err("a failing final commit must be reported");
        assert_eq!(error_code(&err), code::TRANSPORT, "got {err:?}");
        assert!(vol.is_write_fenced(), "a transport failure in the final commit must fence");
        sw.fail_writes.store(false, Ordering::SeqCst);
        vol.close().expect("a second close is a no-op");
    }
    assert_kernel_clean(&path, "failed close");
    let (vol, _dev, _sw) = unlock(&path);
    assert!(vol.read_file("/lost.bin", 1024).is_err(), "a fenced close must not publish the batch later");
}

/// M1: a dead device + an open btrfs writer: close then drop make ONE commit
/// attempt in total; Drop must not re-commit after an explicit close.
#[test]
fn m1_close_then_drop_commits_at_most_once() {
    let path = scratch("gpt-luks-btrfs.img", "m1");
    let (vol, _dev, sw) = unlock(&path);
    let vol_h = into_raw(Payload::Volume(vol));
    {
        let v = volume_ref(vol_h).expect("volume");
        let w = v.begin_file_streaming().expect("begin");
        v.write_file_chunk(w, b"acked").expect("chunk");
        v.finish_file(w, "/", "m1.bin").expect("finish");
        let _open = begin_with(&v, b"open");
    }
    sw.fail_writes.store(true, Ordering::SeqCst);
    let before = sw.attempts.load(Ordering::SeqCst);
    close_volume(vol_h).expect_err("close on a dead device reports the failure");
    let after_close = sw.attempts.load(Ordering::SeqCst);
    assert!(after_close > before, "vacuity: close must have attempted a commit");
    // The handle is gone; make sure the Arc really dropped (Drop ran).
    assert!(volume_ref(vol_h).is_err());
    assert_eq!(sw.attempts.load(Ordering::SeqCst), after_close, "Drop re-committed after close (before={before} after_close={after_close})");
}

/// M1 on the plain Drop route: a dead device makes drop_volume try once, not more.
#[test]
fn m1_drop_alone_commits_at_most_once() {
    let path = scratch("gpt-luks-btrfs.img", "m1-drop");
    let (vol, dev, sw) = unlock(&path);
    let w = vol.begin_file_streaming().expect("begin");
    vol.write_file_chunk(w, b"acked").expect("chunk");
    vol.finish_file(w, "/", "m1d.bin").expect("finish");
    sw.fail_writes.store(true, Ordering::SeqCst);
    let before = sw.attempts.load(Ordering::SeqCst);
    drop(vol);
    let used = sw.attempts.load(Ordering::SeqCst) - before;
    assert_eq!(used, 1, "a dead device must see exactly one commit attempt from Drop");
    drop(dev);
}

/// H1: a volume's Drop (final commit) runs outside the registry lock, so a slow
/// drive on one volume does not block handle lookups for another.
#[test]
fn h1_slow_drop_does_not_block_other_handles() {
    let path_a = scratch("gpt-luks-btrfs.img", "h1-a");
    let path_b = scratch("gpt-luks.img", "h1-b");
    let (vol_a, _dev_a, sw_a) = unlock(&path_a);
    let (vol_b, _dev_b, _sw_b) = unlock(&path_b);
    let w = vol_a.begin_file_streaming().expect("begin");
    vol_a.write_file_chunk(w, b"acked").expect("chunk");
    vol_a.finish_file(w, "/", "h1.bin").expect("finish");
    let a_h = into_raw(Payload::Volume(vol_a));
    let b_h = into_raw(Payload::Volume(vol_b));

    sw_a.block_writes.store(true, Ordering::SeqCst);
    let dropper = std::thread::spawn(move || drop_volume(a_h));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while sw_a.blocked.load(Ordering::SeqCst) == 0 {
        assert!(std::time::Instant::now() < deadline, "vacuity: the drop never reached the device");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let v = volume_ref(b_h).map(|v| v.read_file("/proof.txt", 1024).map(|d| d.len()));
        tx.send(v.map(|r| r.is_ok())).expect("send");
    });
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(3));
    sw_a.block_writes.store(false, Ordering::SeqCst);
    dropper.join().expect("dropper");
    match outcome {
        Ok(Ok(true)) => {}
        other => panic!("another volume's lookup/read was blocked or failed behind a slow drop: {other:?}"),
    }
    drop_volume(b_h);
}

/// Low: write_file / delete_file / rename / create_directory refuse on a closed volume.
#[test]
fn closed_volume_refuses_every_mutator() {
    let path = scratch("gpt-luks.img", "closed");
    let (vol, _dev, _sw) = unlock(&path);
    vol.close().expect("close");
    assert!(vol.write_file("/", "n.txt", b"x").is_err(), "write_file");
    assert!(vol.delete_file("/proof.txt").is_err(), "delete_file");
    assert!(vol.rename("/", "proof.txt", "/", "p2.txt").is_err(), "rename");
    assert!(vol.create_directory("/", "d").is_err(), "create_directory");
    assert!(vol.begin_file(1).is_err(), "begin_file");
    vol.read_file("/proof.txt", 1024).expect("reads still work on a closed volume");
}

/// A6: a poisoned fs lock latches the fence, and close() then does not commit.
#[test]
fn a6_poisoned_fs_fences_and_close_does_not_commit() {
    let path = scratch("gpt-luks-btrfs.img", "a6-fs");
    {
        let (vol, _dev, _sw) = unlock(&path);
        vol.write_file("/", "pre.bin", b"x").expect("write");
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = vol.fs_for_writing().unwrap();
            panic!("simulated panic inside write transaction");
        }));
        assert!(r.is_err(), "vacuity: panicked under the fs lock");
        let err = vol.write_file("/", "post.bin", b"y").expect_err("poisoned");
        assert_eq!(error_code(&err), code::MUTEX_POISONED, "code stays distinguishable: {err:?}");
        assert!(vol.is_write_fenced(), "a poisoned fs latches the fence");
        vol.close().expect("close of a fenced session attempts no commit");
    }
    assert_kernel_clean(&path, "poisoned close");
}

/// A4/W10: closing a writer after its volume is gone is Ok and leaks nothing;
/// an unknown writer handle is BAD_HANDLE.
#[test]
fn a4_close_writer_after_volume_close_is_ok_and_leaks_nothing() {
    let path = scratch("gpt-luks.img", "a4");
    let (vol, _dev, _sw) = unlock(&path);
    let vol_h = into_raw(Payload::Volume(vol));
    let (writer_h, volume_id) = {
        let v = volume_ref(vol_h).expect("volume");
        let writer_id = v.begin_file(1 << 16).expect("begin");
        v.write_file_chunk(writer_id, b"partial").expect("chunk");
        (
            into_raw(Payload::Writer(WriterHandle { volume_handle: vol_h, volume_id: v.id, writer_id })),
            v.id,
        )
    };
    let _ = volume_id;
    close_volume(vol_h).expect("close volume with an open writer");
    assert!(volume_ref(vol_h).is_err(), "vacuity: the volume handle is gone");
    close_writer_handle(vol_h, writer_h).expect("close after the volume is gone must be Ok");
    assert!(writer_ref(writer_h).is_err(), "the global writer handle must be released");
    match close_writer_handle(vol_h, writer_h) {
        Err(WriterError::BadHandle(_)) => {}
        other => panic!("an unknown writer handle must be BAD_HANDLE, got {other:?}"),
    }
}

/// A4: a writer handle presented with a different volume is refused and kept.
#[test]
fn a4_close_writer_with_wrong_volume_is_refused() {
    let path = scratch("gpt-luks.img", "a4-wrong");
    let (vol, _dev, _sw) = unlock(&path);
    let vol_h = into_raw(Payload::Volume(vol));
    let v = volume_ref(vol_h).expect("volume");
    let writer_id = v.begin_file(1 << 16).expect("begin");
    let writer_h = into_raw(Payload::Writer(WriterHandle { volume_handle: vol_h, volume_id: v.id, writer_id }));
    assert!(matches!(close_writer_handle(vol_h + 1000, writer_h), Err(WriterError::BadHandle(_))));
    assert!(writer_ref(writer_h).is_ok(), "a refused close must not free the handle");
    close_writer_handle(vol_h, writer_h).expect("proper close");
    assert_eq!(v.open_writer_count(), 0);
    drop(v);
    close_volume(vol_h).expect("close");
}

/// A7: committing with nothing pending touches the device zero times.
#[test]
fn a7_commit_with_nothing_pending_writes_nothing() {
    for (fixture, name) in [("gpt-luks.img", "a7-ext4"), ("gpt-luks-btrfs.img", "a7-btrfs")] {
        let path = scratch(fixture, name);
        let (vol, _dev, sw) = unlock(&path);
        let before = sw.writes.load(Ordering::SeqCst);
        vol.commit_active_batch().expect("commit");
        vol.commit_active_batch().expect("commit again");
        assert_eq!(sw.writes.load(Ordering::SeqCst), before, "{name}: a no-op commit wrote to the device");
    }
}
