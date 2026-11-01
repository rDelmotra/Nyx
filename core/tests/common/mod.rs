//! A USB Mass Storage device emulator.
//!
//! Speaks Bulk-Only Transport over an in-memory disk image, so the SCSI layer,
//! partition parsing, LUKS unlock and filesystem read can all be exercised
//! without a phone or a physical drive. Only the Android usbfs ioctl layer sits
//! below this and remains untested until hardware bring-up.
//!
//! It also counts commands and bytes, which lets tests assert that reads are
//! actually being batched rather than issued one block at a time.

#![allow(dead_code)]

pub mod oracle;
pub mod btree_validator;
pub mod mem_device;
pub mod scratch;
pub mod temp_guard;
#[cfg(feature = "dangerous-write-support")]
pub mod fail_after;
#[cfg(feature = "dangerous-write-support")]
pub mod accounting;
#[cfg(feature = "dangerous-write-support")]
pub mod csum_pack;
#[cfg(feature = "dangerous-write-support")]
pub mod states;

use luks_core::error::{LuksError, Result};
use luks_core::usb::bot::{CBW_LEN, CBW_SIGNATURE, CSW_LEN, CSW_SIGNATURE};
use luks_core::usb::BulkTransport;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};

/// The Caching mode page a mock drive reports, minus WCE and PS: every other
/// byte of a MODE SELECT'd page must come back unchanged.
const CACHING_PAGE_TAIL: [u8; 17] = [
    0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0x80, 0x14, 0, 0, 0, 0, 0, 0,
];

/// The mock's write-cache state, shared with the test through
/// [`MockUsbDrive::cache_probe`] so it can be inspected after the drive has
/// been moved into a `ScsiBlockDevice` and after that was dropped.
pub struct CacheProbe {
    /// Current WCE.
    pub wce: AtomicBool,
    /// Whether MODE SENSE(changeable) says WCE may be altered.
    pub changeable: AtomicBool,
    /// Accept a MODE SELECT but leave WCE as it was (a lying bridge).
    pub lie: AtomicBool,
    /// MODE SELECT commands received (CDB seen, accepted or not).
    pub mode_selects: AtomicUsize,
    /// MODE SELECTs that applied WCE=0 / WCE=1.
    pub applied_off: AtomicUsize,
    pub applied_on: AtomicUsize,
    /// Anything wrong with a MODE SELECT: SP=1, PF=0, bad header, page changed
    /// beyond WCE, PS set. A test must assert this is empty (`assert_clean`).
    pub violations: Mutex<Vec<String>>,
    /// Exact parameter lists received, in order.
    pub select_params: Mutex<Vec<Vec<u8>>>,
    /// Exact CDBs received, in order.
    pub select_cdbs: Mutex<Vec<Vec<u8>>>,
    /// Revert WCE to on after this many commands once it was switched off;
    /// negative = never.
    pub revert_after: AtomicI64,
    since_off: AtomicUsize,
}

impl CacheProbe {
    fn new() -> Self {
        Self {
            wce: AtomicBool::new(true),
            changeable: AtomicBool::new(true),
            lie: AtomicBool::new(false),
            mode_selects: AtomicUsize::new(0),
            applied_off: AtomicUsize::new(0),
            applied_on: AtomicUsize::new(0),
            violations: Mutex::new(Vec::new()),
            select_params: Mutex::new(Vec::new()),
            select_cdbs: Mutex::new(Vec::new()),
            revert_after: AtomicI64::new(-1),
            since_off: AtomicUsize::new(0),
        }
    }

    pub fn wce(&self) -> bool {
        self.wce.load(SeqCst)
    }

    pub fn mode_selects(&self) -> usize {
        self.mode_selects.load(SeqCst)
    }

    /// Simulate the drive resetting: volatile settings return to the default.
    pub fn revert(&self) {
        self.wce.store(true, SeqCst);
    }

    pub fn assert_clean(&self) {
        let v = self.violations.lock().unwrap();
        assert!(v.is_empty(), "MODE SELECT violations: {v:?}");
    }

