#!/usr/bin/env bash
#
# test-log-report.sh — run tools/log-report.py on the hand-written fixture
# (tools/testdata/logs/) and assert the key facts. Fails (exit 1) on any mismatch.
#
# Usage: tools/test-log-report.sh
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FIX="$ROOT/tools/testdata/logs"
REPORT="$(mktemp)"
trap 'rm -f "$REPORT"' EXIT
python3 "$ROOT/tools/log-report.py" "$FIX" > "$REPORT"

FAILED=0
expect() {  # expect <fixed string that must appear in the report>
  if ! grep -qF -- "$1" "$REPORT"; then
    echo "FAIL: report lacks: $1" >&2
    FAILED=1
  fi
}

expect "lines=34 unparseable=1 processes=2 sessions=2 transfers=2 failures=4"
expect "P1 pid=4101"
expect "P2 pid=4222"
expect "S a1b2c3d4 P1"
expect "fs=btrfs cipher=aes-xts-plain64/512 write=true"
expect "P1 x1 import 2 104857600 4100 24.39 done 0 commit"
expect "P1 x2 export 1 20971520 2000 10.0 failed 5 read LuksException"
expect "-- F1 native_err"
expect "-- F2 xfer_end"
expect "-- F3 unlock_fail"
expect "-- F4 health"
expect "finding leaked_extent n=3"
expect "native_ring (3 records, xfer=2):"
expect "kind=scsi_result lba=812345 nseq=31 sense_key=3 status=2"
expect "usb_timeout=1 usb_reset=1 scsi_reset=0 panics=0 native_dropped=4 ring_lines=3"
expect "scsi sense: sense_key=3 asc=17 x1"
expect "seq gaps=1 lines_lost_to_gaps=3 dropped_kt=2 dropped_rs=4"
expect "gap P1 after seq=12 next seq=16 lost=3"
expect "unparseable lines=1"
expect "events.0.jsonl:3 "

# the 20-line cap: no failure block may list more than 20 preceding lines
if grep -E 'preceding ([2-9][0-9]|[1-9][0-9]{2,}) line' "$REPORT" >/dev/null; then
  echo "FAIL: a failure lists more than 20 preceding lines" >&2; FAILED=1
fi

# --json must be valid JSON and agree on the headline numbers
python3 - "$ROOT" <<'PY' || FAILED=1
import json, subprocess, sys
root = sys.argv[1]
out = subprocess.run([sys.executable, root + "/tools/log-report.py",
                      root + "/tools/testdata/logs", "--json"],
                     capture_output=True, text=True, check=True).stdout
d = json.loads(out)
ok = (len(d["failures"]) == 4 and len(d["transfers"]) == 2
      and d["dropped"] == {"kt": 2, "rs": 4, "lost_to_seq_gaps": 3}
      and len(d["meta"]["unparseable"]) == 1)
if not ok:
    print("FAIL: --json content mismatch", file=sys.stderr); sys.exit(1)
PY

# unreadable input must exit 2
set +e
python3 "$ROOT/tools/log-report.py" /nonexistent/path >/dev/null 2>&1
[ $? -eq 2 ] || { echo "FAIL: unreadable input did not exit 2" >&2; FAILED=1; }
set -e

if [ "$FAILED" -ne 0 ]; then exit 1; fi
echo "ok: log-report fixture checks passed"
