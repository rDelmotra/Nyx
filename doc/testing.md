# Testing & Verification

Correctness is validated against official Linux kernel implementations in automated test suites:

### 0. Generate Local Test Fixtures
Only lightweight headers and trace fixtures are stored in the repository. Full disk and filesystem images are generated locally:
```bash
# See detailed per-platform commands in:
cat tools/README-fixtures.md
```

### 1. Workspace Unit & Integration Tests
```bash
cargo test --workspace
cargo test --workspace --features luks_core/dangerous-write-support,luks_jni/dangerous-write-support
```
`core/tests/` is organized into domain subdirectories (`btrfs_ops/`, `btrfs_tree/`,
`txn_batch/`, etc.) — see [`core/tests/README.md`](../core/tests/README.md) for the
layout. Test names are preserved as explicit
`[[test]]` targets in `core/Cargo.toml`, so `cargo test --test <name>` still works
without knowing which subdirectory a test lives in.

### B-tree state suite
```bash
cargo test -p luks_core --features dangerous-write-support --release --test btrfs_states
```
Drives the real write API through the tree shapes a drive goes through: a
folder growing to a 3-level tree and emptied again, a fragmented free-space
tree, big files filling the checksum tree, a batch writing into two folders,
and a power cut at every write of a split or delete commit. After every phase
the image is graded by `TreeValidator`, `AccountingOracle` and
`btrfs check --readonly` (local btrfs-progs, else colima). Needs
`fixtures/btrfs/states-4k.img` (`tools/gen-btrfs-fixtures.sh`, mkfs only).

### Subvolume safety suite (Fedora-shaped disk)
```bash
cargo test -p luks_core --features dangerous-write-support --release \
    --test btrfs_subvol_states --test btrfs_subvol_power_cut
```
Same shapes as the state suite, run on `fedora-4k.img` (subvolumes `root`
and `home`, zstd) with every write inside `/home/user`: H1 home tree to
3 levels and back, H2 free-space tree, H3 big files, H4 two-folder batch and
a refused batch spanning `root` and `home`, H5 power cuts during splits,
deletes and a big-file write. After every step and every cut the `root`
subvolume and the top-level tree must be byte-identical (`common::states::
Untouched`), the original home files must read back exactly, and the three
checkers must pass. `NYX_BTRFS` points the outside check at another
btrfs-progs. The fixture needs mkfs with `--subvol` (btrfs-progs >= 6.11):
`MKFS_BTRFS=/path/to/mkfs.btrfs tools/gen-btrfs-fixtures.sh <out> fedora-4k`.

`btrfs_stale_block_gate` and `btrfs_owner_gate` cover the two read-side
guards every write relies on: a child whose generation differs from its
parent pointer (a lost write on the device) and, on the write path, a node
owned by another tree.

### 2. Autonomous Test Harness (recommended before a PR)
```bash
# Every tier: fast/integration/oracle/strict/android
tools/run-harness.sh --all

# Quick local iteration: fast unit tests only
tools/run-harness.sh
```
Executes B-tree validation, the in-process accounting oracle, and Linux kernel
oracle verification.
Run `tools/run-harness.sh` with no arguments to view all tier flags.

### 3. Linux Kernel Oracle Graded Suite (granular alternative to `--oracle`)
```bash
tools/test-graded.sh --workspace --features luks_core/dangerous-write-support,luks_jni/dangerous-write-support
```

### 4. Write-Path Safety Gate
Verifies that release builds contain zero write symbols or entry points:
```bash
# Standalone ELF symbol and entry-point check
bash tools/verify-no-write-code.sh
```

In Android builds, Gradle enforces this at build time via the `checkNoWriteCodeInRelease` task. If write-enabled native libraries (`libluks_jni.so`) are present when assembling a release APK, Gradle will fail closed. To deliberately produce a write-enabled release build (as in `v0.4.0`):
```bash
cd android && ./gradlew assembleRelease -PallowWriteInRelease=true
```

### 5. Android Unit Tests
```bash
cd android && ./gradlew testDebugUnitTest
```
Currently 240 unit tests passing, covering session lifecycle, device state, plain volume write consent, breadcrumbs, media block caching, biometric partition key derivation, and error handling.

---

## Diagnostic Logging

The engine includes a 256-slot non-allocating circular memory buffer that tracks USB transfer submissions, reaps, SCSI CDBs, sense codes, and filesystem events.

Extract the live forensic ring buffer via ADB:

```bash
# Trigger an immediate forensic dump to Android logcat
adb shell am broadcast -a dev.luksandroid.DUMP_FORENSIC

# Inspect the structured trace output
adb logcat -d -s LUKS_FORENSIC_DUMP:I
```

*The diagnostic trace logs timing, SCSI status codes, and recovery states while strictly omitting any user data or filenames.*