    fn violation(&self, what: impl Into<String>) {
        self.violations.lock().unwrap().push(what.into());
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub commands: usize,
    pub read_commands: usize,
    pub bytes_read: usize,
    pub largest_read_blocks: u32,
    pub write_commands: usize,
    pub bytes_written: usize,
    pub largest_write_blocks: u32,
    pub sync_commands: usize,
}

enum Phase {
    Idle,
    /// Data to hand back, then the CSW.
    DataIn { data: Vec<u8>, pos: usize },
    /// Data the host still owes us, and where it lands when it arrives.
    ///
    /// Bulk-Only Transport has no framing inside the data phase: after the CBW
    /// the host simply writes bytes to the OUT endpoint, and only the byte
    /// count in the CBW says when the phase ends. So the mock has to remember
    /// what the pending command was, exactly as a real bridge does.
    /// `blocks` rides along so the write statistics can be counted when the
    /// data actually lands, not when the command merely promises it.
    DataOut {
        at: usize,
        want: usize,
        got: Vec<u8>,
        blocks: u32,
    },
    Csw { pos: usize },
    /// MODE SELECT parameter list still owed by the host.
    SelectOut { want: usize, got: Vec<u8>, ten: bool },
}

pub struct MockUsbDrive {
    image: RefCell<Vec<u8>>,
    block_size: u32,
    max_transfer: std::cell::Cell<usize>,
    phase: RefCell<Phase>,
    csw: RefCell<[u8; CSW_LEN]>,
    pub stats: RefCell<Stats>,
    /// Report capacity via the 0xFFFFFFFF escape, forcing READ CAPACITY(16).
    pub force_rc16: bool,
    /// Opcodes to answer with CSW status 1 — "that failed" — instead of doing
    /// them. A physical Android device refused every `WRITE(10)` this way while reads kept
    /// working, and nothing here could reproduce that: the mock had no way to
    /// say no.
    pub fail_opcodes: RefCell<Vec<u8>>,
    /// The sense key / ASC / ASCQ REQUEST SENSE reports. Zeroes mean "no
    /// sense", which is what this mock used to answer unconditionally.
    pub sense: [u8; 3],
    /// Set when a refused command still has an OUT data phase owed to it: the
    /// host has already promised those bytes and will send them regardless, so
    /// they are accepted and dropped rather than written. Reporting the
    /// failure without draining the phase leaves the host's payload arriving
    /// where the mock expects the next CBW, which reads as a bad signature —
    /// a mock artefact that looks nothing like the failure being modelled.
    pending_fail: std::cell::Cell<bool>,
    pub vpd_block_limits: RefCell<Option<(u32, u32)>>,
    pub vpd_reject: std::cell::Cell<bool>,
    pub stall_transfers_larger_than: std::cell::Cell<Option<usize>>,
    pub stalled_endpoints: std::cell::Cell<bool>,
    /// What MODE SENSE(6) reports for the Caching page's WCE bit. `None`
    /// (the default) means the command is rejected like any other opcode this
    /// mock does not implement.
    pub write_cache_enabled: std::cell::Cell<Option<bool>>,
    /// Write-cache state shared with the test; see [`CacheProbe`].
    pub cache: Arc<CacheProbe>,
    /// Reject MODE SENSE(6) / MODE SENSE(10) respectively.
    pub no_sense6: std::cell::Cell<bool>,
    pub no_sense10: std::cell::Cell<bool>,
    /// Put an 8-byte block descriptor between the header and the page.
    pub block_descriptor: std::cell::Cell<bool>,
}

impl MockUsbDrive {
    pub fn new(image: Vec<u8>) -> Self {
        Self {
            image: RefCell::new(image),
            block_size: 512,
            max_transfer: std::cell::Cell::new(128 * 1024),
            phase: RefCell::new(Phase::Idle),
            csw: RefCell::new([0u8; CSW_LEN]),
            stats: RefCell::new(Stats::default()),
            force_rc16: false,
            fail_opcodes: RefCell::new(Vec::new()),
            sense: [0, 0, 0],
            pending_fail: std::cell::Cell::new(false),
            vpd_block_limits: RefCell::new(None),
            vpd_reject: std::cell::Cell::new(false),
            stall_transfers_larger_than: std::cell::Cell::new(None),
            stalled_endpoints: std::cell::Cell::new(false),
            write_cache_enabled: std::cell::Cell::new(None),
            cache: Arc::new(CacheProbe::new()),
            no_sense6: std::cell::Cell::new(false),
            no_sense10: std::cell::Cell::new(false),
            block_descriptor: std::cell::Cell::new(false),
        }
    }

