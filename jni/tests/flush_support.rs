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
use std::sync::atomic::Ordering::SeqCst;
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
// Write durability modes. Writes are never refused for cache/flush reasons:
//   flush          SYNCHRONIZE CACHE accepted
//   write_through  no flush, drive already reports WCE clear (no MODE SELECT)
//   cache_off      no flush, WCE on: we switch it off (MODE SELECT, SP=0)
//                  and a MODE SENSE read-back confirms it
//   unverified     none of the above worked: proceed like Linux's
//                  sd_sync_cache(), and say so
// These run on the shared device layer that both ext4 and btrfs sit on, over a
// plain btrfs image (no LUKS fixture needed).
// ---------------------------------------------------------------------------

use common::states::{judge, Depth};
use common::mem_device::MemoryDevice;
use luks_core::device::ReadAt;
use luks_core::fs::btrfs::Btrfs;
use luks_jni::bridge::SharedDevice;

fn shared_over(drive: MockUsbDrive) -> SharedDevice {
    SharedDevice::new(ScsiBlockDevice::open(drive).expect("open mock drive"))
}

fn btrfs_drive() -> MockUsbDrive {
    MockUsbDrive::new(fixture("btrfs/states-4k.img"))
}

fn no_flush(d: MockUsbDrive) -> MockUsbDrive {
    d.failing(0x35, NO_SUCH_OPCODE).failing(0x91, NO_SUCH_OPCODE)
}

fn durability(dev: &SharedDevice) -> Option<&'static str> {
    WriteAt::write_durability(dev)
}

/// Write one file through btrfs and read it back after a fresh mount.
fn write_and_read_back(dev: &SharedDevice, name: &str, data: &[u8]) {
    let mut fs = Btrfs::mount(dev.clone()).expect("mount");
    fs.create_file_with_data("/", name, data).expect("write must be allowed");
    let fs = Btrfs::mount(dev.clone()).expect("remount");
    assert_eq!(fs.read_file(&format!("/{name}")).expect("read back"), data);
}

/// The whole drive, graded by the same checkers the power-cut tests use.
fn assert_consistent(dev: &SharedDevice, step: &str) {
    let len = ReadAt::len(dev).expect("len") as usize;
    let mut img = vec![0u8; len];
    for (i, chunk) in img.chunks_mut(1 << 20).enumerate() {
        dev.read_at((i << 20) as u64, chunk).expect("dump");
    }
    judge(&MemoryDevice::from_bytes(img), step, Depth::Ours);
}

#[test]
fn a_working_flush_is_mode_flush_and_sends_no_mode_select() {
    let drive = btrfs_drive().with_write_cache(true);
    let probe = drive.cache_probe();
    let dev = shared_over(drive);
    dev.ensure_writes_safe().expect("flush works");
    assert_eq!(durability(&dev), Some("flush"));
    write_and_read_back(&dev, "wb.txt", b"flushed");
    drop(dev);
    assert_eq!(probe.mode_selects(), 0, "f: no MODE SELECT when a flush works");
    assert!(probe.wce(), "the cache was never touched");
    probe.assert_clean();
}

#[test]
fn flush_rejected_and_cache_already_off_is_write_through_with_no_mode_select() {
    let drive = no_flush(btrfs_drive().with_write_cache(false));
    let probe = drive.cache_probe();
    let dev = shared_over(drive);
    dev.ensure_writes_safe().expect("allowed");
    assert_eq!(durability(&dev), Some("write_through"));
    assert!(!WriteAt::write_cache_forced_off(&dev));
    write_and_read_back(&dev, "wt.txt", b"write-through");
    drop(dev);
    assert_eq!(probe.mode_selects(), 0, "g: no MODE SELECT when WCE is already off");
    assert!(!probe.wce());
    probe.assert_clean();
}

