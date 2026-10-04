//! F8: the ext4 drive-health checker, graded by the kernel's `e2fsck -fn`.
//!
//! The expected class is never taken from the checker: it comes from the
//! oracle, through the same `leak_class::non_leak_lines` the writer sweeps
//! use. Kernel-made images (Linux wrote the files) are the false-red corpus.
//!
//! ```text
//! cargo test --test ext4_health
//! cargo test --features dangerous-write-support --test ext4_health
//! ```

#[path = "../common/temp_guard.rs"]
mod temp_guard;
use temp_guard::TempPath;

#[path = "../common/mod.rs"]
mod common;

#[path = "leak_class.rs"]
mod leak_class;

use luks_core::device::FileDevice;
use luks_core::fs::ext4::health::HealthStats;
use luks_core::fs::ext4::Ext4;
use luks_core::fs::health::{FindingKind, HealthReport, UnknownReason, Verdict};
use luks_core::luks::{self, LuksVolume};
use std::path::PathBuf;
use std::process::Command;

fn root() -> String {
    format!("{}/..", env!("CARGO_MANIFEST_DIR"))
}

fn scratch(tag: &str) -> TempPath {
    TempPath::new(std::env::temp_dir().join(format!("ext4-health-{tag}-{}.img", std::process::id())))
}

/// `e2fsck -fn` output (stdout + stderr) and whether it was clean.
#[track_caller]
fn e2fsck(img: &PathBuf) -> (bool, String) {
    assert!(common::oracle::gate(), "oracle required: this test cannot pass without e2fsck");
    let script = format!("{}/tools/verify-ext4.sh", root());
    let out = Command::new("bash").arg(&script).arg(img).output().expect("run verify-ext4.sh");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), text)
}

/// The class the kernel assigns: Clean, Leaks (only leak-class lines) or Corrupt.
fn oracle_class(clean: bool, text: &str) -> Verdict {
    if clean {
        Verdict::Clean
    } else if leak_class::non_leak_lines(text, &[]).is_empty() {
        Verdict::Leaks
    } else {
        Verdict::Corrupt
    }
}

fn check(img: &PathBuf) -> (HealthReport, HealthStats) {
    let dev = FileDevice::open(img).expect("open image read-only");
    let fs = Ext4::mount(dev).expect("mount");
    fs.check_health_stats(&|| false)
}

/// Build a kernel-populated image with `tools/make-ext4-kernel-image.sh`.
fn kernel_image(shape: &str) -> TempPath {
    assert!(common::oracle::gate(), "oracle required: the kernel makes these images");
    let img = scratch(&format!("k-{shape}"));
    let out = Command::new("bash")
        .arg(format!("{}/tools/make-ext4-kernel-image.sh", root()))
        .arg(shape)
        .arg(&*img)
        .output()
        .expect("run make-ext4-kernel-image.sh");
    assert!(out.status.success(), "building shape {shape} failed: {}", String::from_utf8_lossy(&out.stderr));
    img
}

/// (files, blocks) in use, from e2fsck's summary line
/// "N/M files (x% non-contiguous), U/T blocks".
fn summary(text: &str) -> (u64, u64) {
    let line = text.lines().find(|l| l.contains(" files (")).expect("e2fsck summary line");
    let used = |s: &str| s.trim().rsplit(|c: char| !c.is_ascii_digit() && c != '/').next().expect("field").split('/').next().expect("used").parse::<u64>().expect("number");
    let (files, blocks) = line.split_once(" files (").expect("summary shape");
    let blocks = blocks.split_once("), ").expect("blocks part").1;
    (used(files), used(blocks.trim_end_matches(" blocks")))
}

#[track_caller]
fn assert_kind(r: &HealthReport, kind: FindingKind) {
    assert!(r.findings.get(&kind).copied().unwrap_or(0) > 0, "expected {kind:?} in {:?}", r.findings);
}

const IMG_BYTES: u64 = 48 * 1024 * 1024;

