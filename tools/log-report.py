#!/usr/bin/env python3
"""log-report.py - compact incident report from the luks-android JSONL event log.

Usage:
  tools/log-report.py <dir|file> [--json]

  <dir>   a directory containing events.N.jsonl (searched recursively, so the
          output of tools/pull-logs.sh works as-is); files are merged oldest
          (highest N) to newest (events.0).
  <file>  a single events file.
  --json  emit the same content as one JSON object (sorted keys).

Format contract: notes/recon/F9-logging-design.md, C1-C6. Python 3 stdlib only.
Output is deterministic: times are UTC, no wall-clock or path-of-run data.
Exit 0 unless the input is unreadable (exit 2).

Native events: a line is "native" when ev is native / native_ring, or src is
"rs". Its name is the `kind` field (what EventLogger really writes:
ev:"native", kind:"usb_timeout"); a bare ev is accepted as a fallback.
Native lines carry two clocks: `t` is when Kotlin wrote the line (up to one
2 s drain tick late), `nt` is when Rust recorded the event. Failure context is
ordered by min(nt, t) so causes come before the failure that drained them, and
`nt` is shown on every native line.
Value names are the real ones: scsi_result status is a string ("passed",
"failed", "not_ready", ...), usb_reset has reset_kind, usb_state_transition has
from/to, panic has `at`. xfer_end state is completed|failed|cancelled; a
native_err with code 19 is a cancellation, not a failure.
"""
import json
import re
import sys
from datetime import datetime, timezone
from pathlib import Path

CONTEXT = 20          # preceding Kotlin lines per failure
NATIVE_CONTEXT = 40   # plus up to this many preceding native lines from the same window
CANCELLED_CODE = 19
HOT = {"usb_submit", "usb_reap", "scsi_command", "btrfs_write_chunk", "btrfs_begin_file",
       "btrfs_block_cowed", "btrfs_block_reused", "btrfs_height_grew", "btrfs_interior_split",
       "btrfs_leaf_split", "btrfs_node_removed", "btrfs_root_collapsed",
       "btrfs_converge_rounds", "ext4_begin_file"}
OK_STATES = ("completed",)
REQUIRED = ("t", "seq", "ev")
BAD_HEALTH = ("corrupt", "leaks", "unknown")
FILE_RE = re.compile(r"^events\.(\d+)\.jsonl$")
HEADER_KEYS = ("app", "git", "model", "sdk", "vid", "pid", "dev_bytes", "sector",
               "flush", "max_xfer", "fs", "cipher", "key_bits", "write")
FIELD_ORDER = ("xfer", "op", "state", "code", "stage", "exc", "errno", "verdict",
               "reason", "kind", "ino", "parent", "lba", "blocks", "bytes", "ms",
               "n", "nseq", "lvl")


def fmt_t(t, full=False):
    d = datetime.fromtimestamp(t / 1000.0, tz=timezone.utc)
    s = d.strftime("%Y-%m-%dT%H:%M:%S" if full else "%H:%M:%S")
    return "%s.%03dZ" % (s, t % 1000) if full else "%s.%03d" % (s, t % 1000)


def val(v):
    if isinstance(v, list):
        return ",".join(str(x) for x in v)
    if isinstance(v, bool):
        return "true" if v else "false"
    return str(v)


def render(r):
    skip = {"t", "seq", "sid", "src", "ev", "msg"}
    keys = [k for k in FIELD_ORDER if k in r]
    keys += sorted(k for k in r if k not in skip and k not in keys and not k.startswith("_"))
    if r["ev"] == "session_header":
        keys = []  # digest lives under SESSIONS
    parts = ["%s=%s" % (k, val(r[k])) for k in keys]
    if "msg" in r:
        parts.append("msg=" + json.dumps(r["msg"], ensure_ascii=False))
    s = "%s #%s %s %s %s %s" % (fmt_t(r["t"]), r["seq"], r.get("sid", "-"),
                                r.get("src", "?"), r["ev"], " ".join(parts))
    s = s.rstrip()
    return s if len(s) <= 220 else s[:217] + "..."


def native_name(r):
    if r["ev"] in ("native", "native_ring") and isinstance(r.get("kind"), str):
        return r["kind"]
    return r["ev"]


def is_native(r):
    return r["ev"] in ("native", "native_ring") or r.get("src") == "rs"


