//! Forensic in-memory ring buffer for low-level diagnostic tracing.
//!
//! # Security and Privacy Invariants
//!
//! 1. **Zero Filenames & Paths**: Filesystem events store only numeric inode IDs.
//! 2. **Zero File Contents**: Read/write events store only byte lengths, sector counts,
//!    and block/device addresses. File payloads are never buffered.
//! 3. **Zero Key Material**: No passphrases, keyslot metadata, or cipher keys are recorded.
//! 4. **Bounded Capacity**: Fixed circular buffer (256 slots) that drops oldest entries
//!    under load without unbounded memory growth.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const RING_CAPACITY: usize = 256;

/// Capacity of the lossless notable queue (see [`drain_notable`]). At ~100
/// bytes a record this bounds it near 1 MiB even if the consumer stops draining.
const NOTABLE_CAPACITY: usize = 8192;

/// Capacity of the dedicated Btrfs-only ring buffer (see `GLOBAL_BTRFS_RING_BUFFER`).
///
/// USB/SCSI transport events outnumber btrfs structural events by orders of magnitude
/// on a real transfer, so the shared ring above can be entirely evicted of structural
/// events (LeafSplit / InteriorSplit / HeightGrew / RootCollapsed / NodeRemoved) before a
/// dump is ever taken, leaving only whichever btrfs event happened to land last. This
/// second ring holds only `ForensicEvent::Btrfs` records so transport traffic can never
/// evict them.
///
/// Tied to `RING_CAPACITY` rather than a separate literal: `RingBuffer`'s backing array
/// is sized to `RING_CAPACITY` internally (see `RingBuffer::new`), so a diverging literal
/// here would silently lie about the buffer's real capacity.
const BTRFS_RING_CAPACITY: usize = RING_CAPACITY;

static MONOTONIC_SEQ: AtomicU64 = AtomicU64::new(1);