/// Every shape the checker claims to prove, written by the Linux kernel. Each
/// must be clean to e2fsck AND Clean to the checker (never red, never amber),
/// and the scan must have looked at everything e2fsck counted.
#[test]
fn kernel_made_images_of_every_supported_shape_are_clean() {
    let (mut dx, mut shared, mut deep, mut indirect, mut tind) = (0, 0, 0, 0, 0);
    let shapes = ["default", "no64", "nores", "k1", "nosparse", "noflex", "nocsum", "nojournal", "ext3", "ext3x"];
    for shape in shapes {
        let img = kernel_image(shape);
        let (clean, text) = e2fsck(&img);
        assert!(clean, "{shape}: the kernel's own image must be e2fsck-clean:\n{text}");
        let (r, st) = check(&img);
        assert_eq!(r.verdict, Verdict::Clean, "{shape}: {:?} {:?} {:?}", r.unknown, r.findings, r.samples);
        let fs = Ext4::mount(FileDevice::open(&img).expect("open")).expect("mount");
        let sb = fs.superblock();
        let (files, blocks) = summary(&text);
        // Vacuity: the scan covered the whole fs and owns what e2fsck counts.
        assert_eq!(st.groups_scanned, IMG_BYTES.div_ceil(sb.block_size as u64).div_ceil(sb.blocks_per_group as u64), "{shape}: groups");
        assert_eq!(st.inodes_in_use, files, "{shape}: in-use inodes");
        assert_eq!(st.owned_blocks + sb.first_data_block as u64, blocks, "{shape}: owned blocks");
        dx += (st.dx_blocks > 0) as u32;
        shared += (st.shared_ea_blocks > 0) as u32;
        deep += (st.max_extent_depth >= 2) as u32;
        indirect += (st.indirect_inodes > 0) as u32;
        tind += (st.max_indirect_level == 3) as u32;
        if shape == "k1" {
            assert!(st.dx_blocks >= 3, "k1: expected a two-level htree, saw {} dx blocks", st.dx_blocks);
        }
        println!("shape {shape}: Clean, {st:?}");
    }
    // Proof the specials existed: every shape has an htree dir and a shared EA
    // block; all but the pure ext3 have a depth-2 extent tree; ext3 and ext3x
    // have block-mapped files, down to the triple-indirect level.
    let n = shapes.len() as u32;
    assert_eq!((dx, shared, deep, indirect, tind), (n, n, n - 1, 2, 2), "coverage: htree, shared EA, depth-2, block-mapped, TIND");
}

/// A3: what the checker was not graded against is Unknown, never red. The
/// oracle says each of these is a healthy image.
#[test]
fn unsupported_shapes_are_unknown_not_red() {
    for shape in ["sparse2", "bigalloc", "inline"] {
        let img = kernel_image(shape);
        let (clean, text) = e2fsck(&img);
        assert!(clean, "{shape}: the kernel's own image must be e2fsck-clean:\n{text}");
        let (r, _) = check(&img);
        assert_eq!((r.verdict, r.unknown), (Verdict::Unknown, Some(UnknownReason::Unsupported)), "{shape}");
    }
}

const FIXTURES: [&str; 6] =
    ["big-4k.img", "csum-uuid-4k.img", "dirtail-1k.img", "small-1k.img", "many-groups-1k.img", "ext2-1k.img"];

fn fixture_copy(name: &str, tag: &str) -> TempPath {
    let img = scratch(&format!("{tag}-{name}"));
    std::fs::copy(format!("{}/fixtures/ext4/{name}", root()), &*img).expect("copy fixture");
    img
}

#[test]
fn committed_fixtures_are_clean() {
    for name in FIXTURES {
        let img = fixture_copy(name, "fx");
        let (clean, text) = e2fsck(&img);
        assert!(clean, "{name}:\n{text}");
        let (r, st) = check(&img);
        assert_eq!(r.verdict, Verdict::Clean, "{name}: {:?} {:?}", r.unknown, r.findings);
        assert_eq!(summary(&text).0, st.inodes_in_use, "{name}");
    }
}

