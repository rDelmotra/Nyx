//! One filesystem handle over either reader.
//!
//! An enum rather than a `Box<dyn Filesystem>` because the two readers do not
//! have the same shape and pretending they do costs more than it saves. ext4
//! identifies a file by inode number; btrfs needs an inode *and* the tree it
//! lives in, since subvolumes number their inodes independently. A trait
//! flattening both to `u64` is exactly the bug that `Located` exists to
//! prevent — an inode read against the wrong tree returns a real file, with
//! valid checksums, that nobody asked for.
//!
//! This lives in `core` rather than in the JNI bridge so that anything reading
//! a volume — the bridge, the host-side examples, a future desktop build —
//! makes the same decision the same way, and so the decision is testable
//! without an Android toolchain.

use crate::device::ReadAt;
use crate::error::{LuksError, Result};
use crate::fs::btrfs::{self, Btrfs};
use crate::fs::detect::{detect_fs, DetectedFs, FsKind};
use crate::fs::ext4::{self, Ext4};
use crate::fs::{DirEntry, FileInfo, FileType};

/// A mounted filesystem, whichever one the volume turned out to hold.
#[allow(clippy::large_enum_variant)]
pub enum MountedFs<D: ReadAt> {
    Ext4(Ext4<D>),
    Btrfs(Btrfs<D>),
}

/// A resolved file, held open across a streaming read.
///
/// The reason this exists rather than passing a path to each read: hashing a
/// 1 GiB file in 1 MiB chunks would otherwise re-resolve the path a thousand
/// times, and each resolution is a directory walk — over USB, that is a
/// thousand round trips to answer a question already answered.
///
/// # Identity, not a snapshot
///
/// What this holds is the inode as it was when opened, used as an **identity
/// to check**, never as data to read with. Every read re-reads the inode (one
/// small read) and refuses with [`LuksError::FileChanged`] unless it is still
/// the same file: same number, same generation, same type, size and times. So
/// a delete, a rename-over, or a delete-and-create that reuses the inode
/// number between two reads can never feed another file's bytes into the
/// stream (N3), and nothing here pins a btrfs tree root that a later commit
/// may free (R3r).
pub enum OpenFile {
    Ext4(ext4::Inode),
    /// The subvolume's **objectid**, not its root bytenr: the root moves on
    /// every commit, and an inode number alone does not identify a file on
    /// btrfs (each subvolume numbers from 256).
    Btrfs { subvol: u64, inode: btrfs::Inode },
}

impl OpenFile {
    /// Size when opened. Equal to the size at any later successful read, since
    /// a size change fails the identity check.
    pub fn size(&self) -> u64 {
        match self {
            OpenFile::Ext4(i) => i.info().size,
            OpenFile::Btrfs { inode, .. } => inode.size,
        }
    }

    pub fn file_type(&self) -> FileType {
        match self {
            OpenFile::Ext4(i) => i.file_type(),
            OpenFile::Btrfs { inode, .. } => inode.file_type(),
        }
    }
}

impl<D: ReadAt> MountedFs<D> {
    /// Decide what is on the volume, then hand it to exactly one reader.
    ///
    /// The decision comes first because mounting consumes the volume, so
    /// "try one, fall back to the other" is not available — and re-opening a
    /// `LuksVolume` to retry means running Argon2 a second time. See
    /// [`crate::fs::detect`].
    pub fn mount(device: D) -> Result<Self> {
        match detect_fs(&device)? {
            DetectedFs::Ext4 => Ok(MountedFs::Ext4(Ext4::mount(device)?)),
            DetectedFs::Btrfs(sb) => Ok(MountedFs::Btrfs(Btrfs::mount_with_superblock(device, sb)?)),
        }
    }

    pub fn kind(&self) -> FsKind {
        match self {
            MountedFs::Ext4(_) => FsKind::Ext4,
            MountedFs::Btrfs(_) => FsKind::Btrfs,
        }
    }

    /// `None` for a volume that was never given a label — distinct from one
    /// labelled with an empty string, which is why this is not a `&str`.
    pub fn label(&self) -> Option<&str> {
        match self {
            MountedFs::Ext4(fs) => {
                let name = fs.volume_name();
                (!name.is_empty()).then_some(name)
            }
            MountedFs::Btrfs(fs) => fs.volume_name(),
        }
    }

    pub fn uuid(&self) -> [u8; 16] {
        match self {
            MountedFs::Ext4(fs) => fs.uuid(),
            MountedFs::Btrfs(fs) => fs.fsid(),
        }
    }

    /// The allocation unit a user would recognise. ext4 calls it a block;
    /// btrfs calls the same thing a sector and reserves "block" for its
    /// 16 KiB metadata nodes.
    pub fn block_size(&self) -> u32 {
        match self {
            MountedFs::Ext4(fs) => fs.block_size(),
            MountedFs::Btrfs(fs) => fs.sector_size(),
        }
    }

