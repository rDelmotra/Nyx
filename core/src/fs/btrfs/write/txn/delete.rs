//! `Transaction::delete_file`.

use std::collections::HashMap;

use crate::device::ReadAt;
use crate::error::{LuksError, Result};
use crate::fs::btrfs::tree::{
    Key, CSUM_TREE_OBJECTID, DIR_INDEX_KEY, DIR_ITEM_KEY, EXTENT_DATA_KEY, INODE_ITEM_KEY,
    INODE_REF_KEY,
};
use crate::fs::btrfs::write::alloc::FreeSpaceMap;
use crate::fs::btrfs::write::cow::cow_tree_mutate;
use crate::fs::btrfs::write::extent_tree::{converge_and_finalize, record_cow_result, ExtentTree};
use crate::fs::btrfs::write::gate;
use crate::fs::btrfs::write::target::TargetTree;
use crate::fs::btrfs::inode::Inode;
use crate::fs::btrfs::Btrfs;
use crate::fs::FileType;

use super::Transaction;

use crate::fs::btrfs::write::gate::{BTRFS_INODE_APPEND as INODE_APPEND, BTRFS_INODE_IMMUTABLE as INODE_IMMUTABLE};

/// Everything about one inode (and the directory holding its name) that makes
/// the writer refuse to delete it, with the kernel's semantics: an immutable
/// or append-only inode cannot be unlinked, and no entry can be removed from
/// an immutable or append-only directory. `None` means deletable as far as
/// the inode items go (shared extents are checked separately).
///
/// Regular files, directories, symlinks, FIFOs, sockets and device nodes are
/// all deletable: apart from a regular file's data extents (freed explicitly)
/// they own only metadata items keyed under their own objectid, and the
/// delete sweeps every such item.
pub(crate) fn item_refusal(inode: &Inode, parent: &Inode) -> Option<&'static str> {
    if parent.flags & (INODE_IMMUTABLE | INODE_APPEND) != 0 {
        return Some("the containing directory is immutable or append-only");
    }
    if inode.flags & (INODE_IMMUTABLE | INODE_APPEND) != 0 {
        return Some("the item is immutable or append-only");
    }
    if inode.file_type() == FileType::Unknown {
        return Some("deleting items of unknown file type is not supported");
    }
    if inode.nlink > 1 {
        return Some("deleting files with hardlinks (nlink > 1) is not supported");
    }
    None
}

