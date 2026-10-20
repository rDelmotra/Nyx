//! F1.1 (register D3): a bridge that rejects SYNCHRONIZE CACHE must be visible
//! to the layer that talks to the user.
//!
//! ```text
//! cargo test -p luks_jni --features dangerous-write-support --test flush_support
//! ```
//!
//! The device under test is the real `ScsiBlockDevice` over the BOT mock from
//! `core/tests/common`, so the answer travels the whole way: SCSI sense ->
//! `flush_support()` -> `SharedDevice` -> info JSON. Nothing in this file
//! computes the expected value from the code under test.
#![cfg(feature = "dangerous-write-support")]

#[path = "../../core/tests/common/mod.rs"]
mod common;

use common::{fixture, MockUsbDrive};
use luks_core::device::WriteAt;
use luks_core::usb::ScsiBlockDevice;
use luks_jni::bridge::{DeviceHandle, VolumeHandle};
use serde_json::Value;

/// INVALID COMMAND OPERATION CODE: sense key 5, ASC 0x20, ASCQ 0.
const NO_SUCH_OPCODE: [u8; 3] = [0x5, 0x20, 0x00];

fn device_over(drive: MockUsbDrive) -> DeviceHandle {
    let blocks = drive.blocks();
    let scsi = ScsiBlockDevice::open(drive).expect("open mock drive");
    DeviceHandle::new(scsi, 512, blocks, "MOCK".into(), "DRIVE".into()).expect("scan")
}

fn unlock(handle: &DeviceHandle) -> VolumeHandle {
    let offset = handle
        .table
        .luks_partitions()
        .next()
        .expect("fixture has a LUKS partition")
        .offset_bytes();
    handle.unlock(offset, b"test").expect("unlock")
}

fn json(s: String) -> Value {
    serde_json::from_str(&s).expect("info json parses")
}

fn rejecting_both_forms() -> MockUsbDrive {
    MockUsbDrive::new(fixture("disks/gpt-luks.img"))
        .failing(0x35, NO_SUCH_OPCODE)
        .failing(0x91, NO_SUCH_OPCODE)
}

#[test]
fn a_bridge_that_rejects_synchronize_cache_is_reported_unsupported() {
    let dev = device_over(rejecting_both_forms());
    let vol = unlock(&dev);

    dev.dev.flush().expect("a missing flush command is warn+allow, not an error");

    let d = json(dev.info_json());
    assert_eq!(d["flushSupported"], Value::Bool(false), "device info: {d}");
    let v = json(vol.info_json());
    assert_eq!(v["flushSupported"], Value::Bool(false), "volume info: {v}");
}

/// The control: without it the test above would pass for a build that
/// reports `false` for everything.
#[test]
fn a_bridge_that_flushes_is_reported_supported() {
    let dev = device_over(MockUsbDrive::new(fixture("disks/gpt-luks.img")));
    let vol = unlock(&dev);

    dev.dev.flush().expect("flush");

    let d = json(dev.info_json());
    assert_eq!(d["flushSupported"], Value::Bool(true), "device info: {d}");
    let v = json(vol.info_json());
    assert_eq!(v["flushSupported"], Value::Bool(true), "volume info: {v}");
}

/// Nothing has asked the drive yet, so the honest answer is "unknown", not a
/// guess in either direction.
#[test]
fn before_any_flush_the_answer_is_unknown() {
    let dev = device_over(rejecting_both_forms());
    let d = json(dev.info_json());
    // Present-and-null, not merely absent: an absent key would also index to
    // Null, and the UI must be able to tell "unknown" from "not reported".
    assert_eq!(d.get("flushSupported"), Some(&Value::Null), "device info: {d}");
}

/// R4: the flush status must not wait behind device I/O. A flush is held inside
/// the device (and so inside the device mutex); `VolumeHandle::info_json` must
/// still return. (`DeviceHandle::info_json` is not covered: it already reads
/// the device to sniff plain partitions' filesystems, independent of this.)
#[test]
fn info_json_does_not_block_behind_a_device_in_the_middle_of_a_flush() {
    use luks_core::device::ReadAt;
    use std::sync::mpsc;
    use std::time::Duration;

    struct HoldFlush {
        inner: ScsiBlockDevice<MockUsbDrive>,
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
    }
    impl ReadAt for HoldFlush {
        fn read_at(&self, o: u64, b: &mut [u8]) -> luks_core::error::Result<()> {
            self.inner.read_at(o, b)
        }
        fn len(&self) -> Option<u64> {
            self.inner.len()
        }
    }
    impl WriteAt for HoldFlush {
        fn write_at(&self, o: u64, b: &[u8]) -> luks_core::error::Result<()> {
            self.inner.write_at(o, b)
        }
        fn flush(&self) -> luks_core::error::Result<()> {
            self.entered.send(()).expect("test thread alive");
            self.release.recv().expect("released");
            self.inner.flush()
        }
        fn flush_support(&self) -> Option<bool> {
            self.inner.flush_support()
        }
    }

    let drive = MockUsbDrive::new(fixture("disks/gpt-luks.img"));
    let blocks = drive.blocks();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let held = HoldFlush {
        inner: ScsiBlockDevice::open(drive).expect("open"),
        entered: entered_tx,
        release: release_rx,
    };
    let dev = DeviceHandle::new(held, 512, blocks, "M".into(), "D".into()).expect("scan");
    let vol = unlock(&dev);

    let shared = dev.dev.clone();
    let flusher = std::thread::spawn(move || shared.flush());
    entered_rx.recv_timeout(Duration::from_secs(5)).expect("flush entered the device");

    // The device lock is now held by the flusher. Run the info call on
    // another thread and require it to finish while the lock is still held.
    let (done_tx, done_rx) = mpsc::channel();
    let infos = std::thread::scope(|sc| {
        sc.spawn(|| {
            let v = vol.info_json();
            done_tx.send(()).expect("main alive");
            v
        });
        let finished = done_rx.recv_timeout(Duration::from_secs(2));
        // Let everything finish either way so a failure is a clean assert.
        release_tx.send(()).expect("flusher alive");
        finished
    });
    flusher.join().expect("flusher thread").expect("flush ok");
    infos.expect("info_json blocked behind the device lock");
}