    /// Answer MODE SENSE(6) for the Caching page with the given WCE bit.
    pub fn with_write_cache(self, wce: bool) -> Self {
        self.write_cache_enabled.set(Some(wce));
        self.cache.wce.store(wce, SeqCst);
        self
    }

    /// The shared cache state; clone it before moving the drive.
    pub fn cache_probe(&self) -> Arc<CacheProbe> {
        self.cache.clone()
    }

    /// WCE is reported as not changeable.
    pub fn with_wce_not_changeable(self) -> Self {
        self.cache.changeable.store(false, SeqCst);
        self
    }

    /// MODE SELECT is refused (sense: invalid opcode).
    pub fn rejecting_mode_select(self) -> Self {
        self.failing(0x15, [0x5, 0x20, 0x00]).failing(0x55, [0x5, 0x20, 0x00])
    }

    /// MODE SELECT is accepted but WCE stays on.
    pub fn lying_about_mode_select(self) -> Self {
        self.cache.lie.store(true, SeqCst);
        self
    }

    /// WCE goes back on this many commands after it was switched off.
    pub fn reverting_cache_after(self, commands: i64) -> Self {
        self.cache.revert_after.store(commands, SeqCst);
        self
    }

    /// Only the 10-byte MODE SENSE / MODE SELECT exist.
    pub fn with_only_ten_byte_mode_commands(self) -> Self {
        self.no_sense6.set(true);
        self.failing(0x15, [0x5, 0x20, 0x00])
    }

    pub fn with_block_descriptor(self) -> Self {
        self.block_descriptor.set(true);
        self
    }

    /// MODE SENSE response for the Caching page. `pc` is 0 (current) or 1
    /// (changeable).
    fn caching_response(&self, ten: bool, pc: u8) -> Vec<u8> {
        let mut page = vec![0u8; 20];
        page[0] = 0x08 | if pc == 0 { 0x80 } else { 0 }; // PS reported on current values
        page[1] = 18;
        let wce = if pc == 1 { self.cache.changeable.load(SeqCst) } else { self.cache.wce() };
        page[2] = if wce { 0x04 } else { 0x00 };
        page[3..20].copy_from_slice(&CACHING_PAGE_TAIL);
        if pc == 1 {
            page[3..20].fill(0); // nothing else is changeable
        }
        let bd: &[u8] = if self.block_descriptor.get() { &[0, 0, 0, 0, 0, 0, 2, 0] } else { &[] };
        let mut out = if ten { vec![0u8; 8] } else { vec![0u8; 4] };
        if ten {
            out[7] = bd.len() as u8;
        } else {
            out[3] = bd.len() as u8;
        }
        out.extend_from_slice(bd);
        out.extend_from_slice(&page);
        let mdl = out.len() - if ten { 2 } else { 1 };
        if ten {
            out[0..2].copy_from_slice(&(mdl as u16).to_be_bytes());
        } else {
            out[0] = mdl as u8;
        }
        out
    }

    /// Validate and apply a complete MODE SELECT parameter list.
    fn apply_mode_select(&self, ten: bool, params: &[u8]) {
        self.cache.select_params.lock().unwrap().push(params.to_vec());
        let hl = if ten { 8 } else { 4 };
        if params.len() != hl + 20 {
            self.cache.violation(format!("parameter list length {}", params.len()));
            return;
        }
        let (mdl, devspec, bdl) = if ten {
            (u16::from_be_bytes([params[0], params[1]]) as usize, params[3], u16::from_be_bytes([params[6], params[7]]) as usize)
        } else {
            (params[0] as usize, params[2], params[3] as usize)
        };
        if mdl != 0 {
            self.cache.violation("mode data length must be 0 on select");
        }
        if devspec != 0 {
            self.cache.violation("device-specific parameter must be 0 on select");
        }
        if bdl != 0 {
            self.cache.violation("block descriptor length must be 0");
        }
        let page = &params[hl..];
        if page[0] & 0x80 != 0 {
            self.cache.violation("PS bit set in MODE SELECT page");
        }
        if page[0] & 0x7F != 0x08 || page[1] != 18 {
            self.cache.violation("not the caching page, or page length changed");
        }
        if page[2] & !0x04 != 0 || page[3..20] != CACHING_PAGE_TAIL {
            self.cache.violation("page bytes other than WCE differ from what was read");
        }
        if self.cache.lie.load(SeqCst) {
            return;
        }
        if page[2] & 0x04 == 0 {
            self.cache.applied_off.fetch_add(1, SeqCst);
            self.cache.wce.store(false, SeqCst);
        } else {
            self.cache.applied_on.fetch_add(1, SeqCst);
            self.cache.wce.store(true, SeqCst);
        }
    }

