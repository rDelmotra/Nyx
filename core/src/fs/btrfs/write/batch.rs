//! Batch write transaction engine for Btrfs.
//!
//! A `Batch` maintains in-memory state (running roots, dirty `pending_blocks`,
//! allocator, and counters) across one or more file writes to eliminate redundant
//! full-tree scans and amortize transaction commit overhead.

use std::collections::HashMap;
use std::time::Instant;

use crate::device::WriteAt;
use crate::error::{LuksError, Result};
use crate::fs::btrfs::tree::{
    Key, DIR_INDEX_KEY, DIR_ITEM_KEY, EXTENT_DATA_KEY, INODE_ITEM_KEY,
    INODE_REF_KEY,
};
use crate::fs::btrfs::write::alloc::FreeSpaceMap;
use crate::fs::btrfs::write::cow::{cow_tree_insert, cow_tree_mutate};
use crate::fs::btrfs::write::extent_tree::{
    converge_and_finalize, find_item_in_tree, find_max_inode, record_cow_result, ExtentTree,
};
use crate::fs::btrfs::write::file::BtrfsFileWriter;
use crate::fs::btrfs::write::interval_set::IntervalSet;
use crate::fs::btrfs::write::node;
use crate::fs::btrfs::write::target::TargetTree;
use crate::fs::btrfs::write::txn::Transaction;
use crate::fs::btrfs::write::gate;
use crate::fs::btrfs::{Btrfs, Node};

/// Largest extent the merge below will build (the kernel's own write-side
/// extent cap, `BTRFS_MAX_EXTENT_SIZE`).
const MAX_MERGED_EXTENT: u64 = 128 * 1024 * 1024;

/// Merge runs that are consecutive in file order and contiguous on disk, but
/// never across a block-group boundary: `data_groups` is the `(start, end)` of
/// every block group. Data groups are logically adjacent (`next_logical`
/// appends each new chunk), so a run ending at one group's end can touch the
/// next group's first run. An extent spanning two groups cannot be freed
/// (`free_data` needs one containing group) and is accounted to only one (H1).
pub(crate) fn merge_adjacent_runs(runs: &[(u64, u64)], groups: &[(u64, u64)]) -> Vec<(u64, u64)> {
    let group_of = |b: u64| groups.iter().position(|&(s, e)| b >= s && b < e);
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(runs.len());
    for &(bytenr, len) in runs {
        if len == 0 {
            continue;
        }
        if let Some(last) = out.last_mut() {
            if last.0.checked_add(last.1) == Some(bytenr)
                && last.1 + len <= MAX_MERGED_EXTENT
                && group_of(last.0).is_some()
                && group_of(last.0) == group_of(bytenr)
            {
                last.1 += len;
                continue;
            }
        }
        out.push((bytenr, len));
    }
    out
}

/// Snapshot of batch state taken before adding a file, enabling single-file rollback.
#[derive(Debug, Clone)]
pub struct FileMark {
    pub allocator: FreeSpaceMap,
    pub fs_root: (u64, u8),
    pub csum_root: Option<(u64, u8)>,
    pub pending_blocks: HashMap<u64, Vec<u8>>,
    pub blocks_to_add: Vec<(u64, u8, u64)>,
    pub blocks_to_remove: Vec<(u64, u8, u64)>,
    pub data_extents_to_add: Vec<(u64, u64, u64, u64, u64)>,
    pub target: Option<(TargetTree, u64)>,
    pub next_dir_index: HashMap<u64, u64>,
    pub parents: HashMap<String, (u64, u32, u32)>,
    pub handed_out: IntervalSet,
    pub accumulated_files: usize,
    pub accumulated_data_bytes: u64,
}

