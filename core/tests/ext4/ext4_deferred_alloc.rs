//! F2a: ext4 deferred allocation. A file transfer promises inode and blocks in
//! memory only; nothing about it touches on-disk metadata until `finish_file`
//! publishes it inside a dirty -> clean superblock bracket.
//!
//! Everything here is graded two ways, and never by our own reader alone:
//! the kernel's `e2fsck -fn` for consistency, and the raw `s_state` bytes of
//! the image for the dirty flag (e2fsck cannot report a flag it does not care
//! about).
//!
//! ```text
//! cargo test -p luks_core --features dangerous-write-support --test ext4_deferred_alloc
//! ```
#![cfg(feature = "dangerous-write-support")]

#[path = "../common/temp_guard.rs"]
mod temp_guard;
use temp_guard::{TempPath};

#[path = "../common/mod.rs"]
mod common;

use common::fail_after::FailAfterNWrites;
use luks_core::device::{FileDevice, ReadAt, WriteAt};
use luks_core::error::{LuksError, Result};
use luks_core::fs::ext4::Ext4;
use luks_core::fs::FileType;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

const BS: usize = 4096;
/// The two 4 KiB fixtures: one without and one with `metadata_csum`.
const FIXTURES: [&str; 2] = ["big-4k.img", "csum-uuid-4k.img"];
const STATE_OFFSET: u64 = 1024 + 0x3A;
const VALID_FS: u16 = 0x0001;

fn scratch(fixture: &str, tag: &str) -> TempPath {
    let src = format!("{}/../fixtures/ext4/{fixture}", env!("CARGO_MANIFEST_DIR"));
    let dst = std::env::temp_dir().join(format!("ext4-f2a-{tag}-{}-{fixture}", std::process::id()));
    std::fs::copy(src, &dst).expect("copy fixture");
    TempPath::new(dst)
}

fn dev(path: &PathBuf) -> FileDevice {
    let len = std::fs::metadata(path).expect("stat scratch").len();
    FileDevice::open_writable(path.to_str().expect("utf8 path"), len).expect("open writable")
}

/// Kernel verdict: `Ok(())` if e2fsck is clean, else its full output.
#[track_caller]
fn grade(img: &PathBuf) -> std::result::Result<(), String> {
    assert!(common::oracle::gate(), "oracle required: this test cannot pass without e2fsck");
    let script = format!("{}/../tools/verify-ext4.sh", env!("CARGO_MANIFEST_DIR"));
    let out = Command::new("bash").arg(&script).arg(img).output().expect("run verify-ext4.sh");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() { Ok(()) } else { Err(text) }
}

#[track_caller]
fn assert_kernel_clean(img: &PathBuf, what: &str) {
    if let Err(text) = grade(img) {
        panic!("{what}: e2fsck is not clean:\n{text}");
    }
}

/// The raw `s_state` of an image, read from the file, not through the driver.
fn disk_state(img: &PathBuf) -> u16 {
    let f = std::fs::File::open(img).expect("open image");
    let mut b = [0u8; 2];
    f.read_exact_at(&mut b, STATE_OFFSET).expect("read s_state");
    u16::from_le_bytes(b)
}

fn payload(blocks: usize) -> Vec<u8> {
    (0..blocks * BS).map(|i| ((i * 31 + 7) % 251) as u8).collect()
}

// --- probe device ------------------------------------------------------------

struct Rec {
    offset: u64,
    len: usize,
    /// On-disk `s_state`, read just before this write landed.
    state_before: u16,
}

/// A file device that logs every write with the on-disk dirty state at the
/// moment it was issued, and counts flushes.
struct Probe {
    inner: FileDevice,
    peek: std::fs::File,
    log: Mutex<Vec<Rec>>,
    flushes: AtomicUsize,
}

impl Probe {
    fn new(path: &PathBuf) -> Self {
        Probe {
            inner: dev(path),
            peek: std::fs::File::open(path).expect("open peek"),
            log: Mutex::new(Vec::new()),
            flushes: AtomicUsize::new(0),
        }
    }
    fn writes(&self) -> usize {
        self.log.lock().expect("probe log").len()
    }
    fn flush_count(&self) -> usize {
        self.flushes.load(Ordering::SeqCst)
    }
}

impl ReadAt for Probe {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_at(offset, buf)
    }
    fn len(&self) -> Option<u64> {
        self.inner.len()
    }
}