// ---------------------------------------------------------------------------
// Write refusal when ordering cannot be guaranteed.
//
// Rule: writes are allowed only if SYNCHRONIZE CACHE works, or MODE SENSE
// reports the write cache disabled (write-through). Rejected flush plus WCE
// set or unknown -> refused before any byte is written; reads keep working.
// These run on the shared device layer that both ext4 and btrfs sit on, over a
// plain btrfs image (no LUKS fixture needed).
// ---------------------------------------------------------------------------

use luks_core::device::ReadAt;
use luks_core::error::LuksError;
use luks_core::fs::btrfs::Btrfs;
use luks_jni::bridge::{code, error_code, SharedDevice};

fn shared_over(drive: MockUsbDrive) -> SharedDevice {
    SharedDevice::new(ScsiBlockDevice::open(drive).expect("open mock drive"))
}

fn btrfs_drive() -> MockUsbDrive {
    MockUsbDrive::new(fixture("btrfs/states-4k.img"))
}

fn no_flush(d: MockUsbDrive) -> MockUsbDrive {
    d.failing(0x35, NO_SUCH_OPCODE).failing(0x91, NO_SUCH_OPCODE)
}

/// First 1 MiB at the superblock region, to prove nothing was written.
fn head(dev: &SharedDevice) -> Vec<u8> {
    let mut b = vec![0u8; 1 << 20];
    dev.read_at(0, &mut b).expect("read");
    b
}

fn assert_refused_and_untouched(dev: &SharedDevice) {
    let before = head(dev);
    let err = dev.ensure_writes_safe().expect_err("writes must be refused");
    assert!(matches!(err, LuksError::UnsafeWriteCache), "{err:?}");
    assert_eq!(error_code(&err), code::WRITES_UNSAFE);
    assert!(err.to_string().contains("cannot guarantee writes reach the disk"));
    // The backstop in write_at refuses too, whatever path got there.
    let w = dev.write_at(0x10000, &[0xAB; 4096]).expect_err("raw write refused");
    assert!(matches!(w, LuksError::UnsafeWriteCache), "{w:?}");
    assert_eq!(head(dev), before, "a refused write changed the device");
    // Reads continue, filesystem included.
    let fs = Btrfs::mount(dev.clone()).expect("reads still work");
    fs.statfs().expect("statfs");
}

#[test]
fn rejected_flush_with_unknown_cache_state_refuses_writes() {
    // The mock does not implement MODE SENSE here: WCE is Unknown.
    let dev = shared_over(no_flush(btrfs_drive()));
    assert_refused_and_untouched(&dev);
}

#[test]
fn rejected_flush_with_write_cache_enabled_refuses_writes() {
    let dev = shared_over(no_flush(btrfs_drive().with_write_cache(true)));
    assert_refused_and_untouched(&dev);
}

#[test]
fn rejected_flush_refuses_a_btrfs_write_before_anything_is_written() {
    let dev = shared_over(no_flush(btrfs_drive().with_write_cache(true)));
    let before = head(&dev);
    let mut fs = Btrfs::mount(dev.clone()).expect("mount");
    // No ensure_writes_safe call by the caller: the device itself refuses.
    let err = fs.create_file_with_data("/", "x", b"data").expect_err("refused");
    assert!(matches!(err, LuksError::UnsafeWriteCache), "{err:?}");
    assert_eq!(head(&dev), before);
}

#[test]
fn rejected_flush_with_write_cache_disabled_allows_writes() {
    let dev = shared_over(no_flush(btrfs_drive().with_write_cache(false)));
    dev.ensure_writes_safe().expect("write-through drive is safe without flush");
    let mut fs = Btrfs::mount(dev.clone()).expect("mount");
    fs.create_file_with_data("/", "wt.txt", b"write-through").expect("write allowed");
    let fs = Btrfs::mount(dev.clone()).expect("remount");
    assert_eq!(fs.read_file("/wt.txt").expect("read back"), b"write-through");
}

#[test]
fn a_working_flush_allows_writes_even_with_write_cache_enabled() {
    let dev = shared_over(btrfs_drive().with_write_cache(true));
    dev.ensure_writes_safe().expect("flush works");
    let mut fs = Btrfs::mount(dev.clone()).expect("mount");
    fs.create_file_with_data("/", "wb.txt", b"flushed").expect("write allowed");
    let fs = Btrfs::mount(dev.clone()).expect("remount");
    assert_eq!(fs.read_file("/wb.txt").expect("read back"), b"flushed");
}

/// The decision is made by the probe, before any commit could write: the
/// device reports flush support without a single write having happened.
#[test]
fn the_probe_runs_before_the_first_write() {
    let dev = shared_over(btrfs_drive());
    assert_eq!(luks_core::device::WriteAt::flush_support(&dev), None);
    dev.ensure_writes_safe().expect("safe");
    assert_eq!(luks_core::device::WriteAt::flush_support(&dev), Some(true));
}