def collect_files(arg):
    p = Path(arg)
    if p.is_file():
        return [p]
    if not p.is_dir():
        raise OSError("not a file or directory: %s" % arg)
    found = []
    for f in p.rglob("events.*.jsonl"):
        m = FILE_RE.match(f.name)
        if m:
            found.append((str(f.parent), -int(m.group(1)), f))
    found.sort(key=lambda x: (x[0], x[1]))
    if not found:
        raise OSError("no events.N.jsonl files under %s" % arg)
    return [x[2] for x in found]


def load(files):
    recs, bad, nlines = [], [], 0
    for f in files:
        with open(f, "rb") as fh:
            data = fh.read()
        for i, raw in enumerate(data.split(b"\n"), 1):
            if not raw.strip():
                continue
            nlines += 1
            try:
                o = json.loads(raw.decode("utf-8"))
                if not isinstance(o, dict):
                    raise ValueError("not an object")
                for k in REQUIRED:
                    if k not in o:
                        raise ValueError("missing " + k)
                if not isinstance(o["t"], int) or not isinstance(o["seq"], int) \
                        or not isinstance(o["ev"], str):
                    raise ValueError("bad types")
            except (ValueError, UnicodeDecodeError) as e:
                bad.append({"file": f.name, "line": i, "why": str(e)[:60]})
                continue
            o["_file"], o["_line"] = f.name, i
            recs.append(o)
    return recs, bad, nlines


