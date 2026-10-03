#[path = "../common/mod.rs"]
mod common;

use std::sync::Mutex;
use std::thread;

use luks_core::forensic::{
    clear, dump_json, dump_text, record_btrfs, record_panic, record_scsi, record_usb, snapshot,
    BtrfsEvent, ScsiEvent, UsbEvent,
};

static TEST_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn test_ring_buffer_bounded_capacity_and_wrap_around() {
    let _guard = TEST_LOCK.lock().unwrap();
    clear();

    // Push 300 events (exceeding RING_CAPACITY of 256)
    for i in 0..300 {
        record_btrfs(BtrfsEvent::WriteChunk {
            len: 4096,
            bytenr: i * 4096,
        });
    }

    let records = snapshot();
    assert_eq!(records.len(), 256, "Ring buffer must hold exactly 256 entries at capacity");

    // Monotonic sequence verification
    for i in 1..records.len() {
        assert!(
            records[i].seq > records[i - 1].seq,
            "Sequences must be strictly monotonically increasing: {} vs {}",
            records[i].seq,
            records[i - 1].seq
        );
    }

    // Verify the oldest retained entry is from the later portion
    if let luks_core::forensic::ForensicEvent::Btrfs(BtrfsEvent::WriteChunk { bytenr, .. }) =
        records[0].event
    {
        assert_eq!(bytenr, 44 * 4096, "Oldest entry should correspond to index 44 (300 - 256)");
    } else {
        panic!("Unexpected event type");
    }

    // Verify the latest entry
    if let luks_core::forensic::ForensicEvent::Btrfs(BtrfsEvent::WriteChunk { bytenr, .. }) =
        records.last().unwrap().event
    {
        assert_eq!(bytenr, 299 * 4096);
    } else {
        panic!("Unexpected event type");
    }
}

#[test]
fn test_privacy_invariants_zero_exposure() {
    let _guard = TEST_LOCK.lock().unwrap();
    clear();

    let sensitive_filename = "my_confidential_passwords.txt";
    let sensitive_content = "super_secret_master_passphrase_12345";

    // Simulate operations using only numeric metadata
    record_btrfs(BtrfsEvent::CreateFile {
        parent_ino: 256,
        new_ino: 257,
    });
    record_btrfs(BtrfsEvent::BeginFile { file_len: 1024 });
    record_btrfs(BtrfsEvent::WriteChunk {
        len: 1024,
        bytenr: 13631488,
    });
    record_btrfs(BtrfsEvent::FinishFile {
        ino: 257,
        total_bytes: 1024,
    });
    record_scsi(ScsiEvent::Command {
        opcode: 0x2A, // WRITE_10
        tag: 0x1234,
        data_len: 1024,
        dir: "OUT",
        lba: None,
        blocks: None,
    });
    record_usb(UsbEvent::Submit {
        ep: 0x02,
        count: 1,
        bytes: 1024,
        gen: 1,
    });

    let text_dump = dump_text();
    let json_dump = dump_json();

    // Verify sensitive data does NOT appear anywhere in text or JSON dumps
    assert!(!text_dump.contains(sensitive_filename));
    assert!(!text_dump.contains(sensitive_content));
    assert!(!json_dump.contains(sensitive_filename));
    assert!(!json_dump.contains(sensitive_content));

    // Verify structural metadata IS present
    assert!(text_dump.contains("BTRFS create parent_ino=256 ino=257"));
    assert!(text_dump.contains("BTRFS begin_file len=1024"));
    assert!(text_dump.contains("BTRFS write_chunk len=1024 bytenr=13631488"));
    assert!(text_dump.contains("BTRFS finish_file ino=257 bytes=1024"));
    assert!(text_dump.contains("SCSI cmd op=0x2A tag=0x00001234 len=1024 dir=OUT"));
    assert!(text_dump.contains("USB submit ep=2 count=1 bytes=1024 gen=1"));
}