impl WriteAt for Probe {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut b = [0u8; 2];
        self.peek.read_exact_at(&mut b, STATE_OFFSET).expect("peek s_state");
        self.log.lock().expect("probe log").push(Rec {
            offset,
            len: buf.len(),
            state_before: u16::from_le_bytes(b),
        });
        self.inner.write_at(offset, buf)
    }
    fn flush(&self) -> Result<()> {
        self.flushes.fetch_add(1, Ordering::SeqCst);
        self.inner.flush()
    }
}

/// Byte ranges of everything the allocator owns: the superblock, the group
/// descriptor table, and each group's two bitmaps and inode table.
struct Meta {
    sb: (u64, u64),
    other: Vec<(u64, u64)>,
}

fn meta_of<D: ReadAt>(fs: &Ext4<D>) -> Meta {
    let sb = fs.superblock();
    let bs = sb.block_size as u64;
    let gdt = sb.group_desc_offset();
    let mut other = vec![(gdt, gdt + fs.groups().len() as u64 * sb.group_desc_size() as u64)];
    for g in fs.groups() {
        other.push((g.block_bitmap * bs, (g.block_bitmap + 1) * bs));
        other.push((g.inode_bitmap * bs, (g.inode_bitmap + 1) * bs));
        let table = sb.inodes_per_group as u64 * sb.inode_size as u64;
        other.push((g.inode_table * bs, g.inode_table * bs + table));
    }
    Meta { sb: (1024, 2048), other }
}

fn overlaps(r: &Rec, range: (u64, u64)) -> bool {
    r.offset < range.1 && r.offset + r.len as u64 > range.0
}

impl Meta {
    fn touches_sb(&self, r: &Rec) -> bool {
        overlaps(r, self.sb)
    }
    fn touches_other(&self, r: &Rec) -> bool {
        self.other.iter().any(|&m| overlaps(r, m))
    }
}

// --- begin / chunks / abandon touch no metadata -------------------------------

macro_rules! no_metadata_before_finish {
    ($name:ident, $begin:expr, $chunks:expr) => {
        #[test]
        fn $name() {
            for fixture in FIXTURES {
                let img = scratch(fixture, stringify!($name));
                let (writes_after_chunks, flushes_after_chunks);
                {
                    let mut fs = Ext4::mount(Probe::new(&img)).expect("mount");
                    let meta = meta_of(&fs);
                    let mut w = $begin(&mut fs);
                    let data = payload($chunks);
                    for chunk in data.chunks(BS * 3 + 17) {
                        fs.write_chunk(&mut w, chunk).expect("write_chunk");
                    }
                    {
                        let log = fs.device_mut().log.lock().expect("log");
                        assert!(!log.is_empty(), "vacuity: the chunks must reach the device");
                        let bad: Vec<_> = log
                            .iter()
                            .filter(|r| meta.touches_sb(r) || meta.touches_other(r))
                            .map(|r| (r.offset, r.len))
                            .collect();
                        assert!(
                            bad.is_empty(),
                            "{fixture}: begin/write_chunk wrote allocator metadata: {bad:?}"
                        );
                    }
                    writes_after_chunks = fs.device_mut().writes();
                    flushes_after_chunks = fs.device_mut().flush_count();
                    fs.abandon_file(w);
                    assert_eq!(
                        fs.device_mut().writes(),
                        writes_after_chunks,
                        "{fixture}: abandon_file must not write"
                    );
                    assert_eq!(
                        fs.device_mut().flush_count(),
                        flushes_after_chunks,
                        "{fixture}: abandon_file must not flush"
                    );
                }
                assert_kernel_clean(&img, &format!("{fixture}: after abandon"));
                assert_eq!(disk_state(&img) & VALID_FS, VALID_FS, "{fixture}: must read clean");
            }
        }
    };
}

no_metadata_before_finish!(
    sized_begin_and_chunks_write_no_metadata,
    |fs: &mut Ext4<Probe>| fs.begin_file(64 * BS as u64).expect("begin_file"),
    40
);
no_metadata_before_finish!(
    streaming_begin_and_growth_write_no_metadata,
    |fs: &mut Ext4<Probe>| fs.begin_file_streaming().expect("begin_file_streaming"),
    40
);

