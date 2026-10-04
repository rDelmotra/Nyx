//! F4 Part B: identity-based reads through the bridge.
//!
//! ```text
//! cargo test -p luks_jni --features dangerous-write-support --test identity_reads
//! ```
//!
//! A read handle pins a file by identity (inode number plus generation on
//! ext4; subvolume, inode, generation and transid on btrfs). Each `read_at`
//! re-reads the inode under the filesystem lock and answers with the correct
//! bytes or `FileChanged`, never another file's bytes, and never through a
//! tree root a commit has since freed (R3r).
#![cfg(feature = "dangerous-write-support")]

#[path = "../../core/tests/common/temp_guard.rs"]
mod temp_guard;
use temp_guard::TempStr;

use luks_core::device::FileDevice;
use luks_core::error::LuksError;
use luks_core::fs::MountedFs;
use luks_jni::bridge::{
    drop_file, drop_volume, file_ref, into_raw, volume_ref, DeviceHandle, Payload, VolumeHandle,
};

const PASSWORD: &[u8] = b"test";

/// The fs tree's root bytenr on btrfs (it moves on every commit), `None` on ext4.
fn btrfs_root(vol: &VolumeHandle) -> Option<u64> {
    match &*vol.fs_for_reading() {
        MountedFs::Btrfs(fs) => Some(fs.fs_tree().bytenr),
        MountedFs::Ext4(_) => None,
    }
}
const CHUNK: usize = 4096;
const CHUNKS: usize = 4;