#[test]
fn test_concurrent_forensic_logging() {
    let _guard = TEST_LOCK.lock().unwrap();
    clear();

    let threads: Vec<_> = (0..8)
        .map(|t| {
            thread::spawn(move || {
                for i in 0..50 {
                    record_btrfs(BtrfsEvent::WriteChunk {
                        len: 4096,
                        bytenr: (t * 1000 + i) as u64,
                    });
                }
            })
        })
        .collect();

    for handle in threads {
        handle.join().unwrap();
    }

    let records = snapshot();
    assert_eq!(records.len(), 256);

    for i in 1..records.len() {
        assert!(records[i].seq > records[i - 1].seq);
    }
}

#[test]
fn test_panic_trace_sanitization() {
    let _guard = TEST_LOCK.lock().unwrap();
    clear();

    record_panic(
        "/home/developer/workspace/project/core/src/fs/btrfs/mod.rs",
        123,
        45,
        "failed to read /media/usb/private/keyfile.bin: corrupted header",
    );

    let text_dump = dump_text();
    let json_dump = dump_json();

    assert!(text_dump.contains("PANIC at src/fs/btrfs/mod.rs:123:45"));
    assert!(!text_dump.contains("/home/developer"));
    assert!(!text_dump.contains("/media/usb/private/keyfile.bin"));
    assert!(text_dump.contains("<redacted_path>"));

    assert!(json_dump.contains("src/fs/btrfs/mod.rs"));
    assert!(!json_dump.contains("/home/developer"));
}
/// F1.5 (register O3): a SCSI Command event for a READ/WRITE names the block
/// range it moved, so a post-mortem can say *which* sectors were in flight.
/// Raw numbers only (no names or content), so the privacy invariant holds.
#[test]
fn scsi_read_and_write_events_carry_lba_and_block_count() {
    use luks_core::usb::ScsiBlockDevice;

    let _guard = TEST_LOCK.lock().unwrap();
    let dev = ScsiBlockDevice::open(common::MockUsbDrive::new(common::fixture("disks/gpt-luks.img")))
        .expect("open mock drive");
    clear();

    // READ(10)
    let mut buf = vec![0u8; 8 * 512];
    dev.read_blocks(1234, 8, &mut buf).expect("read");
    // READ(16): LBA beyond 32 bits is out of range for the mock, so the
    // command fails, but the event is recorded before it is sent.
    dev.read_blocks((1u64 << 32) + 5, 3, &mut buf)
        .expect_err("precondition: the mock refuses an out-of-range READ(16)");

    let text = dump_text();
    assert!(text.contains("SCSI cmd op=0x28") && text.contains("lba=1234 blocks=8"), "{text}");
    assert!(text.contains("op=0x88") && text.contains("lba=4294967301 blocks=3"), "{text}");
    let json = dump_json();
    let v: serde_json::Value = serde_json::from_str(&json).expect("dump_json is JSON");
    let events = v.as_array().expect("array of records");
    assert!(!events.is_empty(), "vacuity: no records");
    assert!(
        events.iter().any(|r| {
            let c = &r["event"]["detail"]["Command"];
            c["opcode"] == 0x28 && c["lba"] == 1234 && c["blocks"] == 8
        }),
        "{json}"
    );

    #[cfg(feature = "dangerous-write-support")]
    {
        clear();
        dev.write_blocks(77, 2, &vec![0u8; 2 * 512]).expect("write");
        dev.write_blocks((1u64 << 32) + 9, 1, &vec![0u8; 512])
            .expect_err("precondition: the mock refuses an out-of-range WRITE(16)");
        let text = dump_text();
        assert!(text.contains("op=0x2A") && text.contains("lba=77 blocks=2"), "{text}");
        assert!(text.contains("op=0x8A") && text.contains("lba=4294967305 blocks=1"), "{text}");
    }

    // Control: commands that move no blocks carry no extent.
    clear();
    dev.test_unit_ready().expect("tur");
    let text = dump_text();
    assert!(text.contains("SCSI cmd op=0x00"), "{text}");
    assert!(!text.contains("lba="), "a TEST UNIT READY has no LBA: {text}");
}