    pub fn size_bytes(&self) -> u64 {
        match self {
            MountedFs::Ext4(fs) => fs.superblock().blocks_count * fs.block_size() as u64,
            MountedFs::Btrfs(fs) => fs.superblock().total_bytes,
        }
    }

    pub fn list_dir(&self, path: &str) -> Result<Vec<DirEntry>> {
        match self {
            MountedFs::Ext4(fs) => fs.list_dir(path),
            MountedFs::Btrfs(fs) => fs.list_dir(path),
        }
    }

    pub fn file_info(&self, path: &str) -> Result<FileInfo> {
        match self {
            MountedFs::Ext4(fs) => fs.file_info(path),
            MountedFs::Btrfs(fs) => fs.file_info(path),
        }
    }

    pub fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        match self {
            MountedFs::Ext4(fs) => fs.read_file(path),
            MountedFs::Btrfs(fs) => fs.read_file(path),
        }
    }

    pub fn open(&self, path: &str) -> Result<OpenFile> {
        match self {
            MountedFs::Ext4(fs) => Ok(OpenFile::Ext4(fs.resolve(path)?)),
            MountedFs::Btrfs(fs) => {
                let found = fs.resolve_no_follow(fs.fs_tree(), path)?;
                Ok(OpenFile::Btrfs {
                    subvol: found.tree.objectid,
                    inode: found.inode,
                })
            }
        }
    }

    pub fn read_open(&self, file: &OpenFile, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.read_open_covered(file, offset, buf).map(|(n, _)| n)
    }

    /// [`read_open`](Self::read_open), also reporting how many of the bytes
    /// returned were checksum-verified (btrfs data with a stored checksum) and
    /// how many were not (ext4 has no data checksums; a btrfs `NODATASUM`
    /// file opts out). Returns [`LuksError::FileChanged`] if the file is no
    /// longer the one that was opened.
    pub fn read_open_covered(
        &self,
        file: &OpenFile,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<(usize, btrfs::ReadCoverage)> {
        match (self, file) {
            (MountedFs::Ext4(fs), OpenFile::Ext4(opened)) => {
                let now = match fs.read_inode(opened.number) {
                    Ok(now) => now,
                    // An inode number that no longer resolves is a file that
                    // is gone.
                    Err(LuksError::BadInode(_)) => return Err(LuksError::FileChanged),
                    Err(e) => return Err(e),
                };
                if !opened.same_file_as(&now) {
                    return Err(LuksError::FileChanged);
                }
                let n = fs.read_inode_data(&now, offset, buf)?;
                Ok((
                    n,
                    btrfs::ReadCoverage { verified_bytes: 0, unverified_bytes: n as u64 },
                ))
            }
            (MountedFs::Btrfs(fs), OpenFile::Btrfs { subvol, inode: opened }) => {
                // The CURRENT root of the subvolume, looked up now: a commit
                // since the file was opened has moved it (R3r). A subvolume
                // that is gone takes its files with it.
                let Some(tree) = fs.current_tree(*subvol)? else {
                    return Err(LuksError::FileChanged);
                };
                let now = match fs.read_inode(tree.bytenr, opened.objectid) {
                    Ok(now) => now,
                    Err(LuksError::BadInode(_)) => return Err(LuksError::FileChanged),
                    Err(e) => return Err(e),
                };
                if !opened.same_file_as(&now) {
                    return Err(LuksError::FileChanged);
                }
                fs.read_inode_data_covered(tree.bytenr, &now, offset, buf)
            }
            // Only reachable by handing a file opened on one volume to another,
            // which no caller does — but returning an error beats a panic on a
            // path that runs with a device attached.
            _ => Err(LuksError::UnknownFs),
        }
    }

    /// Subvolumes, empty on a filesystem that has no such concept.
    pub fn subvolumes(&self) -> Result<Vec<btrfs::Subvolume>> {
        match self {
            MountedFs::Ext4(_) => Ok(Vec::new()),
            MountedFs::Btrfs(fs) => fs.subvolumes(),
        }
    }

    /// Metadata-cache hits and misses, for the filesystems that keep one.
    ///
    /// `None` for ext4, which has no equivalent — an ext4 inode names physical
    /// blocks directly, so reading a file never re-walks a tree. The
    /// indirection that makes btrfs need a cache is the same indirection that
    /// makes it able to verify data.
    pub fn cache_stats(&self) -> Option<(u64, u64)> {
        match self {
            MountedFs::Ext4(_) => None,
            MountedFs::Btrfs(fs) => Some(fs.node_cache_stats()),
        }
    }

    /// Filesystem capacity and allocation statistics.
    pub fn statfs(&self) -> Result<crate::fs::StatFs> {
        match self {
            MountedFs::Ext4(fs) => fs.statfs(),
            MountedFs::Btrfs(fs) => fs.statfs(),
        }
    }
}