pub struct Batch {
    pub generation: u64,
    pub allocator: FreeSpaceMap,
    pub fs_root: (u64, u8),
    pub csum_root: Option<(u64, u8)>,
    pub pending_blocks: HashMap<u64, Vec<u8>>,
    pub pending_nodes: std::sync::Mutex<HashMap<u64, (u32, Node)>>,
    pub blocks_to_add: Vec<(u64, u8, u64)>,
    pub blocks_to_remove: Vec<(u64, u8, u64)>,
    pub data_extents_to_add: Vec<(u64, u64, u64, u64, u64)>,
    pub target: Option<(TargetTree, u64)>,
    pub next_dir_index: HashMap<u64, u64>,
    pub parents: HashMap<String, (u64, u32, u32)>, // path -> (ino, uid, gid)
    pub handed_out: IntervalSet,
    pub accumulated_files: usize,
    pub accumulated_data_bytes: u64,
    pub start_time: Instant,
}

impl Batch {
    /// Open a new batch by scanning the filesystem's extent tree.
    pub fn open<D: WriteAt>(fs: &mut Btrfs<D>) -> Result<Self> {
        let ext_tree = ExtentTree::read(fs)?;
        let chunk_map = fs.chunk_map().clone();
        let allocator = FreeSpaceMap::from_extent_tree_and_chunk_map(&ext_tree, &chunk_map)?;
        Self::open_with_allocator(fs, allocator)
    }

    /// Open a new batch with a pre-existing FreeSpaceMap allocator.
    pub fn open_with_allocator<D: WriteAt>(
        fs: &mut Btrfs<D>,
        allocator: FreeSpaceMap,
    ) -> Result<Self> {
        let sb = fs.superblock();
        let generation = sb.generation + 1;
        let fs_tree = fs.fs_tree();
        let fs_root = (fs_tree.bytenr, fs_tree.level);

        Ok(Self {
            generation,
            allocator,
            fs_root,
            csum_root: None,
            pending_blocks: HashMap::new(),
            pending_nodes: std::sync::Mutex::new(HashMap::new()),
            blocks_to_add: Vec::new(),
            blocks_to_remove: Vec::new(),
            data_extents_to_add: Vec::new(),
            target: None,
            next_dir_index: HashMap::new(),
            parents: HashMap::new(),
            handed_out: IntervalSet::new(),
            accumulated_files: 0,
            accumulated_data_bytes: 0,
            start_time: Instant::now(),
        })
    }

    /// The target tree of this batch, if bound.
    pub fn target_tree(&self) -> Option<TargetTree> {
        self.target.map(|(t, _)| t)
    }

    /// The next inode number to be allocated by this batch, if bound.
    pub fn next_ino(&self) -> Option<u64> {
        self.target.map(|(_, ino)| ino)
    }

    /// Take a snapshot mark of current batch state before attempting to add a file.
    pub fn mark(&self) -> FileMark {
        FileMark {
            allocator: self.allocator.clone(),
            fs_root: self.fs_root,
            csum_root: self.csum_root,
            pending_blocks: self.pending_blocks.clone(),
            blocks_to_add: self.blocks_to_add.clone(),
            blocks_to_remove: self.blocks_to_remove.clone(),
            data_extents_to_add: self.data_extents_to_add.clone(),
            target: self.target,
            next_dir_index: self.next_dir_index.clone(),
            parents: self.parents.clone(),
            handed_out: self.handed_out.clone(),
            accumulated_files: self.accumulated_files,
            accumulated_data_bytes: self.accumulated_data_bytes,
        }
    }

    /// Roll back to a previously captured mark (reverting single failed file).
    pub fn rollback(&mut self, mark: FileMark) {
        self.allocator = mark.allocator;
        self.fs_root = mark.fs_root;
        self.csum_root = mark.csum_root;
        self.pending_blocks = mark.pending_blocks;
        self.pending_nodes.lock().unwrap().clear();
        self.blocks_to_add = mark.blocks_to_add;
        self.blocks_to_remove = mark.blocks_to_remove;
        self.data_extents_to_add = mark.data_extents_to_add;
        self.target = mark.target;
        self.next_dir_index = mark.next_dir_index;
        self.parents = mark.parents;
        self.handed_out = mark.handed_out;
        self.accumulated_files = mark.accumulated_files;
        self.accumulated_data_bytes = mark.accumulated_data_bytes;
    }

