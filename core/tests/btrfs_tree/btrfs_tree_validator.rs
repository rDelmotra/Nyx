//! Self-tests for the shared `TreeValidator`: it must accept every clean
//! fixture and reject each kind of corruption it claims to catch. Other suites
//! rely on it, so a validator that silently passes would hide real bugs.

#![cfg(feature = "dangerous-write-support")]

#[path = "../common/mod.rs"]
mod common;

use std::collections::HashMap;

use common::btree_validator::TreeValidator;
use common::mem_device::MemoryDevice;

use luks_core::device::WriteAt;
use luks_core::fs::btrfs::tree::{Key, CSUM_TREE_OBJECTID, EXTENT_TREE_OBJECTID};
use luks_core::fs::btrfs::write::alloc::FreeSpaceMap;
use luks_core::fs::btrfs::write::cow::cow_tree_insert;
use luks_core::fs::btrfs::write::extent_tree::ExtentTree;
use luks_core::fs::btrfs::write::node::{InteriorEntry, InteriorNode, Leaf, LeafItem};
use luks_core::fs::btrfs::Btrfs;

/// Helper to initialize an in-memory Btrfs instance from the `mixed-4k.img` fixture.
fn setup_in_memory_fixture() -> (Btrfs<MemoryDevice>, FreeSpaceMap, u64, u8) {
    let mem_dev = MemoryDevice::from_fixture("btrfs/mixed-4k.img");
    let fs = Btrfs::mount(mem_dev).expect("mount mixed-4k fixture into memory");
    let fs_tree = fs.fs_tree();
    let extent_tree = ExtentTree::read(&fs).expect("read extent tree");
    let allocator = FreeSpaceMap::from_extent_tree_and_chunk_map(&extent_tree, fs.chunk_map())
        .expect("build allocator");

    (fs, allocator, fs_tree.bytenr, fs_tree.level)
}


#[test]
fn test_tree_validator_negative_controls() {
    let (fs, _, root_bytenr, root_level) = setup_in_memory_fixture();

    // 1. Wrong level expectation must fail
    let wrong_level = root_level + 5;
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = TreeValidator::validate(&fs, root_bytenr, Some(wrong_level));
    }));
    assert!(res.is_err(), "TreeValidator must fail on wrong root level");

    // 2. Corrupted leaf item order: mutate a node in memory so key[0] > key[1]
    let mem_dev = fs.device().clone();
    let root_node = fs.read_node(root_bytenr).expect("read root");
    let leaf_bytenr = if root_node.is_leaf() {
        root_bytenr
    } else {
        root_node.key_ptr(0).expect("key ptr 0").blockptr
    };

    let leaf_node = fs.read_node(leaf_bytenr).expect("read leaf");
    let mut leaf = Leaf::from_node(&leaf_node, fs.superblock().csum_type).expect("leaf from node");
    if leaf.items.len() >= 2 {
        // Swap first two items to invert sort order!
        leaf.items.swap(0, 1);
        let emitted = leaf.emit(fs.superblock().node_size).expect("emit corrupted leaf");
        mem_dev.write_at(leaf_bytenr, &emitted).expect("write corrupted leaf");

        let remounted = Btrfs::mount(mem_dev).expect("remount corrupted mem dev");
        let catch_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = TreeValidator::validate(&remounted, root_bytenr, Some(root_level));
        }));
        assert!(catch_res.is_err(), "TreeValidator must catch Invariant I-1 inversion!");
    }
}

