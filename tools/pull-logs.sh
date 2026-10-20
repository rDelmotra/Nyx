#!/usr/bin/env bash
#
# pull-logs.sh — pull the app's structured event log off the phone and summarise it.
#
# Usage:
#   tools/pull-logs.sh            # exactly one adb device must be attached
#   tools/pull-logs.sh <serial>   # pick a device explicitly
#
# Needs a DEBUGGABLE build: the log lives in the app-private dir
# (files/logs/events.N.jsonl) and is read through `run-as`, which only works for
# debuggable APKs. No root, no storage permission, nothing written to shared storage.
#
# Output (all under logs/, which is gitignored):
#   logs/<YYYY-MM-DD_HHMMSS>/files/logs/events.*.jsonl   raw pull
#   logs/<YYYY-MM-DD_HHMMSS>/report.txt                  tools/log-report.py output
# The report path is printed last. Read it first; the raw files are for follow-up.
#
# Contract: notes/recon/F9-logging-design.md (C5 storage, C6 tools).
set -euo pipefail

PKG="dev.luksandroid"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SERIAL="${1:-}"

command -v adb >/dev/null 2>&1 || { echo "error: adb not found on PATH" >&2; exit 1; }
command -v python3 >/dev/null 2>&1 || { echo "error: python3 not found on PATH" >&2; exit 1; }

if [ -z "$SERIAL" ]; then
  # `adb devices` lines: "<serial>\tdevice|unauthorized|offline"
  DEVS="$(adb devices | awk 'NR>1 && $2=="device" {print $1}')"
  N="$(printf '%s' "$DEVS" | grep -c . || true)"
  if [ "$N" -eq 0 ]; then
    echo "error: no adb device in 'device' state (check cable, USB debugging, authorisation prompt)" >&2
    exit 1
  elif [ "$N" -gt 1 ]; then
    echo "error: $N devices attached; pass a serial:" >&2
    printf '  %s\n' $DEVS >&2
    exit 1
  fi
  SERIAL="$DEVS"
fi
ADB=(adb -s "$SERIAL")

# run-as only works for debuggable apps; say so plainly instead of dumping tar noise.
if ! RA_OUT="$("${ADB[@]}" shell run-as "$PKG" ls files/logs 2>&1)"; then
  case "$RA_OUT" in
    *"not debuggable"*) echo "error: $PKG is not debuggable on $SERIAL; install the debug APK" >&2 ;;
    *"unknown package"*|*"not installed"*) echo "error: $PKG is not installed on $SERIAL" >&2 ;;
    *"No such file"*) echo "error: $PKG has no files/logs yet on $SERIAL (never unlocked since install?)" >&2 ;;
    *) echo "error: run-as $PKG failed on $SERIAL: $RA_OUT" >&2 ;;
  esac
  exit 1
fi

OUT="$ROOT/logs/$(date +%Y-%m-%d_%H%M%S)"
mkdir -p "$OUT"
"${ADB[@]}" exec-out run-as "$PKG" tar c files/logs | tar x -C "$OUT"

if ! ls "$OUT"/files/logs/events.*.jsonl >/dev/null 2>&1; then
  echo "error: pulled archive contains no events.*.jsonl (kept in $OUT)" >&2
  exit 1
fi

python3 "$ROOT/tools/log-report.py" "$OUT" > "$OUT/report.txt"
echo "$OUT/report.txt"
