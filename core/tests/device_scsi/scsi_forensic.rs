//! F9 review B1: SCSI results reach the notable queue only when they are not
//! a plain pass. Driven through the real CSW path (the mock drive answers with
//! real CSWs), so the status values are the ones production emits, not
//! literals invented here.

#[path = "../common/mod.rs"]
mod common;

use common::{fixture, MockUsbDrive};
use luks_core::device::ReadAt;
use luks_core::forensic::{drain_notable, snapshot, ForensicEvent, ScsiEvent, ScsiStatus};
use luks_core::usb::ScsiBlockDevice;
use std::sync::Mutex;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    match TEST_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn results(recs: &[luks_core::forensic::ForensicRecord]) -> Vec<ScsiStatus> {
    recs.iter()
        .filter_map(|r| match &r.event {
            ForensicEvent::Scsi(ScsiEvent::Result { status, .. }) => Some(*status),
            _ => None,
        })
        .collect()
}

#[test]
fn successful_commands_stay_out_of_the_notable_queue() {
    let _g = lock();
    let dev = ScsiBlockDevice::open(MockUsbDrive::new(fixture("disks/gpt-luks.img"))).expect("open");
    drain_notable();
    let mut buf = vec![0u8; 4096];
    dev.read_at(0, &mut buf).expect("read");

    let (queued, _) = drain_notable();
    assert_eq!(results(&queued), Vec::<ScsiStatus>::new(), "a passed result flooded the queue");
    let ring = results(&snapshot());
    assert!(ring.contains(&ScsiStatus::Passed), "the ring must still see the pass: {ring:?}");
}

#[test]
fn a_failed_command_is_notable_with_its_status() {
    let _g = lock();
    let drive = MockUsbDrive::new(fixture("disks/gpt-luks.img"));
    drive.fail_opcodes.borrow_mut().push(0x28);
    let dev = ScsiBlockDevice::open(drive).expect("open");
    drain_notable();
    let mut buf = vec![0u8; 4096];
    assert!(dev.read_at(0, &mut buf).is_err(), "the mock refuses READ(10)");

    let (queued, _) = drain_notable();
    assert!(results(&queued).contains(&ScsiStatus::Failed), "failed result missing: {:?}", results(&queued));
    let json = luks_core::forensic::ring_json();
    assert!(json.contains(r#""status":"failed""#), "wire status must be lowercase: {json}");
}