/// Abandon must be valid on a device that is already gone: no I/O at all.
#[test]
fn abandon_on_a_dead_device_touches_nothing() {
    let img = scratch("big-4k.img", "dead-abandon");
    {
        let mut fs = Ext4::mount(FailAfterNWrites::new(dev(&img), usize::MAX)).expect("mount");
        let mut w = fs.begin_file(32 * BS as u64).expect("begin_file");
        fs.write_chunk(&mut w, &payload(8)).expect("chunk");
        fs.device_mut().budget.store(0, Ordering::SeqCst);
        let attempted = fs.device_mut().attempted.load(Ordering::SeqCst);
        fs.abandon_file(w);
        assert_eq!(
            fs.device_mut().attempted.load(Ordering::SeqCst),
            attempted,
            "abandon_file attempted device writes"
        );
    }
    assert_kernel_clean(&img, "abandon on a dead device");
}

// --- the publish window -------------------------------------------------------

/// A full transfer, observed: allocator metadata is written only while the
/// disk says "not clean", the first superblock write is the one that says it,
/// and the last write of all is the one that takes it back.
#[test]
fn publish_writes_metadata_only_inside_the_dirty_bracket() {
    for fixture in FIXTURES {
        let img = scratch(fixture, "bracket");
        {
            let mut fs = Ext4::mount(Probe::new(&img)).expect("mount");
            let meta = meta_of(&fs);
            let data = payload(40);
            let mut w = fs.begin_file(data.len() as u64).expect("begin");
            for chunk in data.chunks(BS * 7) {
                fs.write_chunk(&mut w, chunk).expect("chunk");
            }
            let before_finish = fs.device_mut().writes();
            fs.finish_file(w, 2, "published.bin", FileType::Regular).expect("finish");

            let log = fs.device_mut().log.lock().expect("log");
            let window = &log[before_finish..];
            let first_sb = window.iter().position(|r| meta.touches_sb(r)).expect("an sb write");
            assert_eq!(
                window[first_sb].state_before & VALID_FS,
                VALID_FS,
                "{fixture}: the first superblock write must be the one that marks the fs dirty"
            );
            for r in window.iter().filter(|r| meta.touches_other(r)) {
                assert_eq!(
                    r.state_before & VALID_FS,
                    0,
                    "{fixture}: metadata write at {} ran while the disk read clean",
                    r.offset
                );
            }
            let last = window.last().expect("window not empty");
            assert!(meta.touches_sb(last), "{fixture}: the last write must be the clean-mark");
            assert_eq!(last.state_before & VALID_FS, 0, "{fixture}: last write starts from dirty");
            assert!(
                window.iter().filter(|r| meta.touches_other(r)).count() >= 4,
                "vacuity: bitmaps, descriptors and inode must all be in the window"
            );
        }
        assert_eq!(disk_state(&img) & VALID_FS, VALID_FS, "{fixture}: clean after finish");
        assert_kernel_clean(&img, &format!("{fixture}: after finish"));
        let fs = Ext4::mount(dev(&img)).expect("remount");
        assert_eq!(fs.read_file("/published.bin").expect("read back"), payload(40));
    }
}

/// A name that cannot be linked is discovered before any metadata is written.
#[test]
fn a_predictable_publish_failure_writes_no_metadata() {
    let img = scratch("big-4k.img", "dupname");
    {
        let mut fs = Ext4::mount(Probe::new(&img)).expect("mount");
        let meta = meta_of(&fs);
        fs.create_file(2, "taken.bin", b"first", FileType::Regular).expect("seed file");
        let mark = fs.device_mut().writes();

        let data = payload(8);
        let mut w = fs.begin_file(data.len() as u64).expect("begin");
        fs.write_chunk(&mut w, &data).expect("chunk");
        let err = fs.finish_file(w, 2, "taken.bin", FileType::Regular).expect_err("duplicate");
        assert!(matches!(err, LuksError::AlreadyExists(_)), "wrong error: {err:?}");

        let log = fs.device_mut().log.lock().expect("log");
        let bad: Vec<_> = log[mark..]
            .iter()
            .filter(|r| meta.touches_sb(r) || meta.touches_other(r))
            .map(|r| (r.offset, r.len))
            .collect();
        assert!(bad.is_empty(), "a refused publish wrote metadata: {bad:?}");
    }
    assert_eq!(disk_state(&img) & VALID_FS, VALID_FS);
    assert_kernel_clean(&img, "after refused publish");
}

