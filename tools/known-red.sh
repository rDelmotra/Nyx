#!/bin/bash
# Run every KNOWN-RED test and require that each one FAILS.
#
#   tools/known-red.sh
#
# Exit 0 only if every listed test is red. A KNOWN-RED test that passes means
# the bug is gone (remove the #[ignore] and the entry) or the test is vacuous;
# either way it is flagged loudly and the script exits 1.
#
# Add a line to RED per test:  "<ID>|<command that runs exactly that test>"
# The command must run the ignored test (--include-ignored / --ignored, or the
# Kotlin equivalent) and exit non-zero when the test fails. A command that
# cannot run at all (build error) is also non-zero, so the table shows the
# exit code and the log path; read the log before trusting a "red".
set -u
cd "$(dirname "$0")/.."

CORE="cargo test -p luks_core --features dangerous-write-support"
KT="kt_red"
JNI="cargo test -p luks_jni --features dangerous-write-support --test exit_routes -- --ignored --exact"
kt_red() {
  local out; out="$( (cd android && ./gradlew --offline -PknownRed=1 :app:testDebugUnitTest --tests "$1") 2>&1 )"; local rc=$?
  echo "$out"
  local n; n="$(grep -cE ' > .* FAILED$' <<<"$out")"
  [ "$n" -gt 0 ] && echo "tests completed, $n failed (counted from per-test FAILED lines)"
  return $rc
}
export -f kt_red
RED=(
  "D2|$CORE --test btrfs_batch_ack_durability f0_4_d2 -- --include-ignored"
  "B1|$CORE --test btrfs_batch_ack_durability f0_5_b1 -- --include-ignored"
  # Kotlin (F0.7): gated by -PknownRed=1 (Assume), never by editing source.
  # Filters are per test method, because the same class also holds a deliberately green test.
  "W5|$KT '*TransferIntegrityRedTest.w5_*'"
  "W6|$KT '*TransferIntegrityRedTest.w6_*'"
  "C1|$KT '*TransferIntegrityRedTest.c1_*'"
  "C5|$KT '*TransferIntegrityRedTest.c5_cancelRecorded*'"
  "N3-ext4|$JNI n3_export_chunks_never_stitch_another_file_ext4"
  "N3-btrfs|$JNI n3_export_chunks_never_stitch_another_file_btrfs"
)

LOGDIR="$(mktemp -d -t known-red)"
bad=0
printf '%-14s %-8s %s\n' ID RESULT LOG
for entry in "${RED[@]}"; do
  id="${entry%%|*}"; cmd="${entry#*|}"
  log="$LOGDIR/$(echo "$id" | tr '/' '_').log"
  bash -c "$cmd" >"$log" 2>&1
  rc=$?
  if [ $rc -eq 0 ]; then
    printf '%-14s %-8s %s\n' "$id" "GREEN!!" "$log"
    echo "  *** $id is GREEN: the bug is fixed (drop the #[ignore] and this entry) or the test is vacuous." >&2
    bad=1
  elif ! grep -qE "panicked at|test result: FAILED|tests completed, [0-9]+ failed" "$log"; then
    printf '%-14s %-8s %s\n' "$id" "NO-RUN" "$log"
    echo "  *** $id exited $rc without a test failure (build error?). Not counted as red." >&2
    bad=1
  else
    printf '%-14s %-8s %s\n' "$id" red "$log"
  fi
done
exit $bad