    /// Refuse `opcode` with CSW status 1, reporting `sense` when asked why.
    pub fn failing(mut self, opcode: u8, sense: [u8; 3]) -> Self {
        self.fail_opcodes.borrow_mut().push(opcode);
        self.sense = sense;
        self
    }

    /// Refuse everything, REQUEST SENSE included. The point is the recursion
    /// guard: asking why a failure happened must not itself ask why.
    pub fn failing_everything(mut self) -> Self {
        self.fail_opcodes = RefCell::new((0u8..=0xFF).collect());
        self
    }

    pub fn with_block_size(mut self, block_size: u32) -> Self {
        self.block_size = block_size;
        self
    }

    pub fn with_max_transfer(self, max_transfer: usize) -> Self {
        self.max_transfer.set(max_transfer);
        self
    }

    pub fn with_vpd_block_limits(self, max_blocks: u32, opt_blocks: u32) -> Self {
        *self.vpd_block_limits.borrow_mut() = Some((max_blocks, opt_blocks));
        self
    }

    pub fn with_vpd_reject(self) -> Self {
        self.vpd_reject.set(true);
        self
    }

    pub fn with_stall_transfers_larger_than(self, bytes: usize) -> Self {
        self.stall_transfers_larger_than.set(Some(bytes));
        self
    }

    pub fn blocks(&self) -> u64 {
        self.image.borrow().len() as u64 / self.block_size as u64
    }

    fn set_csw(&self, tag: u32, residue: u32, status: u8) {
        let mut csw = [0u8; CSW_LEN];
        csw[0..4].copy_from_slice(&CSW_SIGNATURE.to_le_bytes());
        csw[4..8].copy_from_slice(&tag.to_le_bytes());
        csw[8..12].copy_from_slice(&residue.to_le_bytes());
        csw[12] = status;
        *self.csw.borrow_mut() = csw;
    }