def analyse(recs):
    # segment by process_start
    seg = 0
    for r in recs:
        if r["ev"] == "process_start":
            seg += 1
        r["_seg"] = seg
    segs = {}
    for idx, r in enumerate(recs):
        r["_idx"] = idx
        segs.setdefault(r["_seg"], []).append(r)

    # causal order inside a process: native lines by min(nt, t), file order breaks ties
    order = {}
    for sg, rs_ in segs.items():
        lst = sorted(rs_, key=lambda r: (min(r["nt"], r["t"]) if is_native(r)
                                         and isinstance(r.get("nt"), int) else r["t"], r["_idx"]))
        for pos, r in enumerate(lst):
            r["_pos"] = pos
        order[sg] = lst

    processes, gaps, anomalies = [], [], []
    dropped_kt = dropped_rs = 0
    for s in sorted(segs):
        rs = segs[s]
        prev = None
        for r in rs:
            if prev is not None:
                if r["seq"] > prev["seq"] + 1:
                    gaps.append({"seg": s, "after_seq": prev["seq"], "next_seq": r["seq"],
                                 "lost": r["seq"] - prev["seq"] - 1, "t": r["t"]})
                elif r["seq"] <= prev["seq"]:
                    anomalies.append({"seg": s, "seq": r["seq"], "prev_seq": prev["seq"],
                                      "file": r["_file"], "line": r["_line"]})
            elif r["ev"] == "process_start" and r["seq"] != 1:
                gaps.append({"seg": s, "after_seq": 0, "next_seq": r["seq"],
                             "lost": r["seq"] - 1, "t": r["t"]})
            prev = r
        first = rs[0]
        processes.append({
            "seg": s, "pid": first.get("n") if first["ev"] == "process_start" else None,
            "start_t": first["t"], "end_t": rs[-1]["t"], "lines": len(rs),
            "last_seq": rs[-1]["seq"], "has_start": first["ev"] == "process_start",
            "ended_by_uncaught": any(r["ev"] == "uncaught" for r in rs)})
        for r in rs:
            if r["ev"] == "dropped":
                dropped_kt += int(r.get("n", 0))
            elif r["ev"] == "native_dropped":
                dropped_rs += int(r.get("n", 0))

    # sessions
    sessions = {}
    for r in recs:
        sid = r.get("sid", "-")
        if sid == "-":
            continue
        k = (r["_seg"], sid)
        s = sessions.setdefault(k, {"seg": r["_seg"], "sid": sid, "first_t": r["t"],
                                    "last_t": r["t"], "lines": 0, "header": None,
                                    "unlock_ms": None, "end": None, "xfers": set(),
                                    "health": None})
        s["last_t"] = r["t"]
        s["lines"] += 1
        if "xfer" in r:
            s["xfers"].add(r["xfer"])
        if r["ev"] == "session_header":
            s["header"] = {k2: r[k2] for k2 in HEADER_KEYS if k2 in r}
        elif r["ev"] == "unlock":
            s["unlock_ms"] = r.get("ms")
        elif r["ev"] == "unlock_fail":
            s["end"] = "unlock_fail"
        elif r["ev"] in ("lock", "detach"):
            s["end"] = "%s:%s" % (r["ev"], r.get("reason", "?"))
        elif r["ev"] == "idle_lock":
            s["end"] = "idle_lock"
        elif r["ev"] == "health":
            s["health"] = r.get("verdict")
    sess_out = []
    for k in sorted(sessions):
        s = sessions[k]
        s["dur_ms"] = s["last_t"] - s["first_t"]
        s["xfers"] = len(s["xfers"])
        sess_out.append(s)

    # transfers
    xf = {}
    for r in recs:
        if "xfer" not in r or r["ev"] not in ("xfer_queued", "xfer_start", "xfer_phase", "xfer_end"):
            continue
        t = xf.setdefault((r["_seg"], r["xfer"]), {
            "seg": r["_seg"], "xfer": r["xfer"], "op": None, "files": None, "bytes": None,
            "ms": None, "state": "no_end", "code": None, "stage": None, "exc": None,
            "phases": {}, "queued_t": None, "end_t": None, "sid": r.get("sid", "-")})
        if r["ev"] == "xfer_queued":
            t["op"], t["files"], t["queued_t"] = r.get("op"), r.get("n"), r["t"]
            t["bytes"] = r.get("bytes")
        elif r["ev"] == "xfer_phase":
            st = r.get("stage", "?")
            t["phases"][st] = t["phases"].get(st, 0) + int(r.get("ms", 0))
        elif r["ev"] == "xfer_end":
            t["state"], t["code"], t["stage"], t["exc"] = (
                r.get("state"), r.get("code"), r.get("stage"), r.get("exc"))
            t["ms"], t["end_t"] = r.get("ms"), r["t"]
            if "bytes" in r:
                t["bytes"] = r["bytes"]
            if "n" in r:
                t["files"] = r["n"]
    xfers = []
    for k in sorted(xf):
        t = xf[k]
        t["mibs"] = None
        if t["bytes"] and t["ms"]:
            t["mibs"] = round(t["bytes"] / 1048576.0 / (t["ms"] / 1000.0), 2)
        xfers.append(t)

    # health
    health = []
    for r in recs:
        if r["ev"] == "health":
            health.append({"idx": r["_idx"], "seg": r["_seg"], "t": r["t"],
                           "sid": r.get("sid", "-"), "verdict": r.get("verdict"),
                           "reason": r.get("reason"), "ms": r.get("ms"), "findings": []})
        elif r["ev"] == "health_finding" and health and health[-1]["seg"] == r["_seg"]:
            health[-1]["findings"].append({"kind": r.get("kind"), "n": r.get("n")})

    # failures
    def is_failure(r):
        e = r["ev"]
        if e == "xfer_end":
            return r.get("state") == "failed"
        if e == "native_err":
            return r.get("code") != CANCELLED_CODE
        if e in ("unlock_fail", "uncaught"):
            return True
        if e == "health":
            return r.get("verdict") in BAD_HEALTH
        return False

    rings = {}
    orphans = 0
    for r in recs:
        if r["ev"] == "native_ring":
            rings.setdefault((r["_seg"], r.get("xfer")), []).append(r)
    failures = []
    for r in recs:
        if not is_failure(r):
            continue
        lst = order[r["_seg"]]
        kt_n = nat_n = 0
        pre = []
        for x in reversed(lst[:r["_pos"]]):  # bounded: stops at the caps
            if x["ev"] == "native_ring":
                continue
            if is_native(x):
                if native_name(x) in HOT or nat_n >= NATIVE_CONTEXT:
                    continue
                nat_n += 1
            else:
                if kt_n >= CONTEXT:
                    break
                kt_n += 1
            pre.append(x)
            if kt_n >= CONTEXT and nat_n >= NATIVE_CONTEXT:
                break
        pre.reverse()
        f = {"ev": r["ev"], "seg": r["_seg"], "seq": r["seq"], "t": r["t"],
             "sid": r.get("sid", "-"), "xfer": r.get("xfer"), "record": r,
             "context": pre, "ring": [], "ring_ref": None}
        if r["ev"] == "health":
            f["findings"] = next((h["findings"] for h in health if h["idx"] == r["_idx"]), [])
        failures.append(f)
    attached = set()
    for pref in ("xfer_end", None):
        for f in failures:
            key = (f["seg"], f["xfer"])
            if f["xfer"] is None or key not in rings:
                continue
            if pref and f["ev"] != pref:
                continue
            if key in attached:
                if f["ring_ref"] is None and not f["ring"]:
                    f["ring_ref"] = "ring attached to xfer %s failure above" % f["xfer"]
                continue
            f["ring"] = rings[key]
            attached.add(key)
    orphans = sum(len(v) for k, v in rings.items() if k not in attached)

    cancelled = {"xfer_cancelled": sum(1 for t in xfers if t["state"] == "cancelled"),
                 "native_err_code19": sum(1 for r in recs
                                          if r["ev"] == "native_err" and r.get("code") == CANCELLED_CODE)}

    # native summary
    nat = {"counts": {}, "usb_timeout": 0, "usb_reset": 0, "scsi_reset": 0,
           "fs_errors": {},
           "panics": 0, "ring_lines": 0, "dropped": dropped_rs,
           "usb_reset_kinds": {}, "usb_transitions": {}, "scsi_status": {}, "panic_at": {},
           "usb_drain_aborted": 0, "ext4_left_dirty": {}, "flush_failed": 0}
    for r in recs:
        if not is_native(r):
            continue
        if r["ev"] == "native_ring":
            nat["ring_lines"] += 1
            continue
        n = native_name(r)
        nat["counts"][n] = nat["counts"].get(n, 0) + 1
        if n == "usb_timeout":
            nat["usb_timeout"] += 1
        elif n == "usb_reset":
            nat["usb_reset"] += 1
            k = "%s status=%s" % (r.get("reset_kind", "?"), r.get("status", "?"))
            nat["usb_reset_kinds"][k] = nat["usb_reset_kinds"].get(k, 0) + 1
        elif n == "usb_state_transition":
            k = "%s->%s" % (r.get("from", "?"), r.get("to", "?"))
            nat["usb_transitions"][k] = nat["usb_transitions"].get(k, 0) + 1
        elif n == "usb_drain_aborted":
            nat["usb_drain_aborted"] += 1
        elif n == "scsi_reset":
            nat["scsi_reset"] += 1
        elif n == "panic":
            nat["panics"] += 1
            k = "%s:%s" % (r.get("at", "?"), r.get("line", "?"))
            nat["panic_at"][k] = nat["panic_at"].get(k, 0) + 1
        elif n in ("ext4_error", "btrfs_error"):
            key = "%s stage=%s code=%s" % (n, r.get("stage", "?"), r.get("code", "?"))
            nat["fs_errors"][key] = nat["fs_errors"].get(key, 0) + 1
        elif n == "ext4_left_dirty":
            k = "resync_ok=%s cleanup_code=%s" % (val(r.get("resync_ok")), r.get("cleanup_code", "-"))
            nat["ext4_left_dirty"][k] = nat["ext4_left_dirty"].get(k, 0) + 1
        elif n == "ext4_flush" and r.get("ok") is False:
            nat["flush_failed"] += 1
        elif n == "scsi_result":
            st = r.get("status")
            if st != "passed" or r.get("sense_key") is not None:
                k = "status=%s sense_key=%s" % (st, r.get("sense_key", "-"))
                nat["scsi_status"][k] = nat["scsi_status"].get(k, 0) + 1

    return {"processes": processes, "sessions": sess_out, "transfers": xfers,
            "failures": failures, "health": health, "native": nat, "seq_gaps": gaps,
            "seq_anomalies": anomalies, "cancelled": cancelled,
            "dropped": {"kt": dropped_kt, "rs": dropped_rs,
                        "lost_to_seq_gaps": sum(g["lost"] for g in gaps)},
            "orphan_ring_lines": orphans}


