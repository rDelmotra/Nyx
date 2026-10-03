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