    /// `flags` is the CBW's `bmCBWFlags`: bit 7 set means the host expects an
    /// IN data phase. A real bridge phase-errors when the flag contradicts the
    /// command; a mock that ignored it (as this one did) would pass a WRITE
    /// issued with `Direction::In` — agreeable in exactly the direction that
    /// matters, given this mock is the only evidence the SCSI write path has
    /// until it meets real hardware.
    fn execute(&self, cdb: &[u8], tag: u32, expected_len: u32, flags: u8) -> Result<()> {
        self.stats.borrow_mut().commands += 1;

        // Scripted revert: the drive resets some commands after WCE went off.
        let after = self.cache.revert_after.load(SeqCst);
        if after >= 0 && !self.cache.wce() {
            if self.cache.since_off.fetch_add(1, SeqCst) as i64 >= after {
                self.cache.wce.store(true, SeqCst);
                self.cache.since_off.store(0, SeqCst);
            }
        }
        if matches!(cdb[0], 0x15 | 0x55) {
            self.cache.mode_selects.fetch_add(1, SeqCst);
            self.cache.select_cdbs.lock().unwrap().push(cdb.to_vec());
            if cdb[1] & 0x10 == 0 {
                self.cache.violation("PF=0");
            }
            if cdb[1] & 0x01 != 0 {
                self.cache.violation("SP=1: would permanently alter the drive");
            }
        }

        let host_expects_in = flags & 0x80 != 0;
        let phase_error = |why: &str| {
            let _ = why; // named for the reader; the CSW carries only a status
            self.set_csw(tag, expected_len, 2); // 2 = phase error
            *self.phase.borrow_mut() = Phase::Csw { pos: 0 };
        };

        // Direction check: every command with a data phase declares its
        // direction in the opcode; the CBW flag must agree.
        let wants_in = matches!(cdb[0], 0x12 | 0x03 | 0x25 | 0x9E | 0x28 | 0x88 | 0x1A | 0x5A);
        let wants_out = matches!(cdb[0], 0x2A | 0x8A | 0x15 | 0x55);
        if expected_len > 0 && wants_in && !host_expects_in {
            phase_error("IN command sent with an OUT flag");
            return Ok(());
        }
        if wants_out && host_expects_in {
            phase_error("OUT command sent with an IN flag");
            return Ok(());
        }

        if self.fail_opcodes.borrow().contains(&cdb[0]) {
            self.set_csw(tag, expected_len, 1); // 1 = command failed
            if expected_len > 0 && wants_out {
                // Drain what the host is about to send, then fail. A real
                // bridge may stall the endpoint instead; accepting and
                // discarding is the other legal answer and is the one that
                // keeps this mock's phase machine honest.
                self.pending_fail.set(true);
                *self.phase.borrow_mut() = Phase::DataOut {
                    at: 0,
                    want: expected_len as usize,
                    got: Vec::new(),
                    blocks: 0,
                };
            } else if expected_len > 0 && wants_in {
                // Serve the data phase, then fail. A drive that stalls instead
                // would let the host's recovery path unwind the exchange by
                // itself — which is what this mock used to do by accident, and
                // it made a test of the sense-fetch recursion guard pass with
                // the guard removed. This is the adversarial answer: a clean,
                // well-formed exchange that ends in "failed", so a host that
                // asks why after every failure asks forever.
                *self.phase.borrow_mut() = Phase::DataIn {
                    data: vec![0u8; expected_len as usize],
                    pos: 0,
                };
            } else {
                *self.phase.borrow_mut() = Phase::Csw { pos: 0 };
            }
            return Ok(());
        }

        let data: Vec<u8> = match cdb[0] {
            // INQUIRY
            0x12 => {
                if cdb.len() >= 3 && cdb[1] & 0x01 != 0 && cdb[2] == 0xB0 {
                    if self.vpd_reject.get() {
                        self.set_csw(tag, expected_len, 1);
                        *self.phase.borrow_mut() = Phase::Csw { pos: 0 };
                        return Ok(());
                    }
                    if let Some((max, opt)) = *self.vpd_block_limits.borrow() {
                        let mut d = vec![0u8; 64];
                        d[0] = 0x00; // direct-access block device
                        d[1] = 0xB0; // Block Limits VPD page code
                        d[2..4].copy_from_slice(&60u16.to_be_bytes()); // page length
                        d[8..12].copy_from_slice(&max.to_be_bytes()); // max transfer blocks
                        d[12..16].copy_from_slice(&opt.to_be_bytes()); // optimal transfer blocks
                        d
                    } else {
                        let mut d = vec![0u8; 36];
                        d[0] = 0x00;
                        d[1] = 0x80;
                        d[3] = 0x02;
                        d[4] = 31;
                        d[8..16].copy_from_slice(b"MOCKVEND");
                        d[16..32].copy_from_slice(b"MOCK USB DISK   ");
                        d[32..36].copy_from_slice(b"1.00");
                        d
                    }
                } else {
                    let mut d = vec![0u8; 36];
                    d[0] = 0x00; // direct-access block device
                    d[1] = 0x80; // removable
                    d[3] = 0x02;
                    d[4] = 31;
                    d[8..16].copy_from_slice(b"MOCKVEND");
                    d[16..32].copy_from_slice(b"MOCK USB DISK   ");
                    d[32..36].copy_from_slice(b"1.00");
                    d
                }
            }
            // TEST UNIT READY
            0x00 => Vec::new(),
            // REQUEST SENSE
            0x03 => {
                let mut d = vec![0u8; 18];
                d[0] = 0x70; // current error
                d[2] = self.sense[0]; // sense key, low nibble
                d[7] = 10;
                d[12] = self.sense[1]; // ASC
                d[13] = self.sense[2]; // ASCQ
                d
            }
            // READ CAPACITY(10)
            0x25 => {
                let last = self.blocks() - 1;
                let reported = if self.force_rc16 || last > u32::MAX as u64 {
                    u32::MAX
                } else {
                    last as u32
                };
                let mut d = vec![0u8; 8];
                d[0..4].copy_from_slice(&reported.to_be_bytes());
                d[4..8].copy_from_slice(&self.block_size.to_be_bytes());
                d
            }
            // READ CAPACITY(16)
            0x9E => {
                let mut d = vec![0u8; 32];
                d[0..8].copy_from_slice(&(self.blocks() - 1).to_be_bytes());
                d[8..12].copy_from_slice(&self.block_size.to_be_bytes());
                d
            }
            // READ(10) / READ(16)
            0x28 | 0x88 => {
                let (lba, count) = if cdb[0] == 0x28 {
                    (
                        u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]) as u64,
                        u16::from_be_bytes([cdb[7], cdb[8]]) as u32,
                    )
                } else {
                    (
                        u64::from_be_bytes(cdb[2..10].try_into().unwrap()),
                        u32::from_be_bytes([cdb[10], cdb[11], cdb[12], cdb[13]]),
                    )
                };

                let start = lba * self.block_size as u64;
                let len = count as u64 * self.block_size as u64;
                if start + len > self.image.borrow().len() as u64 {
                    // Out of range: fail the command the way a real drive would.
                    self.set_csw(tag, expected_len, 1);
                    *self.phase.borrow_mut() = Phase::Csw { pos: 0 };
                    return Ok(());
                }

                if let Some(threshold) = self.stall_transfers_larger_than.get() {
                    if len as usize > threshold {
                        self.stalled_endpoints.set(true);
                    }
                }

                let mut s = self.stats.borrow_mut();
                s.read_commands += 1;
                s.bytes_read += len as usize;
                s.largest_read_blocks = s.largest_read_blocks.max(count);
                drop(s);

                self.image.borrow()[start as usize..(start + len) as usize].to_vec()
            }
            // WRITE(10) / WRITE(16). The CDB layout is identical to READ, and
            // it is decoded by the same arithmetic on purpose: if the reader
            // and writer ever disagree about where an LBA lives, the tests
            // should fail rather than agree with the bug.
            0x2A | 0x8A => {
                let (lba, count) = if cdb[0] == 0x2A {
                    (
                        u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]) as u64,
                        u16::from_be_bytes([cdb[7], cdb[8]]) as u32,
                    )
                } else {
                    (
                        u64::from_be_bytes(cdb[2..10].try_into().unwrap()),
                        u32::from_be_bytes([cdb[10], cdb[11], cdb[12], cdb[13]]),
                    )
                };