fn scratch(fixture: &str, test: &str) -> TempStr {
    let src = format!("{}/../fixtures/disks/{fixture}", env!("CARGO_MANIFEST_DIR"));
    let dst = std::env::temp_dir().join(format!("luks-jni-ident-{test}.img"));
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

/// Chunk `i` of the test file is filled with byte `base + i`, so a wrong
/// chunk, a wrong file and a stale read are all distinguishable.
fn body(base: u8) -> Vec<u8> {
    (0..CHUNKS).flat_map(|i| vec![base + i as u8; CHUNK]).collect()
}

fn chunk_ok(buf: &[u8], base: u8, i: usize) -> bool {
    buf.len() == CHUNK && buf.iter().all(|&c| c == base + i as u8)
}

/// R3r / B3: a commit lands between export chunks. The file was not touched,
/// so every chunk must still be its own bytes: the handle resolves the
/// *current* tree root, not the one captured at open.
fn commit_between_chunks(fixture: &str, tag: &str) {
    let path = scratch(fixture, tag);
    let vol = unlock(&path);
    vol.write_file("/", "big", &body(10)).expect("write big");
    vol.commit_active_batch().expect("commit");

    let file = vol.open_file("/big").expect("open");
    let mut buf = vec![0u8; CHUNK];
    assert_eq!(vol.read_at(&file, 0, &mut buf).expect("chunk 0"), CHUNK);
    assert!(chunk_ok(&buf, 10, 0), "vacuity: chunk 0 is the file's");

    let root_before = btrfs_root(&vol);

    // Enough unrelated metadata churn that the tree roots move, and (on
    // btrfs) the pre-commit root blocks are free to be reused.
    for round in 0..3u8 {
        for k in 0..60u8 {
            vol.write_file("/", &format!("noise-{round}-{k}"), &[k; 300]).expect("noise");
        }
        vol.commit_active_batch().expect("commit between chunks");
    }

    assert_eq!(btrfs_root(&vol).is_some(), root_before.is_some());
    if let Some(before) = root_before {
        assert_ne!(btrfs_root(&vol), Some(before), "vacuity: the commits must have moved the tree root");
    }

    for i in 1..CHUNKS {
        let mut buf = vec![0u8; CHUNK];
        match vol.read_at(&file, (i * CHUNK) as u64, &mut buf) {
            Ok(n) => assert!(
                chunk_ok(&buf[..n], 10, i),
                "chunk {i} after a commit is not the file's bytes: {:?}",
                &buf[..n.min(8)]
            ),
            Err(LuksError::FileChanged) => {
                panic!("an unrelated commit must not change this file's identity (chunk {i})")
            }
            Err(e) => panic!("chunk {i}: {e:?}"),
        }
    }
}

#[test]
fn a_commit_between_export_chunks_keeps_btrfs_bytes_correct() {
    commit_between_chunks("gpt-luks-btrfs.img", "commit-btrfs");
}

#[test]
fn a_commit_between_export_chunks_keeps_ext4_bytes_correct() {
    commit_between_chunks("gpt-luks.img", "commit-ext4");
}

/// The same commit, but it deletes the file and puts a different one at the
/// same path: the answer is `FileChanged` or the original bytes, never the
/// replacement's.
#[test]
fn a_replace_between_chunks_is_file_changed_on_btrfs() {
    let path = scratch("gpt-luks-btrfs.img", "replace-btrfs");
    let vol = unlock(&path);
    vol.write_file("/", "big", &body(10)).expect("write big");
    vol.commit_active_batch().expect("commit");
    let file = vol.open_file("/big").expect("open");
    let mut buf = vec![0u8; CHUNK];
    vol.read_at(&file, 0, &mut buf).expect("chunk 0");

    vol.delete_file("/big").expect("delete");
    vol.write_file("/", "big", &body(50)).expect("recreate");
    vol.commit_active_batch().expect("commit");

    let mut buf = vec![0u8; CHUNK];
    match vol.read_at(&file, CHUNK as u64, &mut buf) {
        Err(LuksError::FileChanged) => {}
        other => panic!("expected FileChanged for a replaced file, got {other:?}"),
    }
}

/// ext4 reuses the lowest free inode number, so delete + create lands the new
/// file on the old file's inode. Only the generation tells them apart.
#[test]
fn delete_and_recreate_at_the_same_ext4_inode_number_is_file_changed() {
    let path = scratch("gpt-luks.img", "reuse-ext4");
    let vol = unlock(&path);
    let ino_a = vol.write_file("/", "a", &body(10)).expect("write a");
    vol.commit_active_batch().expect("commit");
    let file = vol.open_file("/a").expect("open a");
    let mut buf = vec![0u8; CHUNK];
    vol.read_at(&file, 0, &mut buf).expect("chunk 0");

    vol.delete_file("/a").expect("delete a");
    // Same size, same type: only the generation can differ.
    let ino_c = vol.write_file("/", "c", &body(50)).expect("write c");
    vol.commit_active_batch().expect("commit");
    assert_eq!(ino_a, ino_c, "vacuity: the new file must land on the old inode number");

    let mut buf = vec![0u8; CHUNK];
    match vol.read_at(&file, CHUNK as u64, &mut buf) {
        Err(LuksError::FileChanged) => {}
        other => panic!("expected FileChanged for a reused inode number, got {other:?}"),
    }
}

/// Coverage is reported per handle: btrfs checksums its data, ext4 does not.
#[test]
fn coverage_counts_verified_and_unverified_bytes() {
    let path = scratch("gpt-luks-btrfs.img", "cov-btrfs");
    let vol = unlock(&path);
    vol.write_file("/", "f", &body(10)).expect("write");
    vol.commit_active_batch().expect("commit");
    let file = vol.open_file("/f").expect("open");
    let mut buf = vec![0u8; 2 * CHUNK];
    assert_eq!(vol.read_at(&file, 0, &mut buf).expect("read"), 2 * CHUNK);
    assert_eq!(file.coverage(), (2 * CHUNK as u64, 0), "btrfs data is checksum-verified");

    let path = scratch("gpt-luks.img", "cov-ext4");
    let vol = unlock(&path);
    vol.write_file("/", "f", &body(10)).expect("write");
    vol.commit_active_batch().expect("commit");
    let file = vol.open_file("/f").expect("open");
    assert_eq!(vol.read_at(&file, 0, &mut buf).expect("read"), 2 * CHUNK);
    assert_eq!(file.coverage(), (0, 2 * CHUNK as u64), "ext4 has no data checksums");
}

#[test]
fn opening_a_directory_is_refused_and_a_missing_path_is_not_found() {
    let path = scratch("gpt-luks.img", "open-refuse");
    let vol = unlock(&path);
    vol.create_directory("/", "d").expect("mkdir");
    assert!(
        matches!(vol.open_file("/d"), Err(LuksError::IsADirectory(_))),
        "a directory is not a readable file"
    );
    assert!(matches!(vol.open_file("/nope"), Err(LuksError::NotFound(_))));
}

/// The registry side: a file handle is its own kind, resolves back to the
/// handle, and closing it removes it (a second close is a no-op).
#[test]
fn file_handles_round_trip_through_the_registry_and_close_once() {
    let path = scratch("gpt-luks.img", "registry");
    let vol_h = into_raw(Payload::Volume(unlock(&path)));
    let file_h = {
        let vol = volume_ref(vol_h).expect("volume");
        vol.write_file("/", "f", &body(10)).expect("write");
        vol.commit_active_batch().expect("commit");
        let f = vol.open_file("/f").expect("open");
        into_raw(Payload::File(f))
    };
    assert!(file_ref(file_h).is_ok(), "a file handle resolves");
    assert!(volume_ref(file_h).is_err(), "a file handle is not a volume");
    assert!(file_ref(vol_h).is_err(), "a volume handle is not a file");
    drop_file(file_h);
    assert!(file_ref(file_h).is_err(), "closed handles no longer resolve");
    drop_file(file_h);
    drop_volume(vol_h);
}

/// The size of an open file comes from the handle, with the identity check:
/// no second path lookup that could name a different file.
fn size_by_handle(fixture: &str, tag: &str) {
    let path = scratch(fixture, tag);
    let vol = unlock(&path);
    vol.write_file("/", "f", &body(10)).expect("write f");
    vol.commit_active_batch().expect("commit");
    let file = vol.open_file("/f").expect("open");
    assert_eq!(vol.file_size(&file).expect("size"), (CHUNKS * CHUNK) as u64);

    vol.delete_file("/f").expect("delete");
    vol.write_file("/", "f", &vec![7u8; 100]).expect("recreate with another size");
    vol.commit_active_batch().expect("commit");
    assert!(
        matches!(vol.file_size(&file), Err(LuksError::FileChanged)),
        "the size of a replaced file must not be answered from the new file"
    );
}

#[test]
fn file_size_comes_from_the_handle_on_ext4() {
    size_by_handle("gpt-luks.img", "size-ext4");
}

#[test]
fn file_size_comes_from_the_handle_on_btrfs() {
    size_by_handle("gpt-luks-btrfs.img", "size-btrfs");
}