#[test]
fn test_validate_all_on_all_fixtures() {
    let fixtures = [
        "btrfs/plain.img",
        "btrfs/compress.img",
        "btrfs/mixed-4k.img",
        "btrfs/nonmixed-4k.img",
        "btrfs/subvol.img",
        "btrfs/subvol-aged.img",
        "btrfs/subvol-shared.img",
        "btrfs/sha256-4k.img",
    ];

    for fixture in fixtures {
        let mem_dev = MemoryDevice::from_fixture(fixture);
        let fs = Btrfs::mount(mem_dev).unwrap_or_else(|e| panic!("failed to mount {fixture}: {e}"));

        let report = TreeValidator::validate_all(&fs)
            .unwrap_or_else(|e| panic!("validate_all failed on {fixture}: {e}"));

        println!(
            "{fixture}: total_nodes={}, total_items={}, fs_items={}, root_items={}, extent_items={}, dev_items={}, csum_items={:?}, subvols={}",
            report.total_nodes(),
            report.total_items(),
            report.fs_tree.total_items,
            report.root_tree.total_items,
            report.extent_tree.total_items,
            report.dev_tree.total_items,
            report.csum_tree.as_ref().map(|c| c.total_items),
            report.subvolume_trees.len(),
        );

        assert!(
            report.fs_tree.total_items > 0,
            "{fixture}: fs_tree has 0 items"
        );
        assert!(
            report.root_tree.total_items > 0,
            "{fixture}: root_tree has 0 items"
        );
        assert!(
            report.extent_tree.total_items > 0,
            "{fixture}: extent_tree has 0 items"
        );
        assert!(
            report.dev_tree.total_items > 0,
            "{fixture}: dev_tree has 0 items"
        );
        assert!(
            report.csum_tree.is_some(),
            "{fixture}: csum_tree missing"
        );
        assert!(
            report.csum_tree.as_ref().unwrap().total_nodes > 0,
            "{fixture}: csum_tree has 0 nodes"
        );

        if fixture.starts_with("btrfs/subvol") {
            assert!(
                !report.subvolume_trees.is_empty(),
                "{fixture}: must have subvolume trees validated"
            );
            for (id, sub_rep) in &report.subvolume_trees {
                assert!(
                    sub_rep.total_nodes > 0,
                    "{fixture}: subvolume {id} has 0 nodes"
                );
                assert!(
                    sub_rep.total_items > 0,
                    "{fixture}: subvolume {id} has 0 items"
                );
            }
        }
    }
}

#[test]
fn test_tree_validator_root_single_child_allowed_and_counted() {
    let (fs, mut allocator, root_bytenr, _) = setup_in_memory_fixture();
    let mem_dev = fs.device().clone();
    let node_size = fs.superblock().node_size;
    let csum_type = fs.superblock().csum_type;
    let owner = fs.fs_tree().objectid;
    let generation = fs.fs_tree().generation + 1;

    // Find a valid leaf bytenr and its minimum key
    let root_node = fs.read_node(root_bytenr).expect("read root");
    let (leaf_bytenr, leaf_min_key, leaf_gen) = if root_node.is_leaf() {
        (root_bytenr, root_node.key(0).expect("key 0"), root_node.generation)
    } else {
        let ptr = root_node.key_ptr(0).expect("key ptr 0");
        let child = fs.read_node(ptr.blockptr).expect("read child");
        (ptr.blockptr, child.key(0).expect("child key 0"), child.generation)
    };

    // Allocate an address for a synthetic interior root node with exactly 1 child
    let interior_bytenr = allocator
        .allocate_metadata_for_owner(node_size, owner)
        .expect("allocate metadata");

    let mut interior = InteriorNode::new(
        interior_bytenr,
        generation,
        owner,
        1, // level 1
        root_node.metadata_uuid(),
        csum_type,
    )
    .expect("create interior node");

    interior.entries.push(InteriorEntry {
        key: leaf_min_key,
        blockptr: leaf_bytenr,
        generation: leaf_gen,
    });

    let emitted = interior.emit(node_size).expect("emit interior node");
    mem_dev.write_at(interior_bytenr, &emitted).expect("write interior node");

    let remounted = Btrfs::mount(mem_dev).expect("remount btrfs");

    // A root with one child is legal btrfs (the kernel leaves it too, and
    // `btrfs check` accepts it): it must validate and be counted.
    let report = TreeValidator::validate(&remounted, interior_bytenr, Some(1))
        .expect("an interior root with one child is legal");
    assert_eq!(report.tree_height, 1);
    assert_eq!(report.total_single_child_interior_nodes, 1, "the root itself is counted");
}