                let start = lba * self.block_size as u64;
                let len = count as u64 * self.block_size as u64;
                if start + len > self.image.borrow().len() as u64 {
                    self.set_csw(tag, expected_len, 1);
                    *self.phase.borrow_mut() = Phase::Csw { pos: 0 };
                    return Ok(());
                }

                if let Some(threshold) = self.stall_transfers_larger_than.get() {
                    if len as usize > threshold {
                        self.stalled_endpoints.set(true);
                    }
                }
                // The CBW's byte count and the CDB's block count are two
                // statements of the same length; a host that disagrees with
                // itself gets a phase error, as from a real bridge.
                if expected_len as u64 != len {
                    phase_error("CBW length disagrees with the WRITE CDB");
                    return Ok(());
                }

                // Statistics are counted when the data lands (see `write`),
                // not here — a command is not a write until its payload
                // arrives.
                *self.phase.borrow_mut() = Phase::DataOut {
                    at: start as usize,
                    want: len as usize,
                    got: Vec::with_capacity(len as usize),
                    blocks: count,
                };
                // The CSW is prepared now and sent once the data has arrived.
                self.set_csw(tag, 0, 0);
                return Ok(());
            }
            // SYNCHRONIZE CACHE(10): nothing is cached here, but the command
            // must succeed or a flush looks like a device error.
            // Both forms of SYNCHRONIZE CACHE. The 16-byte one is here
            // because a drive that implements only one of the two is exactly
            // the case the fallback exists for, and a mock that answered the
            // 16-byte form from its unknown-opcode arm made that fallback
            // untestable — it looked like a drive with no flush at all.
            0x35 | 0x91 => {
                self.stats.borrow_mut().sync_commands += 1;
                Vec::new()
            }
            // MODE SENSE(6)/(10), only when the test configured a cache state.
            0x1A if self.write_cache_enabled.get().is_some() && !self.no_sense6.get() => {
                self.caching_response(false, cdb[2] >> 6)
            }
            0x5A if self.write_cache_enabled.get().is_some() && !self.no_sense10.get() => {
                self.caching_response(true, cdb[2] >> 6)
            }
            // MODE SELECT(6)/(10): the parameter list arrives in a data-out
            // phase and is validated when complete.
            0x15 | 0x55 if self.write_cache_enabled.get().is_some() => {
                let ten = cdb[0] == 0x55;
                let declared = if ten {
                    u16::from_be_bytes([cdb[7], cdb[8]]) as usize
                } else {
                    cdb[4] as usize
                };
                if declared != expected_len as usize {
                    phase_error("CBW length disagrees with the MODE SELECT CDB");
                    return Ok(());
                }
                if declared == 0 {
                    self.set_csw(tag, 0, 0);
                    *self.phase.borrow_mut() = Phase::Csw { pos: 0 };
                    return Ok(());
                }
                *self.phase.borrow_mut() = Phase::SelectOut { want: declared, got: Vec::new(), ten };
                self.set_csw(tag, 0, 0);
                return Ok(());
            }
            _ => {
                self.set_csw(tag, expected_len, 1);
                *self.phase.borrow_mut() = Phase::Csw { pos: 0 };
                return Ok(());
            }
        };

        let residue = expected_len.saturating_sub(data.len() as u32);
        self.set_csw(tag, residue, 0);
        *self.phase.borrow_mut() = if data.is_empty() {
            Phase::Csw { pos: 0 }
        } else {
            Phase::DataIn { data, pos: 0 }
        };
        Ok(())
    }
}