fn debugfs(img: &PathBuf, cmd: &str) -> String {
    let tool = "/opt/homebrew/opt/e2fsprogs/sbin/debugfs";
    assert!(std::path::Path::new(tool).exists(), "debugfs not found: brew install e2fsprogs");
    let out = Command::new(tool).args(["-w", "-R", cmd]).arg(img).output().expect("run debugfs");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "debugfs {cmd}: {text}");
    text
}

/// Physical block of logical block 0 of `path`.
fn first_block(img: &PathBuf, path: &str) -> u64 {
    let t = debugfs(img, &format!("bmap {path} 0"));
    t.lines().find_map(|l| l.trim().parse().ok()).unwrap_or_else(|| panic!("bmap output: {t}"))
}

/// Byte offset of an inode record, from `imap`.
fn inode_offset(img: &PathBuf, path: &str, block_size: u64) -> u64 {
    let t = debugfs(img, &format!("imap {path}"));
    let at = t.split("located at block ").nth(1).expect("imap output");
    let (blk, rest) = at.split_once(", offset 0x").expect("imap shape");
    blk.trim().parse::<u64>().expect("block") * block_size + u64::from_str_radix(rest.split_whitespace().next().expect("offset field"), 16).expect("offset")
}

fn flip(img: &PathBuf, offset: u64) {
    use std::os::unix::fs::FileExt;
    let f = std::fs::OpenOptions::new().read(true).write(true).open(img).expect("open image");
    let mut b = [0u8; 1];
    f.read_exact_at(&mut b, offset).expect("read byte");
    b[0] ^= 0x40;
    f.write_all_at(&b, offset).expect("write byte");
}

/// Injected damage, one per image, each graded blue -> red by the kernel too.
#[test]
fn injected_corruption_is_red_and_the_oracle_agrees() {
    const BS: u64 = 4096;
    type Inject = fn(&PathBuf);
    let cases: [(&str, Inject, FindingKind); 5] = [
        ("cross-linked block", |i| {
            let target = first_block(i, "/hello.txt");
            // block[5] is ee_start_lo of the inline extent of readme.md.
            debugfs(i, &format!("set_inode_field /docs/readme.md block[5] {target}"));
        }, FindingKind::DoubleOwnedBlock),
        ("bitmap bit cleared under a live file", |i| {
            let blk = first_block(i, "/big.bin");
            debugfs(i, &format!("freeb {blk}"));
        }, FindingKind::BlockInUseButFree),
        ("dirent to a cleared inode", |i| {
            debugfs(i, "clri /hello.txt");
        }, FindingKind::DirentToFreeInode),
        ("corrupted inode checksum", |i| {
            // i_atime of /hello.txt: inside the record, not in the checksum field.
            flip(i, inode_offset(i, "/hello.txt", BS) + 8);
        }, FindingKind::InodeChecksum),
        ("corrupted directory block checksum", |i| {
            // First byte of the first entry's name area of /docs.
            flip(i, first_block(i, "/docs") * BS + 8);
        }, FindingKind::DirChecksum),
    ];
    // The control: the pristine fixture is blue and the kernel agrees.
    let pristine = fixture_copy("big-4k.img", "inj-ctl");
    let (clean, text) = e2fsck(&pristine);
    assert!(clean, "{text}");
    assert_eq!(check(&pristine).0.verdict, Verdict::Clean);

    for (name, inject, kind) in cases {
        let img = fixture_copy("big-4k.img", "inj");
        inject(&img);
        let (clean, text) = e2fsck(&img);
        assert_eq!(oracle_class(clean, &text), Verdict::Corrupt, "{name}: the kernel must call this non-leak:\n{text}");
        let (r, _) = check(&img);
        assert_eq!(r.verdict, Verdict::Corrupt, "{name}: {:?} {:?}", r.unknown, r.findings);
        assert_kind(&r, kind);
    }
}