/// (a) The case from the user's SSD adapter.
#[test]
fn flush_rejected_and_cache_on_switches_the_cache_off_and_restores_it_on_close() {
    let drive = no_flush(btrfs_drive().with_write_cache(true));
    let probe = drive.cache_probe();
    let dev = shared_over(drive);
    dev.ensure_writes_safe().expect("allowed");
    assert_eq!(durability(&dev), Some("cache_off"));
    assert!(WriteAt::write_cache_forced_off(&dev));
    assert!(!probe.wce(), "the drive reports the cache off");
    assert_eq!(probe.mode_selects(), 1);

    write_and_read_back(&dev, "slow.txt", b"safe slow mode");
    assert_consistent(&dev, "cache_off");
    assert!(!probe.wce(), "still off during the session");
    assert_eq!(probe.mode_selects(), 1, "no further MODE SELECT while connected");

    drop(dev); // the last handle going away is the close point
    assert!(probe.wce(), "restored on close");
    assert_eq!(probe.mode_selects(), 2);
    assert_eq!(probe.applied_off.load(SeqCst), 1);
    assert_eq!(probe.applied_on.load(SeqCst), 1);
    probe.assert_clean(); // never SP=1, always PF=1, BDL=0, PS clear
}

/// An MBR disk with one Linux partition holding the btrfs fixture, so a real
/// `DeviceHandle` (and its info JSON) can sit on the mock without a LUKS fixture.
fn mbr_disk_with_btrfs() -> Vec<u8> {
    let fs = fixture("btrfs/states-4k.img");
    let start_sectors = 2048u32;
    let mut img = vec![0u8; start_sectors as usize * 512];
    img[446 + 4] = 0x83;
    img[446 + 8..446 + 12].copy_from_slice(&start_sectors.to_le_bytes());
    img[446 + 12..446 + 16].copy_from_slice(&((fs.len() / 512) as u32).to_le_bytes());
    img[510] = 0x55;
    img[511] = 0xAA;
    img.extend_from_slice(&fs);
    img
}

#[test]
fn the_device_info_json_reports_the_mode() {
    let drive = no_flush(MockUsbDrive::new(mbr_disk_with_btrfs()).with_write_cache(true));
    let probe = drive.cache_probe();
    let dev = device_over(drive);
    assert_eq!(json(dev.info_json()).get("writeDurability"), Some(&Value::Null), "unknown before the check");
    dev.dev.ensure_writes_safe().expect("allowed");
    let d = json(dev.info_json());
    assert_eq!(d["writeDurability"], "cache_off", "{d}");
    assert_eq!(d["writeCacheForcedOff"], Value::Bool(true), "{d}");
    assert_eq!(d["flushSupported"], Value::Bool(false), "{d}");
    drop(dev);
    assert!(probe.wce());
    probe.assert_clean();
}

#[test]
fn the_ten_byte_mode_commands_work_end_to_end_with_a_block_descriptor() {
    let drive = no_flush(btrfs_drive().with_write_cache(true).with_only_ten_byte_mode_commands().with_block_descriptor());
    let probe = drive.cache_probe();
    let dev = shared_over(drive);
    dev.ensure_writes_safe().expect("allowed");
    assert_eq!(durability(&dev), Some("cache_off"));
    assert_eq!(probe.select_cdbs.lock().unwrap()[0][0], 0x55);
    drop(dev);
    assert!(probe.wce());
    assert_eq!(probe.select_cdbs.lock().unwrap().last().unwrap()[0], 0x55);
    probe.assert_clean();
}

/// (b) MODE SELECT rejected: allowed anyway, as Linux does.
#[test]
fn mode_select_rejected_runs_unverified_and_still_writes() {
    let drive = no_flush(btrfs_drive().with_write_cache(true)).rejecting_mode_select();
    let probe = drive.cache_probe();
    let dev = shared_over(drive);
    dev.ensure_writes_safe().expect("never refused for cache reasons");
    assert_eq!(durability(&dev), Some("unverified"));
    assert!(!WriteAt::write_cache_forced_off(&dev));
    write_and_read_back(&dev, "u.txt", b"unverified");
    assert_consistent(&dev, "unverified-rejected");
    drop(dev);
    assert!(probe.wce(), "never changed");
    probe.assert_clean();
}

/// (c) A bridge that accepts MODE SELECT and keeps the cache on.
#[test]
fn a_lying_bridge_runs_unverified_and_still_writes() {
    let drive = no_flush(btrfs_drive().with_write_cache(true)).lying_about_mode_select();
    let probe = drive.cache_probe();
    let dev = shared_over(drive);
    dev.ensure_writes_safe().expect("allowed");
    assert_eq!(durability(&dev), Some("unverified"));
    write_and_read_back(&dev, "lie.txt", b"lie");
    drop(dev);
    assert!(probe.wce());
    probe.assert_clean();
}