/// Read-only walk of everything a recursive delete of `path` would remove,
/// running every refusal the per-item delete would run, so the delete is
/// refused as a whole (naming the first offending path) before anything is
/// written. Iterative, with the same depth bound as the delete itself.
pub(crate) fn preflight_recursive_delete<D: ReadAt>(fs: &Btrfs<D>, path: &str) -> Result<()> {
    use crate::fs::FileType as Ft;
    let subvol = |p: &str| {
        LuksError::UnsupportedFsFeature(format!("subvolume deletion is not supported ({p})"))
    };

    gate::check_writeable_fs(fs.superblock())?;
    let trimmed = path.trim_matches('/');
    if trimmed.is_empty() {
        return Err(LuksError::IsADirectory("/".into()));
    }
    let (parent_path, filename) = match trimmed.rsplit_once('/') {
        Some((p, n)) => (p, n),
        None => ("", trimmed),
    };
    let located_parent = fs.resolve_no_follow(fs.fs_tree(), parent_path)?;
    if !located_parent.inode.file_type().is_dir() {
        return Err(LuksError::NotADirectory(parent_path.to_string()));
    }
    gate::check_writeable_subvolume(&located_parent.tree)?;
    let dirent = fs
        .lookup(located_parent.tree.bytenr, located_parent.inode.objectid, filename)?
        .ok_or_else(|| LuksError::NotFound(path.to_string()))?;
    if dirent.is_subvolume() {
        return Err(subvol(path));
    }
    let located = fs.resolve_no_follow(fs.fs_tree(), path)?;
    if located.tree.objectid != located_parent.tree.objectid {
        return Err(subvol(path));
    }
    gate::check_writeable_subvolume(&located.tree)?;
    let tree = located.tree.bytenr;

    let extent_tree = ExtentTree::read(fs)?;
    gate::check_no_shared_history_or_quota(fs, &extent_tree)?;
    let extents: HashMap<u64, usize> = extent_tree
        .extents
        .iter()
        .enumerate()
        .map(|(i, e)| (e.bytenr, i))
        .collect();

    let mut seen = std::collections::HashSet::new();
    let base = path.trim_end_matches('/').to_string();
    let mut stack = vec![(base, located.inode, located_parent.inode, 0usize)];
    while let Some((p, inode, parent, depth)) = stack.pop() {
        if depth > super::super::MAX_DELETE_DEPTH {
            return Err(LuksError::CorruptFs(
                "directory delete recursed past a sane depth — likely a cycle",
            ));
        }
        if let Some(why) = item_refusal(&inode, &parent) {
            return Err(LuksError::UnsupportedFsFeature(format!("cannot delete {p}: {why}")));
        }
        if !seen.insert(inode.objectid) {
            return Err(LuksError::CorruptFs("directory tree aliases an inode already visited"));
        }
        if inode.file_type() == Ft::Directory {
            let children = fs.list_dir_by_inode(tree, inode.objectid)?;
            let mut next = Vec::with_capacity(children.len());
            for child in children {
                let child_path = format!("{}/{}", p.trim_end_matches('/'), child.name);
                if child.is_subvolume {
                    return Err(subvol(&child_path));
                }
                match fs.lookup(tree, inode.objectid, &child.name)? {
                    Some(d) if d.location.objectid == child.inode => {}
                    _ => return Err(LuksError::NotFound(child_path)),
                }
                let child_inode = fs.read_inode(tree, child.inode)?;
                next.push((child_path, child_inode, inode.clone(), depth + 1));
            }
            // Popped in listing order, i.e. the order the delete visits them.
            stack.extend(next.into_iter().rev());
        } else {
            let mut bad: Option<LuksError> = None;
            fs.for_each_item(tree, inode.objectid, EXTENT_DATA_KEY, &mut |key, data| {
                let ext = crate::fs::btrfs::extent::FileExtent::parse(key.offset, data)?;
                if ext.has_disk_bytes() {
                    if let Some(&i) = extents.get(&ext.disk_bytenr) {
                        if let Err(e) = gate::check_data_extent_freeable(&extent_tree.extents[i]) {
                            bad = Some(LuksError::UnsupportedFsFeature(format!(
                                "cannot delete {p}: {e}"
                            )));
                            return Ok(false);
                        }
                    }
                }
                Ok(true)
            })?;
            if let Some(e) = bad {
                return Err(e);
            }
        }
    }
    Ok(())
}

