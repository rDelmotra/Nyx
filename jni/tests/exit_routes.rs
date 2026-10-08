//! F0 red tests: writer exit routes and export stitching, through the bridge.
//!
//! ```text
//! cargo test -p luks_jni --features dangerous-write-support --test exit_routes -- --ignored
//! ```
//!
//! Every test asserts the CORRECT behaviour; those still `#[ignore]`d are
//! `KNOWN-RED` on current code (see `tools/known-red.sh`). Disk state is
//! graded by the kernel oracle (`tools/verify-image.sh`,
//! real `e2fsck` / `btrfs check` in colima) after the volume is dropped and
//! the image is re-opened from scratch, never by our own reader.
#![cfg(feature = "dangerous-write-support")]

#[path = "../../core/tests/common/temp_guard.rs"]
mod temp_guard;
use temp_guard::{TempStr};

#[path = "../../core/tests/common/oracle.rs"]
mod oracle;

use luks_core::device::FileDevice;
use luks_core::error::LuksError;
use luks_jni::bridge::{
    drop_volume, into_raw, volume_ref, DeviceHandle, Payload, VolumeHandle,
};
use std::process::Command;

const PASSWORD: &[u8] = b"test";

fn scratch(fixture: &str, test: &str) -> TempStr {
    let src = format!("{}/../fixtures/disks/{fixture}", env!("CARGO_MANIFEST_DIR"));
    let dst = std::env::temp_dir().join(format!("luks-jni-exit-{test}.img"));
    std::fs::copy(src, &dst).expect("copy fixture");
    TempStr::new(dst.to_string_lossy().into_owned())
}

fn unlock(path: &str) -> VolumeHandle {
    let len = std::fs::metadata(path).expect("stat").len();
    let dev = FileDevice::open_writable(path, len).expect("open writable");
    let handle =
        DeviceHandle::new(dev, 512, len / 512, "TEST".into(), "IMAGE".into()).expect("scan");
    let offset = handle
        .table
        .luks_partitions()
        .next()
        .expect("a LUKS partition")
        .offset_bytes();
    handle.unlock(offset, PASSWORD).expect("unlock")
}

/// Grade `path` with the kernel oracle. An unreachable oracle FAILS the test.
fn assert_kernel_clean(path: &str, what: &str) {
    assert!(
        oracle::gate(),
        "oracle unreachable (ALLOW_NO_ORACLE set?): a graded test cannot pass without it"
    );
    let script = format!("{}/../tools/verify-image.sh", env!("CARGO_MANIFEST_DIR"));
    let out = Command::new("bash")
        .arg(&script)
        .arg(path)
        .arg("test")
        .output()
        .expect("run verify-image");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success() && text.contains("VERDICT: clean"),
        "{what}: the kernel found the image unclean:\n{text}"
    );
}

/// Sized ext4 writer with a partial chunk written: the inode and every data
/// block are reserved in memory only (F2a), so nothing is marked used on disk.
fn open_partial_writer(vol: &VolumeHandle) -> u64 {
    let w = vol.begin_file(1 << 20).expect("begin");
    vol.write_file_chunk(w, b"halfway there").expect("chunk");
    w
}

/// W3: abandon on a fenced session is refused before reaching the driver. Green
/// since F2a: the reservation is memory-only, so the refusal leaks nothing on
/// disk. (F4 still owns making the bridge release it.)
#[test]
fn w3_abandon_after_fence_leaves_ext4_clean() {
    let path = scratch("gpt-luks.img", "w3");
    {
        let vol = unlock(&path);
        let w = open_partial_writer(&vol);
        vol.fence_writes(LuksError::ScsiProtocol("injected fence"));
        assert!(vol.is_write_fenced(), "vacuity: the session must be fenced");
        let r = vol.abandon_file(w);
        eprintln!("abandon_file on fenced session returned: {r:?}");
    }
    assert_kernel_clean(&path, "abandon after fence");
}

/// W4: finish on a fenced session drops the writer without abandoning it.
/// Green since F2a: the dropped reservation never reached the disk.
#[test]
fn w4_finish_after_fence_leaves_ext4_clean() {
    let path = scratch("gpt-luks.img", "w4");
    {
        let vol = unlock(&path);
        let w = open_partial_writer(&vol);
        vol.fence_writes(LuksError::ScsiProtocol("injected fence"));
        assert!(vol.is_write_fenced(), "vacuity: the session must be fenced");
        let r = vol.finish_file(w, "/", "fenced.bin");
        assert!(r.is_err(), "a fenced finish must not report success: {r:?}");
    }
    assert_kernel_clean(&path, "finish after fence");
}

/// W2: closing the volume with a writer still open abandons nothing. Green
/// since F2a: the reservation dies with the in-memory volume.
#[test]
fn w2_drop_volume_with_open_writer_leaves_ext4_clean() {
    let path = scratch("gpt-luks.img", "w2");
    {
        let vol_h = into_raw(Payload::Volume(unlock(&path)));
        let w = {
            let v = volume_ref(vol_h).expect("volume handle");
            open_partial_writer(&v)
        };
        drop_volume(vol_h);
        let _ = w; // the writer is deliberately never finished or abandoned
    }
    assert_kernel_clean(&path, "drop_volume with open writer");
}

const CHUNK: usize = 4096;

/// N3: an export pins the file it opened. A replace between chunks must not
/// stitch another file's bytes into the export; the second chunk of the
/// original `/a` is refused with `FileChanged` (never `/b`'s bytes).
///
/// History: this read through `read_chunk(path, ..)`, which re-resolved the
/// path per chunk. F4 B1 replaced it with `open_file` + `read_at` by identity.
fn assert_export_does_not_stitch(path: &str) {
    let vol = unlock(path);
    vol.write_file("/", "a", &[b'A'; 2 * CHUNK]).expect("write a");
    vol.write_file("/", "b", &[b'B'; 2 * CHUNK]).expect("write b");
    vol.commit_active_batch().expect("commit");

    let file = vol.open_file("/a").expect("open a");
    let mut buf = vec![0u8; CHUNK];
    let n = vol.read_at(&file, 0, &mut buf).expect("chunk 0");
    assert_eq!(n, CHUNK);
    assert!(buf.iter().all(|&c| c == b'A'), "vacuity: chunk 0 must be /a's");

    vol.delete_file("/a").expect("delete a");
    vol.rename("/", "b", "/", "a").expect("rename b over a");
    vol.commit_active_batch().expect("commit");

    let mut buf = vec![0u8; CHUNK];
    match vol.read_at(&file, CHUNK as u64, &mut buf) {
        Err(LuksError::FileChanged) => {}
        Err(e) => panic!("expected FileChanged, got {e:?}"),
        Ok(n) => assert!(
            buf[..n].iter().all(|&c| c == b'A'),
            "chunk 1 of the original /a came back with another file's bytes: {:?}",
            &buf[..n.min(8)]
        ),
    }
}

#[test]
fn n3_export_chunks_never_stitch_another_file_ext4() {
    let path = scratch("gpt-luks.img", "n3-ext4");
    assert_export_does_not_stitch(&path);
}

#[test]
fn n3_export_chunks_never_stitch_another_file_btrfs() {
    let path = scratch("gpt-luks-btrfs.img", "n3-btrfs");
    assert_export_does_not_stitch(&path);
}
