#!/bin/bash
# Build a dual-partition LUKS2 image (Partition 1: ext4, Partition 2: Btrfs)
# and optionally flash it directly to a physical USB drive.
#
#   tools/make-dual-stick-image.sh [options] <output.img | /dev/rdiskN>
#
# Options:
#   --ext4-size <size>    Size of the ext4 partition (default: 4G)
#   --btrfs-size <size>   Size of the Btrfs partition (default: 4G)
#   --password <pass>     Passphrase for both LUKS containers (default: test)
#   --password-ext4 <p>   Separate passphrase for ext4 partition
#   --password-btrfs <p>  Separate passphrase for Btrfs partition
#   --phone-kdf           Use phone-optimized Argon2id (256 MiB, 1 thread, ~1s unlock) [default]
#   --desktop-kdf         Use standard heavy Argon2id (1 GiB, 4 threads, ~5s unlock)
#   --subvol              Create test Btrfs subvolumes (/home, /var)
#   --flash               Allow writing directly to a /dev/rdiskN block device
#
# Both filesystems are built by real `sgdisk`, real `cryptsetup`, real `mkfs.ext4`,
# and real `mkfs.btrfs` inside the colima VM.
set -euo pipefail

EXT4_SIZE="4G"
BTRFS_SIZE="4G"
PASSWORD="test"
PASSWORD_EXT4=""
PASSWORD_BTRFS=""
PHONE_KDF=1
WITH_SUBVOL=0
ALLOW_FLASH=0
TARGET=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --ext4-size)
            EXT4_SIZE="$2"
            shift 2
            ;;
        --btrfs-size)
            BTRFS_SIZE="$2"
            shift 2
            ;;
        --password)
            PASSWORD="$2"
            shift 2
            ;;
        --password-ext4)
            PASSWORD_EXT4="$2"
            shift 2
            ;;
        --password-btrfs)
            PASSWORD_BTRFS="$2"
            shift 2
            ;;
        --phone-kdf)
            PHONE_KDF=1
            shift
            ;;
        --desktop-kdf)
            PHONE_KDF=0
            shift
            ;;
        --subvol)
            WITH_SUBVOL=1
            shift
            ;;
        --flash)
            ALLOW_FLASH=1
            shift
            ;;
        -h|--help)
            echo "usage: $0 [options] <output.img | /dev/rdiskN>"
            exit 0
            ;;
        *)
            TARGET="$1"
            shift
            ;;
    esac
done

if [ -z "$TARGET" ]; then
    echo "error: no target specified" >&2
    echo "usage: $0 [options] <output.img | /dev/rdiskN>" >&2
    exit 2
fi

PASSWORD_EXT4="${PASSWORD_EXT4:-$PASSWORD}"
PASSWORD_BTRFS="${PASSWORD_BTRFS:-$PASSWORD}"

if [ "$PHONE_KDF" = "1" ]; then
    PBKDF_MEM=262144
    PBKDF_PARALLEL=1
    PBKDF_ITERS=4
else
    PBKDF_MEM=1048576
    PBKDF_PARALLEL=4
    PBKDF_ITERS=4
fi

command -v colima >/dev/null || { echo "colima not installed" >&2; exit 2; }
colima status >/dev/null 2>&1 || { echo "colima is not running — run 'colima start'" >&2; exit 2; }

# Safety check for raw disk writing
IS_BLOCK_DEV=0
if [[ "$TARGET" =~ ^/dev/(r)?disk[0-9]+$ ]]; then
    IS_BLOCK_DEV=1
    if [ "$ALLOW_FLASH" != "1" ]; then
        echo "error: $TARGET is a physical drive node. Pass --flash to confirm writing." >&2
        exit 2
    fi
    # Safety: Refuse to flash internal drive disk0
    if [[ "$TARGET" =~ disk0 ]]; then
        echo "REFUSAL: Target is internal system drive disk0! Aborting for safety." >&2
        exit 1
    fi
fi