impl Transaction {
    /// Prepare a transaction that deletes an existing regular file at `path`.
    pub fn delete_file<D: ReadAt>(
        fs: &Btrfs<D>,
        path: &str,
        now_sec: u64,
        now_nsec: u32,
    ) -> Result<Self> {
        gate::check_writeable_fs(fs.superblock())?;

        let trimmed = path.trim_matches('/');
        if trimmed.is_empty() {
            return Err(LuksError::IsADirectory("/".into()));
        }

        let (parent_path, filename) = match trimmed.rsplit_once('/') {
            Some((p, n)) => (p, n),
            None => ("", trimmed),
        };

        let located_parent = fs.resolve_no_follow(fs.fs_tree(), parent_path)?;
        if !located_parent.inode.file_type().is_dir() {
            return Err(LuksError::NotADirectory(parent_path.to_string()));
        }
        gate::check_writeable_subvolume(&located_parent.tree)?;

        let dirent = fs
            .lookup(located_parent.tree.bytenr, located_parent.inode.objectid, filename)?
            .ok_or_else(|| LuksError::NotFound(path.to_string()))?;
        if dirent.is_subvolume() {
            return Err(LuksError::UnsupportedFsFeature(
                "subvolume deletion is not supported".into(),
            ));
        }

        let located_target = fs.resolve_no_follow(fs.fs_tree(), path)?;
        if located_target.tree.objectid != located_parent.tree.objectid {
            return Err(LuksError::UnsupportedFsFeature(
                "subvolume deletion is not supported".into(),
            ));
        }
        gate::check_writeable_subvolume(&located_target.tree)?;
        let target_tree = TargetTree::new(located_target.tree);
        let target_objectid = target_tree.objectid;

        let target_is_dir = located_target.inode.file_type().is_dir();
        if target_is_dir {
            // DIR_INDEX and DIR_ITEM always come in pairs per entry (see
            // `list_dir_by_inode`), so one DIR_INDEX item proves the
            // directory has at least one child. Below, deleting a directory
            // reuses the same generic "every item keyed under this objectid"
            // sweep as deleting a file's own metadata — for a directory that
            // sweep would also catch its children's DIR_ITEM/DIR_INDEX
            // entries (they're keyed under the *parent's* objectid), erasing
            // the names without freeing what they pointed to. Refusing a
            // non-empty directory here is what prevents that.
            let mut has_children = false;
            fs.for_each_item(
                target_tree.bytenr(),
                located_target.inode.objectid,
                DIR_INDEX_KEY,
                &mut |_, _| {
                    has_children = true;
                    Ok(false)
                },
            )?;
            if has_children {
                return Err(LuksError::DirectoryNotEmpty(path.to_string()));
            }
        }
        if let Some(why) = item_refusal(&located_target.inode, &located_parent.inode) {
            return Err(LuksError::UnsupportedFsFeature(why.into()));
        }

        let parent_ino = located_parent.inode.objectid;
        let target_ino = located_target.inode.objectid;

        // 1. Locate DIR_ITEM and verify it exists
        let name_hash = crate::fs::btrfs::crc32c::name_hash(filename.as_bytes());
        let dir_item_key = Key::new(parent_ino, DIR_ITEM_KEY, name_hash);
        let dir_item_raw = fs
            .find_item(target_tree.bytenr(), &dir_item_key)?
            .ok_or_else(|| LuksError::NotFound(path.to_string()))?;
        let dir_entries = crate::fs::btrfs::inode::parse_dir_entries(&dir_item_raw)?;
        if !dir_entries
            .iter()
            .any(|e| e.name == filename.as_bytes() && e.location.objectid == target_ino)
        {
            return Err(LuksError::NotFound(path.to_string()));
        }

        // 2. Find dir_index for DIR_INDEX
        let inode_ref_key = Key::new(target_ino, INODE_REF_KEY, parent_ino);
        let dir_index = if let Some(ref_data) = fs.find_item(target_tree.bytenr(), &inode_ref_key)? {
            if ref_data.len() >= 8 {
                u64::from_le_bytes(ref_data[0..8].try_into().unwrap())
            } else {
                0
            }
        } else {
            0
        };
        let dir_index = if dir_index != 0 {
            dir_index
        } else {
            let mut found_idx = 0;
            fs.for_each_item(
                target_tree.bytenr(),
                parent_ino,
                DIR_INDEX_KEY,
                &mut |key, data| {
                    if let Ok(entries) = crate::fs::btrfs::inode::parse_dir_entries(data) {
                        for e in entries {
                            if e.name == filename.as_bytes() && e.location.objectid == target_ino {
                                found_idx = key.offset;
                                return Ok(false);
                            }
                        }
                    }
                    Ok(true)
                },
            )?;
            if found_idx == 0 {
                return Err(LuksError::CorruptFs("dir index not found for file"));
            }
            found_idx
        };
        let dir_index_key = Key::new(parent_ino, DIR_INDEX_KEY, dir_index);

        // 3. Scan all items for target_ino
        let mut target_items: Vec<(Key, Vec<u8>)> = Vec::new();
        let mut cursor = fs.search(target_tree.bytenr(), &Key::new(target_ino, 0, 0))?;
        while cursor.valid() {
            let key = cursor.key()?;
            if key.objectid != target_ino {
                break;
            }
            target_items.push((key, cursor.data()?.to_vec()));
            cursor.advance()?;
        }

        // 4. Collect data extents to free
        let mut data_extents_to_free: Vec<(u64, u64)> = Vec::new();
        for (key, data) in &target_items {
            if key.item_type == EXTENT_DATA_KEY {
                let file_ext = crate::fs::btrfs::extent::FileExtent::parse(key.offset, data)?;
                if file_ext.has_disk_bytes() {
                    data_extents_to_free.push((file_ext.disk_bytenr, file_ext.disk_num_bytes));
                }
            }
        }
        data_extents_to_free.sort_unstable();
        data_extents_to_free.dedup();

        // G1: Refuse to free shared extents (refs > 1), or one whose only
        // ref lives outside the extent item. This prevents corruption when
        // deleting files that share extents via reflink, deduplication, or
        // snapshots.
        let extent_tree = ExtentTree::read(fs)?;
        gate::check_no_shared_history_or_quota(fs, &extent_tree)?;
        for &(bytenr, _num_bytes) in &data_extents_to_free {
            if let Some(ext) = extent_tree.extents.iter().find(|e| e.bytenr == bytenr) {
                gate::check_data_extent_freeable(ext)?;
            }
        }

        let sb = fs.superblock();
        let new_generation = sb.generation + 1;

        let mut allocator = FreeSpaceMap::from_extent_tree_and_chunk_map(&extent_tree, fs.chunk_map())?;
        let mut pending_blocks = HashMap::new();
        let mut blocks_to_add = Vec::<(u64, u8, u64)>::new();
        let mut blocks_to_remove = Vec::<(u64, u8, u64)>::new();

        let mut fs_root_bytenr = target_tree.bytenr();
        let mut fs_root_level = target_tree.level();

        // 5. CoW target tree: Update parent directory DIR_ITEM
        let res = cow_tree_mutate(
            fs,
            &pending_blocks,
            fs_root_bytenr,
            fs_root_level,
            target_objectid,
            &dir_item_key,
            new_generation,
            &mut allocator,
            |leaf| {
                let idx = leaf
                    .find_item(&dir_item_key)
                    .ok_or_else(|| LuksError::NotFound("dir item not found".into()))?;
                let existing_data = &leaf.items[idx].data;
                let mut new_dir_item_data = Vec::new();
                let mut cur = &existing_data[..];
                while !cur.is_empty() {
                    if cur.len() < 30 {
                        return Err(LuksError::CorruptFs("dir item truncated"));
                    }
                    let data_len = u16::from_le_bytes([cur[25], cur[26]]) as usize;
                    let name_len = u16::from_le_bytes([cur[27], cur[28]]) as usize;
                    let total = 30 + name_len + data_len;
                    if total > cur.len() {
                        return Err(LuksError::CorruptFs("dir item overruns"));
                    }
                    let name = &cur[30..30 + name_len];
                    let child_ino = u64::from_le_bytes(cur[0..8].try_into().unwrap());
                    if !(name == filename.as_bytes() && child_ino == target_ino) {
                        new_dir_item_data.extend_from_slice(&cur[..total]);
                    }
                    cur = &cur[total..];
                }
                if new_dir_item_data.is_empty() {
                    leaf.delete_item(&dir_item_key)?;
                } else {
                    leaf.items[idx].data = new_dir_item_data;
                }
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
            target_objectid,
        )?;
        fs_root_bytenr = res.new_root_bytenr;
        fs_root_level = res.new_root_level;

        // 6. CoW target tree: Delete parent directory DIR_INDEX
        let res = cow_tree_mutate(
            fs,
            &pending_blocks,
            fs_root_bytenr,
            fs_root_level,
            target_objectid,
            &dir_index_key,
            new_generation,
            &mut allocator,
            |leaf| {
                leaf.delete_item(&dir_index_key)?;
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
            target_objectid,
        )?;
        fs_root_bytenr = res.new_root_bytenr;
        fs_root_level = res.new_root_level;

        // 7. CoW target tree: Update parent directory INODE_ITEM
        let parent_inode_key = Key::new(parent_ino, INODE_ITEM_KEY, 0);
        let name_len = filename.len() as u64;
        let res = cow_tree_mutate(
            fs,
            &pending_blocks,
            fs_root_bytenr,
            fs_root_level,
            target_objectid,
            &parent_inode_key,
            new_generation,
            &mut allocator,
            |leaf| {
                let idx = leaf
                    .find_item(&parent_inode_key)
                    .ok_or_else(|| LuksError::NotFound("parent inode not found".into()))?;
                let item_data = &mut leaf.items[idx].data;
                if item_data.len() < 160 {
                    return Err(LuksError::CorruptFs("parent inode item truncated"));
                }
                item_data[8..16].copy_from_slice(&new_generation.to_le_bytes()); // transid
                let cur_size = u64::from_le_bytes(item_data[16..24].try_into().unwrap());
                item_data[16..24]
                    .copy_from_slice(&(cur_size.saturating_sub(name_len * 2)).to_le_bytes()); // size
                let seq =
                    u64::from_le_bytes(item_data[72..80].try_into().unwrap()).wrapping_add(1);
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
            target_objectid,
        )?;
        fs_root_bytenr = res.new_root_bytenr;
        fs_root_level = res.new_root_level;

        // 8. CoW target tree: Delete all items with objectid == target_ino
        for (item_key, _) in &target_items {
            let res = cow_tree_mutate(
                fs,
                &pending_blocks,
                fs_root_bytenr,
                fs_root_level,
                target_objectid,
                item_key,
                new_generation,
                &mut allocator,
                |leaf| {
                    leaf.delete_item(item_key)?;
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
                target_objectid,
            )?;
            fs_root_bytenr = res.new_root_bytenr;
            fs_root_level = res.new_root_level;
        }

        // 9. Free data extents in allocator
        for &(bytenr, num_bytes) in &data_extents_to_free {
            allocator.free_data(bytenr, num_bytes)?;
        }

        // 10. CoW CSUM_TREE: Delete/trim EXTENT_CSUM items covering freed data extents
        let mut csum_root_opt = None;
        if !data_extents_to_free.is_empty() {
            if let Ok(csum_root) = fs.tree_root(CSUM_TREE_OBJECTID) {
                csum_root_opt = crate::fs::btrfs::write::csum_delete::delete_extent_csums(
                    fs,
                    &mut pending_blocks,
                    csum_root.bytenr,
                    csum_root.level,
                    &data_extents_to_free,
                    new_generation,
                    &mut allocator,
                    &mut blocks_to_add,
                    &mut blocks_to_remove,
                )?;
            }
        }

        let mut new_fs_tree = target_tree.root;
        new_fs_tree.bytenr = fs_root_bytenr;
        new_fs_tree.level = fs_root_level;
        new_fs_tree.generation = new_generation;

        crate::forensic::record_btrfs(crate::forensic::BtrfsEvent::DeleteFile {
            parent_ino,
            ino: target_ino,
        });

        converge_and_finalize(
            fs,
            new_generation,
            new_fs_tree,
            csum_root_opt,
            None,
            None,
            None,
            pending_blocks,
            Vec::new(),
            allocator,
            blocks_to_add,
            blocks_to_remove,
            Vec::new(),
            data_extents_to_free,
        )
    }
}
