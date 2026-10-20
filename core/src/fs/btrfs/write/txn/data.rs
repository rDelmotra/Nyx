//! `Transaction::write_file_data`.

use std::collections::HashMap;

use crate::device::ReadAt;
use crate::error::{LuksError, Result};
use crate::fs::btrfs::tree::{
    Key, CSUM_TREE_OBJECTID, INODE_ITEM_KEY,
};
use crate::fs::btrfs::write::alloc::FreeSpaceMap;
use crate::fs::btrfs::write::cow::{cow_tree_insert, cow_tree_mutate};
use crate::fs::btrfs::write::extent_tree::{
    converge_and_finalize, record_cow_result, ExtentTree,
};
use crate::fs::btrfs::write::gate;
use crate::fs::btrfs::write::target::TargetTree;
use crate::fs::btrfs::Btrfs;

use super::Transaction;

impl Transaction {
    /// Prepare a transaction that writes `data` into the file at `file_path`.
    pub fn write_file_data<D: ReadAt>(
        fs: &Btrfs<D>,
        file_path: &str,
        data: &[u8],
        now_sec: u64,
        now_nsec: u32,
    ) -> Result<Self> {
        gate::check_writeable_fs(fs.superblock())?;

        let located = fs.resolve_no_follow(fs.fs_tree(), file_path)?;
        if located.inode.file_type().is_dir() {
            return Err(LuksError::IsADirectory(file_path.to_string()));
        }
        gate::check_writeable_subvolume(&located.tree)?;

        let target_tree = TargetTree::new(located.tree);
        let ino = located.inode.objectid;
        let sb = fs.superblock();
        let new_generation = sb.generation + 1;
        let sector_size = sb.sector_size;

        let extent_tree = ExtentTree::read(fs)?;

        gate::check_no_shared_history_or_quota(fs, &extent_tree)?;
        let mut allocator = FreeSpaceMap::from_extent_tree_and_chunk_map(&extent_tree, fs.chunk_map())?;
        let mut pending_blocks = HashMap::new();
        let mut blocks_to_add = Vec::<(u64, u8, u64)>::new();
        let mut blocks_to_remove = Vec::<(u64, u8, u64)>::new();
        let mut data_extents_to_add = Vec::<(u64, u64, u64, u64, u64)>::new();

        // 1. Allocate data extent
        let data_len = data.len() as u64;
        let disk_num_bytes = if data_len == 0 {
            0
        } else {
            data_len.div_ceil(sector_size as u64) * sector_size as u64
        };

        let mut pending_data = Vec::new();
        let disk_bytenr = if disk_num_bytes > 0 {
            let bytenr = allocator.allocate_data(disk_num_bytes, sector_size)?;
            let mut padded = vec![0u8; disk_num_bytes as usize];
            padded[..data.len()].copy_from_slice(data);
            pending_data.push((bytenr, padded));
            data_extents_to_add.push((bytenr, disk_num_bytes, target_tree.objectid, ino, 0));
            bytenr
        } else {
            0
        };

        let mut fs_root_bytenr = target_tree.bytenr();
        let mut fs_root_level = target_tree.level();

        // 2. CoW FS_TREE: Update INODE_ITEM (size, nbytes, sequence, transid, mtime/ctime)
        let inode_key = Key::new(ino, INODE_ITEM_KEY, 0);
        let res = cow_tree_mutate(
            fs,
            &pending_blocks,
            fs_root_bytenr,
            fs_root_level,
            target_tree.objectid,
            &inode_key,
            new_generation,
            &mut allocator,
            |leaf| {
                let idx = leaf
                    .find_item(&inode_key)
                    .ok_or_else(|| LuksError::NotFound("inode not found".into()))?;
                let item_data = &mut leaf.items[idx].data;
                if item_data.len() < 160 {
                    return Err(LuksError::CorruptFs("inode item truncated"));
                }
                item_data[8..16].copy_from_slice(&new_generation.to_le_bytes()); // transid
                item_data[16..24].copy_from_slice(&data_len.to_le_bytes()); // size
                item_data[24..32].copy_from_slice(&disk_num_bytes.to_le_bytes()); // nbytes
                let seq = u64::from_le_bytes(item_data[72..80].try_into().unwrap()).wrapping_add(1);
                item_data[72..80].copy_from_slice(&seq.to_le_bytes()); // sequence
                item_data[124..132].copy_from_slice(&now_sec.to_le_bytes()); // ctime
                item_data[132..136].copy_from_slice(&now_nsec.to_le_bytes());
                item_data[136..144].copy_from_slice(&now_sec.to_le_bytes()); // mtime
                item_data[144..148].copy_from_slice(&now_nsec.to_le_bytes());
                Ok(())
            },
        )?;
        record_cow_result(
            &res,
            &mut blocks_to_add,
            &mut blocks_to_remove,
            &mut allocator,
            &mut pending_blocks,
            sb.node_size,
            target_tree.objectid,
        )?;
        fs_root_bytenr = res.new_root_bytenr;
        fs_root_level = res.new_root_level;

        // 3. CoW FS_TREE: Insert EXTENT_DATA item if disk_num_bytes > 0
        if disk_num_bytes > 0 {
            let extent_data_key = Key::new(ino, crate::fs::btrfs::tree::EXTENT_DATA_KEY, 0);
            let extent_payload = crate::fs::btrfs::write::node::build_regular_file_extent(
                new_generation,
                disk_num_bytes,
                disk_bytenr,
                disk_num_bytes,
                disk_num_bytes,
            );
            let res = cow_tree_insert(
                fs,
                &pending_blocks,
                fs_root_bytenr,
                fs_root_level,
                target_tree.objectid,
                extent_data_key,
                extent_payload,
                new_generation,
                &mut allocator,
            )?;
            record_cow_result(
                &res,
                &mut blocks_to_add,
                &mut blocks_to_remove,
                &mut allocator,
                &mut pending_blocks,
                sb.node_size,
                target_tree.objectid,
            )?;
            fs_root_bytenr = res.new_root_bytenr;
            fs_root_level = res.new_root_level;
        }

        // 4. CoW CSUM_TREE: insert the EXTENT_CSUM items covering this extent.
        //
        // Items, plural, and that is the fix for the ~15 MiB ceiling. One
        // `EXTENT_CSUM` item per file has to fit in one leaf, so on the
        // standard geometry it caps a file at 16228 / 4 × 4096 ≈ 15.8 MiB —
        // reached by a 20 MB phone→stick write on 2026-08-16 and reported, in
        // the most misleading way available, as "no space left on the
        // filesystem". `build_extent_csum_items` tiles the extent across as
        // many items as the geometry needs, which is what the key's logical
        // address is *for* and what real btrfs does (measured 2026-08-15 on a
        // kernel-written 50 MB file).
        let mut csum_root_opt = None;
        if disk_num_bytes > 0 {
            if let Ok(csum_root) = fs.tree_root(CSUM_TREE_OBJECTID) {
                let csum_items = crate::fs::btrfs::write::node::build_extent_csum_items(
                    sb.csum_type,
                    sb.node_size,
                    sector_size,
                    disk_bytenr,
                    disk_num_bytes,
                    data,
                )?;

                let mut csum_root_bytenr = csum_root.bytenr;
                let mut csum_root_level = csum_root.level;

                for (range_start, csum_payload) in csum_items {
                    let csum_key = Key::new(
                        crate::fs::btrfs::tree::EXTENT_CSUM_OBJECTID,
                        crate::fs::btrfs::tree::EXTENT_CSUM_KEY,
                        range_start,
                    );
                    let res = cow_tree_insert(
                        fs,
                        &pending_blocks,
                        csum_root_bytenr,
                        csum_root_level,
                        CSUM_TREE_OBJECTID,
                        csum_key,
                        csum_payload,
                        new_generation,
                        &mut allocator,
                    )?;
                    record_cow_result(
                        &res,
                        &mut blocks_to_add,
                        &mut blocks_to_remove,
                        &mut allocator,
                        &mut pending_blocks,
                        sb.node_size,
                        CSUM_TREE_OBJECTID,
                    )?;
                    csum_root_bytenr = res.new_root_bytenr;
                    csum_root_level = res.new_root_level;
                }

                csum_root_opt = Some((csum_root_bytenr, csum_root_level));
            }
        }


        let mut new_fs_tree = target_tree.root;
        new_fs_tree.bytenr = fs_root_bytenr;
        new_fs_tree.level = fs_root_level;
        new_fs_tree.generation = new_generation;

        converge_and_finalize(
            fs,
            new_generation,
            new_fs_tree,
            csum_root_opt,
            None,
            None,
            None,
            pending_blocks,
            pending_data,
            allocator,
            blocks_to_add,
            blocks_to_remove,
            data_extents_to_add,
            Vec::new(),
        )
    }

}