NAME="mkdual-$$"
REMOTE="/tmp/$NAME.img"
REMOTE_SH="/tmp/$NAME.sh"
PW_EXT4_REMOTE="/tmp/$NAME-ext4.pw"
PW_BTRFS_REMOTE="/tmp/$NAME-btrfs.pw"
LOCAL_SH="$(mktemp)"
trap 'rm -f "$LOCAL_SH"' EXIT

cat > "$LOCAL_SH" <<'REMOTE_SCRIPT'
set -euo pipefail
IMG="$1"
EXT4_SIZE="$2"
BTRFS_SIZE="$3"
NAME="$4"
PBKDF_MEM="$5"
PBKDF_PARALLEL="$6"
PBKDF_ITERS="$7"
PW_EXT4="$8"
PW_BTRFS="$9"
WITH_SUBVOL="${10:-0}"

MAPPER_EXT4="/dev/mapper/${NAME}-ext4"
MAPPER_BTRFS="/dev/mapper/${NAME}-btrfs"
MNT="/tmp/mnt-$NAME"
LOOP=""

cleanup() {
    umount "$MNT" 2>/dev/null || true
    cryptsetup close "${NAME}-ext4" 2>/dev/null || true
    cryptsetup close "${NAME}-btrfs" 2>/dev/null || true
    [ -n "$LOOP" ] && losetup -d "$LOOP" 2>/dev/null || true
    rm -rf "$MNT" "$PW_EXT4" "$PW_BTRFS"
}
trap cleanup EXIT

# Parse size strings (e.g. 4G -> bytes)
to_bytes() {
    local s="$1"
    local num="${s%[GgMmKk]*}"
    local unit="${s#$num}"
    case "$unit" in
        G|g) echo $((num * 1024 * 1024 * 1024)) ;;
        M|m) echo $((num * 1024 * 1024)) ;;
        K|k) echo $((num * 1024)) ;;
        *) echo "$num" ;;
    esac
}

EXT4_BYTES=$(to_bytes "$EXT4_SIZE")
BTRFS_BYTES=$(to_bytes "$BTRFS_SIZE")
# Add 32 MiB overhead for GPT headers, partition alignments, and secondary GPT
TOTAL_BYTES=$((EXT4_BYTES + BTRFS_BYTES + 32 * 1024 * 1024))

echo "Creating image sparse buffer (${TOTAL_BYTES} bytes)..."
rm -f "$IMG"
truncate -s "$TOTAL_BYTES" "$IMG"

echo "Partitioning GPT with sgdisk (p1: $EXT4_SIZE, p2: $BTRFS_SIZE)..."
sgdisk -o "$IMG" >/dev/null
sgdisk --new=1:1MiB:+${EXT4_SIZE} --typecode=1:8300 --change-name=1:luks-ext4 "$IMG" >/dev/null
sgdisk --new=2:0:+${BTRFS_SIZE}   --typecode=2:8300 --change-name=2:luks-btrfs "$IMG" >/dev/null

wait_for_partitions() {
    local loop="$1" i p
    udevadm settle --timeout=5 >/dev/null 2>&1 || true
    for i in $(seq 1 50); do
        for p in "${loop}"p*; do
            [ -b "$p" ] && return 0
        done
        sleep 0.1
    done
    return 1
}

LOOP="$(losetup --find --show --partscan "$IMG")"
wait_for_partitions "$LOOP" || {
    echo "partition nodes never appeared for $LOOP" >&2
    exit 1
}

PART1="${LOOP}p1"
PART2="${LOOP}p2"
[ -b "$PART1" ] || { echo "missing $PART1" >&2; exit 1; }
[ -b "$PART2" ] || { echo "missing $PART2" >&2; exit 1; }

echo "Formatting Partition 1 (ext4 LUKS2)..."
cryptsetup luksFormat \
    --type luks2 \
    --cipher aes-xts-plain64 \
    --key-size 512 \
    --pbkdf argon2id \
    --pbkdf-memory "$PBKDF_MEM" \
    --pbkdf-parallel "$PBKDF_PARALLEL" \
    --pbkdf-force-iterations "$PBKDF_ITERS" \
    --batch-mode \
    --key-file="$PW_EXT4" \
    "$PART1"