#[test]
fn test_tree_validator_non_root_single_child_allowed_and_counted() {
    let (fs, mut allocator, root_bytenr, _) = setup_in_memory_fixture();
    let mem_dev = fs.device().clone();
    let node_size = fs.superblock().node_size;
    let csum_type = fs.superblock().csum_type;
    let owner = fs.fs_tree().objectid;
    let generation = fs.fs_tree().generation + 1;
    let root_node = fs.read_node(root_bytenr).expect("read root");
    let metadata_uuid = root_node.metadata_uuid();

    // 1. Create Leaf 0 (keys 1000, 1001)
    let leaf0_bytenr = allocator
        .allocate_metadata_for_owner(node_size, owner)
        .expect("alloc leaf0");
    let mut leaf0 = Leaf::new(leaf0_bytenr, generation, owner, metadata_uuid, csum_type);
    leaf0.items.push(LeafItem {
        key: Key::new(1000, 1, 0),
        data: vec![1; 64],
    });
    leaf0.items.push(LeafItem {
        key: Key::new(1000, 1, 1),
        data: vec![2; 64],
    });
    mem_dev.write_at(leaf0_bytenr, &leaf0.emit(node_size).unwrap()).unwrap();

    // 2. Create Leaf 1 (keys 2000, 2001)
    let leaf1_bytenr = allocator
        .allocate_metadata_for_owner(node_size, owner)
        .expect("alloc leaf1");
    let mut leaf1 = Leaf::new(leaf1_bytenr, generation, owner, metadata_uuid, csum_type);
    leaf1.items.push(LeafItem {
        key: Key::new(2000, 1, 0),
        data: vec![3; 64],
    });
    leaf1.items.push(LeafItem {
        key: Key::new(2000, 1, 1),
        data: vec![4; 64],
    });
    mem_dev.write_at(leaf1_bytenr, &leaf1.emit(node_size).unwrap()).unwrap();

    // 3. Create Leaf 2 (keys 3000, 3001)
    let leaf2_bytenr = allocator
        .allocate_metadata_for_owner(node_size, owner)
        .expect("alloc leaf2");
    let mut leaf2 = Leaf::new(leaf2_bytenr, generation, owner, metadata_uuid, csum_type);
    leaf2.items.push(LeafItem {
        key: Key::new(3000, 1, 0),
        data: vec![5; 64],
    });
    leaf2.items.push(LeafItem {
        key: Key::new(3000, 1, 1),
        data: vec![6; 64],
    });
    mem_dev.write_at(leaf2_bytenr, &leaf2.emit(node_size).unwrap()).unwrap();

    // 4. Create Interior 1 (level 1) with EXACTLY ONE child: Leaf 0 (sparsity / underfull non-root!)
    let int1_bytenr = allocator
        .allocate_metadata_for_owner(node_size, owner)
        .expect("alloc int1");
    let mut int1 = InteriorNode::new(int1_bytenr, generation, owner, 1, metadata_uuid, csum_type).unwrap();
    int1.entries.push(InteriorEntry {
        key: Key::new(1000, 1, 0),
        blockptr: leaf0_bytenr,
        generation,
    });
    mem_dev.write_at(int1_bytenr, &int1.emit(node_size).unwrap()).unwrap();

    // 5. Create Interior 2 (level 1) with TWO children: Leaf 1 and Leaf 2
    let int2_bytenr = allocator
        .allocate_metadata_for_owner(node_size, owner)
        .expect("alloc int2");
    let mut int2 = InteriorNode::new(int2_bytenr, generation, owner, 1, metadata_uuid, csum_type).unwrap();
    int2.entries.push(InteriorEntry {
        key: Key::new(2000, 1, 0),
        blockptr: leaf1_bytenr,
        generation,
    });
    int2.entries.push(InteriorEntry {
        key: Key::new(3000, 1, 0),
        blockptr: leaf2_bytenr,
        generation,
    });
    mem_dev.write_at(int2_bytenr, &int2.emit(node_size).unwrap()).unwrap();

    // 6. Create Root Interior Node (level 2) with TWO children: Interior 1 and Interior 2
    let root2_bytenr = allocator
        .allocate_metadata_for_owner(node_size, owner)
        .expect("alloc root2");
    let mut root2 = InteriorNode::new(root2_bytenr, generation, owner, 2, metadata_uuid, csum_type).unwrap();
    root2.entries.push(InteriorEntry {
        key: Key::new(1000, 1, 0),
        blockptr: int1_bytenr,
        generation,
    });
    root2.entries.push(InteriorEntry {
        key: Key::new(2000, 1, 0),
        blockptr: int2_bytenr,
        generation,
    });
    mem_dev.write_at(root2_bytenr, &root2.emit(node_size).unwrap()).unwrap();

    let remounted = Btrfs::mount(mem_dev).expect("remount btrfs");

    // Validating root2 (level = 2, with a single-child non-root interior child)
    // MUST pass cleanly! It must NOT reject or mutate the single-child interior node.
    let report = TreeValidator::validate(&remounted, root2_bytenr, Some(2))
        .expect("TreeValidator must allow non-root single-child interior nodes");

    assert_eq!(report.tree_height, 2);
    assert_eq!(report.total_interior_nodes, 3); // root2 + int1 + int2
    assert_eq!(report.total_leaves, 3);         // leaf0 + leaf1 + leaf2
    assert_eq!(report.total_nodes, 6);
    assert_eq!(report.total_items, 6);
    assert_eq!(
        report.total_single_child_interior_nodes, 1,
        "Single-child non-root interior node must be accurately reported in metric"
    );
}

