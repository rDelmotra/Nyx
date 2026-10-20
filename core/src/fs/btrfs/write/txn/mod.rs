//! Btrfs transaction coordination and fixed-point convergence loop.
//!
//! A transaction computes all CoW path copies and extent tree bookkeeping
//! completely in memory until no further metadata blocks need allocation.
//!
//! Each `Transaction::*` constructor lives in its own submodule (split out of
//! a single 3,200+ line file for reviewability):
//! - `mtime`: `update_inode_mtime`
//! - `create`: `create_empty_file`, `create_directory`, `create_directory_in_dir`
//! - `rename`: `rename` (927 lines on its own; further split into private
//!   helpers within that module)
//! - `data`: `write_file_data`
//! - `delete`: `delete_file`
//! - `chunk`: `allocate_data_chunk` (thin driver over `chunk_alloc`)

mod chunk;
mod create;
mod data;
pub(crate) mod delete;
mod mtime;
mod rename;

use std::collections::HashMap;

use crate::fs::btrfs::TreeRoot;

/// A completed, self-consistent in-memory btrfs transaction.
#[derive(Clone)]
pub struct Transaction {
    pub new_generation: u64,
    pub final_root_bytenr: u64,
    pub final_root_level: u8,
    pub final_chunk_root: Option<(u64, u8)>,
    pub final_bytes_used: u64,
    pub new_fs_tree: TreeRoot,
    pub pending_blocks: HashMap<u64, Vec<u8>>,
    pub pending_data: Vec<(u64, Vec<u8>)>,
    pub final_allocator: Option<crate::fs::btrfs::write::alloc::FreeSpaceMap>,
    /// Chunks created by this transaction. The commit resolves addresses
    /// through them, but the live chunk map only learns them after the commit
    /// succeeded (B4).
    pub new_chunks: Vec<crate::fs::btrfs::chunk::Chunk>,
    /// The chunk tree's DEV_ITEM `bytes_used` as written by this transaction
    /// (absolute). The commit stores it in the superblock's embedded dev_item.
    pub final_dev_bytes_used: Option<u64>,
}