    /// Whether this batch holds any uncommitted file mutations.
    pub fn has_uncommitted_files(&self) -> bool {
        self.accumulated_files > 0
            || !self.pending_blocks.is_empty()
            || !self.blocks_to_add.is_empty()
            || !self.blocks_to_remove.is_empty()
            || !self.data_extents_to_add.is_empty()
    }

    /// Check if batch commit threshold policy is satisfied:
    /// - 16 files
    /// - 8 MiB pending metadata memory
    /// - 64 MiB written data
    /// - 2.0s wall clock time
    pub fn should_commit(&self) -> bool {
        if self.accumulated_files >= 16 {
            return true;
        }
        let pending_bytes: usize = self.pending_blocks.values().map(|v| v.len()).sum();
        if pending_bytes >= 8 * 1024 * 1024 {
            return true;
        }
        if self.accumulated_data_bytes >= 64 * 1024 * 1024 {
            return true;
        }
        if self.start_time.elapsed() >= std::time::Duration::from_secs(2) {
            return true;
        }
        false
    }

    /// Record an allocated range in the double-allocation ledger guard.
    /// Fails immediately if an overlapping range was already handed out.
    pub fn record_allocation(&mut self, start: u64, length: u64) -> Result<()> {
        if !self.handed_out.insert(start, length) {
            return Err(LuksError::CorruptFs(
                "double allocation detected in batch allocation ledger",
            ));
        }
        Ok(())
    }

    /// Release an allocated range from the reservation ledger.
    /// Fails immediately with `CorruptFs` if the range was not present in the ledger.
    pub fn release_allocation(&mut self, start: u64, length: u64) -> Result<()> {
        if !self.handed_out.remove_exact(start, length) {
            return Err(LuksError::CorruptFs(
                "attempted to release allocation not present in reservation ledger",
            ));
        }
        Ok(())
    }