def clean(r):
    return {k: v for k, v in r.items() if not k.startswith("_")}


def to_json(rep, meta):
    out = dict(rep)
    out["meta"] = meta
    out["sessions"] = [dict(s) for s in rep["sessions"]]
    fl = []
    for f in rep["failures"]:
        g = dict(f)
        g["record"] = clean(f["record"])
        g["context"] = [clean(x) for x in f["context"]]
        g["ring"] = [clean(x) for x in f["ring"]]
        fl.append(g)
    out["failures"] = fl
    return json.dumps(out, sort_keys=True, indent=1, ensure_ascii=False)


def kv(d):
    return "; ".join("%s x%s" % (k, d[k]) for k in sorted(d)) or "none"


def to_text(rep, meta):
    L = []
    a = L.append
    nf = len(rep["failures"])
    a("LOG REPORT")
    a("files=%s lines=%d unparseable=%d processes=%d sessions=%d transfers=%d failures=%d" % (
        ",".join(meta["files"]), meta["lines"], len(meta["unparseable"]),
        len(rep["processes"]), len(rep["sessions"]), len(rep["transfers"]), nf))
    if rep["processes"]:
        a("span=%s .. %s UTC" % (fmt_t(rep["processes"][0]["start_t"], True),
                                 fmt_t(max(p["end_t"] for p in rep["processes"]), True)))
    xf_bad = [t for t in rep["transfers"] if t["state"] not in OK_STATES + ("cancelled",)]
    c = rep["cancelled"]
    a("verdict: %s" % ("%d failure event(s), %d transfer(s) failed/unfinished" % (nf, len(xf_bad))
                       if nf or xf_bad else "no failure events"))
    a("cancelled (not failures): transfers=%d native_err_code19=%d" % (
        c["xfer_cancelled"], c["native_err_code19"]))

    a("")
    a("== PROCESSES ==")
    for p in rep["processes"]:
        a("P%d pid=%s %s .. %s lines=%d last_seq=%d%s%s" % (
            p["seg"], p["pid"] if p["pid"] is not None else "?", fmt_t(p["start_t"], True),
            fmt_t(p["end_t"], True), p["lines"], p["last_seq"],
            "" if p["has_start"] else " (no process_start: head rotated away)",
            " UNCAUGHT" if p["ended_by_uncaught"] else ""))

    a("")
    a("== SESSIONS ==")
    for s in rep["sessions"]:
        h = s["header"]
        if h:
            d = "app=%s git=%s model=%s sdk=%s usb=%s:%s dev_bytes=%s sector=%s flush=%s max_xfer=%s fs=%s cipher=%s/%s write=%s" % (
                h.get("app"), h.get("git"), h.get("model"), h.get("sdk"), h.get("vid"),
                h.get("pid"), h.get("dev_bytes"), h.get("sector"), val(h.get("flush")),
                h.get("max_xfer"), h.get("fs"), h.get("cipher"), h.get("key_bits"),
                val(h.get("write")))
        else:
            d = "(no session_header in log)"
        a("S %s P%d %s dur=%.1fs unlock_ms=%s xfers=%d health=%s end=%s" % (
            s["sid"], s["seg"], fmt_t(s["first_t"], True), s["dur_ms"] / 1000.0,
            s["unlock_ms"], s["xfers"], s["health"], s["end"] or "open"))
        a("  " + d)

    a("")
    a("== TRANSFERS ==")
    if rep["transfers"]:
        a("P xfer op files bytes ms MiB/s state code stage exc phases_ms")
    for t in rep["transfers"]:
        ph = ",".join("%s:%d" % (k, t["phases"][k]) for k in sorted(t["phases"])) or "-"
        a("P%d x%s %s %s %s %s %s %s %s %s %s %s" % (
            t["seg"], t["xfer"], t["op"] or "?", t["files"], t["bytes"], t["ms"],
            t["mibs"] if t["mibs"] is not None else "-", t["state"],
            t["code"] if t["code"] is not None else "-", t["stage"] or "-",
            t["exc"] or "-", ph))

    a("")
    a("== HEALTH ==")
    for h in rep["health"]:
        a("P%d %s sid=%s verdict=%s reason=%s ms=%s findings=%s" % (
            h["seg"], fmt_t(h["t"]), h["sid"], h["verdict"], h["reason"] or "-", h["ms"],
            ",".join("%s:%s" % (f["kind"], f["n"]) for f in h["findings"]) or "-"))
    if not rep["health"]:
        a("none")

    a("")
    a("== FAILURES (%d) ==" % nf)
    for i, f in enumerate(rep["failures"], 1):
        a("-- F%d %s P%d seq=%d %s sid=%s%s" % (
            i, f["ev"], f["seg"], f["seq"], fmt_t(f["t"], True), f["sid"],
            " xfer=%s" % f["xfer"] if f["xfer"] is not None else ""))
        a("  >> " + render(f["record"]))
        for fnd in f.get("findings", []):
            a("  finding %s n=%s" % (fnd["kind"], fnd["n"]))
        if "frames" in f["record"]:
            a("  frames: " + val(f["record"]["frames"]))
        ctx = f["context"]
        prev = next((g for g in reversed(rep["failures"][:i - 1])
                     if g["seg"] == f["seg"] and g["record"]["_idx"] in [c["_idx"] for c in ctx]), None)
        if prev is not None:
            ctx = [c for c in ctx if c["_pos"] > prev["record"]["_pos"]]
            a("  preceding lines: same as F%d up to and including it (#%d), then %d more:" % (
                rep["failures"].index(prev) + 1, prev["seq"], len(ctx)))
        else:
            a("  preceding %d line(s) of P%d (native ordered by min(nt,t)):" % (len(ctx), f["seg"]))
        for x in ctx:
            a("   | " + render(x))
        if f["ring"]:
            a("  native_ring (%d records, xfer=%s):" % (len(f["ring"]), f["xfer"]))
            for x in f["ring"]:
                a("   ~ " + render(x))
        elif f["ring_ref"]:
            a("  native_ring: " + f["ring_ref"])
    if not nf:
        a("none")

    n = rep["native"]
    a("")
    a("== NATIVE (drained notable events) ==")
    a("usb_timeout=%d usb_reset=%d scsi_reset=%d panics=%d native_dropped=%d ring_lines=%d orphan_ring_lines=%d" % (
        n["usb_timeout"], n["usb_reset"], n["scsi_reset"], n["panics"], n["dropped"],
        n["ring_lines"], rep["orphan_ring_lines"]))
    a("usb reset kinds: " + kv(n["usb_reset_kinds"]))
    a("usb state transitions: " + kv(n["usb_transitions"]) + "; drain_aborted=%d" % n["usb_drain_aborted"])
    a("scsi not-passed / sense: " + kv(n["scsi_status"]))
    a("ext4 left dirty: " + kv(n["ext4_left_dirty"]) + "; flush_failed=%d" % n["flush_failed"])
    a("panics at: " + kv(n["panic_at"]))
    a("fs errors: " + kv(n["fs_errors"]))
    a("all native: " + kv(n["counts"]))

    a("")
    a("== INTEGRITY OF THE LOG ==")
    d = rep["dropped"]
    a("seq gaps=%d lines_lost_to_gaps=%d dropped_kt=%d dropped_rs=%d seq_backwards=%d" % (
        len(rep["seq_gaps"]), d["lost_to_seq_gaps"], d["kt"], d["rs"], len(rep["seq_anomalies"])))
    for g in rep["seq_gaps"]:
        a("gap P%d after seq=%d next seq=%d lost=%d at %s" % (
            g["seg"], g["after_seq"], g["next_seq"], g["lost"], fmt_t(g["t"], True)))
    for g in rep["seq_anomalies"]:
        a("seq went backwards P%d seq=%d after %d (%s:%d)" % (
            g["seg"], g["seq"], g["prev_seq"], g["file"], g["line"]))
    bad = meta["unparseable"]
    a("unparseable lines=%d" % len(bad))
    for b in bad[:10]:
        a("  %s:%d (%s)" % (b["file"], b["line"], b["why"]))
    if len(bad) > 10:
        a("  ... %d more" % (len(bad) - 10))
    return "\n".join(L) + "\n"


def main(argv):
    args = [a for a in argv[1:] if not a.startswith("--")]
    flags = [a for a in argv[1:] if a.startswith("--")]
    if len(args) != 1 or any(f != "--json" for f in flags):
        sys.stderr.write("usage: log-report.py <dir|file> [--json]\n")
        return 2
    try:
        files = collect_files(args[0])
        recs, bad, nlines = load(files)
    except OSError as e:
        sys.stderr.write("log-report: cannot read input: %s\n" % e)
        return 2
    rep = analyse(recs)
    meta = {"files": [f.name for f in files], "lines": nlines, "unparseable": bad}
    sys.stdout.write((to_json(rep, meta) + "\n") if flags else to_text(rep, meta))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