#[test]
fn test_tree_validator_search_walk_parity_negative_control() {
    let (fs, _, root_bytenr, _) = setup_in_memory_fixture();

    // Missing key from walk fails parity assert
    let missing_key = Key::new(999999, 255, 999999);
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = TreeValidator::verify_search_walk_parity(&fs, root_bytenr, &[missing_key]);
    }));
    assert!(
        res.is_err(),
        "verify_search_walk_parity must fail when a key discovered during walk cannot be found via Cursor::search"
    );
    let err = res.expect_err("must fail");
    let msg = if let Some(s) = err.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else {
        String::new()
    };
    assert!(
        msg.contains("Search-vs-walk parity violation"),
        "Expected parity violation panic message, got: {msg}"
    );
}

#[test]
fn test_tree_validator_search_walk_parity_parent_separator_corruption() {
    let (fs, mut allocator, mut root_bytenr, mut root_level) = setup_in_memory_fixture();
    let mem_dev = fs.device().clone();
    let owner = fs.fs_tree().objectid;
    let generation = fs.fs_tree().generation + 1;
    let mut pending = HashMap::new();

    // Insert 60 items with 100-byte payloads to force leaf split and root height increment to level 1
    let num_items = 60usize;
    let keys: Vec<Key> = (0..num_items)
        .map(|i| Key::new(5000 + i as u64, 1, 0))
        .collect();

    for key in &keys {
        let payload = vec![0xEEu8; 100];
        let res = cow_tree_insert(
            &fs,
            &pending,
            root_bytenr,
            root_level,
            owner,
            *key,
            payload,
            generation,
            &mut allocator,
        )
        .expect("insert item");
        root_bytenr = res.new_root_bytenr;
        root_level = res.new_root_level;
        for (b, block) in res.emitted_blocks {
            pending.insert(b, block);
        }
    }

    assert!(root_level >= 1, "tree height must reach at least 1");

    // Commit pending blocks to memory device
    for (&bytenr, block) in &pending {
        mem_dev.write_at(bytenr, block).expect("write block");
    }

    let fs_clean = Btrfs::mount(mem_dev.clone()).expect("mount clean fs");
    // Verify baseline clean tree passes both TreeValidator::validate and verify_search_walk_parity
    let rep = TreeValidator::validate(&fs_clean, root_bytenr, Some(root_level))
        .expect("baseline validation must pass");
    assert!(rep.total_items >= num_items, "expected at least {num_items} items, got {}", rep.total_items);
    TreeValidator::verify_search_walk_parity(&fs_clean, root_bytenr, &keys)
        .expect("baseline parity must pass");

    // Read the root interior node
    let root_node = fs_clean.read_node(root_bytenr).expect("read root node");
    assert!(!root_node.is_leaf(), "root must be an interior node");
    assert!(root_node.nr_items >= 2, "root interior must have at least 2 child entries");

    // Corrupt the separator key of entry 1 (set to a key higher than any in child 1)
    let mut interior = InteriorNode::from_node(&root_node, fs_clean.superblock().csum_type)
        .expect("parse interior");
    // Set separator key artificially high: Key(99999, 255, 99999)
    interior.entries[1].key = Key::new(99999, 255, 99999);
    let corrupted_bytes = interior
        .emit(fs_clean.superblock().node_size)
        .expect("emit corrupted interior node");
    mem_dev
        .write_at(root_bytenr, &corrupted_bytes)
        .expect("write corrupted interior root");

    let fs_corrupted = Btrfs::mount(mem_dev.clone()).expect("mount corrupted fs");

    // Call verify_search_walk_parity directly (bypassing walk_node which would trip I-2 first)
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = TreeValidator::verify_search_walk_parity(&fs_corrupted, root_bytenr, &keys);
    }));

    assert!(
        res.is_err(),
        "verify_search_walk_parity must fail when an interior parent separator key is corrupted"
    );
    let err = res.expect_err("must fail");
    let msg = if let Some(s) = err.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = err.downcast_ref::<String>() {
        s.clone()
    } else {
        String::new()
    };
    assert!(
        msg.contains("Search-vs-walk parity violation"),
        "Expected parity violation panic message, got: {msg}"
    );
    println!("[TEST] Parent separator corruption caught as expected by parity assertion:\n  {msg}");
}