#[test]
fn a_cancelled_check_is_unknown_cancelled() {
    let img = fixture_copy("many-groups-1k.img", "cancel");
    let fs = Ext4::mount(FileDevice::open(&img).expect("open")).expect("mount");
    // Control: never cancelled, the same image is Clean (so Unknown is the cancel).
    assert_eq!(fs.check_health(&|| false).verdict, Verdict::Clean);
    let polls = std::sync::atomic::AtomicU32::new(0);
    let r = fs.check_health(&|| polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 3);
    assert_eq!((r.verdict, r.unknown), (Verdict::Unknown, Some(UnknownReason::Cancelled)));
    assert!(polls.load(std::sync::atomic::Ordering::SeqCst) >= 4, "the poll must have run, not been skipped");
}

/// The checker parses hostile bytes: no bit flip in the metadata may panic it.
#[test]
fn bit_flips_in_metadata_never_panic() {
    let orig = std::fs::read(format!("{}/fixtures/ext4/small-1k.img", root())).expect("read fixture");
    let regions = {
        let fs = Ext4::mount(FileDevice::open(format!("{}/fixtures/ext4/small-1k.img", root())).expect("open")).expect("mount");
        let bs = fs.block_size() as u64;
        let it = fs.groups()[0].inode_table * bs;
        let mut dirs = Vec::new();
        for path in ["/", "/docs"] {
            let ino = fs.resolve(path).expect("resolve");
            let first = u32::from_le_bytes(ino.block_area[20..24].try_into().expect("4 bytes")) as u64;
            dirs.push(first * bs);
        }
        // superblock + descriptors + bitmaps, the first inodes, two dir blocks
        vec![(1024u64, 256 * 1024u64), (it, 24 * 1024), (dirs[0], 1024), (dirs[1], 1024)]
    };
    let (mut x, mut non_clean, mut mounted) = (0x2545F4914F6CDD1Du64, 0u32, 0u32);
    for _ in 0..400 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let (base, len) = regions[(x % regions.len() as u64) as usize];
        let off = (base + (x >> 8) % len) as usize;
        let mut bytes = orig.clone();
        bytes[off] ^= 1 << ((x >> 40) % 8);
        let dev = common::mem_device::MemoryDevice::from_bytes(bytes);
        let Ok(fs) = Ext4::mount(dev) else { continue };
        mounted += 1;
        let r = fs.check_health(&|| false);
        non_clean += (r.verdict != Verdict::Clean) as u32;
    }
    assert!(mounted > 300, "vacuity: most flipped images must still mount ({mounted}/400)");
    assert!(non_clean > 20, "vacuity: flips must be able to change the verdict ({non_clean})");
}

const E5_NAME: &str = "stick-p1-20261002.img";

fn loud_skip(reason: &str) {
    use std::io::Write;
    use std::os::fd::FromRawFd;
    // Straight to fd 2: libtest captures eprintln! for passing tests.
    let mut f = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(2) });
    f.write_all(format!("\n*** E5 SKIPPED: {reason} ***\n").as_bytes()).expect("write to stderr");
}

/// Open the LUKS2 stick image read-only and judge the ext4 inside.
fn check_luks_image(path: &std::path::Path) -> (HealthReport, HealthStats) {
    let dev = FileDevice::open(path).expect("open image read-only");
    let header = luks::read_from(&dev, 0).expect("LUKS header");
    let volume = LuksVolume::open(&dev, 0, None, &header, b"test").expect("unlock");
    let fs = Ext4::mount(volume).expect("mount ext4 inside the container");
    fs.check_health_stats(&|| false)
}

