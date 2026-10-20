#!/bin/bash
# Build an ext4 image the Linux kernel populated (not debugfs, not our writer).
#
#   tools/make-ext4-kernel-image.sh <shape> <out-image>
#
# mke2fs formats a sparse file inside the colima VM, the kernel mounts it over
# a loop device, and ordinary tools write to it: files of several sizes,
# hardlinks, fast and slow symlinks, a fifo and device nodes, shared xattr
# blocks, unwritten (fallocate) extents, a sparse file, a fragmented file deep
# enough for a depth-2 extent tree, an htree directory, and some deletions.
# The image is copied out and the VM copy removed. Used by the F8 health
# tests as the false-red corpus: e2fsck -fn must find these clean.
#
# Shapes: default no64 nores k1 nosparse noflex nocsum nojournal sparse2
#         ext3 ext3x bigalloc inline
set -euo pipefail

SHAPE="${1:?usage: make-ext4-kernel-image.sh <shape> <out-image>}"
OUT="${2:?usage: make-ext4-kernel-image.sh <shape> <out-image>}"

command -v colima >/dev/null || { echo "colima not installed" >&2; exit 2; }
colima status >/dev/null 2>&1 || { echo "colima is not running; 'colima start'" >&2; exit 2; }

NAME="kimg-$$"
REMOTE_SH="/tmp/$NAME.sh"
LOCAL_SH="$(mktemp)"
trap 'rm -f "$LOCAL_SH"; colima ssh -- sudo rm -f "/tmp/$NAME.img" "$REMOTE_SH" 2>/dev/null || true' EXIT

cat > "$LOCAL_SH" <<'REMOTE_SCRIPT'
set -euo pipefail
SHAPE="$1"; IMG="/tmp/$2.img"; MNT="/tmp/mnt-$2"
G="-b 4096 -g 2048"
BS=4096
FST=ext4
EXTENTS=1
case "$SHAPE" in
    default)    ARGS="$G -O orphan_file" ;;
    no64)       ARGS="$G -O ^64bit" ;;
    nores)      ARGS="$G -O ^resize_inode" ;;
    k1)         ARGS="-b 1024"; BS=1024 ;;
    nosparse)   ARGS="$G -O ^sparse_super,^resize_inode" ;;
    noflex)     ARGS="$G -O ^flex_bg" ;;
    nocsum)     ARGS="$G -O ^metadata_csum,uninit_bg" ;;
    nojournal)  ARGS="$G -O ^has_journal" ;;
    sparse2)    ARGS="$G -O sparse_super2" ;;
    ext3|ext3x) ARGS="-b 1024"; BS=1024; FST=ext3; EXTENTS=0 ;;
    bigalloc)   ARGS="-b 4096 -O bigalloc -C 16384" ;;
    inline)     ARGS="-b 4096 -O inline_data" ;;
    *) echo "unknown shape $SHAPE" >&2; exit 2 ;;
esac

rm -f "$IMG"; truncate -s 48M "$IMG"
# shellcheck disable=SC2086
mke2fs -q -t $FST $ARGS "$IMG"
mkdir -p "$MNT"
cleanup() { umount "$MNT" 2>/dev/null || true; rmdir "$MNT" 2>/dev/null || true; }
trap cleanup EXIT
mount -o loop "$IMG" "$MNT"
cd "$MNT"

mkdir -p dir/sub/deeper
echo hello > hello
head -c 3000000 /dev/urandom > big
for i in $(seq 1 40); do head -c $((i * 997)) /dev/urandom > "dir/f$i"; done
echo nested > dir/sub/deeper/leaf
ln hello hard1; ln hello dir/hard2
ln -s hello fast
ln -s "$(printf 'x%.0s' $(seq 1 120))" slow
mkfifo fifo; mknod chr c 1 3; mknod blk b 7 0
PYDIR="/tmp/$2-py"; export PYDIR
mkdir -p "$PYDIR"
cat > "$PYDIR/extent_files.py" <<'PY'
import os, ctypes
def extent_files(d):
    libc = ctypes.CDLL(None, use_errno=True)
    libc.fallocate.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_long, ctypes.c_long]
    # Unwritten extents.
    fd = os.open(d + "/unwritten", os.O_CREAT | os.O_WRONLY)
    assert libc.fallocate(fd, 0, 0, 1 << 20) == 0
    os.close(fd)
    # Fragmented file: every other block punched out, so the extent tree
    # needs depth 2 (more extents than four leaves hold).
    bs = int(os.environ["BS"])
    fd = os.open(d + "/deep", os.O_CREAT | os.O_WRONLY)
    n = 4096
    assert libc.fallocate(fd, 0, 0, n * bs) == 0
    for b in range(1, n, 2):
        assert libc.fallocate(fd, 3, b * bs, bs) == 0   # PUNCH_HOLE | KEEP_SIZE
    os.close(fd)
PY
python3 - <<'PY'
import os, socket, sys
sys.path.insert(0, os.environ["PYDIR"])
from extent_files import extent_files
s = socket.socket(socket.AF_UNIX); s.bind("sock"); s.close()
# Shared extended-attribute block: identical large values dedupe to one block.
v = b"v" * 300
for n in ("xa1", "xa2", "xa3", "xa4"):
    open(n, "wb").write(b"x")
    os.setxattr(n, "user.big", v)
fd = os.open("sparse", os.O_CREAT | os.O_WRONLY)
os.pwrite(fd, b"end", 5 << 20)
os.close(fd)
if os.environ["EXTENTS"] == "1":
    extent_files(".")
else:
    # Past 64 MiB at 1 KiB blocks: reaches the triple-indirect block.
    fd = os.open("tind", os.O_CREAT | os.O_WRONLY)
    os.pwrite(fd, b"far", 70 << 20)
    os.close(fd)
PY
# htree directory; two levels on 1 KiB blocks.
mkdir ht
cd ht
count=$((BS == 1024 ? 3000 : 800))
python3 - "$count" <<'PY'
import sys
for i in range(int(sys.argv[1])):
    open("file-with-a-fairly-long-name-%06d-padding-xxxxxxxxxx" % i, "wb").close()
PY
cd ..
rm -f dir/f3 dir/f7 dir/f11 hard1
sync
if [ "$SHAPE" = ext3x ]; then
    # Now allow extents and write more: block-mapped and extent files share one fs.
    cd /
    umount "$MNT"
    tune2fs -O extent "$IMG" >/dev/null
    mount -o loop "$IMG" "$MNT"
    cd "$MNT"
    mkdir later
    head -c 700000 /dev/urandom > later/new-extent-file
    python3 -c 'import os, sys; sys.path.insert(0, os.environ["PYDIR"]); from extent_files import extent_files; extent_files("later")'
    sync
fi
cd /
umount "$MNT"
trap - EXIT
rmdir "$MNT"
rm -rf "$PYDIR"
REMOTE_SCRIPT

colima ssh -- tee "$REMOTE_SH" < "$LOCAL_SH" > /dev/null
BSV=4096; EXT=1
case "$SHAPE" in k1) BSV=1024 ;; ext3|ext3x) BSV=1024; EXT=0 ;; esac
colima ssh -- sudo env BS="$BSV" EXTENTS="$EXT" bash "$REMOTE_SH" "$SHAPE" "$NAME"
colima ssh -- sudo cat "/tmp/$NAME.img" > "$OUT"
