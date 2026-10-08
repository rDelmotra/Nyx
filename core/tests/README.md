# Integration Test Suite (`core/tests/`)

This directory contains the integration test suite for `luks_core`, organized into domain-specific subdirectories.

All integration tests are registered in [`core/Cargo.toml`](../Cargo.toml) with explicit target names, ensuring that running tests by name (e.g. `cargo test -p luks_core --test <name>`) works seamlessly without needing directory path arguments.

---

## Directory Organization

| Directory | Count | Domain Description | Feature Gate |
|---|---|---|---|
| [`crypto_luks/`](crypto_luks/) | 7 | Cryptographic backends, Argon2id/PBKDF2, AES, secret zeroization, LUKS2 headers, keyslot unlocking | Default / `dangerous-write-support` |
| [`device_scsi/`](device_scsi/) | 6 | USB Mass Storage Bulk-Only Transport (BOT), SCSI command emulation (INQUIRY, READ/WRITE), mock drive, write cost metrics | Default / `dangerous-write-support` |
| [`ext4/`](ext4/) | 11 | ext4 inode allocation/deallocation, directory indexing (htree), checksumming, block allocators, streaming | Default / `dangerous-write-support` |
| [`btrfs_tree/`](btrfs_tree/) | 6 | B-tree regressions (root split, empty leaf, CoW key fixup, node round-trip) and self-tests for the shared tree and accounting checkers | `dangerous-write-support` |
| [`txn_batch/`](txn_batch/) | 10 | Atomic commit pipeline, reservation stages (B2, C, FG), transaction batching, crash safety, fail-closed aborts | `dangerous-write-support` |
| [`alloc_chunk/`](alloc_chunk/) | 8 | Btrfs chunk allocation, free space search, block group management, system chunk valves, extent trees | Default / `dangerous-write-support` |
| [`btrfs_ops/`](btrfs_ops/) | 16 | High-level file and directory CRUD (read, create, write, delete, mkdir, rename, finish csum, timestamp, gates, subvolume prep & write, byte-identical determinism) | Default / `dangerous-write-support` |
| [`integration/`](integration/) | 6 | Full-stack scenarios, statfs capacity, tree import replays, forensic logs, and oracle ledgers | Default / `dangerous-write-support` |
| [`common/`](common/) | - | Shared test harnesses: `accounting.rs`, `btree_validator.rs`, `csum_pack.rs`, `fail_after.rs`, `mem_device.rs`, `oracle.rs`, `scratch.rs`, `temp_guard.rs` | Shared helper modules |

---

## How to Run Tests

### Running Individual Tests by Name
Because test names are preserved in `core/Cargo.toml`, you do not need to know the subfolder path to run any test:
```bash
# Read-only tests (default build):
cargo test -p luks_core --test aes_backend
cargo test -p luks_core --test luks2_header

# Write-enabled tests:
cargo test -p luks_core --features dangerous-write-support --test btrfs_tree_validator
cargo test -p luks_core --features dangerous-write-support --test btrfs_conformance
```

### Running Entire Test Suites
```bash
# Run all read-only tests across workspace:
cargo test --workspace

# Run all core tests including write paths:
cargo test -p luks_core --features dangerous-write-support

# Run every harness tier:
bash tools/run-harness.sh --all
```

---

## Authoring New Tests

When adding a new integration test:
1. Place the `.rs` file in the appropriate domain subdirectory (`core/tests/<category>/<name>.rs`).
2. If it requires shared utilities from `common/`, import via:
   ```rust
   #[path = "../common/mod.rs"]
   mod common;
   ```
3. Register the test in [`core/Cargo.toml`](../Cargo.toml):
   ```toml
   [[test]]
   name = "<name>"
   path = "tests/<category>/<name>.rs"
   required-features = ["dangerous-write-support"]  # omit if read-only
   ```
