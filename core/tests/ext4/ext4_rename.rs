//! Testing ext4 same-directory file rename (`rename_file`).
//!
//! ```text
//! cargo test --release --features dangerous-write-support --test ext4_rename
//! ```
#![cfg(feature = "dangerous-write-support")]

#[path = "../common/mod.rs"]
mod common;

use common::fail_after::FailAfterNWrites;
use luks_core::device::FileDevice;
use luks_core::error::LuksError;
use luks_core::fs::ext4::Ext4;
use luks_core::fs::FileType;
use std::process::Command;
use std::sync::atomic::Ordering;

#[path = "leak_class.rs"]
mod leak_class;

fn fixture(name: &str) -> String {
    format!("{}/../fixtures/ext4/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn scratch(name: &str, test: &str) -> String {
    let dst = std::env::temp_dir().join(format!("ext4-rename-{test}-{name}"));
    std::fs::copy(fixture(name), &dst).expect("copy fixture");
    dst.to_string_lossy().into_owned()
}

fn open(path: &str) -> Ext4<FileDevice> {
    let len = std::fs::metadata(path).expect("stat scratch").len();
    Ext4::mount(FileDevice::open_writable(path, len).expect("open writable")).expect("mount")
}

fn verify_script() -> Option<String> {
    if !common::oracle::gate() {
        return None;
    }
    let script = format!("{}/../tools/verify-ext4.sh", env!("CARGO_MANIFEST_DIR"));
    assert!(
        std::path::Path::new(&script).exists(),
        "oracle gate passed, but verify-ext4.sh was not found at {script}"
    );
    Some(script)
}

const FIXTURES: [&str; 3] = ["big-4k.img", "csum-uuid-4k.img", "many-groups-1k.img"];

#[test]
fn same_directory_rename_and_collision_replacement_leave_e2fsck_clean() {
    let Some(script) = verify_script() else {
        eprintln!("skipping: colima is not running");
        return;
    };

    for name in FIXTURES {
        let path = scratch(name, "rename-clean");
        let content_a = b"content of file a\n";
        let content_b = b"content of file b\n";

        // 1. Create file_a.txt and file_b.txt in root
        {
            let mut fs = open(&path);
            fs.create_file(2, "file_a.txt", content_a, FileType::Regular)
                .expect("create file_a");
            fs.create_file(2, "file_b.txt", content_b, FileType::Regular)
                .expect("create file_b");
            fs.flush().expect("flush");
        }

        // 2. Rename file_a.txt to file_c.txt (no collision)
        {
            let mut fs = open(&path);
            fs.rename_file(2, "file_a.txt", "file_c.txt")
                .expect("rename file_a to file_c");
            fs.flush().expect("flush");
        }

        // Verify file_a is gone and file_c has content_a
        {
            let fs = Ext4::mount(FileDevice::open(&path).expect("reopen")).expect("mount");
            let entries = fs.list_dir("/").expect("list root");
            assert!(!entries.iter().any(|e| e.name == "file_a.txt"));
            assert!(entries.iter().any(|e| e.name == "file_c.txt"));
            let got_c = fs.read_file("/file_c.txt").expect("read file_c");
            assert_eq!(got_c, content_a);
        }

        // 3. Rename file_c.txt to file_b.txt (collision replacement)
        {
            let mut fs = open(&path);
            fs.rename_file(2, "file_c.txt", "file_b.txt")
                .expect("overwrite file_b with file_c");
            fs.flush().expect("flush");
        }

        // Verify file_c is gone and file_b now has content_a
        {
            let fs = Ext4::mount(FileDevice::open(&path).expect("reopen")).expect("mount");
            let entries = fs.list_dir("/").expect("list root");
            assert!(!entries.iter().any(|e| e.name == "file_c.txt"));
            assert!(entries.iter().any(|e| e.name == "file_b.txt"));
            let got_b = fs.read_file("/file_b.txt").expect("read file_b");
            assert_eq!(got_b, content_a, "{name}: overwritten file content mismatch");
        }

        // 4. Verify e2fsck is completely clean
        let out = Command::new("bash")
            .arg(&script)
            .arg(&path)
            .output()
            .expect("run verifier");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.status.success(),
            "{name}: e2fsck failed after rename:\n{text}"
        );
    }
}

#[test]
fn rename_refusal_gates() {
    let path = scratch("csum-uuid-4k.img", "rename-gates");

    let _sub_ino = {
        let mut fs = open(&path);
        let sub = fs.create_directory(2, "some_dir").expect("mkdir");
        fs.create_file(2, "some_file.txt", b"data\n", FileType::Regular)
            .expect("create file");
        fs.flush().expect("flush");
        sub
    };

    let mut fs = open(&path);

    // Refusal 1: Directory rename by name
    let err_dir = fs.rename_file(2, "some_dir", "new_dir_name");
    assert!(
        matches!(err_dir, Err(LuksError::UnsupportedFsFeature(_))),
        "expected UnsupportedFsFeature on directory rename, got: {err_dir:?}"
    );

    // Refusal 2: Cross-directory move by name containing '/'
    let err_cross = fs.rename_file(2, "some_file.txt", "some_dir/moved.txt");
    assert!(
        matches!(err_cross, Err(LuksError::UnsupportedFsFeature(_)) | Err(LuksError::CorruptFs(_))),
        "expected refusal on cross-directory rename, got: {err_cross:?}"
    );

    // Refusal 3: Non-existent source
    let err_notfound = fs.rename_file(2, "nonexistent.txt", "target.txt");
    assert!(
        matches!(err_notfound, Err(LuksError::NotFound(_))),
        "expected NotFound on missing source, got: {err_notfound:?}"
    );

    // Refusal 4: Overwriting a directory with a regular file
    let err_overwrite_dir = fs.rename_file(2, "some_file.txt", "some_dir");
    assert!(
        matches!(err_overwrite_dir, Err(LuksError::UnsupportedFsFeature(_))),
        "expected UnsupportedFsFeature when overwriting a directory, got: {err_overwrite_dir:?}"
    );
}