// --- reservations are visible to every allocator -----------------------------

#[test]
fn immediate_allocators_never_take_a_reserved_block() {
    for fixture in FIXTURES {
        let img = scratch(fixture, "exclusion");
        let data = payload(48);
        {
            let mut fs = Ext4::mount(dev(&img)).expect("mount");
            let mut w = fs.begin_file(data.len() as u64).expect("begin");
            // On disk the reserved blocks are free, so a first-fit allocator
            // that has not heard of the reservation hands them straight out.
            fs.create_directory(2, "d").expect("mkdir during an open writer");
            fs.create_file(2, "f2a-small.bin", &payload(5), FileType::Regular).expect("create_file");
            fs.create_file(2, "f2a-other.bin", &payload(20), FileType::Regular).expect("create_file");
            for chunk in data.chunks(BS * 9) {
                fs.write_chunk(&mut w, chunk).expect("chunk");
            }
            fs.finish_file(w, 2, "xfer.bin", FileType::Regular).expect("finish");
        }
        assert_kernel_clean(&img, &format!("{fixture}: interleaved allocation"));
        let fs = Ext4::mount(dev(&img)).expect("remount");
        assert_eq!(fs.read_file("/xfer.bin").expect("read big"), data);
        assert_eq!(fs.read_file("/f2a-small.bin").expect("read small"), payload(5));
        assert_eq!(fs.read_file("/f2a-other.bin").expect("read other"), payload(20));
    }
}

/// Someone else claims a reserved block on disk (a second mount, standing in
/// for any other writer of the medium). Publishing must notice and refuse
/// rather than double-allocate.
#[test]
fn a_reserved_bit_claimed_on_disk_fails_the_publish() {
    let img = scratch("big-4k.img", "assert-clear");
    let thief_data = payload(48);
    {
        let mut fs = Ext4::mount(dev(&img)).expect("mount");
        let data = payload(48);
        let mut w = fs.begin_file(data.len() as u64).expect("begin");
        fs.write_chunk(&mut w, &data).expect("chunk");

        // Same fixture, same first-fit goal: the thief lands on exactly the
        // blocks `w` reserved, because they are still free on disk.
        let mut thief = Ext4::mount(dev(&img)).expect("second mount");
        thief
            .create_file(2, "thief.bin", &thief_data, FileType::Regular)
            .expect("thief create_file");
        thief.flush().expect("thief flush");

        let err = fs
            .finish_file(w, 2, "victim.bin", FileType::Regular)
            .expect_err("publishing over a claimed block must fail");
        assert!(matches!(err, LuksError::CorruptFs(_)), "wrong error: {err:?}");
    }
    assert_eq!(disk_state(&img) & VALID_FS, VALID_FS, "a refused publish must leave the fs clean");
    assert_kernel_clean(&img, "after refused publish over a claimed block");
    let fs = Ext4::mount(dev(&img)).expect("remount");
    assert_eq!(fs.read_file("/thief.bin").expect("thief intact"), thief_data);
    assert!(fs.read_file("/victim.bin").is_err(), "the refused file must not exist");
}

// --- immediate-path operations are bracketed too -----------------------------

#[test]
fn create_file_writes_metadata_only_inside_the_dirty_bracket() {
    let img = scratch("big-4k.img", "create-bracket");
    {
        let mut fs = Ext4::mount(Probe::new(&img)).expect("mount");
        let meta = meta_of(&fs);
        fs.create_file(2, "a.bin", &payload(12), FileType::Regular).expect("create_file");
        let log = fs.device_mut().log.lock().expect("log");
        let first_sb = log.iter().position(|r| meta.touches_sb(r)).expect("an sb write");
        assert_eq!(log[first_sb].state_before & VALID_FS, VALID_FS, "first sb write marks dirty");
        assert!(
            log.iter().take(first_sb).all(|r| !meta.touches_other(r)),
            "metadata was written before the fs was marked dirty"
        );
        for r in log.iter().filter(|r| meta.touches_other(r)) {
            assert_eq!(r.state_before & VALID_FS, 0, "metadata write at {} ran while clean", r.offset);
        }
    }
    assert_eq!(disk_state(&img) & VALID_FS, VALID_FS);
    assert_kernel_clean(&img, "after create_file");
}