    /// Add a written file into this batch session via its writer.
    pub fn add_writer<D: WriteAt>(
        &mut self,
        fs: &mut Btrfs<D>,
        writer: &BtrfsFileWriter,
        parent_path: &str,
        name: &str,
    ) -> Result<u64> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        self.add_writer_with_time(
            fs,
            writer,
            parent_path,
            name,
            now.as_secs(),
            now.subsec_nanos(),
        )
    }

    /// Add a written file into this batch session via its writer with deterministic timestamps.
    pub fn add_writer_with_time<D: WriteAt>(
        &mut self,
        fs: &mut Btrfs<D>,
        writer: &BtrfsFileWriter,
        parent_path: &str,
        name: &str,
        now_sec: u64,
        now_nsec: u32,
    ) -> Result<u64> {
        self.add_file_internal(
            fs,
            writer.is_streaming,
            writer.written_bytes,
            writer.size,
            &writer.data_runs,
            &writer.checksums,
            parent_path,
            name,
            now_sec,
            now_nsec,
            true,
        )
    }

    /// Add a written file into this batch session.
    pub fn add_file<D: WriteAt>(
        &mut self,
        fs: &mut Btrfs<D>,
        is_streaming: bool,
        written_bytes: u64,
        declared_size: u64,
        data_runs: &[(u64, u64)],
        checksums: &[(u64, u32)],
        parent_path: &str,
        name: &str,
    ) -> Result<u64> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        self.add_file_with_time(
            fs,
            is_streaming,
            written_bytes,
            declared_size,
            data_runs,
            checksums,
            parent_path,
            name,
            now.as_secs(),
            now.subsec_nanos(),
        )
    }

    /// Add a written file into this batch session with explicit timestamps.
    pub fn add_file_with_time<D: WriteAt>(
        &mut self,
        fs: &mut Btrfs<D>,
        is_streaming: bool,
        written_bytes: u64,
        declared_size: u64,
        data_runs: &[(u64, u64)],
        checksums: &[(u64, u32)],
        parent_path: &str,
        name: &str,
        now_sec: u64,
        now_nsec: u32,
    ) -> Result<u64> {
        self.add_file_internal(
            fs,
            is_streaming,
            written_bytes,
            declared_size,
            data_runs,
            checksums,
            parent_path,
            name,
            now_sec,
            now_nsec,
            false,
        )
    }

    fn add_file_internal<D: WriteAt>(
        &mut self,
        fs: &mut Btrfs<D>,
        is_streaming: bool,
        written_bytes: u64,
        declared_size: u64,
        data_runs: &[(u64, u64)],
        checksums: &[(u64, u32)],
        parent_path: &str,
        name: &str,
        now_sec: u64,
        now_nsec: u32,
        already_reserved: bool,
    ) -> Result<u64> {
        if !is_streaming && written_bytes != declared_size {
            return Err(LuksError::OutOfBounds);
        }
        let size = if is_streaming {
            written_bytes
        } else {
            declared_size
        };

        // B7: resolve the csum tree BEFORE touching any batch state. A lookup
        // failure propagates; an absent csum tree refuses the write rather than
        // silently publishing data with no checksums on a filesystem whose
        // other data has them. (This used to be `if let Ok(..)`: any failure
        // skipped the csum insert and the file was still counted as written.)
        let csum_tree_root = if checksums.is_empty() {
            None
        } else {
            match crate::fs::btrfs::write::extent_tree::tree_root_if_present(
                fs,
                crate::fs::btrfs::tree::CSUM_TREE_OBJECTID,
            )? {
                Some(r) => Some(r),
                None => {
                    return Err(LuksError::UnsupportedFsFeature(
                        "writing file data to a filesystem that has no checksum tree".into(),
                    ))
                }
            }
        };

        // Record data runs into double-allocation ledger (if not already reserved)
        // and promote reservations to batch allocator.
        for &(bytenr, run_len) in data_runs {
            if run_len > 0 {
                if !already_reserved {
                    self.record_allocation(bytenr, run_len)?;
                }
                self.allocator.mark_allocated(bytenr, run_len)?;
            }
        }

        let (parent_ino, parent_uid, parent_gid) =
            if let (Some(_), Some(&(ino, uid, gid))) = (self.target, self.parents.get(parent_path)) {
                (ino, uid, gid)
            } else {
                let located_parent = fs.resolve_no_follow(fs.fs_tree(), parent_path)?;
                if !located_parent.inode.file_type().is_dir() {
                    return Err(LuksError::NotADirectory(parent_path.to_string()));
                }
                gate::check_writeable_subvolume(&located_parent.tree)?;

                if let Some((ref target_tree, _)) = self.target {
                    if target_tree.objectid != located_parent.tree.objectid {
                        return Err(LuksError::UnsupportedFsFeature(
                            "multi-subvolume batch not supported".into(),
                        ));
                    }
                }

                if self.target.is_none() {
                    let max_ino = find_max_inode(fs, located_parent.tree.bytenr)?;
                    let next = if max_ino < 256 { 256 } else { max_ino + 1 };
                    self.target = Some((TargetTree::new(located_parent.tree), next));
                    self.fs_root = (located_parent.tree.bytenr, located_parent.tree.level);
                }

                let ino = located_parent.inode.objectid;
                let uid = located_parent.inode.uid;
                let gid = located_parent.inode.gid;
                self.parents
                    .insert(parent_path.to_string(), (ino, uid, gid));
                (ino, uid, gid)
            };

        // Check if exists using pending-aware search
        let name_hash = crate::fs::btrfs::crc32c::name_hash(name.as_bytes());
        let dir_item_key = Key::new(parent_ino, DIR_ITEM_KEY, name_hash);
        if let Some(item_data) =
            find_item_in_tree(fs, &self.pending_blocks, self.fs_root.0, &dir_item_key)?
        {
            let entries = crate::fs::btrfs::inode::parse_dir_entries(&item_data)?;
            if entries.iter().any(|e| e.name == name.as_bytes()) {
                return Err(LuksError::AlreadyExists(format!(
                    "file '{}' already exists in '{}'",
                    name, parent_path
                )));
            }
        }

        let sb = fs.superblock();
        let node_size = sb.node_size as u64;
        let new_generation = self.generation;
        let (ref mut target_tree, ref mut next_ino_ref) = match self.target.as_mut() {
            Some(t) => t,
            None => {
                return Err(LuksError::CorruptFs(
                    "batch target tree and inode sequencer must be bound before file insertion",
                ));
            }
        };
        let new_ino = *next_ino_ref;
        *next_ino_ref += 1;
        let target_objectid = target_tree.objectid;

        let dir_index = if let Some(next_idx) = self.next_dir_index.get_mut(&parent_ino) {
            let idx = *next_idx;
            *next_idx += 1;
            idx
        } else {
            let mut max_dir_index = 1u64;
            fs.for_each_item(self.fs_root.0, parent_ino, DIR_INDEX_KEY, &mut |key, _| {
                if key.offset > max_dir_index {
                    max_dir_index = key.offset;
                }
                Ok(true)
            })?;
            let idx = max_dir_index + 1;
            self.next_dir_index.insert(parent_ino, idx + 1);
            idx
        };

        let mut fs_root_bytenr = self.fs_root.0;
        let mut fs_root_level = self.fs_root.1;

        // 1. Update parent directory INODE_ITEM
        let parent_inode_key = Key::new(parent_ino, INODE_ITEM_KEY, 0);
        let name_len = name.len() as u64;
        let res = cow_tree_mutate(
            fs,
            &self.pending_blocks,
            fs_root_bytenr,
            fs_root_level,
            target_objectid,
            &parent_inode_key,
            new_generation,
            &mut self.allocator,
            |leaf| {
                let idx = leaf
                    .find_item(&parent_inode_key)
                    .ok_or_else(|| LuksError::NotFound("parent inode not found in leaf".into()))?;
                let item = &mut leaf.items[idx].data;
                item[8..16].copy_from_slice(&new_generation.to_le_bytes());
                let cur_size = u64::from_le_bytes(item[16..24].try_into().unwrap());
                item[16..24].copy_from_slice(&(cur_size + name_len * 2).to_le_bytes());
                let seq = u64::from_le_bytes(item[72..80].try_into().unwrap()).wrapping_add(1);
                item[72..80].copy_from_slice(&seq.to_le_bytes());
                item[124..132].copy_from_slice(&now_sec.to_le_bytes());
                item[132..136].copy_from_slice(&now_nsec.to_le_bytes());
                item[136..144].copy_from_slice(&now_sec.to_le_bytes());
                item[144..148].copy_from_slice(&now_nsec.to_le_bytes());
                Ok(())
            },
        )?;
        for &(bytenr, _level) in &res.allocated {
            self.record_allocation(bytenr, node_size)?;
        }
        record_cow_result(
            &res,
            &mut self.blocks_to_add,
            &mut self.blocks_to_remove,
            &mut self.allocator,
            &mut self.pending_blocks,
            sb.node_size,
            target_objectid,
        )?;
        fs_root_bytenr = res.new_root_bytenr;
        fs_root_level = res.new_root_level;

        // 2. Insert DIR_ITEM
        let dir_item_data = node::build_dir_item(new_ino, new_generation, 1, name);
        let res = cow_tree_insert(
            fs,
            &self.pending_blocks,
            fs_root_bytenr,
            fs_root_level,
            target_objectid,
            dir_item_key,
            dir_item_data.clone(),
            new_generation,
            &mut self.allocator,
        )?;
        for &(bytenr, _level) in &res.allocated {
            self.record_allocation(bytenr, node_size)?;
        }
        record_cow_result(
            &res,
            &mut self.blocks_to_add,
            &mut self.blocks_to_remove,
            &mut self.allocator,
            &mut self.pending_blocks,
            sb.node_size,
            target_objectid,
        )?;
        fs_root_bytenr = res.new_root_bytenr;
        fs_root_level = res.new_root_level;

        // 3. Insert DIR_INDEX
        let dir_index_key = Key::new(parent_ino, DIR_INDEX_KEY, dir_index);
        let res = cow_tree_insert(
            fs,
            &self.pending_blocks,
            fs_root_bytenr,
            fs_root_level,
            target_objectid,
            dir_index_key,
            dir_item_data,
            new_generation,
            &mut self.allocator,
        )?;
        for &(bytenr, _level) in &res.allocated {
            self.record_allocation(bytenr, node_size)?;
        }
        record_cow_result(
            &res,
            &mut self.blocks_to_add,
            &mut self.blocks_to_remove,
            &mut self.allocator,
            &mut self.pending_blocks,
            sb.node_size,
            target_objectid,
        )?;
        fs_root_bytenr = res.new_root_bytenr;
        fs_root_level = res.new_root_level;

        // 4. Insert new INODE_ITEM
        let inode_key = Key::new(new_ino, INODE_ITEM_KEY, 0);
        let mode = 0o100644; // Regular file
        let total_disk_bytes: u64 = data_runs.iter().map(|(_, len)| *len).sum();
        let inode_data = {
            let mut data = node::build_empty_inode_item(
                new_generation,
                mode,
                parent_uid,
                parent_gid,
                1,
                now_sec,
                now_nsec,
            );
            data[16..24].copy_from_slice(&size.to_le_bytes()); // size
            data[24..32].copy_from_slice(&total_disk_bytes.to_le_bytes()); // nbytes
            data
        };
        let res = cow_tree_insert(
            fs,
            &self.pending_blocks,
            fs_root_bytenr,
            fs_root_level,
            target_objectid,
            inode_key,
            inode_data,
            new_generation,
            &mut self.allocator,
        )?;
        for &(bytenr, _level) in &res.allocated {
            self.record_allocation(bytenr, node_size)?;
        }
        record_cow_result(
            &res,
            &mut self.blocks_to_add,
            &mut self.blocks_to_remove,
            &mut self.allocator,
            &mut self.pending_blocks,
            sb.node_size,
            target_objectid,
        )?;
        fs_root_bytenr = res.new_root_bytenr;
        fs_root_level = res.new_root_level;

        // 5. Insert INODE_REF
        let inode_ref_key = Key::new(new_ino, INODE_REF_KEY, parent_ino);
        let inode_ref_data = node::build_inode_ref(dir_index, name);
        let res = cow_tree_insert(
            fs,
            &self.pending_blocks,
            fs_root_bytenr,
            fs_root_level,
            target_objectid,
            inode_ref_key,
            inode_ref_data,
            new_generation,
            &mut self.allocator,
        )?;
        for &(bytenr, _level) in &res.allocated {
            self.record_allocation(bytenr, node_size)?;
        }
        record_cow_result(
            &res,
            &mut self.blocks_to_add,
            &mut self.blocks_to_remove,
            &mut self.allocator,
            &mut self.pending_blocks,
            sb.node_size,
            target_objectid,
        )?;
        fs_root_bytenr = res.new_root_bytenr;
        fs_root_level = res.new_root_level;

        // 6. Insert EXTENT_DATA (if any)
        let mut file_offset = 0u64;
        // P-3: physically adjacent runs (consecutive in file order, each
        // starting where the previous ends) become one EXTENT_DATA item and one
        // extent-tree item. The reservation ledger and allocator above were
        // updated per original run, so releasing the writer's `data_runs`
        // still matches what was recorded.
        let groups: Vec<(u64, u64)> = self
            .allocator
            .block_groups
            .iter()
            .map(|bg| (bg.block_group.start, bg.block_group.start + bg.block_group.length))
            .collect();
        let merged_runs = merge_adjacent_runs(data_runs, &groups);
        for &(bytenr, run_len) in &merged_runs {
            if run_len > 0 {
                let extent_data_key = Key::new(new_ino, EXTENT_DATA_KEY, file_offset);
                let extent_data_item = node::build_regular_file_extent(
                    new_generation,
                    run_len,
                    bytenr,
                    run_len,
                    run_len,
                );
                let res = cow_tree_insert(
                    fs,
                    &self.pending_blocks,
                    fs_root_bytenr,
                    fs_root_level,
                    target_objectid,
                    extent_data_key,
                    extent_data_item,
                    new_generation,
                    &mut self.allocator,
                )?;
                for &(bytenr, _level) in &res.allocated {
                    self.record_allocation(bytenr, node_size)?;
                }
                record_cow_result(
                    &res,
                    &mut self.blocks_to_add,
                    &mut self.blocks_to_remove,
                    &mut self.allocator,
                    &mut self.pending_blocks,
                    sb.node_size,
                    target_objectid,
                )?;
                fs_root_bytenr = res.new_root_bytenr;
                fs_root_level = res.new_root_level;
                self.data_extents_to_add.push((
                    bytenr,
                    run_len,
                    target_objectid,
                    new_ino,
                    file_offset,
                ));
                file_offset += run_len;
            }
        }

        self.fs_root = (fs_root_bytenr, fs_root_level);
        if let Some((ref mut target_tree, _)) = self.target {
            target_tree.update_root(fs_root_bytenr, fs_root_level, new_generation);
        }

        // 7. CoW CSUM_TREE: insert EXTENT_CSUM items covering all data runs (if CSUM tree is present and non-empty)
        if let Some(csum_root) = csum_tree_root {
            {
                let mut csum_root_bytenr =
                    self.csum_root.map(|(b, _)| b).unwrap_or(csum_root.bytenr);
                let mut csum_root_level = self.csum_root.map(|(_, l)| l).unwrap_or(csum_root.level);

                let csum_items =
                    node::build_extent_csum_items_from_checksums(sb.node_size, checksums)?;

                for (range_start, csum_payload) in csum_items {
                    let csum_key = Key::new(
                        crate::fs::btrfs::tree::EXTENT_CSUM_OBJECTID,
                        crate::fs::btrfs::tree::EXTENT_CSUM_KEY,
                        range_start,
                    );
                    let res = cow_tree_insert(
                        fs,
                        &self.pending_blocks,
                        csum_root_bytenr,
                        csum_root_level,
                        crate::fs::btrfs::tree::CSUM_TREE_OBJECTID,
                        csum_key,
                        csum_payload,
                        new_generation,
                        &mut self.allocator,
                    )?;
                    for &(bytenr, _level) in &res.allocated {
                        self.record_allocation(bytenr, node_size)?;
                    }
                    record_cow_result(
                        &res,
                        &mut self.blocks_to_add,
                        &mut self.blocks_to_remove,
                        &mut self.allocator,
                        &mut self.pending_blocks,
                        sb.node_size,
                        crate::fs::btrfs::tree::CSUM_TREE_OBJECTID,
                    )?;
                    csum_root_bytenr = res.new_root_bytenr;
                    csum_root_level = res.new_root_level;
                }
                self.csum_root = Some((csum_root_bytenr, csum_root_level));
            }
        }

        self.accumulated_files += 1;
        self.accumulated_data_bytes += size;

        Ok(new_ino)
    }

    /// Build the transaction for the batch's current state WITHOUT consuming or
    /// mutating the batch. Everything handed to `converge_and_finalize` is a
    /// clone, so a failure here or in the later device commit leaves every
    /// pending block, extent delta, allocator reservation and ledger entry in
    /// place for a retry (D2/N7/N14/B3).
    ///
    /// The caller must hold the batch OUTSIDE `fs.active_batch` while this runs
    /// (as `finish_file` / `commit_active_batch` do): `fs.fs_tree()` and
    /// `read_node` consult the active batch, which would make the commit see
    /// its own uncommitted root as the committed one.
    pub fn prepare_commit<D: WriteAt>(&self, fs: &Btrfs<D>) -> Result<Transaction> {
        let mut new_fs_tree = self.target_tree().map(|t| t.root).unwrap_or_else(|| fs.fs_tree());
        new_fs_tree.bytenr = self.fs_root.0;
        new_fs_tree.level = self.fs_root.1;
        new_fs_tree.generation = self.generation;

        converge_and_finalize(
            fs,
            self.generation,
            new_fs_tree,
            self.csum_root,
            None,
            None,
            None,
            self.pending_blocks.clone(),
            Vec::new(),
            self.allocator.clone(),
            self.blocks_to_add.clone(),
            self.blocks_to_remove.clone(),
            self.data_extents_to_add.clone(),
            Vec::new(),
        )
    }

    /// Re-arm the batch after `txn` (built by `prepare_commit`) became durable:
    /// adopt the post-commit allocator with freed ranges unpinned (only now is
    /// it safe to reuse them, B3), forget everything the commit covered, and
    /// move to the NEXT generation (B1). `commit_transaction` leaves
    /// `sb.generation == txn.new_generation`, and every later node header,
    /// inode transid, dir item and extent item stamps `self.generation`, so the
    /// next commit must be strictly greater.
    pub fn rearm_after_commit(&mut self, txn: &mut Transaction) {
        if let Some(final_alloc) = txn.final_allocator.take() {
            self.allocator = final_alloc;
            self.allocator.unpin_freed();
        }
        self.pending_blocks.clear();
        self.pending_nodes.lock().unwrap().clear();
        self.blocks_to_add.clear();
        self.blocks_to_remove.clear();
        self.data_extents_to_add.clear();
        self.fs_root = (txn.new_fs_tree.bytenr, txn.new_fs_tree.level);
        if let Some((ref mut target_tree, _)) = self.target {
            target_tree.update_root(txn.new_fs_tree.bytenr, txn.new_fs_tree.level, txn.new_generation);
        }
        self.csum_root = None;
        self.generation = txn.new_generation + 1;
        self.handed_out.clear();
        self.accumulated_files = 0;
        self.accumulated_data_bytes = 0;
        self.start_time = Instant::now();
    }

    /// Commit the batch to the device and re-arm it. On `Err` the batch is
    /// untouched (see `prepare_commit`, and the retry argument in `commit.rs`).
    pub fn commit_and_rearm<D: WriteAt>(
        &mut self,
        fs: &mut Btrfs<D>,
    ) -> std::result::Result<(), crate::fs::btrfs::write::commit::CommitError> {
        let mut txn = self.prepare_commit(fs)?;
        crate::fs::btrfs::write::commit::commit_transaction_ref(fs, &txn)?;
        self.rearm_after_commit(&mut txn);
        Ok(())
    }

    /// Commit the batch into a Transaction ready to be committed to disk.
    pub fn commit<D: WriteAt>(self, fs: &mut Btrfs<D>) -> Result<Transaction> {
        let mut new_fs_tree = self.target_tree().map(|t| t.root).unwrap_or_else(|| fs.fs_tree());
        new_fs_tree.bytenr = self.fs_root.0;
        new_fs_tree.level = self.fs_root.1;
        new_fs_tree.generation = self.generation;

        converge_and_finalize(
            fs,
            self.generation,
            new_fs_tree,
            self.csum_root,
            None,
            None,
            None,
            self.pending_blocks,
            Vec::new(),
            self.allocator,
            self.blocks_to_add,
            self.blocks_to_remove,
            self.data_extents_to_add,
            Vec::new(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::merge_adjacent_runs;

    #[test]
    fn merge_stops_at_block_group_boundary() {
        let groups = [(0, 1000), (1000, 2000)];
        // Adjacent inside group 0: merged. Adjacent across 1000: not.
        let runs = [(0, 300), (300, 700), (1000, 200), (1200, 100)];
        assert_eq!(merge_adjacent_runs(&runs, &groups), vec![(0, 1000), (1000, 300)]);
        // Run outside every group is never merged.
        assert_eq!(merge_adjacent_runs(&[(5000, 10), (5010, 10)], &groups), vec![(5000, 10), (5010, 10)]);
    }
}