// --- W15: rename onto an existing name ------------------------------------

/// Raw `s_state` bit 0 (cleanly unmounted), read from the file, not through
/// the driver.
fn marked_clean(path: &str) -> bool {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(path).expect("open image");
    let mut b = [0u8; 2];
    f.read_exact_at(&mut b, 1024 + 0x3A).expect("read s_state");
    u16::from_le_bytes(b) & 1 == 1
}

fn kernel_verdict(script: &str, path: &str) -> Result<(), String> {
    let out = Command::new("bash").arg(script).arg(path).output().expect("run verifier");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() { Ok(()) } else { Err(text) }
}

/// W15 (L2-O17): whatever write the device dies on, `rename_file` over an
/// existing name must never leave the destination name unreachable. Either the
/// old destination or the source's data answers to it; when the old
/// destination does, the source is still under its old name. And what e2fsck
/// finds is leak-class only, flagged by the not-clean bit.
#[test]
fn w15_rename_over_existing_never_loses_the_destination_name() {
    let Some(script) = verify_script() else {
        panic!("oracle required: this test cannot pass without e2fsck");
    };
    let src_data: Vec<u8> = (0..9000u32).map(|i| (i % 251) as u8).collect();
    let dst_data: Vec<u8> = (0..13000u32).map(|i| (i % 241) as u8 ^ 0x5a).collect();

    for name in ["big-4k.img", "csum-uuid-4k.img"] {
        let base = scratch(name, "w15-base");
        let (src_ino, dst_ino) = {
            let mut fs = open(&base);
            fs.create_file(2, "src.bin", &src_data, FileType::Regular).expect("create src");
            fs.create_file(2, "dst.bin", &dst_data, FileType::Regular).expect("create dst");
            fs.flush().expect("flush");
            (fs.resolve("/src.bin").expect("src").number, fs.resolve("/dst.bin").expect("dst").number)
        };
        kernel_verdict(&script, &base).unwrap_or_else(|t| panic!("{name}: the base image is not clean:\n{t}"));
        let len = std::fs::metadata(&base).expect("stat").len();

        // Unlimited run: how many writes the rename makes.
        let total = {
            let img = scratch(name, "w15-count");
            std::fs::copy(&base, &img).expect("copy base");
            let dev = FailAfterNWrites::new(FileDevice::open_writable(&img, len).expect("open"), usize::MAX);
            let mut fs = Ext4::mount(dev).expect("mount");
            fs.rename_file(2, "src.bin", "dst.bin").expect("unlimited rename");
            let n = fs.device_mut().attempted.load(Ordering::SeqCst);
            assert!(n >= 4, "{name}: a rename over an existing name made only {n} writes");
            n
        };

        let mut violations = Vec::new();
        let mut flagged = 0;
        for n in 0..=total {
            let img = scratch(name, &format!("w15-{n}"));
            std::fs::copy(&base, &img).expect("copy base");
            {
                // The last iteration is the unlimited control: a budget of exactly
                // `total` would still fail the trailing flush.
                let budget = if n == total { usize::MAX } else { n };
                let dev = FailAfterNWrites::new(FileDevice::open_writable(&img, len).expect("open"), budget);
                let mut fs = Ext4::mount(dev).expect("mount");
                let r = fs.rename_file(2, "src.bin", "dst.bin");
                if n == total {
                    r.expect("the full budget must succeed");
                }
            }

            // Reachability, through the driver, on the post-failure image.
            let fs = Ext4::mount(FileDevice::open(&img).expect("reopen")).expect("mount after failure");
            match fs.read_file("/dst.bin") {
                Err(e) => violations.push(format!("N={n}/{total}: /dst.bin is unreachable: {e}")),
                Ok(got) if got == src_data => {}
                Ok(got) if got == dst_data => match fs.read_file("/src.bin") {
                    Ok(s) if s == src_data => {}
                    other => violations.push(format!(
                        "N={n}/{total}: /dst.bin is the old destination but the source is lost: {:?}",
                        other.map(|v| v.len())
                    )),
                },
                Ok(got) => violations.push(format!("N={n}/{total}: /dst.bin holds {} unknown bytes", got.len())),
            }

            let clean_flag = marked_clean(&img);
            match kernel_verdict(&script, &img) {
                Ok(()) => {}
                Err(text) => {
                    let bad = leak_class::non_leak_lines(&text, &[src_ino, dst_ino]);
                    if !bad.is_empty() {
                        violations.push(format!("N={n}/{total}: not leak-class: {bad:?}"));
                    }
                    if clean_flag {
                        violations.push(format!("N={n}/{total}: e2fsck dirty but the image reads clean"));
                    } else {
                        flagged += 1;
                    }
                }
            }
            if n == total {
                assert!(clean_flag, "{name}: the complete rename must end marked clean");
                assert!(fs.read_file("/src.bin").is_err(), "{name}: src.bin must be gone after the rename");
            }
        }
        assert!(flagged > 0, "{name}: vacuity: some boundary must leave a flagged, dirty image");
        assert!(
            violations.is_empty(),
            "{name}: {} of {} boundaries violate the rename contract:\n{}",
            violations.len(),
            total + 1,
            violations.join("\n")
        );
    }
}