cryptsetup open --key-file="$PW_EXT4" "$PART1" "${NAME}-ext4"
mkfs.ext4 -q -L "luks-ext4" "$MAPPER_EXT4"

mkdir -p "$MNT"
mount "$MAPPER_EXT4" "$MNT"
echo "hello from LUKS ext4 partition" > "$MNT/welcome.txt"
mkdir -p "$MNT/docs"
echo "# LUKS ext4 Partition" > "$MNT/docs/README.md"
sync
umount "$MNT"
cryptsetup close "${NAME}-ext4"
echo "  -> Partition 1 (ext4) ready."

echo "Formatting Partition 2 (Btrfs LUKS2)..."
cryptsetup luksFormat \
    --type luks2 \
    --cipher aes-xts-plain64 \
    --key-size 512 \
    --pbkdf argon2id \
    --pbkdf-memory "$PBKDF_MEM" \
    --pbkdf-parallel "$PBKDF_PARALLEL" \
    --pbkdf-force-iterations "$PBKDF_ITERS" \
    --batch-mode \
    --key-file="$PW_BTRFS" \
    "$PART2"

cryptsetup open --key-file="$PW_BTRFS" "$PART2" "${NAME}-btrfs"
mkfs.btrfs -q -L "luks-btrfs" "$MAPPER_BTRFS"

mount "$MAPPER_BTRFS" "$MNT"
echo "hello from LUKS Btrfs partition" > "$MNT/welcome.txt"
mkdir -p "$MNT/docs"
echo "# LUKS Btrfs Partition" > "$MNT/docs/README.md"

if [ "$WITH_SUBVOL" = "1" ]; then
    echo "  Creating Btrfs subvolumes (@home, @var)..."
    btrfs subvolume create "$MNT/@home"
    echo "User subvolume" > "$MNT/@home/info.txt"
    btrfs subvolume create "$MNT/@var"
    echo "Var subvolume" > "$MNT/@var/info.txt"
fi

sync
umount "$MNT"
cryptsetup close "${NAME}-btrfs"
echo "  -> Partition 2 (Btrfs) ready."

losetup -d "$LOOP"
LOOP=""
echo "Both partitions formatted and initialized successfully."
REMOTE_SCRIPT

printf '%s' "$PASSWORD_EXT4" | colima ssh -- tee "$PW_EXT4_REMOTE" > /dev/null
printf '%s' "$PASSWORD_BTRFS" | colima ssh -- tee "$PW_BTRFS_REMOTE" > /dev/null
colima ssh -- tee "$REMOTE_SH" < "$LOCAL_SH" > /dev/null

echo "Executing generator in colima VM..."
colima ssh -- sudo bash "$REMOTE_SH" "$REMOTE" "$EXT4_SIZE" "$BTRFS_SIZE" "$NAME" "$PBKDF_MEM" "$PBKDF_PARALLEL" "$PBKDF_ITERS" "$PW_EXT4_REMOTE" "$PW_BTRFS_REMOTE" "$WITH_SUBVOL"

if [ "$IS_BLOCK_DEV" = "1" ]; then
    # Raw disk writing on macOS
    DISK_ID="${TARGET#/dev/}"
    DISK_ID="${DISK_ID#r}"
    echo "Unmounting $TARGET on host..."
    diskutil unmountDisk "/dev/$DISK_ID" || true
    echo "Flashing image to $TARGET via sudo dd..."
    colima ssh -- sudo cat "$REMOTE" | sudo dd of="$TARGET" bs=1m status=progress
    sync
    diskutil unmountDisk "/dev/$DISK_ID" || true
    echo "SUCCESS: $TARGET has been flashed with dual LUKS2 partitions!"
else
    echo "Transferring generated image to $TARGET..."
    colima ssh -- sudo cat "$REMOTE" > "$TARGET"
    echo "SUCCESS: Wrote dual-partition image to $TARGET ($(du -h "$TARGET" | cut -f1))"
fi

colima ssh -- sudo rm -f "$REMOTE" "$REMOTE_SH" "$PW_EXT4_REMOTE" "$PW_BTRFS_REMOTE"
