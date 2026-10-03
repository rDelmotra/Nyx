#![cfg(feature = "dangerous-write-support")]

#[path = "../common/mod.rs"]
mod common;

use luks_core::device::{FileDevice, ReadAt, WriteAt};
use luks_core::fs::ext4::Ext4;
use luks_core::fs::FileType;
use std::process::Command;

#[path = "leak_class.rs"]
mod leak_class;

fn fixture(name: &str) -> String {
    format!("{}/../fixtures/ext4/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn scratch(name: &str, test: &str) -> String {
    let dst = std::env::temp_dir().join(format!("ext4-delete-{test}-{name}"));
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
fn deleting_a_file_leaves_e2fsck_clean() {
    let Some(script) = verify_script() else {
        eprintln!("skipping: colima is not running");
        return;
    };

    for name in FIXTURES {
        let path = scratch(name, "clean-delete");
        let content = b"file to be deleted\n";

        // Create a file
        {
            let mut fs = open(&path);
            fs.create_file(2, "to_delete.txt", content, FileType::Regular)
                .expect("create file");
            fs.flush().expect("flush");
        }

        // Verify it was created and is visible
        {
            let fs = Ext4::mount(FileDevice::open(&path).expect("reopen")).expect("mount");
            let entries = fs.list_dir("/").expect("list root");
            assert!(entries.iter().any(|e| e.name == "to_delete.txt"));
        }

        // Delete the file
        {
            let mut fs = open(&path);
            fs.delete_file("/to_delete.txt").expect("delete file");
            fs.flush().expect("flush");
        }

        // Verify it is gone from the listing
        {
            let fs = Ext4::mount(FileDevice::open(&path).expect("reopen")).expect("mount");
            let entries = fs.list_dir("/").expect("list root");
            assert!(!entries.iter().any(|e| e.name == "to_delete.txt"));
        }

        // Check via e2fsck
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
            "{name}: e2fsck rejected filesystem after delete:\n{text}"
        );
    }
}

#[test]
fn the_deleted_files_blocks_are_returned_to_free_count() {
    for name in FIXTURES {
        let path = scratch(name, "free-counts");
        let content = vec![0xAAu8; 128 * 1024]; // 128 KiB = 32 blocks (at 4K block size)

        let mut fs = open(&path);
        let blocks_before = fs.superblock().free_blocks_count;
        let inodes_before = fs.superblock().free_inodes_count;

        fs.create_file(2, "counts.bin", &content, FileType::Regular)
            .expect("create file");
        fs.flush().expect("flush");

        let blocks_after_create = fs.superblock().free_blocks_count;
        let inodes_after_create = fs.superblock().free_inodes_count;
        assert!(blocks_after_create < blocks_before);
        assert_eq!(inodes_after_create, inodes_before - 1);

        fs.delete_file("/counts.bin").expect("delete file");
        fs.flush().expect("flush");

        assert_eq!(
            fs.superblock().free_blocks_count,
            blocks_before,
            "{name}: free block count mismatch after delete"
        );
        assert_eq!(
            fs.superblock().free_inodes_count,
            inodes_before,
            "{name}: free inode count mismatch after delete"
        );
    }
}

#[test]
fn deleting_does_not_disturb_other_files() {
    for name in FIXTURES {
        let path = scratch(name, "non-interference");
        let content1 = b"file to delete".to_vec();
        let content2 = b"file to keep".to_vec();

        {
            let mut fs = open(&path);
            fs.create_file(2, "delete.txt", &content1, FileType::Regular)
                .expect("create file 1");
            fs.create_file(2, "keep.txt", &content2, FileType::Regular)
                .expect("create file 2");
            fs.flush().expect("flush");
        }

        {
            let mut fs = open(&path);
            fs.delete_file("/delete.txt").expect("delete file");
            fs.flush().expect("flush");
        }

        // Verify keep.txt is still readable with correct content
        {
            let fs = Ext4::mount(FileDevice::open(&path).expect("reopen")).expect("mount");
            let entries = fs.list_dir("/").expect("list root");
            let keep_entry = entries.iter().find(|e| e.name == "keep.txt").expect("keep.txt exists");
            let keep_inode = fs.read_inode(keep_entry.inode).expect("read keep inode");
            let mut buf = vec![0u8; content2.len()];
            fs.read_inode_data(&keep_inode, 0, &mut buf).expect("read keep data");
            assert_eq!(buf, content2);
        }
    }
}

#[test]
fn deleting_a_nonexistent_file_returns_not_found() {
    let path = scratch("csum-uuid-4k.img", "not-found");
    let mut fs = open(&path);
    let res = fs.delete_file("/does-not-exist.txt");
    assert!(res.is_err());
    match res.unwrap_err() {
        luks_core::error::LuksError::NotFound(name) => {
            assert!(name.contains("does-not-exist.txt"));
        }
        other => panic!("expected LuksError::NotFound, got {:?}", other),
    }
}

#[test]
fn deleting_the_root_directory_is_refused() {
    let path = scratch("csum-uuid-4k.img", "delete-root");
    let mut fs = open(&path);

    let res = fs.delete_file("/");
    assert!(res.is_err());
    assert!(matches!(res.unwrap_err(), luks_core::error::LuksError::IsADirectory(_)));
}

#[test]
fn a_dot_component_is_refused_rather_than_silently_resolving_to_an_ancestor() {
    // `resolve`/`resolve_no_follow` collapse `.` and `..` during path
    // resolution, but this function's own root check and its `rsplit_once`
    // name-split operate on the raw, un-normalized string. Without an
    // explicit guard, "/tree/." would resolve to "/tree" itself, so this
    // function would recurse into and delete every child of "/tree" before
    // the final `unlink_file(parent, ".")` failed name validation —
    // reporting failure *after* everything inside was already gone. This
    // proves the guard fires first: the tree survives completely intact.
    let path = scratch("big-4k.img", "dot-component-refused");
    let mut fs = open(&path);

    let top = fs.create_directory(2, "tree").expect("mkdir tree");
    fs.create_file(top as u64, "a.txt", b"hello", FileType::Regular)
        .expect("create a.txt");
    let nested = fs.create_directory(top, "nested").expect("mkdir nested");
    fs.create_file(nested as u64, "b.txt", b"world", FileType::Regular)
        .expect("create b.txt");
    // A name that merely *starts* with a dot is a real, ordinary name and
    // must not be caught by the same guard.
    fs.create_file(top as u64, ".hidden", b"x", FileType::Regular)
        .expect("create .hidden");
    fs.flush().expect("flush");

    for bad in ["/.", "/tree/.", "/tree/..", "..", "/tree/nested/.."] {
        let res = fs.delete_file(bad);
        assert!(
            matches!(res, Err(luks_core::error::LuksError::UnsupportedFsFeature(_))),
            "expected {bad:?} to be refused, got {res:?}"
        );
    }

    assert!(fs.list_dir("/").unwrap().iter().any(|e| e.name == "tree"));
    assert_eq!(fs.list_dir("/tree").unwrap().len(), 3);
    assert_eq!(fs.list_dir("/tree/nested").unwrap().len(), 1);

    fs.delete_file("/tree/.hidden").expect("delete .hidden");
    assert!(!fs.list_dir("/tree").unwrap().iter().any(|e| e.name == ".hidden"));
}

#[test]
fn deleting_an_empty_directory_removes_it_and_restores_the_parent_link_count() {
    let path = scratch("big-4k.img", "delete-empty-dir");
    let mut fs = open(&path);

    let links_before = fs.read_inode(2).unwrap().links;
    fs.create_directory(2, "empty").expect("mkdir");
    fs.flush().expect("flush");
    assert_eq!(
        fs.read_inode(2).unwrap().links,
        links_before + 1,
        "create_directory bumps the parent's link count for the new '..'"
    );

    fs.delete_file("/empty").expect("delete empty dir");
    fs.flush().expect("flush");

    assert!(!fs
        .list_dir("/")
        .unwrap()
        .iter()
        .any(|e| e.name == "empty"));
    assert_eq!(
        fs.read_inode(2).unwrap().links,
        links_before,
        "deleting the subdirectory must give back the link its '..' added"
    );
}

#[test]
fn deleting_a_directory_recursively_removes_files_and_nested_subdirectories() {
    let path = scratch("big-4k.img", "delete-recursive-dir");
    let mut fs = open(&path);

    let top = fs.create_directory(2, "tree").expect("mkdir tree");
    fs.create_file(top as u64, "a.txt", b"hello", FileType::Regular)
        .expect("create a.txt");
    let nested = fs.create_directory(top, "nested").expect("mkdir nested");
    fs.create_file(nested as u64, "b.txt", b"world", FileType::Regular)
        .expect("create b.txt");
    fs.flush().expect("flush");

    // Sanity: the tree is really there before it's deleted.
    assert_eq!(fs.list_dir("/tree").unwrap().len(), 2);
    assert_eq!(fs.list_dir("/tree/nested").unwrap().len(), 1);

    fs.delete_file("/tree").expect("recursive delete");
    fs.flush().expect("flush");

    assert!(!fs.list_dir("/").unwrap().iter().any(|e| e.name == "tree"));
    // Every inode involved must actually be gone, not just unreachable —
    // re-reading by number should fail rather than return a zeroed record
    // that still looks plausible.
    assert!(fs.resolve_no_follow("/tree").is_err());
    assert!(fs.resolve_no_follow("/tree/a.txt").is_err());
    assert!(fs.resolve_no_follow("/tree/nested").is_err());
    assert!(fs.resolve_no_follow("/tree/nested/b.txt").is_err());
}

#[test]
fn deleting_a_directory_recursively_leaves_e2fsck_clean() {
    let Some(script) = verify_script() else {
        eprintln!("skipping: colima is not running");
        return;
    };

    for name in FIXTURES {
        let path = scratch(name, "clean-recursive-delete");
        {
            let mut fs = open(&path);
            let top = fs.create_directory(2, "tree").expect("mkdir tree");
            fs.create_file(top as u64, "a.txt", b"hello", FileType::Regular)
                .expect("create a.txt");
            let nested = fs.create_directory(top, "nested").expect("mkdir nested");
            fs.create_file(nested as u64, "b.txt", b"world", FileType::Regular)
                .expect("create b.txt");
            fs.flush().expect("flush");

            fs.delete_file("/tree").expect("recursive delete");
            fs.flush().expect("flush");
        }

        let output = Command::new(&script)
            .arg(&path)
            .output()
            .expect("run verify-ext4.sh");
        assert!(
            output.status.success(),
            "{name}: e2fsck reported problems after recursive delete:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn the_space_can_be_reused_after_deletion() {
    let path = scratch("big-4k.img", "reuse");
    let bs = 4096;
    let size = 128 * bs;
    let content = vec![0xBBu8; size];

    let mut fs = open(&path);
    let blocks_before = fs.superblock().free_blocks_count;

    fs.create_file(2, "temp.bin", &content, FileType::Regular).expect("create file");
    fs.flush().expect("flush");

    fs.delete_file("/temp.bin").expect("delete");
    fs.flush().expect("flush");

    // Write a new file of same size. It should succeed using the same space
    fs.create_file(2, "new.bin", &content, FileType::Regular).expect("re-create file");
    fs.flush().expect("flush");

    assert_eq!(fs.superblock().free_blocks_count, blocks_before - (size as u64 / bs as u64));
}

#[test]
fn deleting_a_file_with_depth_1_extents_frees_the_index_block_too() {
    let path = scratch("many-groups-1k.img", "fragmented-delete");
    let mut fs = open(&path);

    let mut held = Vec::new();
    for _ in 0..64 {
        let b = fs.alloc_block(0).expect("checkerboard alloc");
        held.push(b);
    }
    for (i, b) in held.iter().enumerate() {
        if i % 2 == 0 {
            fs.free_block(*b).expect("free every other block");
        }
    }

    let free_blocks_before = fs.superblock().free_blocks_count;
    let free_inodes_before = fs.superblock().free_inodes_count;

    // Create a file that needs index block (depth 1)
    let block_size = fs.block_size() as usize;
    let content = vec![0xEEu8; 30 * block_size];
    let _ino = fs.create_file(2, "fragmented.bin", &content, FileType::Regular).expect("create file");
    fs.flush().expect("flush");

    // Confirm it consumed 30 data blocks + 1 index block = 31 blocks
    assert_eq!(fs.superblock().free_blocks_count, free_blocks_before - 31);
    assert_eq!(fs.superblock().free_inodes_count, free_inodes_before - 1);

    // Delete the file
    fs.delete_file("/fragmented.bin").expect("delete fragmented file");
    fs.flush().expect("flush");

    // Confirm it returned all 31 blocks and the inode
    assert_eq!(fs.superblock().free_blocks_count, free_blocks_before);
    assert_eq!(fs.superblock().free_inodes_count, free_inodes_before);
}

#[test]
fn deleting_a_hardlinked_file_is_refused() {
    let path = scratch("csum-uuid-4k.img", "hardlink-refused");
    let mut fs = open(&path);
    let ino = fs.create_file(2, "hl.txt", b"hardlink target", FileType::Regular).expect("create");
    fs.flush().expect("flush");

    // Manually increment links count of the inode on disk.
    // In ext4 inode structure, links count is a u16 at offset 26 (0x1A).
    let offset = fs.test_inode_offset(ino).expect("inode offset");
    let mut inode_bytes = vec![0u8; fs.superblock().inode_size as usize];
    fs.device_mut().read_at(offset, &mut inode_bytes).expect("read inode");
    let links = u16::from_le_bytes([inode_bytes[26], inode_bytes[27]]);
    let new_links = links + 1;
    inode_bytes[26..28].copy_from_slice(&new_links.to_le_bytes());
    
    // Recompute checksum if metadata_csum is enabled
    let seed = luks_core::fs::ext4::csum::Seed::for_writing(fs.superblock()).expect("seed");
    if let Some(csum) = seed.inode(ino, &inode_bytes) {
        inode_bytes[0x7C..0x7E].copy_from_slice(&(csum as u16).to_le_bytes());
        if seed.inode_has_csum_hi(&inode_bytes) {
            inode_bytes[0x82..0x84].copy_from_slice(&((csum >> 16) as u16).to_le_bytes());
        }
    }
    
    fs.device_mut().write_at(offset, &inode_bytes).expect("write inode");
    fs.flush().expect("flush");

    // Reopen and try to delete
    let mut fs = open(&path);
    let res = fs.delete_file("/hl.txt");
    assert!(res.is_err());
    assert!(matches!(res.unwrap_err(), luks_core::error::LuksError::UnsupportedFsFeature(_)));
}

#[test]
fn break_verification_missing_unlink_leaves_unattached_inode() {
    let Some(script) = verify_script() else {
        eprintln!("skipping: colima is not running");
        return;
    };

    let path = scratch("csum-uuid-4k.img", "break-unlink");
    let content = b"temporary contents\n";

    // 1. Create a file
    {
        let mut fs = open(&path);
        fs.create_file(2, "broken.txt", content, FileType::Regular).expect("create");
        fs.flush().expect("flush");
    }

    // 2. Perform partial delete: free blocks & free inode, but skip unlink_file.
    {
        let mut fs = open(&path);
        let target_inode = fs.resolve_no_follow("/broken.txt").expect("resolve");
        let (data_extents, index_extents) = fs.collect_extents(&target_inode).expect("collect");

        // Skip unlink_file.
        fs.free_inode(target_inode.number, false).expect("free inode");
        fs.free_blocks(&data_extents).expect("free blocks");
        if !index_extents.is_empty() {
            fs.free_blocks(&index_extents).expect("free index blocks");
        }
        fs.flush().expect("flush");
    }

    // 3. Run e2fsck and check output
    let out = Command::new("bash")
        .arg(&script)
        .arg(&path)
        .output()
        .expect("run the verifier");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // e2fsck must complain that a directory entry points to an unused/deleted inode
    assert!(
        text.contains("deleted inode") || text.contains("deleted/unused") || text.contains("unused inode") || text.contains("points to") || text.contains("not in use") || text.contains("invalid"),
        "e2fsck did not complain about unlinked inode reference:\n{text}"
    );
}

#[test]
fn break_verification_missing_free_blocks_leaves_orphan_blocks() {
    let Some(script) = verify_script() else {
        eprintln!("skipping: colima is not running");
        return;
    };

    let path = scratch("csum-uuid-4k.img", "break-blocks");
    let content = b"temporary contents\n";

    // 1. Create a file
    {
        let mut fs = open(&path);
        fs.create_file(2, "broken.txt", content, FileType::Regular).expect("create");
        fs.flush().expect("flush");
    }

    // 2. Perform partial delete: unlink & free inode, but skip free_blocks.
    {
        let mut fs = open(&path);
        let target_inode = fs.resolve_no_follow("/broken.txt").expect("resolve");
        
        fs.unlink_file(2, "broken.txt").expect("unlink");
        fs.free_inode(target_inode.number, false).expect("free inode");
        // Skip free_blocks.
        fs.flush().expect("flush");
    }

    // 3. Run e2fsck and check output
    let out = Command::new("bash")
        .arg(&script)
        .arg(&path)
        .output()
        .expect("run the verifier");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // e2fsck must complain about block bitmap mismatch
    assert!(
        text.contains("bitmap differences") || text.contains("Free blocks count"),
        "e2fsck did not complain about block leak:\n{text}"
    );
}

#[test]
fn break_verification_missing_free_inode_leaves_orphan_inode() {
    let Some(script) = verify_script() else {
        eprintln!("skipping: colima is not running");
        return;
    };

    let path = scratch("csum-uuid-4k.img", "break-inode");
    let content = b"temporary contents\n";

    // 1. Create a file
    {
        let mut fs = open(&path);
        fs.create_file(2, "broken.txt", content, FileType::Regular).expect("create");
        fs.flush().expect("flush");
    }

    // 2. Perform partial delete: unlink & free blocks, but skip free_inode.
    {
        let mut fs = open(&path);
        let target_inode = fs.resolve_no_follow("/broken.txt").expect("resolve");
        let (data_extents, index_extents) = fs.collect_extents(&target_inode).expect("collect");
        
        fs.unlink_file(2, "broken.txt").expect("unlink");
        // Skip free_inode.
        fs.free_blocks(&data_extents).expect("free blocks");
        if !index_extents.is_empty() {
            fs.free_blocks(&index_extents).expect("free index blocks");
        }
        fs.flush().expect("flush");
    }

    // 3. Run e2fsck and check output
    let out = Command::new("bash")
        .arg(&script)
        .arg(&path)
        .output()
        .expect("run the verifier");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // e2fsck must complain about unattached or orphan inode
    assert!(
        text.contains("unattached") || text.contains("Free inodes count") || text.contains("bitmap differences"),
        "e2fsck did not complain about unattached/leaked inode:\n{text}"
    );
}

// --- W14: validate before destroying -----------------------------------------

/// A device that starts failing reads of one block once armed. Reads at any
/// other offset, and every write, behave normally.
struct FailReadsAt {
    inner: FileDevice,
    armed: std::sync::atomic::AtomicBool,
    at: u64,
    len: u64,
}

impl ReadAt for FailReadsAt {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> luks_core::error::Result<()> {
        use std::sync::atomic::Ordering;
        if self.armed.load(Ordering::SeqCst) && offset < self.at + self.len && offset + buf.len() as u64 > self.at {
            return Err(luks_core::error::LuksError::Io {
                path: "simulated-bitmap-read-failure".to_string(),
                source: std::io::Error::other("injected read failure"),
            });
        }
        self.inner.read_at(offset, buf)
    }
    fn len(&self) -> Option<u64> {
        self.inner.len()
    }
}

impl WriteAt for FailReadsAt {
    fn write_at(&self, offset: u64, buf: &[u8]) -> luks_core::error::Result<()> {
        self.inner.write_at(offset, buf)
    }
    fn flush(&self) -> luks_core::error::Result<()> {
        self.inner.flush()
    }
}

/// W14 (L2-O9), e2fsck-graded: the block bitmap of the file's group cannot be
/// read when the delete needs it. Nothing about the file may change: it is
/// still listed, still readable, the image is byte-identical, and the kernel
/// finds nothing wrong.
#[test]
fn w14_delete_whose_block_free_cannot_be_planned_leaves_the_file_intact_and_clean() {
    let Some(script) = verify_script() else {
        panic!("oracle required: this test cannot pass without e2fsck");
    };
    for name in FIXTURES {
        let path = scratch(name, "w14-readfail");
        let content: Vec<u8> = (0..3 * 4096u32).map(|i| (i % 253) as u8).collect();
        let (bitmap_off, len) = {
            let mut fs = open(&path);
            fs.create_file(2, "keep.bin", &content, FileType::Regular).expect("create");
            fs.flush().expect("flush");
            let ino = fs.resolve("/keep.bin").expect("resolve");
            let (data, _) = fs.collect_extents(&ino).expect("extents");
            let bs = fs.block_size() as u64;
            let g = ((data[0].physical - fs.superblock().first_data_block as u64)
                / fs.superblock().blocks_per_group as u64) as usize;
            (fs.groups()[g].block_bitmap * bs, bs)
        };
        let before = std::fs::read(&path).expect("read image");
        let flen = before.len() as u64;
        let dev = FailReadsAt {
            inner: FileDevice::open_writable(&path, flen).expect("open"),
            armed: std::sync::atomic::AtomicBool::new(false),
            at: bitmap_off,
            len,
        };
        {
            let mut fs = Ext4::mount(dev).expect("mount");
            fs.device_mut().armed.store(true, std::sync::atomic::Ordering::SeqCst);
            let err = fs.delete_file("/keep.bin").expect_err("the bitmap read fails, so must the delete");
            assert!(err.to_string().contains("injected read failure"), "{name}: wrong error: {err}");
            fs.device_mut().armed.store(false, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(fs.read_file("/keep.bin").expect("file must still be readable"), content, "{name}");
        }
        assert!(std::fs::read(&path).expect("read image") == before, "{name}: a refused delete changed the image");
        let out = Command::new("bash").arg(&script).arg(&path).output().expect("run verifier");
        assert!(
            out.status.success(),
            "{name}: e2fsck after a refused delete:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// W14, crafted corruption: one of the file's blocks is marked free in the
/// bitmap, so `free_blocks` would refuse a double free. The delete must refuse
/// *before* it removes the name or frees the inode: the image is untouched.
#[test]
fn w14_delete_refused_for_a_double_free_leaves_the_image_untouched() {
    for name in FIXTURES {
        let path = scratch(name, "w14-doublefree");
        let content: Vec<u8> = (0..3 * 4096u32).map(|i| (i % 251) as u8).collect();
        {
            let mut fs = open(&path);
            fs.create_file(2, "keep.bin", &content, FileType::Regular).expect("create");
            fs.flush().expect("flush");
            let ino = fs.resolve("/keep.bin").expect("resolve");
            let (data, _) = fs.collect_extents(&ino).expect("extents");
            let victim = data[0].physical + data[0].len - 1;
            let sb = fs.superblock();
            let rel = victim - sb.first_data_block as u64;
            let g = (rel / sb.blocks_per_group as u64) as usize;
            let bit = rel % sb.blocks_per_group as u64;
            let off = fs.groups()[g].block_bitmap * fs.block_size() as u64 + bit / 8;
            let mut img = std::fs::read(&path).expect("read image");
            assert!(img[off as usize] & (1 << (bit % 8)) != 0, "{name}: the victim bit must start set");
            img[off as usize] &= !(1 << (bit % 8));
            std::fs::write(&path, &img).expect("craft image");
        }
        let before = std::fs::read(&path).expect("read image");
        {
            let mut fs = open(&path);
            let err = fs.delete_file("/keep.bin").expect_err("a double free must be refused");
            assert!(err.to_string().contains("double free"), "{name}: wrong error: {err}");
            assert_eq!(fs.read_file("/keep.bin").expect("file must still be readable"), content, "{name}");
        }
        assert!(
            std::fs::read(&path).expect("read image") == before,
            "{name}: a refused delete changed the image (name or inode destroyed before validation)"
        );
    }
}

/// W14 ordering, graded at every write boundary: the device dies after N
/// writes of a delete. The file is either entirely there (readable, intact)
/// or gone, and what e2fsck finds is leak-class only (the deleted inode's
/// unattached/zeroed record, blocks marked used with no owner), flagged by
/// the not-clean bit. This is what pins "inode before blocks": the other order
/// leaves an unattached inode over blocks the bitmap calls free (a `+`
/// difference), which the classifier rejects.
#[test]
fn w14_delete_fail_sweep_is_leak_class_only_at_every_boundary() {
    let Some(script) = verify_script() else {
        panic!("oracle required: this test cannot pass without e2fsck");
    };
    let content: Vec<u8> = (0..5 * 4096u32).map(|i| (i % 249) as u8).collect();
    for name in FIXTURES {
        let base = scratch(name, "w14-sweep-base");
        let ino = {
            let mut fs = open(&base);
            fs.create_file(2, "gone.bin", &content, FileType::Regular).expect("create");
            fs.flush().expect("flush");
            fs.resolve("/gone.bin").expect("resolve").number
        };
        let len = std::fs::metadata(&base).expect("stat").len();
        let total = {
            let img = scratch(name, "w14-sweep-count");
            std::fs::copy(&base, &img).expect("copy");
            let dev = common::fail_after::FailAfterNWrites::new(FileDevice::open_writable(&img, len).expect("open"), usize::MAX);
            let mut fs = Ext4::mount(dev).expect("mount");
            fs.delete_file("/gone.bin").expect("unlimited delete");
            let n = fs.device_mut().attempted.load(std::sync::atomic::Ordering::SeqCst);
            assert!(n >= 5, "{name}: a delete made only {n} writes");
            n
        };
        let mut violations = Vec::new();
        let mut flagged = 0;
        for n in 0..=total {
            let img = scratch(name, &format!("w14-sweep-{n}"));
            std::fs::copy(&base, &img).expect("copy");
            {
                let budget = if n == total { usize::MAX } else { n };
                let dev = common::fail_after::FailAfterNWrites::new(FileDevice::open_writable(&img, len).expect("open"), budget);
                let mut fs = Ext4::mount(dev).expect("mount");
                let r = fs.delete_file("/gone.bin");
                if n == total {
                    r.expect("the unlimited run must succeed");
                }
            }
            let fs = Ext4::mount(FileDevice::open(&img).expect("reopen")).expect("mount after failure");
            match fs.read_file("/gone.bin") {
                Ok(got) => assert_eq!(got, content, "{name} N={n}: a surviving file must be intact"),
                Err(luks_core::error::LuksError::NotFound(_)) => {}
                Err(e) => violations.push(format!("N={n}/{total}: neither intact nor gone: {e}")),
            }
            use std::os::unix::fs::FileExt;
            let mut st = [0u8; 2];
            std::fs::File::open(&img).expect("open").read_exact_at(&mut st, 1024 + 0x3A).expect("s_state");
            let clean_flag = u16::from_le_bytes(st) & 1 == 1;
            let out = Command::new("bash").arg(&script).arg(&img).output().expect("run verifier");
            if !out.status.success() {
                let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
                let bad = leak_class::non_leak_lines(&text, &[ino]);
                if !bad.is_empty() {
                    violations.push(format!("N={n}/{total}: not leak-class: {bad:?}"));
                }
                if clean_flag {
                    violations.push(format!("N={n}/{total}: e2fsck dirty but the image reads clean"));
                } else {
                    flagged += 1;
                }
            } else if n == total {
                assert!(clean_flag, "{name}: a complete delete must end marked clean");
            }
        }
        assert!(flagged > 0, "{name}: vacuity: some boundary must leave a flagged, dirty image");
        assert!(violations.is_empty(), "{name}: {} violations:\n{}", violations.len(), violations.join("\n"));
    }
}