/// (d) WCE reported not changeable: no MODE SELECT is even attempted.
#[test]
fn wce_not_changeable_runs_unverified_without_sending_mode_select() {
    let drive = no_flush(btrfs_drive().with_write_cache(true).with_wce_not_changeable());
    let probe = drive.cache_probe();
    let dev = shared_over(drive);
    dev.ensure_writes_safe().expect("allowed");
    assert_eq!(durability(&dev), Some("unverified"));
    assert_eq!(probe.mode_selects(), 0);
    write_and_read_back(&dev, "nc.txt", b"fixed");
    drop(dev);
    assert_eq!(probe.mode_selects(), 0);
}

/// MODE SENSE unusable: nothing to switch, nothing to confirm.
#[test]
fn unreadable_cache_state_runs_unverified_and_still_writes() {
    let dev = shared_over(no_flush(btrfs_drive()));
    dev.ensure_writes_safe().expect("allowed");
    assert_eq!(durability(&dev), Some("unverified"));
    write_and_read_back(&dev, "unk.txt", b"unknown");
}

/// (e) The drive resets mid-session and the cache comes back on.
#[test]
fn a_revert_mid_session_downgrades_to_unverified_and_the_commit_completes() {
    let drive = no_flush(btrfs_drive().with_write_cache(true));
    let probe = drive.cache_probe();
    let dev = shared_over(drive);
    dev.ensure_writes_safe().expect("allowed");
    assert_eq!(durability(&dev), Some("cache_off"));
    write_and_read_back(&dev, "before.txt", b"before the reset");

    probe.revert(); // the drive reset: volatile settings are back to defaults
    assert!(probe.wce());

    // The commit's pre-superblock flush notices; the commit still completes.
    write_and_read_back(&dev, "after.txt", b"after the reset");
    assert_eq!(durability(&dev), Some("unverified"));
    assert!(!WriteAt::write_cache_forced_off(&dev));

    // Reads and the earlier file are intact, and the disk is consistent.
    let fs = Btrfs::mount(dev.clone()).expect("mount");
    assert_eq!(fs.read_file("/before.txt").unwrap(), b"before the reset");
    assert_consistent(&dev, "after-revert");

    // Later writes keep working in the downgraded mode.
    write_and_read_back(&dev, "later.txt", b"later");
    drop(dev);
    assert_eq!(probe.applied_on.load(SeqCst), 0, "restore finds the cache already on and sends nothing");
    probe.assert_clean();
}

#[test]
fn the_revert_is_also_caught_by_a_scripted_reset() {
    let drive = no_flush(btrfs_drive().with_write_cache(true)).reverting_cache_after(40);
    let dev = shared_over(drive);
    dev.ensure_writes_safe().expect("allowed");
    for i in 0..6 {
        write_and_read_back(&dev, &format!("f{i}.txt"), b"x");
    }
    assert_eq!(durability(&dev), Some("unverified"));
    assert_consistent(&dev, "scripted-revert");
}

/// Ext4 sits on the same device layer: same modes, same restore, and ext4's
/// own flushes (dirty bracket) go through the same re-check.
#[test]
fn ext4_gets_the_same_modes() {
    let drive = no_flush(MockUsbDrive::new(fixture("ext4/small-1k.img")).with_write_cache(true));
    let probe = drive.cache_probe();
    let dev = shared_over(drive);
    dev.ensure_writes_safe().expect("allowed");
    assert_eq!(durability(&dev), Some("cache_off"));
    let mut fs = luks_core::fs::ext4::Ext4::mount(dev.clone()).expect("mount ext4");
    fs.create_file(2, "e.txt", b"ext4 safe slow", luks_core::fs::FileType::Regular).expect("write allowed");
    let fs = luks_core::fs::ext4::Ext4::mount(dev.clone()).expect("remount");
    assert_eq!(fs.read_file("/e.txt").unwrap(), b"ext4 safe slow");
    probe.revert();
    let mut fs = luks_core::fs::ext4::Ext4::mount(dev.clone()).expect("mount");
    fs.create_file(2, "f.txt", b"after reset", luks_core::fs::FileType::Regular).expect("commit completes");
    assert_eq!(durability(&dev), Some("unverified"));
    drop((fs, dev));
    probe.assert_clean();
}