/// An event captured in the forensic ring buffer.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", content = "detail")]
pub enum ForensicEvent {
    Usb(UsbEvent),
    Scsi(ScsiEvent),
    Btrfs(BtrfsEvent),
    Ext4(Ext4Event),
    Panic(PanicEvent),
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum UsbEvent {
    Submit { ep: u8, count: usize, bytes: usize, gen: u64 },
    Reap { ep: u8, status: i32, bytes: usize, gen: u64 },
    Timeout { ep: u8, elapsed_ms: u64, active_urbs: usize, gen: u64 },
    StateTransition { from: &'static str, to: &'static str },
    Reset { kind: &'static str, status: i32 },
    Drain { discarded_count: usize, remaining: usize },
    DrainAborted { errno: i32 },
    UnrecognizedReap { ptr: usize },
}

/// Outcome of a SCSI command as seen at the Bulk-Only Transport. An enum, not a
/// string, so the notable classification cannot drift from what the emitter
/// writes (an earlier version compared against a literal nobody produced).
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum ScsiStatus {
    #[serde(rename = "PASSED")]
    Passed,
    #[serde(rename = "FAILED")]
    Failed,
    #[serde(rename = "PHASE_ERROR")]
    PhaseError,
    #[serde(rename = "RESIDUE_ERROR")]
    ResidueError,
    #[serde(rename = "NOT READY")]
    NotReady,
}

impl ScsiStatus {
    /// Human label used by `dump_text` / `dump_json` (unchanged from before).
    pub fn label(self) -> &'static str {
        match self {
            Self::Passed => "PASSED",
            Self::Failed => "FAILED",
            Self::PhaseError => "PHASE_ERROR",
            Self::ResidueError => "RESIDUE_ERROR",
            Self::NotReady => "NOT READY",
        }
    }

    /// Wire form for the event JSON (C7): `^[a-z0-9_.:-]{1,48}$`.
    pub fn wire(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::PhaseError => "phase_error",
            Self::ResidueError => "residue_error",
            Self::NotReady => "not_ready",
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum ScsiEvent {
    /// `lba`/`blocks` are set for READ/WRITE(10/16) only. Raw numbers, no
    /// names or content, so the privacy invariant is unchanged.
    Command { opcode: u8, tag: u32, data_len: u32, dir: &'static str, lba: Option<u64>, blocks: Option<u32> },
    Result { opcode: u8, status: ScsiStatus, transferred: usize, sense_key: Option<u8> },
    Reset,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum BtrfsEvent {
    Mount { generation: u64, root_bytenr: u64 },
    BeginFile { file_len: u64 },
    WriteChunk { len: usize, bytenr: u64 },
    FinishFile { ino: u64, total_bytes: u64 },
    AbandonFile { runs_count: usize, total_bytes: u64 },
    CreateFile { parent_ino: u64, new_ino: u64 },
    DeleteFile { parent_ino: u64, ino: u64 },
    Mkdir { parent_ino: u64, new_ino: u64 },
    Rename { from_parent: u64, to_parent: u64, ino: u64 },
    ChunkAlloc { logical: u64, length: u64, dev_offset: u64 },
    Commit { tree: u64, generation: u64, transid: u64, nodes_written: usize },
    Error { stage: &'static str, code: i32 },
    LeafSplit { tree: u64, shape: u8, groups: u8, pos_class: u8 },
    InteriorSplit { tree: u64, level: u8 },
    HeightGrew { tree: u64, from: u8, to: u8 },
    RootCollapsed { tree: u64, from: u8, to: u8 },
    NodeRemoved { tree: u64, level: u8 },
    BlockReused { tree: u64, level: u8 },
    BlockCowed { tree: u64, level: u8 },
    ConvergeRounds { rounds: u32 },
}

/// ext4 write-path events. Numbers and fixed literals only: inode numbers and
/// byte counts, never a name or path (privacy invariant 1).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum Ext4Event {
    BeginFile { size: u64 },
    FinishFile { ino: u64, bytes: u64 },
    AbandonFile { bytes: u64 },
    CreateFile { parent: u64, ino: u64 },
    DeleteFile { parent: u64, ino: u64 },
    Mkdir { parent: u64, ino: u64 },
    Rename { from_parent: u64, to_parent: u64, ino: u64 },
    MarkDirty,
    MarkClean,
    Flush { ok: bool },
    /// A mutation window failed with the metadata inconsistent, so the disk is
    /// deliberately left marked dirty. `cleanup_code` is the suppressed
    /// rollback failure (if the rollback itself failed), `resync_ok` whether
    /// memory could be reloaded from disk afterwards (false: this session
    /// now refuses writes).
    LeftDirty { cleanup_code: Option<i32>, resync_ok: bool },
    /// `code` is [`crate::error::LuksError::forensic_code`].
    Error { stage: &'static str, code: i32 },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PanicEvent {
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub message: String,
}

/// A timestamped, sequenced record stored in the ring buffer.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ForensicRecord {
    pub seq: u64,
    pub timestamp_ms: u64,
    pub event: ForensicEvent,
}

struct RingBuffer {
    entries: [Option<ForensicRecord>; RING_CAPACITY],
    head: usize,
    total_records: u64,
}

impl RingBuffer {
    const fn new() -> Self {
        const NONE_ENTRY: Option<ForensicRecord> = None;
        Self {
            entries: [NONE_ENTRY; RING_CAPACITY],
            head: 0,
            total_records: 0,
        }
    }

    fn push(&mut self, event: ForensicEvent) -> &ForensicRecord {
        let seq = MONOTONIC_SEQ.fetch_add(1, Ordering::Relaxed);
        let timestamp_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let record = ForensicRecord {
            seq,
            timestamp_ms,
            event,
        };

        let slot = self.head;
        self.head = (self.head + 1) % RING_CAPACITY;
        self.total_records = self.total_records.saturating_add(1);
        self.entries[slot].insert(record)
    }

    /// Snapshot plus how many records have been evicted since the last clear.
    fn snapshot_with_evicted(&self) -> (Vec<ForensicRecord>, u64) {
        let snap = self.snapshot();
        let evicted = self.total_records.saturating_sub(snap.len() as u64);
        (snap, evicted)
    }

    fn snapshot(&self) -> Vec<ForensicRecord> {
        let mut out = Vec::with_capacity(RING_CAPACITY);
        if self.total_records < RING_CAPACITY as u64 {
            for i in 0..self.head {
                if let Some(ref rec) = self.entries[i] {
                    out.push(rec.clone());
                }
            }
        } else {
            for i in 0..RING_CAPACITY {
                let idx = (self.head + i) % RING_CAPACITY;
                if let Some(ref rec) = self.entries[idx] {
                    out.push(rec.clone());
                }
            }
        }
        out
    }

    fn clear(&mut self) {
        for entry in self.entries.iter_mut() {
            *entry = None;
        }
        self.head = 0;
        self.total_records = 0;
    }
}

static GLOBAL_RING_BUFFER: Mutex<RingBuffer> = Mutex::new(RingBuffer::new());

/// Dedicated ring buffer holding only `ForensicEvent::Btrfs` records. See
/// `BTRFS_RING_CAPACITY` for why this exists. Guarded exactly like `GLOBAL_RING_BUFFER`
/// (a plain `Mutex`, lock failures ignored rather than unwrapped) so a poisoned lock can
/// never panic or deadlock the JNI thread driving a live transfer.
static GLOBAL_BTRFS_RING_BUFFER: Mutex<RingBuffer> = Mutex::new(RingBuffer::new());

/// Record a generic forensic event into the global ring buffer.
///
/// Every event goes to the ring. Events for which [`is_notable`] holds are
/// also copied into the lossless notable queue. Never blocks beyond the two
/// short mutex sections and never panics: a poisoned notable queue is counted
/// in the dropped total, not ignored.
pub fn record(event: ForensicEvent) {
    let notable = is_notable(&event);
    let copy = if let Ok(mut buf) = GLOBAL_RING_BUFFER.lock() {
        let rec = buf.push(event);
        if notable { Some(rec.clone()) } else { None }
    } else {
        None
    };
    if let Some(rec) = copy {
        match NOTABLE_QUEUE.lock() {
            Ok(mut q) => q.push(rec),
            Err(_) => {
                NOTABLE_POISON_DROPPED.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

struct NotableQueue {
    q: VecDeque<ForensicRecord>,
    dropped: u64,
}

impl NotableQueue {
    const fn new() -> Self {
        Self { q: VecDeque::new(), dropped: 0 }
    }

    /// Keep the most recent `NOTABLE_CAPACITY`: at capacity the OLDEST record
    /// is evicted and counted. The newest events are the ones closest to a
    /// failure, so they are the ones worth keeping.
    fn push(&mut self, rec: ForensicRecord) {
        if self.q.len() >= NOTABLE_CAPACITY {
            self.q.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.q.push_back(rec);
    }
}

static NOTABLE_QUEUE: Mutex<NotableQueue> = Mutex::new(NotableQueue::new());

/// Records lost because the notable queue's mutex was poisoned. Folded into
/// the next [`drain_notable`] dropped count.
static NOTABLE_POISON_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Whether an event is copied to the notable queue (design F9 C4). Hot
/// per-URB / per-command / per-chunk events stay ring-only.
pub fn is_notable(event: &ForensicEvent) -> bool {
    match event {
        ForensicEvent::Usb(u) => match u {
            UsbEvent::Submit { .. } | UsbEvent::Reap { .. } => false,
            UsbEvent::Timeout { .. }
            | UsbEvent::StateTransition { .. }
            | UsbEvent::Reset { .. }
            | UsbEvent::Drain { .. }
            | UsbEvent::DrainAborted { .. }
            | UsbEvent::UnrecognizedReap { .. } => true,
        },
        ForensicEvent::Scsi(s) => match s {
            ScsiEvent::Command { .. } => false,
            ScsiEvent::Result { status, sense_key, .. } => *status != ScsiStatus::Passed || sense_key.is_some(),
            ScsiEvent::Reset => true,
        },
        ForensicEvent::Btrfs(b) => match b {
            BtrfsEvent::Mount { .. }
            | BtrfsEvent::FinishFile { .. }
            | BtrfsEvent::AbandonFile { .. }
            | BtrfsEvent::CreateFile { .. }
            | BtrfsEvent::DeleteFile { .. }
            | BtrfsEvent::Mkdir { .. }
            | BtrfsEvent::Rename { .. }
            | BtrfsEvent::ChunkAlloc { .. }
            | BtrfsEvent::Commit { .. }
            | BtrfsEvent::Error { .. } => true,
            BtrfsEvent::BeginFile { .. }
            | BtrfsEvent::WriteChunk { .. }
            | BtrfsEvent::LeafSplit { .. }
            | BtrfsEvent::InteriorSplit { .. }
            | BtrfsEvent::HeightGrew { .. }
            | BtrfsEvent::RootCollapsed { .. }
            | BtrfsEvent::NodeRemoved { .. }
            | BtrfsEvent::BlockReused { .. }
            | BtrfsEvent::BlockCowed { .. }
            | BtrfsEvent::ConvergeRounds { .. } => false,
        },
        ForensicEvent::Ext4(_) => true,
        ForensicEvent::Panic(_) => true,
    }
}

/// Take everything in the notable queue, oldest first, plus how many records
/// were lost (evicted at capacity, or dropped on a poisoned lock) since the
/// previous drain. Both are reset. A poisoned queue is recovered rather than
/// abandoned, so one panic cannot silence the log for the rest of the process.
pub fn drain_notable() -> (Vec<ForensicRecord>, u64) {
    let (records, mut dropped) = match NOTABLE_QUEUE.lock() {
        Ok(mut q) => (q.q.drain(..).collect(), std::mem::take(&mut q.dropped)),
        Err(poisoned) => {
            let mut q = poisoned.into_inner();
            let out = (q.q.drain(..).collect(), std::mem::take(&mut q.dropped));
            drop(q);
            NOTABLE_QUEUE.clear_poison();
            out
        }
    };
    dropped = dropped.saturating_add(NOTABLE_POISON_DROPPED.swap(0, Ordering::Relaxed));
    (records, dropped)
}

/// Helper to record an ext4 event.
pub fn record_ext4(event: Ext4Event) {
    record(ForensicEvent::Ext4(event));
}

/// Helper to record a USB event.
pub fn record_usb(event: UsbEvent) {
    record(ForensicEvent::Usb(event));
}

/// Helper to record a SCSI event.
pub fn record_scsi(event: ScsiEvent) {
    record(ForensicEvent::Scsi(event));
}

/// Helper to record a Btrfs event.
pub fn record_btrfs(event: BtrfsEvent) {
    // Cloned before the move into `record()` below so the dedicated btrfs-only ring
    // (see `GLOBAL_BTRFS_RING_BUFFER`) also gets a copy; the main ring must keep
    // receiving every event exactly as before so its interleaved USB/SCSI/Btrfs
    // ordering is unaffected.
    let btrfs_ring_copy = event.clone();
    record(ForensicEvent::Btrfs(event));
    if let Ok(mut buf) = GLOBAL_BTRFS_RING_BUFFER.lock() {
        buf.push(ForensicEvent::Btrfs(btrfs_ring_copy));
    }
}

/// Helper to record a panic event.
///
/// `message` is `&'static str` on purpose: a panic payload is formatted from
/// whatever the panicking code held (a path, a name), and the previous
/// word-prefix sanitizer only caught `/`-led words. A literal cannot carry
/// runtime data, so the compiler now enforces what the sanitizer only
/// approximated. The source location is the diagnostic.
pub fn record_panic(file: &str, line: u32, col: u32, message: &'static str) {
    record(ForensicEvent::Panic(PanicEvent {
        file: sanitize_file_path(file),
        line,
        col,
        message: message.to_string(),
    }));
}

fn sanitize_file_path(path: &str) -> String {
    // Retain only the filename / relative rust source path
    if let Some(pos) = path.rfind("/src/") {
        path[pos + 1..].to_string()
    } else if let Some(pos) = path.rfind('\\') {
        path[pos + 1..].to_string()
    } else if let Some(pos) = path.rfind('/') {
        path[pos + 1..].to_string()
    } else {
        path.to_string()
    }
}

/// Take an in-memory chronological snapshot of the ring buffer records.
pub fn snapshot() -> Vec<ForensicRecord> {
    if let Ok(buf) = GLOBAL_RING_BUFFER.lock() {
        buf.snapshot()
    } else {
        Vec::new()
    }
}

/// Take an in-memory chronological snapshot of the dedicated Btrfs-only ring buffer.
fn btrfs_snapshot() -> Vec<ForensicRecord> {
    if let Ok(buf) = GLOBAL_BTRFS_RING_BUFFER.lock() {
        buf.snapshot()
    } else {
        Vec::new()
    }
}

/// Reset the ring buffers (primarily for test isolation).
pub fn clear() {
    if let Ok(mut buf) = GLOBAL_RING_BUFFER.lock() {
        buf.clear();
    }
    if let Ok(mut buf) = GLOBAL_BTRFS_RING_BUFFER.lock() {
        buf.clear();
    }
}

/// Format the ring buffer snapshot as JSON.
pub fn dump_json() -> String {
    let records = snapshot();
    serde_json::to_string_pretty(&records).unwrap_or_else(|_| "[]".to_string())
}

/// Format a single Btrfs event exactly the way `dump_text()` has always formatted it,
/// including the trailing newline. Factored out of the main dump loop so the dedicated
/// Btrfs-only ring section (see `GLOBAL_BTRFS_RING_BUFFER`) can reuse the identical
/// per-event line format instead of duplicating each `format!` string.
fn format_btrfs_event_line(b: &BtrfsEvent) -> String {
    match b {
        BtrfsEvent::Mount { generation, root_bytenr } => {
            format!("BTRFS mount gen={generation} root={root_bytenr}\n")
        }
        BtrfsEvent::BeginFile { file_len } => {
            format!("BTRFS begin_file len={file_len}\n")
        }
        BtrfsEvent::WriteChunk { len, bytenr } => {
            format!("BTRFS write_chunk len={len} bytenr={bytenr}\n")
        }
        BtrfsEvent::FinishFile { ino, total_bytes } => {
            format!("BTRFS finish_file ino={ino} bytes={total_bytes}\n")
        }
        BtrfsEvent::AbandonFile { runs_count, total_bytes } => {
            format!("BTRFS abandon_file runs={runs_count} bytes={total_bytes}\n")
        }
        BtrfsEvent::CreateFile { parent_ino, new_ino } => {
            format!("BTRFS create parent_ino={parent_ino} ino={new_ino}\n")
        }
        BtrfsEvent::DeleteFile { parent_ino, ino } => {
            format!("BTRFS delete parent_ino={parent_ino} ino={ino}\n")
        }
        BtrfsEvent::Mkdir { parent_ino, new_ino } => {
            format!("BTRFS mkdir parent_ino={parent_ino} ino={new_ino}\n")
        }
        BtrfsEvent::Rename { from_parent, to_parent, ino } => {
            format!("BTRFS rename from_parent={from_parent} to_parent={to_parent} ino={ino}\n")
        }
        BtrfsEvent::ChunkAlloc { logical, length, dev_offset } => {
            format!("BTRFS chunk_alloc logical={logical} len={length} dev_offset={dev_offset}\n")
        }
        BtrfsEvent::Commit { tree, generation, transid, nodes_written } => {
            format!("BTRFS commit tree={tree} gen={generation} transid={transid} nodes={nodes_written}\n")
        }
        BtrfsEvent::Error { stage, code } => {
            format!("BTRFS error stage={stage} code={code}\n")
        }
        BtrfsEvent::LeafSplit { tree, shape, groups, pos_class } => {
            format!("BTRFS leaf_split tree={tree} shape={shape} groups={groups} pos={pos_class}\n")
        }
        BtrfsEvent::InteriorSplit { tree, level } => {
            format!("BTRFS interior_split tree={tree} level={level}\n")
        }
        BtrfsEvent::HeightGrew { tree, from, to } => {
            format!("BTRFS height_grew tree={tree} from={from} to={to}\n")
        }
        BtrfsEvent::RootCollapsed { tree, from, to } => {
            format!("BTRFS root_collapsed tree={tree} from={from} to={to}\n")
        }
        BtrfsEvent::NodeRemoved { tree, level } => {
            format!("BTRFS node_removed tree={tree} level={level}\n")
        }
        BtrfsEvent::BlockReused { tree, level } => {
            format!("BTRFS block_reused tree={tree} level={level}\n")
        }
        BtrfsEvent::BlockCowed { tree, level } => {
            format!("BTRFS block_cowed tree={tree} level={level}\n")
        }
        BtrfsEvent::ConvergeRounds { rounds } => {
            format!("BTRFS converge_rounds rounds={rounds}\n")
        }
    }
}

/// Format the ring buffer snapshot as monotonic text lines.
pub fn dump_text() -> String {
    let records = snapshot();
    let mut out = String::new();
    for rec in records {
        out.push_str(&format!("[#{:06} +{}ms] ", rec.seq, rec.timestamp_ms));
        match rec.event {
            ForensicEvent::Usb(ref u) => match u {
                UsbEvent::Submit { ep, count, bytes, gen } => {
                    out.push_str(&format!("USB submit ep={ep} count={count} bytes={bytes} gen={gen}\n"));
                }
                UsbEvent::Reap { ep, status, bytes, gen } => {
                    out.push_str(&format!("USB reap ep={ep} status={status} bytes={bytes} gen={gen}\n"));
                }
                UsbEvent::Timeout { ep, elapsed_ms, active_urbs, gen } => {
                    out.push_str(&format!("USB timeout ep={ep} elapsed={elapsed_ms}ms active={active_urbs} gen={gen}\n"));
                }
                UsbEvent::StateTransition { from, to } => {
                    out.push_str(&format!("USB state: {from} -> {to}\n"));
                }
                UsbEvent::Reset { kind, status } => {
                    out.push_str(&format!("USB reset kind={kind} status={status}\n"));
                }
                UsbEvent::Drain { discarded_count, remaining } => {
                    out.push_str(&format!("USB drain discarded={discarded_count} remaining={remaining}\n"));
                }
                UsbEvent::DrainAborted { errno } => {
                    out.push_str(&format!("USB drain aborted errno={errno}\n"));
                }
                UsbEvent::UnrecognizedReap { ptr } => {
                    out.push_str(&format!("USB unrecognized reap ptr=0x{ptr:X}\n"));
                }
            },
            ForensicEvent::Scsi(ref s) => match s {
                ScsiEvent::Command { opcode, tag, data_len, dir, lba, blocks } => {
                    out.push_str(&format!("SCSI cmd op=0x{opcode:02X} tag=0x{tag:08X} len={data_len} dir={dir}"));
                    if let (Some(lba), Some(blocks)) = (lba, blocks) {
                        out.push_str(&format!(" lba={lba} blocks={blocks}"));
                    }
                    out.push('\n');
                }
                ScsiEvent::Result { opcode, status, transferred, sense_key } => {
                    out.push_str(&format!(
                        "SCSI res op=0x{opcode:02X} status={} xfer={transferred} sense={:?}\n",
                        status.label(),
                        sense_key
                    ));
                }
                ScsiEvent::Reset => {
                    out.push_str("SCSI reset\n");
                }
            },
            ForensicEvent::Btrfs(ref b) => {
                out.push_str(&format_btrfs_event_line(b));
            }
            ForensicEvent::Ext4(ref e) => {
                out.push_str(&format!("EXT4 {e:?}\n"));
            }
            ForensicEvent::Panic(ref p) => {
                out.push_str(&format!("PANIC at {}:{}:{} - {}\n", p.file, p.line, p.col, p.message));
            }
        }
    }

    // Dedicated section for the Btrfs-only ring (see `GLOBAL_BTRFS_RING_BUFFER`).
    // Appended after the existing dump above so the pre-existing text format and any
    // parsers reading it are unaffected; this is purely additive.
    out.push_str(&format!(
        "=== BTRFS EVENT RING (last {BTRFS_RING_CAPACITY} btrfs events) ===\n"
    ));
    for rec in btrfs_snapshot() {
        out.push_str(&format!("[#{:06} +{}ms] ", rec.seq, rec.timestamp_ms));
        if let ForensicEvent::Btrfs(ref b) = rec.event {
            out.push_str(&format_btrfs_event_line(b));
        }
    }
    out.push_str("=== END BTRFS EVENT RING ===\n");

    out
}

/// Render a snapshot of the ring (not cleared) as `{"dropped":n,"events":[..]}`
/// where `dropped` is how many records the ring has evicted since it was last
/// cleared.
pub fn ring_json() -> String {
    match GLOBAL_RING_BUFFER.lock() {
        Ok(buf) => {
            let (recs, evicted) = buf.snapshot_with_evicted();
            drop(buf);
            events_to_json(&recs, evicted)
        }
        Err(_) => RING_UNAVAILABLE_JSON.to_string(),
    }
}

/// Drain the notable queue and render it as `{"dropped":n,"events":[..]}`.
pub fn drain_notable_json() -> String {
    let (recs, dropped) = drain_notable();
    events_to_json(&recs, dropped)
}

/// What [`ring_json`] returns when the ring's mutex is poisoned: valid JSON in
/// the same shape, with `dropped` saturated so a reader cannot mistake it for
/// an empty ring.
const RING_UNAVAILABLE_JSON: &str = r#"{"dropped":18446744073709551615,"events":[]}"#;

type Fields = Vec<(&'static str, serde_json::Value)>;

fn num<T: Into<serde_json::Number>>(v: T) -> serde_json::Value {
    serde_json::Value::Number(v.into())
}

/// Make a fixed native string wire-safe (C7): lowercase, `/` and `\\` become
/// `.`, any other character outside `[a-z0-9_.:-]` becomes `_`, at most 48
/// characters, never empty. Applied to every string value on the wire so a
/// new event cannot emit something the Kotlin side would reject.
fn wire_string(v: &str) -> String {
    let mapped: Vec<char> = v
        .chars()
        .map(|c| match c.to_ascii_lowercase() {
            c @ ('a'..='z' | '0'..='9' | '_' | '.' | ':' | '-') => c,
            '/' | '\\' => '.',
            _ => '_',
        })
        .collect();
    // Keep the tail when too long: for a source location that is the file name.
    let keep = mapped.len().saturating_sub(48);
    let mut out: String = mapped[keep..].iter().collect();
    if out.is_empty() {
        out.push('_');
    }
    out
}

fn text(v: &str) -> serde_json::Value {
    serde_json::Value::String(wire_string(v))
}

/// `usize` has no `Into<Number>`; widen explicitly (usize is at most 64 bits
/// on every target we build for).
fn unum(v: usize) -> serde_json::Value {
    serde_json::Value::Number(serde_json::Number::from(v as u64))
}

/// The flat JSON name and fields of one event. Exhaustive over every variant
/// so a new event cannot be added without deciding how it renders. Values are
/// numbers, bools or `&'static str` only; a panic contributes its source
/// location and never its message.
fn event_fields(event: &ForensicEvent) -> (String, Fields) {
    use serde_json::Value::Bool;
    match event {
        ForensicEvent::Usb(u) => match u {
            UsbEvent::Submit { ep, count, bytes, gen } => ("usb_submit".into(), vec![("ep", num(*ep)), ("count", unum(*count)), ("bytes", unum(*bytes)), ("gen", num(*gen))]),
            UsbEvent::Reap { ep, status, bytes, gen } => ("usb_reap".into(), vec![("ep", num(*ep)), ("status", num(*status)), ("bytes", unum(*bytes)), ("gen", num(*gen))]),
            UsbEvent::Timeout { ep, elapsed_ms, active_urbs, gen } => ("usb_timeout".into(), vec![("ep", num(*ep)), ("elapsed_ms", num(*elapsed_ms)), ("active_urbs", unum(*active_urbs)), ("gen", num(*gen))]),
            UsbEvent::StateTransition { from, to } => ("usb_state_transition".into(), vec![("from", text(from)), ("to", text(to))]),
            UsbEvent::Reset { kind, status } => ("usb_reset".into(), vec![("reset_kind", text(kind)), ("status", num(*status))]),
            UsbEvent::Drain { discarded_count, remaining } => ("usb_drain".into(), vec![("discarded_count", unum(*discarded_count)), ("remaining", unum(*remaining))]),
            UsbEvent::DrainAborted { errno } => ("usb_drain_aborted".into(), vec![("errno", num(*errno))]),
            UsbEvent::UnrecognizedReap { .. } => ("usb_unrecognized_reap".into(), vec![]), // a heap address: kept out of the log (ASLR)
        },
        ForensicEvent::Scsi(s) => match s {
            ScsiEvent::Command { opcode, tag, data_len, dir, lba, blocks } => {
                let mut f: Fields = vec![("opcode", num(*opcode)), ("tag", num(*tag)), ("data_len", num(*data_len)), ("direction", text(dir))];
                if let Some(l) = lba {
                    f.push(("lba", num(*l)));
                }
                if let Some(b) = blocks {
                    f.push(("blocks", num(*b)));
                }
                ("scsi_command".into(), f)
            }
            ScsiEvent::Result { opcode, status, transferred, sense_key } => {
                let mut f: Fields = vec![("opcode", num(*opcode)), ("status", text(status.wire())), ("transferred", unum(*transferred))];
                if let Some(k) = sense_key {
                    f.push(("sense_key", num(*k)));
                }
                ("scsi_result".into(), f)
            }
            ScsiEvent::Reset => ("scsi_reset".into(), vec![]),
        },
        ForensicEvent::Btrfs(b) => match b {
            BtrfsEvent::Mount { generation, root_bytenr } => ("btrfs_mount".into(), vec![("generation", num(*generation)), ("root_bytenr", num(*root_bytenr))]),
            BtrfsEvent::BeginFile { file_len } => ("btrfs_begin_file".into(), vec![("len_bytes", num(*file_len))]),
            BtrfsEvent::WriteChunk { len, bytenr } => ("btrfs_write_chunk".into(), vec![("len", unum(*len)), ("bytenr", num(*bytenr))]),
            BtrfsEvent::FinishFile { ino, total_bytes } => ("btrfs_finish_file".into(), vec![("ino", num(*ino)), ("total_bytes", num(*total_bytes))]),
            BtrfsEvent::AbandonFile { runs_count, total_bytes } => ("btrfs_abandon_file".into(), vec![("runs_count", unum(*runs_count)), ("total_bytes", num(*total_bytes))]),
            BtrfsEvent::CreateFile { parent_ino, new_ino } => ("btrfs_create_file".into(), vec![("parent_ino", num(*parent_ino)), ("new_ino", num(*new_ino))]),
            BtrfsEvent::DeleteFile { parent_ino, ino } => ("btrfs_delete_file".into(), vec![("parent_ino", num(*parent_ino)), ("ino", num(*ino))]),
            BtrfsEvent::Mkdir { parent_ino, new_ino } => ("btrfs_mkdir".into(), vec![("parent_ino", num(*parent_ino)), ("new_ino", num(*new_ino))]),
            BtrfsEvent::Rename { from_parent, to_parent, ino } => ("btrfs_rename".into(), vec![("from_parent", num(*from_parent)), ("to_parent", num(*to_parent)), ("ino", num(*ino))]),
            BtrfsEvent::ChunkAlloc { logical, length, dev_offset } => ("btrfs_chunk_alloc".into(), vec![("logical", num(*logical)), ("length", num(*length)), ("dev_offset", num(*dev_offset))]),
            BtrfsEvent::Commit { tree, generation, transid, nodes_written } => ("btrfs_commit".into(), vec![("tree", num(*tree)), ("generation", num(*generation)), ("transid", num(*transid)), ("nodes_written", unum(*nodes_written))]),
            BtrfsEvent::Error { stage, code } => ("btrfs_error".into(), vec![("stage", text(stage)), ("code", num(*code))]),
            BtrfsEvent::LeafSplit { tree, shape, groups, pos_class } => ("btrfs_leaf_split".into(), vec![("tree", num(*tree)), ("shape", num(*shape)), ("groups", num(*groups)), ("pos_class", num(*pos_class))]),
            BtrfsEvent::InteriorSplit { tree, level } => ("btrfs_interior_split".into(), vec![("tree", num(*tree)), ("level", num(*level))]),
            BtrfsEvent::HeightGrew { tree, from, to } => ("btrfs_height_grew".into(), vec![("tree", num(*tree)), ("from", num(*from)), ("to", num(*to))]),
            BtrfsEvent::RootCollapsed { tree, from, to } => ("btrfs_root_collapsed".into(), vec![("tree", num(*tree)), ("from", num(*from)), ("to", num(*to))]),
            BtrfsEvent::NodeRemoved { tree, level } => ("btrfs_node_removed".into(), vec![("tree", num(*tree)), ("level", num(*level))]),
            BtrfsEvent::BlockReused { tree, level } => ("btrfs_block_reused".into(), vec![("tree", num(*tree)), ("level", num(*level))]),
            BtrfsEvent::BlockCowed { tree, level } => ("btrfs_block_cowed".into(), vec![("tree", num(*tree)), ("level", num(*level))]),
            BtrfsEvent::ConvergeRounds { rounds } => ("btrfs_converge_rounds".into(), vec![("rounds", num(*rounds))]),
        },
        ForensicEvent::Ext4(e) => match e {
            Ext4Event::BeginFile { size } => ("ext4_begin_file".into(), vec![("size", num(*size))]),
            Ext4Event::FinishFile { ino, bytes } => ("ext4_finish_file".into(), vec![("ino", num(*ino)), ("bytes", num(*bytes))]),
            Ext4Event::AbandonFile { bytes } => ("ext4_abandon_file".into(), vec![("bytes", num(*bytes))]),
            Ext4Event::CreateFile { parent, ino } => ("ext4_create_file".into(), vec![("parent", num(*parent)), ("ino", num(*ino))]),
            Ext4Event::DeleteFile { parent, ino } => ("ext4_delete_file".into(), vec![("parent", num(*parent)), ("ino", num(*ino))]),
            Ext4Event::Mkdir { parent, ino } => ("ext4_mkdir".into(), vec![("parent", num(*parent)), ("ino", num(*ino))]),
            Ext4Event::Rename { from_parent, to_parent, ino } => ("ext4_rename".into(), vec![("from_parent", num(*from_parent)), ("to_parent", num(*to_parent)), ("ino", num(*ino))]),
            Ext4Event::MarkDirty => ("ext4_mark_dirty".into(), vec![]),
            Ext4Event::MarkClean => ("ext4_mark_clean".into(), vec![]),
            Ext4Event::Flush { ok } => ("ext4_flush".into(), vec![("ok", Bool(*ok))]),
            Ext4Event::LeftDirty { cleanup_code, resync_ok } => {
                let mut f: Fields = vec![("resync_ok", Bool(*resync_ok))];
                if let Some(c) = cleanup_code {
                    f.push(("cleanup_code", num(*c)));
                }
                ("ext4_left_dirty".into(), f)
            }
            Ext4Event::Error { stage, code } => ("ext4_error".into(), vec![("stage", text(stage)), ("code", num(*code))]),
        },
        // Location only. `message` is deliberately not rendered: it is a
        // literal today, but this is the one function every line passes
        // through, so the rule is enforced here too.
        ForensicEvent::Panic(p) => ("panic".into(), vec![("at", text(&p.file)), ("line", num(p.line))]),
    }
}

/// Render records as `{"dropped":n,"events":[{"nseq":..,"t":..,"ev":"usb_timeout",..}]}`.
/// Each event is a flat object (design F9 C4); `ev` is `<group>_<variant>`.
pub fn events_to_json(records: &[ForensicRecord], dropped: u64) -> String {
    let events: Vec<serde_json::Value> = records
        .iter()
        .map(|r| {
            let (ev, fields) = event_fields(&r.event);
            let mut o = serde_json::Map::new();
            o.insert("nseq".into(), num(r.seq));
            o.insert("t".into(), num(r.timestamp_ms));
            o.insert("ev".into(), serde_json::Value::String(ev));
            for (k, v) in fields {
                o.insert(k.into(), v);
            }
            serde_json::Value::Object(o)
        })
        .collect();
    let mut root = serde_json::Map::new();
    root.insert("dropped".into(), num(dropped));
    root.insert("events".into(), serde_json::Value::Array(events));
    // `Value`'s Display cannot fail (all keys are strings), unlike `to_string`
    // on an arbitrary Serialize, so there is no error to swallow here.
    serde_json::Value::Object(root).to_string()
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// The notable queue and the ring are process-global and lib tests run in
    /// parallel, so tests here serialise on this lock and identify their own
    /// events by marker values rather than by counting everything.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        match TEST_LOCK.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    const MARK: u64 = 0xF9F9_0000_0000;
/// Each test owns a disjoint marker window so earlier tests' events left in
/// the ring cannot be mistaken for its own.
const WINDOW: u64 = 0x1_0000;

    /// One of every event variant. `witness` below makes adding a variant
    /// without extending this list a compile error.
    fn every_variant() -> Vec<ForensicEvent> {
        use ForensicEvent as E;
        vec![
            E::Usb(UsbEvent::Submit { ep: 1, count: 2, bytes: 3, gen: 4 }),
            E::Usb(UsbEvent::Reap { ep: 1, status: -2, bytes: 3, gen: 4 }),
            E::Usb(UsbEvent::Timeout { ep: 1, elapsed_ms: 2, active_urbs: 3, gen: 4 }),
            E::Usb(UsbEvent::StateTransition { from: "Healthy", to: "Dead" }),
            E::Usb(UsbEvent::Reset { kind: "bot_mass_storage_reset", status: 0 }),
            E::Usb(UsbEvent::Drain { discarded_count: 1, remaining: 2 }),
            E::Usb(UsbEvent::DrainAborted { errno: 110 }),
            E::Usb(UsbEvent::UnrecognizedReap { ptr: 0xdead }),
            E::Scsi(ScsiEvent::Command { opcode: 0x2a, tag: 7, data_len: 512, dir: "OUT", lba: Some(9), blocks: Some(1) }),
            E::Scsi(ScsiEvent::Command { opcode: 0x00, tag: 7, data_len: 0, dir: "NONE", lba: None, blocks: None }),
            E::Scsi(ScsiEvent::Result { opcode: 0x2a, status: ScsiStatus::Passed, transferred: 512, sense_key: None }),
            E::Scsi(ScsiEvent::Result { opcode: 0x2a, status: ScsiStatus::Failed, transferred: 0, sense_key: Some(3) }),
            E::Scsi(ScsiEvent::Reset),
            E::Btrfs(BtrfsEvent::Mount { generation: 1, root_bytenr: 2 }),
            E::Btrfs(BtrfsEvent::BeginFile { file_len: 1 }),
            E::Btrfs(BtrfsEvent::WriteChunk { len: 1, bytenr: 2 }),
            E::Btrfs(BtrfsEvent::FinishFile { ino: 1, total_bytes: 2 }),
            E::Btrfs(BtrfsEvent::AbandonFile { runs_count: 1, total_bytes: 2 }),
            E::Btrfs(BtrfsEvent::CreateFile { parent_ino: 1, new_ino: 2 }),
            E::Btrfs(BtrfsEvent::DeleteFile { parent_ino: 1, ino: 2 }),
            E::Btrfs(BtrfsEvent::Mkdir { parent_ino: 1, new_ino: 2 }),
            E::Btrfs(BtrfsEvent::Rename { from_parent: 1, to_parent: 2, ino: 3 }),
            E::Btrfs(BtrfsEvent::ChunkAlloc { logical: 1, length: 2, dev_offset: 3 }),
            E::Btrfs(BtrfsEvent::Commit { tree: 1, generation: 2, transid: 3, nodes_written: 4 }),
            E::Btrfs(BtrfsEvent::Error { stage: "commit", code: 9 }),
            E::Btrfs(BtrfsEvent::LeafSplit { tree: 1, shape: 2, groups: 3, pos_class: 0 }),
            E::Btrfs(BtrfsEvent::InteriorSplit { tree: 1, level: 1 }),
            E::Btrfs(BtrfsEvent::HeightGrew { tree: 1, from: 0, to: 1 }),
            E::Btrfs(BtrfsEvent::RootCollapsed { tree: 1, from: 1, to: 0 }),
            E::Btrfs(BtrfsEvent::NodeRemoved { tree: 1, level: 0 }),
            E::Btrfs(BtrfsEvent::BlockReused { tree: 1, level: 0 }),
            E::Btrfs(BtrfsEvent::BlockCowed { tree: 1, level: 0 }),
            E::Btrfs(BtrfsEvent::ConvergeRounds { rounds: 2 }),
            E::Ext4(Ext4Event::BeginFile { size: 1 }),
            E::Ext4(Ext4Event::FinishFile { ino: 12, bytes: 2 }),
            E::Ext4(Ext4Event::AbandonFile { bytes: 2 }),
            E::Ext4(Ext4Event::CreateFile { parent: 2, ino: 12 }),
            E::Ext4(Ext4Event::DeleteFile { parent: 2, ino: 12 }),
            E::Ext4(Ext4Event::Mkdir { parent: 2, ino: 13 }),
            E::Ext4(Ext4Event::Rename { from_parent: 2, to_parent: 2, ino: 12 }),
            E::Ext4(Ext4Event::MarkDirty),
            E::Ext4(Ext4Event::MarkClean),
            E::Ext4(Ext4Event::Flush { ok: false }),
            E::Ext4(Ext4Event::LeftDirty { cleanup_code: Some(9), resync_ok: false }),
            E::Ext4(Ext4Event::LeftDirty { cleanup_code: None, resync_ok: true }),
            E::Ext4(Ext4Event::Error { stage: "finish_file", code: 5 }),
            E::Panic(PanicEvent {
                file: "src/fs/ext4/file.rs".into(),
                line: 10,
                col: 4,
                message: "cannot open /sdcard/secret-folder/file.txt".into(),
            }),
        ]
    }

    /// Exhaustive over every leaf variant: returns a distinct ordinal. If this
    /// stops compiling, add the new variant to `every_variant` too.
    fn witness(e: &ForensicEvent) -> usize {
        use ForensicEvent as E;
        match e {
            E::Usb(u) => match u {
                UsbEvent::Submit { .. } => 0,
                UsbEvent::Reap { .. } => 1,
                UsbEvent::Timeout { .. } => 2,
                UsbEvent::StateTransition { .. } => 3,
                UsbEvent::Reset { .. } => 4,
                UsbEvent::Drain { .. } => 5,
                UsbEvent::DrainAborted { .. } => 6,
                UsbEvent::UnrecognizedReap { .. } => 7,
            },
            E::Scsi(s) => match s {
                ScsiEvent::Command { .. } => 8,
                ScsiEvent::Result { .. } => 9,
                ScsiEvent::Reset => 10,
            },
            E::Btrfs(b) => match b {
                BtrfsEvent::Mount { .. } => 11,
                BtrfsEvent::BeginFile { .. } => 12,
                BtrfsEvent::WriteChunk { .. } => 13,
                BtrfsEvent::FinishFile { .. } => 14,
                BtrfsEvent::AbandonFile { .. } => 15,
                BtrfsEvent::CreateFile { .. } => 16,
                BtrfsEvent::DeleteFile { .. } => 17,
                BtrfsEvent::Mkdir { .. } => 18,
                BtrfsEvent::Rename { .. } => 19,
                BtrfsEvent::ChunkAlloc { .. } => 20,
                BtrfsEvent::Commit { .. } => 21,
                BtrfsEvent::Error { .. } => 22,
                BtrfsEvent::LeafSplit { .. } => 23,
                BtrfsEvent::InteriorSplit { .. } => 24,
                BtrfsEvent::HeightGrew { .. } => 25,
                BtrfsEvent::RootCollapsed { .. } => 26,
                BtrfsEvent::NodeRemoved { .. } => 27,
                BtrfsEvent::BlockReused { .. } => 28,
                BtrfsEvent::BlockCowed { .. } => 29,
                BtrfsEvent::ConvergeRounds { .. } => 30,
            },
            E::Ext4(x) => match x {
                Ext4Event::BeginFile { .. } => 31,
                Ext4Event::FinishFile { .. } => 32,
                Ext4Event::AbandonFile { .. } => 33,
                Ext4Event::CreateFile { .. } => 34,
                Ext4Event::DeleteFile { .. } => 35,
                Ext4Event::Mkdir { .. } => 36,
                Ext4Event::Rename { .. } => 37,
                Ext4Event::MarkDirty => 38,
                Ext4Event::MarkClean => 39,
                Ext4Event::Flush { .. } => 40,
                Ext4Event::Error { .. } => 41,
                Ext4Event::LeftDirty { .. } => 42,
            },
            E::Panic(_) => 43,
        }
    }

    #[test]
    fn every_variant_list_is_complete() {
        let mut seen: Vec<usize> = every_variant().iter().map(witness).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen, (0..=43).collect::<Vec<_>>(), "every_variant() misses a variant");
    }

    #[test]
    fn classification_hot_events_stay_out_notable_ones_are_in() {
        use ForensicEvent as E;
        // Hot: ring only.
        let hot = [
            E::Usb(UsbEvent::Submit { ep: 1, count: 1, bytes: 1, gen: 1 }),
            E::Usb(UsbEvent::Reap { ep: 1, status: 0, bytes: 1, gen: 1 }),
            E::Scsi(ScsiEvent::Command { opcode: 0x2a, tag: 1, data_len: 1, dir: "out", lba: Some(1), blocks: Some(1) }),
            E::Scsi(ScsiEvent::Result { opcode: 0x2a, status: ScsiStatus::Passed, transferred: 1, sense_key: None }),
            E::Btrfs(BtrfsEvent::WriteChunk { len: 1, bytenr: 1 }),
            E::Btrfs(BtrfsEvent::BeginFile { file_len: 1 }),
            E::Btrfs(BtrfsEvent::LeafSplit { tree: 1, shape: 1, groups: 1, pos_class: 1 }),
            E::Btrfs(BtrfsEvent::InteriorSplit { tree: 1, level: 1 }),
            E::Btrfs(BtrfsEvent::HeightGrew { tree: 1, from: 0, to: 1 }),
            E::Btrfs(BtrfsEvent::RootCollapsed { tree: 1, from: 1, to: 0 }),
            E::Btrfs(BtrfsEvent::NodeRemoved { tree: 1, level: 0 }),
            E::Btrfs(BtrfsEvent::BlockReused { tree: 1, level: 0 }),
            E::Btrfs(BtrfsEvent::BlockCowed { tree: 1, level: 0 }),
            E::Btrfs(BtrfsEvent::ConvergeRounds { rounds: 1 }),
        ];
        for e in &hot {
            assert!(!is_notable(e), "hot event must stay ring-only: {e:?}");
        }
        let notable = [
            E::Usb(UsbEvent::Timeout { ep: 1, elapsed_ms: 1, active_urbs: 1, gen: 1 }),
            E::Usb(UsbEvent::StateTransition { from: "a", to: "b" }),
            E::Usb(UsbEvent::Reset { kind: "bot_mass_storage_reset", status: 0 }),
            E::Usb(UsbEvent::Drain { discarded_count: 1, remaining: 0 }),
            E::Usb(UsbEvent::DrainAborted { errno: 1 }),
            E::Usb(UsbEvent::UnrecognizedReap { ptr: 1 }),
            E::Scsi(ScsiEvent::Result { opcode: 0x2a, status: ScsiStatus::Failed, transferred: 0, sense_key: Some(2) }),
            E::Scsi(ScsiEvent::Result { opcode: 0x2a, status: ScsiStatus::NotReady, transferred: 0, sense_key: None }),
            E::Scsi(ScsiEvent::Reset),
            E::Btrfs(BtrfsEvent::Mount { generation: 1, root_bytenr: 1 }),
            E::Btrfs(BtrfsEvent::FinishFile { ino: 1, total_bytes: 1 }),
            E::Btrfs(BtrfsEvent::AbandonFile { runs_count: 1, total_bytes: 1 }),
            E::Btrfs(BtrfsEvent::CreateFile { parent_ino: 1, new_ino: 2 }),
            E::Btrfs(BtrfsEvent::DeleteFile { parent_ino: 1, ino: 2 }),
            E::Btrfs(BtrfsEvent::Mkdir { parent_ino: 1, new_ino: 2 }),
            E::Btrfs(BtrfsEvent::Rename { from_parent: 1, to_parent: 1, ino: 2 }),
            E::Btrfs(BtrfsEvent::ChunkAlloc { logical: 1, length: 1, dev_offset: 1 }),
            E::Btrfs(BtrfsEvent::Commit { tree: 1, generation: 1, transid: 1, nodes_written: 1 }),
            E::Btrfs(BtrfsEvent::Error { stage: "x", code: 1 }),
            E::Ext4(Ext4Event::BeginFile { size: 1 }),
            E::Ext4(Ext4Event::FinishFile { ino: 1, bytes: 1 }),
            E::Ext4(Ext4Event::AbandonFile { bytes: 1 }),
            E::Ext4(Ext4Event::CreateFile { parent: 1, ino: 2 }),
            E::Ext4(Ext4Event::DeleteFile { parent: 1, ino: 2 }),
            E::Ext4(Ext4Event::Mkdir { parent: 1, ino: 2 }),
            E::Ext4(Ext4Event::Rename { from_parent: 1, to_parent: 1, ino: 2 }),
            E::Ext4(Ext4Event::MarkDirty),
            E::Ext4(Ext4Event::MarkClean),
            E::Ext4(Ext4Event::Flush { ok: true }),
            E::Ext4(Ext4Event::LeftDirty { cleanup_code: None, resync_ok: true }),
            E::Ext4(Ext4Event::Error { stage: "x", code: 1 }),
            E::Panic(PanicEvent { file: "src/a.rs".into(), line: 1, col: 1, message: String::new() }),
        ];
        for e in &notable {
            assert!(is_notable(e), "notable event must reach the queue: {e:?}");
        }
        // Between them the two tables cover every variant.
        let mut covered: Vec<usize> = hot.iter().chain(notable.iter()).map(witness).collect();
        covered.sort_unstable();
        covered.dedup();
        assert_eq!(covered, (0..=43).collect::<Vec<_>>());
    }

    /// Drain until our markers are provably out of the way.
    fn drain_all() -> (Vec<ForensicRecord>, u64) {
        drain_notable()
    }

    fn marked(recs: &[ForensicRecord], is_hot: bool, base: u64) -> Vec<u64> {
        let inside = |g: u64| g >= base && g < base + WINDOW;
        recs.iter()
            .filter_map(|r| match &r.event {
                ForensicEvent::Usb(UsbEvent::Submit { gen, .. }) if is_hot && inside(*gen) => Some(*gen),
                ForensicEvent::Usb(UsbEvent::Timeout { gen, .. }) if !is_hot && inside(*gen) => Some(*gen),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn record_queues_notable_only_and_ring_keeps_both() {
        let _g = lock();
        drain_all();
        record_usb(UsbEvent::Submit { ep: 1, count: 1, bytes: 1, gen: MARK + 1 });
        record_usb(UsbEvent::Timeout { ep: 1, elapsed_ms: 1, active_urbs: 1, gen: MARK + 2 });
        let (recs, _) = drain_all();
        assert_eq!(marked(&recs, true, MARK), Vec::<u64>::new(), "hot event leaked into the queue");
        assert_eq!(marked(&recs, false, MARK), vec![MARK + 2]);
        let ring = snapshot();
        assert_eq!(marked(&ring, true, MARK), vec![MARK + 1], "ring must keep hot events");
        assert_eq!(marked(&ring, false, MARK), vec![MARK + 2], "ring must keep notable events");
    }

    #[test]
    fn drain_empties_the_queue_and_resets_dropped() {
        let _g = lock();
        drain_all();
        record_usb(UsbEvent::Timeout { ep: 1, elapsed_ms: 1, active_urbs: 1, gen: MARK + WINDOW + 10 });
        let (first, _) = drain_all();
        assert_eq!(marked(&first, false, MARK + WINDOW), vec![MARK + WINDOW + 10]);
        let (second, dropped) = drain_all();
        assert_eq!(marked(&second, false, MARK + WINDOW), Vec::<u64>::new(), "drain must remove what it returns");
        assert_eq!(dropped, 0, "dropped counter must reset on drain");
        // Sequence numbers of what was drained are strictly increasing.
        for w in first.windows(2) {
            assert!(w[0].seq < w[1].seq);
        }
    }

    #[test]
    fn overflow_evicts_oldest_keeps_newest_and_counts() {
        let _g = lock();
        drain_all();
        let n = NOTABLE_CAPACITY as u64 + 100;
        for i in 0..n {
            record_usb(UsbEvent::Timeout { ep: 1, elapsed_ms: 1, active_urbs: 1, gen: MARK + 2 * WINDOW + i });
        }
        let (recs, dropped) = drain_all();
        assert_eq!(recs.len(), NOTABLE_CAPACITY, "queue holds exactly its capacity");
        assert!(dropped >= 100, "100 events were evicted, dropped={dropped}");
        let base = MARK + 2 * WINDOW;
        let m = marked(&recs, false, base);
        assert_eq!(*m.last().unwrap(), base + n - 1, "the newest event must survive");
        assert!(!m.contains(&base), "the oldest event must be the one evicted");
        assert!(m.windows(2).all(|w| w[1] == w[0] + 1), "survivors are contiguous and ordered");
    }

    fn leaf_ok(v: &Value) -> bool {
        matches!(v, Value::Number(_) | Value::Bool(_) | Value::String(_))
    }

    fn snake_ok(s: &str) -> bool {
        !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    }

    #[test]
    fn json_every_variant_renders_flat_with_snake_ev() {
        let recs: Vec<ForensicRecord> = every_variant()
            .into_iter()
            .enumerate()
            .map(|(i, event)| ForensicRecord { seq: i as u64 + 1, timestamp_ms: 1_700_000_000_000 + i as u64, event })
            .collect();
        let text = events_to_json(&recs, 7);
        let v: Value = serde_json::from_str(&text).expect("output must be valid JSON");
        assert_eq!(v["dropped"], 7);
        let events = v["events"].as_array().expect("events array");
        assert_eq!(events.len(), recs.len());
        for (rec, obj) in recs.iter().zip(events) {
            let obj = obj.as_object().expect("event is an object");
            assert_eq!(obj["nseq"], rec.seq);
            assert_eq!(obj["t"], rec.timestamp_ms);
            let ev = obj["ev"].as_str().expect("ev is a string");
            assert!(snake_ok(ev), "ev {ev:?} must match ^[a-z0-9_]+$");
            for (k, val) in obj {
                assert!(leaf_ok(val), "field {k} of {ev} is not a number/bool/string: {val}");
            }
        }
        let by_ev = |name: &str| events.iter().find(|o| o["ev"] == name).unwrap_or_else(|| panic!("no {name}"));
        assert_eq!(by_ev("usb_timeout")["ep"], 1);
        assert_eq!(by_ev("usb_state_transition")["from"], "healthy");
        assert_eq!(by_ev("usb_reset")["reset_kind"], "bot_mass_storage_reset");
        assert_eq!(by_ev("scsi_result")["status"], "passed");
        assert_eq!(by_ev("ext4_finish_file")["ino"], 12);
        assert_eq!(by_ev("ext4_error")["stage"], "finish_file");
        assert_eq!(by_ev("ext4_mark_dirty")["ev"], "ext4_mark_dirty");
        assert_eq!(by_ev("btrfs_commit")["transid"], 3);
        assert_eq!(by_ev("scsi_reset")["ev"], "scsi_reset");
        // Optional SCSI fields appear only when present.
        let with_lba: Vec<_> = events.iter().filter(|o| o["ev"] == "scsi_command" && o.get("lba").is_some()).collect();
        assert_eq!(with_lba.len(), 1);
    }

    #[test]
    fn json_panic_carries_location_only() {
        let rec = ForensicRecord {
            seq: 1,
            timestamp_ms: 1,
            event: ForensicEvent::Panic(PanicEvent {
                file: "src/lib.rs".into(),
                line: 10,
                col: 4,
                message: "cannot open /sdcard/secret-folder/file.txt".into(),
            }),
        };
        let text = events_to_json(&[rec], 0);
        assert!(!text.contains("secret"), "panic message leaked: {text}");
        assert!(!text.contains("sdcard"), "panic message leaked: {text}");
        let v: Value = serde_json::from_str(&text).expect("valid JSON");
        let o = &v["events"][0];
        assert_eq!(o["ev"], "panic");
        assert_eq!(o["at"], "src.lib.rs");
        assert_eq!(o["line"], 10);
        assert!(o.get("message").is_none());
    }

    #[test]
    fn poisoned_queue_never_panics_counts_dropped_and_heals_on_drain() {
        let _g = lock();
        drain_all();
        let poisoner = std::thread::spawn(|| {
            let _held = NOTABLE_QUEUE.lock();
            panic!("deliberate: poison the notable queue");
        });
        assert!(poisoner.join().is_err());
        assert!(NOTABLE_QUEUE.lock().is_err(), "control: queue must be poisoned");
        // Must not panic, must not silently vanish.
        record_usb(UsbEvent::Timeout { ep: 1, elapsed_ms: 1, active_urbs: 1, gen: MARK + 3 * WINDOW });
        let (recs, dropped) = drain_all();
        assert_eq!(marked(&recs, false, MARK + 3 * WINDOW), Vec::<u64>::new());
        assert!(dropped >= 1, "the lost record must be counted, dropped={dropped}");
        // Healed: the next event is queued normally.
        record_usb(UsbEvent::Timeout { ep: 1, elapsed_ms: 1, active_urbs: 1, gen: MARK + 3 * WINDOW + 1 });
        let (recs, _) = drain_all();
        assert_eq!(marked(&recs, false, MARK + 3 * WINDOW), vec![MARK + 3 * WINDOW + 1]);
    }

    #[test]
    fn ring_json_snapshot_is_not_cleared() {
        let _g = lock();
        record_usb(UsbEvent::Submit { ep: 2, count: 1, bytes: 1, gen: MARK + 4 * WINDOW });
        let a: Value = serde_json::from_str(&ring_json()).expect("valid JSON");
        let b: Value = serde_json::from_str(&ring_json()).expect("valid JSON");
        for v in [&a, &b] {
            assert!(v["dropped"].is_u64());
            let hit = v["events"].as_array().expect("events").iter().any(|o| o["ev"] == "usb_submit" && o["gen"] == MARK + 4 * WINDOW);
            assert!(hit, "ring json lacks the hot marker event");
        }
    }

    fn every_record() -> Vec<ForensicRecord> {
        every_variant()
            .into_iter()
            .enumerate()
            .map(|(i, event)| ForensicRecord { seq: i as u64 + 1, timestamp_ms: 1_700_000_000_000 + i as u64, event })
            .collect()
    }

    /// C7: every rendered key and string value satisfies the grammar the
    /// Kotlin sanitizer enforces. Checked on rendered output, so a new
    /// variant, a renamed field or a new string literal is covered.
    #[test]
    fn wire_contract_every_key_and_value_is_kotlin_safe() {
        const RESERVED: [&str; 9] = ["t", "seq", "sid", "src", "ev", "kind", "nt", "nseq", "xfer"];
        const ENVELOPE: [&str; 3] = ["nseq", "t", "ev"];
        const BANNED: [&str; 9] = ["name", "path", "file", "filename", "dir", "label", "uuid", "serial", "entry"];
        let wire_value = |s: &str| {
            (1..=48).contains(&s.len())
                && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.:-".contains(c))
        };
        let v: Value = serde_json::from_str(&events_to_json(&every_record(), 0)).expect("valid JSON");
        let events = v["events"].as_array().expect("events");
        assert_eq!(events.len(), every_variant().len());
        for o in events {
            let o = o.as_object().expect("object");
            for (k, val) in o {
                assert!(snake_ok(k), "key {k:?} is not [a-z0-9_]+");
                if !ENVELOPE.contains(&k.as_str()) {
                    assert!(!RESERVED.contains(&k.as_str()), "payload key {k:?} collides with a reserved key in {o:?}");
                }
                for part in k.split('_') {
                    assert!(!BANNED.contains(&part), "key {k:?} contains banned word {part:?}");
                }
                if let Value::String(sv) = val {
                    assert!(wire_value(sv), "value {sv:?} of {k} breaks ^[a-z0-9_.:-]{{1,48}}$ in {o:?}");
                }
            }
        }
    }

    fn golden_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../android/app/src/test/resources/native-events-golden.json")
    }

    /// The cross-language contract: one record of EVERY native variant,
    /// rendered by the real renderer, checked in. The Kotlin NativeDrain test
    /// feeds this exact file through its parser and asserts nothing is
    /// dropped. Rewrite with `UPDATE_GOLDEN=1 cargo test -p luks_core --lib
    /// forensic::tests::golden`.
    #[test]
    fn golden_native_events_file_is_current() {
        let v: Value = serde_json::from_str(&events_to_json(&every_record(), 3)).expect("valid JSON");
        let mut want = serde_json::to_string_pretty(&v).expect("pretty-print");
        want.push('\n');
        let path = golden_path();
        if std::env::var("UPDATE_GOLDEN").is_ok_and(|v| v == "1") {
            let dir = path.parent().expect("golden dir");
            std::fs::create_dir_all(dir).expect("create golden dir");
            std::fs::write(&path, &want).expect("write golden");
            return;
        }
        let have = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("golden {} unreadable ({e}); regenerate with UPDATE_GOLDEN=1", path.display()));
        assert_eq!(have, want, "native-events-golden.json is stale; regenerate with UPDATE_GOLDEN=1");
    }
}