/// A refusal known up-front (duplicate name) must not leave the fs dirty or
/// write any metadata.
#[test]
fn a_refused_create_file_writes_no_metadata_and_stays_clean() {
    let img = scratch("big-4k.img", "create-refused");
    {
        let mut fs = Ext4::mount(Probe::new(&img)).expect("mount");
        let meta = meta_of(&fs);
        fs.create_file(2, "a.bin", b"x", FileType::Regular).expect("create_file");
        let mark = fs.device_mut().writes();
        let err = fs.create_file(2, "a.bin", b"y", FileType::Regular).expect_err("duplicate");
        assert!(matches!(err, LuksError::AlreadyExists(_)), "wrong error: {err:?}");
        let log = fs.device_mut().log.lock().expect("log");
        let bad: Vec<_> = log[mark..]
            .iter()
            .filter(|r| meta.touches_sb(r) || meta.touches_other(r))
            .map(|r| (r.offset, r.len))
            .collect();
        assert!(bad.is_empty(), "a refused create wrote metadata: {bad:?}");
    }
    assert_eq!(disk_state(&img) & VALID_FS, VALID_FS);
    assert_kernel_clean(&img, "after refused create_file");
}

/// Crash sweep over the immediate-path operations: the device dies after N
/// writes, for every N. The invariant is the dirty flag's whole point: an
/// image is either consistent to the kernel, or it says it is not clean. A
/// consistent-looking-clean image that e2fsck rejects is the failure.
/// (Which crash states are merely leaks is F2b's concern, not this test's.)
fn sweep_immediate<F>(tag: &str, prep: fn(&mut Ext4<FileDevice>), op: F)
where
    F: Fn(&mut Ext4<FailAfterNWrites>) -> std::result::Result<(), LuksError>,
{
    let total = {
        let img = scratch("big-4k.img", &format!("{tag}-count"));
        {
            let mut fs = Ext4::mount(dev(&img)).expect("mount");
            prep(&mut fs);
        }
        let mut fs = Ext4::mount(FailAfterNWrites::new(dev(&img), usize::MAX)).expect("mount");
        op(&mut fs).expect("the op must succeed with an unlimited budget");
        let n = fs.device_mut().attempted.load(Ordering::SeqCst);
        assert!(n >= 2, "{tag}: suspiciously few writes: {n}");
        n
    };
    let mut lying = Vec::new();
    let mut failures = 0;
    for n in 0..=total {
        let img = scratch("big-4k.img", &format!("{tag}-n{n}"));
        {
            let mut fs = Ext4::mount(dev(&img)).expect("mount");
            prep(&mut fs);
        }
        let result = {
            let mut fs = Ext4::mount(FailAfterNWrites::new(dev(&img), n)).expect("mount");
            op(&mut fs)
        };
        match &result {
            Ok(()) => {
                assert_eq!(n, total, "{tag}: succeeded with fewer writes than it needs ({n}/{total})");
            }
            Err(_) => failures += 1,
        }
        let dirty = disk_state(&img) & VALID_FS == 0;
        if let Err(text) = grade(&img) {
            if !dirty {
                let first = text.lines().find(|l| l.contains("differences") || l.contains("count")).unwrap_or("(see e2fsck)");
                lying.push(format!("N={n}/{total}: marked clean but e2fsck says: {first}"));
            }
        } else if result.is_ok() {
            assert!(!dirty, "{tag}: a successful op left the fs marked dirty");
        }
    }
    assert!(failures > 0, "vacuity: no budget produced a failure");
    assert!(lying.is_empty(), "{tag}: {} images look clean but are not:\n{}", lying.len(), lying.join("\n"));
}

fn prep_noop(_fs: &mut Ext4<FileDevice>) {}
fn prep_file(fs: &mut Ext4<FileDevice>) {
    fs.create_file(2, "victim.bin", &payload(6), FileType::Regular).expect("prep file");
    fs.flush().expect("prep flush");
}

#[test]
fn sweep_create_file_never_looks_clean_while_inconsistent() {
    sweep_immediate("sw-create", prep_noop, |fs| {
        fs.create_file(2, "n.bin", &payload(10), FileType::Regular).map(|_| ())
    });
}

#[test]
fn sweep_create_directory_never_looks_clean_while_inconsistent() {
    sweep_immediate("sw-mkdir", prep_noop, |fs| fs.create_directory(2, "dir").map(|_| ()));
}