impl BulkTransport for MockUsbDrive {
    fn write(&self, data: &[u8]) -> Result<usize> {
        // MODE SELECT parameter list.
        {
            let mut phase = self.phase.borrow_mut();
            if let Phase::SelectOut { want, got, ten } = &mut *phase {
                let n = data.len().min(*want - got.len());
                got.extend_from_slice(&data[..n]);
                if got.len() == *want {
                    let (got, ten) = (std::mem::take(got), *ten);
                    *phase = Phase::Csw { pos: 0 };
                    drop(phase);
                    self.apply_mode_select(ten, &got);
                }
                return Ok(n);
            }
        }
        // Mid data-out phase: these bytes are payload, not a new command.
        {
            let mut phase = self.phase.borrow_mut();
            if let Phase::DataOut {
                at,
                want,
                got,
                blocks,
            } = &mut *phase
            {
                if self.stalled_endpoints.get() {
                    return Err(LuksError::UsbErrno {
                        errno: 32,
                        reset_failed: false,
                        reset_gone: false,
                        detail: "USBDEVFS_BULK: errno 32 (Broken pipe) (endpoint stalled)".into(),
                    });
                }
                let n = data.len().min(*want - got.len()).min(self.max_transfer.get());
                got.extend_from_slice(&data[..n]);
                if got.len() == *want {
                    let (at, got, blocks) = (*at, std::mem::take(got), *blocks);
                    *phase = Phase::Csw { pos: 0 };
                    drop(phase);
                    if self.pending_fail.replace(false) {
                        // Refused: the bytes are dropped, the image is
                        // untouched, and the CSW set at command time already
                        // says so. Not counted as a write, because none
                        // happened — a `bytes_written` assertion must not be
                        // satisfiable by a command the drive rejected.
                        return Ok(n);
                    }
                    self.image.borrow_mut()[at..at + got.len()].copy_from_slice(&got);
                    // Counted here, from the payload that actually arrived —
                    // a `bytes_written` assertion should prove a data phase
                    // happened, not that a command once made a promise.
                    let mut st = self.stats.borrow_mut();
                    st.write_commands += 1;
                    st.bytes_written += got.len();
                    st.largest_write_blocks = st.largest_write_blocks.max(blocks);
                }
                return Ok(n);
            }
        }
        if data.len() < CBW_LEN {
            return Err(LuksError::UsbTransfer("short CBW".into()));
        }
        let signature = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        if signature != CBW_SIGNATURE {
            return Err(LuksError::UsbTransfer("bad CBW signature".into()));
        }
        let tag = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
        let expected_len = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
        let flags = data[12];
        let cdb_len = data[14] as usize;
        let cdb = &data[15..15 + cdb_len.min(16)];

        self.execute(cdb, tag, expected_len, flags)?;
        Ok(CBW_LEN)
    }

    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        let mut phase = self.phase.borrow_mut();
        match &mut *phase {
            Phase::Idle => Ok(0),
            // The host is reading while the drive is still waiting for the
            // data it promised to send. A real bridge stalls; returning zero
            // surfaces it as a hang in the test rather than as silent success.
            Phase::DataOut { .. } | Phase::SelectOut { .. } => Ok(0),
            Phase::DataIn { data, pos } => {
                if self.stalled_endpoints.get() {
                    return Err(LuksError::UsbErrno {
                        errno: 32,
                        reset_failed: false,
                        reset_gone: false,
                        detail: "USBDEVFS_BULK: errno 32 (Broken pipe) (endpoint stalled)".into(),
                    });
                }
                // Serve at most max_transfer per call, as a real endpoint would.
                let n = (data.len() - *pos).min(buf.len()).min(self.max_transfer.get());
                buf[..n].copy_from_slice(&data[*pos..*pos + n]);
                *pos += n;
                if *pos == data.len() {
                    *phase = Phase::Csw { pos: 0 };
                }
                Ok(n)
            }
            Phase::Csw { pos } => {
                let csw = *self.csw.borrow();
                let n = (CSW_LEN - *pos).min(buf.len());
                buf[..n].copy_from_slice(&csw[*pos..*pos + n]);
                *pos += n;
                if *pos == CSW_LEN {
                    *phase = Phase::Idle;
                }
                Ok(n)
            }
        }
    }

    fn max_transfer(&self) -> usize {
        self.max_transfer.get()
    }

    fn set_max_transfer(&self, bytes: usize) {
        self.max_transfer.set(bytes);
    }

    fn clear_halt(&self, _endpoint_in: bool) -> Result<()> {
        self.stalled_endpoints.set(false);
        Ok(())
    }

    fn reset(&self) -> Result<()> {
        self.stalled_endpoints.set(false);
        *self.phase.borrow_mut() = Phase::Idle;
        Ok(())
    }
}