#[test]
fn test_tree_validator_validate_all_catches_extent_and_csum_corruption() {
    // 1. Corrupt EXTENT_TREE
    {
        let mem_dev = MemoryDevice::from_fixture("btrfs/mixed-4k.img");
        let fs = Btrfs::mount(mem_dev.clone()).expect("mount mixed-4k");
        let extent_root = fs.tree_root(EXTENT_TREE_OBJECTID).expect("extent tree root");

        let node = fs.read_node(extent_root.bytenr).expect("read extent root node");
        let mut leaf = Leaf::from_node(&node, fs.superblock().csum_type).expect("leaf from node");
        assert!(leaf.items.len() >= 2, "extent tree must have at least 2 items");
        leaf.items.swap(0, 1); // Invert sort order in extent tree leaf
        let emitted = leaf.emit(fs.superblock().node_size).expect("emit leaf");
        mem_dev.write_at(extent_root.bytenr, &emitted).expect("write corrupted extent leaf");

        let remounted = Btrfs::mount(mem_dev).expect("remount");
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = TreeValidator::validate_all(&remounted);
        }));
        assert!(
            res.is_err(),
            "validate_all must catch structural corruption in EXTENT_TREE"
        );
    }

    // 2. Corrupt CSUM_TREE
    {
        let mem_dev = MemoryDevice::from_fixture("btrfs/mixed-4k.img");
        let fs = Btrfs::mount(mem_dev.clone()).expect("mount mixed-4k");
        let csum_root = fs.tree_root(CSUM_TREE_OBJECTID).expect("csum tree root");

        let node = fs.read_node(csum_root.bytenr).expect("read csum root node");
        let mut leaf = Leaf::from_node(&node, fs.superblock().csum_type).expect("leaf from node");
        let min_key = leaf.items[0].key;
        leaf.items.push(LeafItem {
            key: Key::new(min_key.objectid.saturating_sub(1), min_key.item_type, 0),
            data: vec![0; 4],
        });
        let emitted = leaf.emit(fs.superblock().node_size).expect("emit leaf");
        mem_dev.write_at(csum_root.bytenr, &emitted).expect("write corrupted csum leaf");

        let remounted = Btrfs::mount(mem_dev).expect("remount");
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = TreeValidator::validate_all(&remounted);
        }));
        assert!(
            res.is_err(),
            "validate_all must catch structural corruption in CSUM_TREE"
        );
    }

    // 3. Corrupt Subvolume Tree (subvol 257 in subvol.img)
    {
        let mem_dev = MemoryDevice::from_fixture("btrfs/subvol.img");
        let fs = Btrfs::mount(mem_dev.clone()).expect("mount subvol");
        let subvol_root = fs.tree_root(257).expect("subvol tree root 257");

        let node = fs.read_node(subvol_root.bytenr).expect("read subvol root node");
        let mut leaf = Leaf::from_node(&node, fs.superblock().csum_type).expect("leaf from node");
        assert!(leaf.items.len() >= 2, "subvolume tree must have at least 2 items");
        leaf.items.swap(0, 1); // Invert sort order in subvolume tree leaf
        let emitted = leaf.emit(fs.superblock().node_size).expect("emit leaf");
        let stripes = fs.chunk_map().map_all_stripes(subvol_root.bytenr).expect("map all stripes");
        for phys in stripes {
            mem_dev.write_at(phys, &emitted).expect("write corrupted subvol leaf");
        }

        let remounted = Btrfs::mount(mem_dev).expect("remount");
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            TreeValidator::validate_all(&remounted).expect("validate_all must fail on corrupted subvolume");
        }));
        assert!(
            res.is_err(),
            "validate_all must catch structural corruption in subvolume tree"
        );
    }
}