#[test]
fn sweep_delete_file_never_looks_clean_while_inconsistent() {
    sweep_immediate("sw-delete", prep_file, |fs| fs.delete_file("/victim.bin"));
}

#[test]
fn sweep_rename_never_looks_clean_while_inconsistent() {
    sweep_immediate("sw-rename", prep_file, |fs| fs.rename("/", "victim.bin", "/", "renamed.bin"));
}

// --- error identity -----------------------------------------------------------

/// A transport failure at any write inside the publish window must reach the
/// caller still classified as a fence, so the session ends instead of
/// continuing on a dead device.
#[test]
fn a_transport_failure_inside_publish_still_fences_the_session() {
    let mut failures = 0;
    for k in 0..9 {
        let img = scratch("big-4k.img", &format!("fence-{k}"));
        let mut fs = Ext4::mount(FailAfterNWrites::new(dev(&img), usize::MAX)).expect("mount");
        let data = payload(8);
        let mut w = fs.begin_file(data.len() as u64).expect("begin");
        fs.write_chunk(&mut w, &data).expect("chunk");
        fs.device_mut().budget.store(k, Ordering::SeqCst);
        let err = fs
            .finish_file(w, 2, "fenced.bin", FileType::Regular)
            .expect_err("a publish needs 10 writes; fewer must fail");
        failures += 1;
        assert!(err.fences_write_session(), "budget {k}: the transport error lost its class: {err:?}");
        assert!(fs.take_cleanup_failure().is_none(), "budget {k}: publish has no rollback to fail");
    }
    assert_eq!(failures, 9, "vacuity");
}

/// A transport primary skips the device rollback entirely: after the first
/// failed write nothing more is attempted (the device is gone).
#[test]
fn a_transport_failure_skips_the_device_rollback() {
    for budget in 1..10 {
        let img = scratch("big-4k.img", &format!("skip-{budget}"));
        let mut fs = Ext4::mount(FailAfterNWrites::new(dev(&img), budget)).expect("mount");
        let err = fs
            .create_file(2, "gone.bin", &payload(12), FileType::Regular)
            .expect_err("budget too small to finish");
        assert!(err.fences_write_session(), "budget {budget}: {err:?}");
        let attempted = fs.device_mut().attempted.load(Ordering::SeqCst);
        assert!(
            attempted <= budget + 1,
            "budget {budget}: {attempted} writes attempted; a rollback ran on a dead device"
        );
        assert!(fs.take_cleanup_failure().is_none());
    }
}

/// A refusal (no space) whose rollback then hits a dead device: the caller
/// still gets the refusal, and the rollback failure is attached.
#[test]
fn a_rollback_failure_never_replaces_the_primary_error() {
    let img = scratch("big-4k.img", "rollback-fail");
    // 1 write marks dirty, 4 more claim the inode; then the allocation of the
    // data blocks is refused and its rollback has no device left.
    let mut fs = Ext4::mount(FailAfterNWrites::new(dev(&img), 5)).expect("mount");
    let too_big = vec![0u8; 200 << 20];
    let err = fs
        .create_file(2, "huge.bin", &too_big, FileType::Regular)
        .expect_err("cannot fit");
    assert!(matches!(err, LuksError::FilesystemFull), "primary must be preserved: {err:?}");
    let cleanup = fs.take_cleanup_failure().expect("the failed rollback must be recorded");
    assert!(cleanup.fences_write_session(), "the cleanup failure is the transport error: {cleanup:?}");
    assert!(fs.take_cleanup_failure().is_none(), "taken once");
}

// --- statfs -------------------------------------------------------------------

#[test]
fn statfs_does_not_count_reserved_space_as_free() {
    let img = scratch("big-4k.img", "statfs");
    let mut fs = Ext4::mount(dev(&img)).expect("mount");
    let before = fs.statfs().expect("statfs");
    let w = fs.begin_file(40 * BS as u64).expect("begin");
    let during = fs.statfs().expect("statfs");
    assert_eq!(before.free_bytes - during.free_bytes, 40 * BS as u64);
    assert_eq!(before.free_inodes - during.free_inodes, 1);
    assert!(before.available_bytes > during.available_bytes, "available must drop too");
    fs.abandon_file(w);
    let after = fs.statfs().expect("statfs");
    assert_eq!(after.free_bytes, before.free_bytes);
    assert_eq!(after.free_inodes, before.free_inodes);
    assert_eq!(after.available_bytes, before.available_bytes);
}