pub fn fixture(rel: &str) -> Vec<u8> {
    let path = format!("{}/../fixtures/{rel}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("missing fixture {path}: {e}"))
}

/// One way a bridge can hand back a wrapper that BOT section 6.5 calls
/// invalid or not meaningful, armed on [`TamperTransport`].
#[derive(Clone, Copy, Debug)]
pub enum Tamper {
    /// The next 13-byte CSW read comes back with a corrupted signature.
    BadCswSignature,
    /// ... with a tag that is not the command's.
    CswTagMismatch,
    /// ... with a reserved bCSWStatus value.
    ReservedCswStatus,
    /// The next 31-byte CBW write reports one byte fewer than it was given.
    ShortCbwWrite,
}

/// Wraps a transport, corrupts exactly one wrapper when armed, and counts how
/// many BOT resets the layer above issued. Everything else passes through.
pub struct TamperTransport<T> {
    pub inner: T,
    armed: std::cell::Cell<Option<Tamper>>,
    pub resets: std::cell::Cell<usize>,
}

impl<T> TamperTransport<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            armed: std::cell::Cell::new(None),
            resets: std::cell::Cell::new(0),
        }
    }

    pub fn arm(&self, t: Tamper) {
        self.armed.set(Some(t));
    }
}

impl<T: BulkTransport> BulkTransport for TamperTransport<T> {
    fn write(&self, data: &[u8]) -> Result<usize> {
        if data.len() == CBW_LEN && matches!(self.armed.get(), Some(Tamper::ShortCbwWrite)) {
            // The device never saw a complete CBW, so it starts no command;
            // the inner drive is not told at all.
            self.armed.set(None);
            return Ok(CBW_LEN - 1);
        }
        self.inner.write(data)
    }

    fn read(&self, buf: &mut [u8]) -> Result<usize> {
        let n = self.inner.read(buf)?;
        if buf.len() == CSW_LEN && n == CSW_LEN {
            match self.armed.get() {
                Some(Tamper::BadCswSignature) => buf[0] ^= 0xFF,
                Some(Tamper::CswTagMismatch) => buf[4] ^= 0xFF,
                Some(Tamper::ReservedCswStatus) => buf[12] = 0x7F,
                _ => return Ok(n),
            }
            self.armed.set(None);
        }
        Ok(n)
    }

    fn max_transfer(&self) -> usize {
        self.inner.max_transfer()
    }

    fn set_max_transfer(&self, bytes: usize) {
        self.inner.set_max_transfer(bytes);
    }

    fn clear_halt(&self, endpoint_in: bool) -> Result<()> {
        self.inner.clear_halt(endpoint_in)
    }

    fn reset(&self) -> Result<()> {
        self.resets.set(self.resets.get() + 1);
        self.inner.reset()
    }
}