/// E5: the damaged real stick image is amber with exactly the leak the kernel
/// found; a repaired copy is blue. Never writes to the original.
#[test]
fn e5_stick_image_is_amber_and_its_repaired_copy_is_blue() {
    let home = std::env::var("HOME").expect("HOME");
    let orig = std::path::Path::new(&home).join(E5_NAME);
    if !orig.exists() {
        loud_skip(&format!("{E5_NAME} is absent from the home directory"));
        return;
    }
    assert!(common::oracle::gate(), "oracle required");
    let (r, st) = check_luks_image(&orig);
    println!("E5 damaged: {:?} {:?} {:?} elapsed={:?} stats={st:?}", r.verdict, r.findings, r.samples, r.elapsed);
    assert_eq!(r.verdict, Verdict::Leaks, "{:?} {:?}", r.unknown, r.findings);
    assert_eq!(r.findings.get(&FindingKind::LeakedBlocks), Some(&1869));
    assert_eq!(r.findings.get(&FindingKind::LeakedInodes), Some(&1));
    assert_eq!(r.samples.get(&FindingKind::LeakedInodes).map(|v| v.as_slice()), Some(&[37u64][..]));
    assert!(st.groups_scanned > 1 && st.inodes_in_use > 10, "vacuity: {st:?}");
    let elapsed_damaged = r.elapsed;

    // A copy, repaired with the project script; both files are guarded.
    let copy = TempPath::new(std::env::temp_dir().join(format!("ext4-health-e5-{}.img", std::process::id())));
    let repaired = TempPath::new(std::env::temp_dir().join(format!("ext4-health-e5-{}.img.repaired", std::process::id())));
    // `cp -c` clones on APFS: a real, independent copy that costs no disk
    // until written, and the disk here is too tight for two more 4 GiB files.
    let cp = Command::new("cp").arg("-c").arg(&orig).arg(&*copy).output().expect("run cp -c");
    assert!(cp.status.success(), "cloning the stick image failed: {}", String::from_utf8_lossy(&cp.stderr));
    let out = Command::new("bash")
        .arg(format!("{}/tools/repair-image.sh", root()))
        .arg(&*copy)
        .output()
        .expect("run repair-image.sh");
    assert!(out.status.success(), "repair failed: {}", String::from_utf8_lossy(&out.stderr));
    let (r, _) = check_luks_image(&repaired);
    println!("E5 repaired: {:?} {:?} elapsed={:?} (damaged took {:?})", r.verdict, r.findings, r.elapsed, elapsed_damaged);
    assert_eq!(r.verdict, Verdict::Clean, "{:?} {:?}", r.unknown, r.findings);
}

#[cfg(feature = "dangerous-write-support")]
mod after_our_writes {
    use super::*;
    use luks_core::fs::FileType;

    /// Twenty files, a directory with files in it, a delete and a rename, all
    /// through our writer: the kernel finds the image clean, and so does the
    /// checker, on every fixture with checksums.
    #[test]
    fn images_after_our_own_writes_are_clean() {
        for name in ["big-4k.img", "many-groups-1k.img", "dirtail-1k.img"] {
            let img = fixture_copy(name, "own");
            {
                let len = std::fs::metadata(&*img).expect("stat").len();
                let dev = FileDevice::open_writable(img.to_str().expect("utf8"), len).expect("open writable");
                let mut fs = Ext4::mount(dev).expect("mount");
                for i in 0..20u32 {
                    let data: Vec<u8> = (0..(i as usize + 1) * 3001).map(|b| (b * 7 + i as usize) as u8).collect();
                    let mut w = fs.begin_file(data.len() as u64).expect("begin");
                    fs.write_chunk(&mut w, &data).expect("chunk");
                    fs.finish_file(w, 2, &format!("w{i}.bin"), FileType::Regular).expect("finish");
                }
                let dir = fs.create_directory(2, "made").expect("mkdir") as u64;
                for i in 0..5u32 {
                    let mut w = fs.begin_file(5000).expect("begin");
                    fs.write_chunk(&mut w, &[i as u8; 5000]).expect("chunk");
                    fs.finish_file(w, dir, &format!("in{i}"), FileType::Regular).expect("finish");
                }
                fs.delete_file("/w3.bin").expect("delete");
                fs.rename("/", "w4.bin", "/", "renamed.bin").expect("rename");
                fs.flush().expect("flush");
            }
            let (clean, text) = e2fsck(&img);
            assert!(clean, "{name}: kernel rejected our writes:\n{text}");
            let (r, st) = check(&img);
            assert_eq!(r.verdict, Verdict::Clean, "{name}: {:?} {:?} {:?}", r.unknown, r.findings, r.samples);
            assert_eq!(st.inodes_in_use, summary(&text).0, "{name}");
        }
    }
}

