use thiserror::Error;

#[derive(Debug, Error)]
pub enum LuksError {
    #[error("not a LUKS partition: bad magic {found:02x?}")]
    BadMagic { found: [u8; 6] },

    #[error("LUKS version {0} is not supported (expected 1 or 2)")]
    UnsupportedVersion(u16),

    #[error("LUKS1 headers are not implemented yet")]
    Luks1NotImplemented,

    #[error("header truncated: needed {needed} bytes, got {got}")]
    Truncated { needed: usize, got: usize },

    #[error("declared header size {0} is implausible")]
    ImplausibleHeaderSize(u64),

    #[error("header checksum mismatch (corrupt header, or we parsed it wrong)")]
    ChecksumMismatch,

    #[error("unsupported header checksum algorithm: {0}")]
    UnsupportedChecksumAlg(String),

    #[error("JSON metadata area is not valid UTF-8")]
    JsonNotUtf8,

    #[error("JSON metadata parse failed: {0}")]
    JsonParse(#[from] serde_json::Error),

    #[error("header declares no {0}")]
    Missing(&'static str),

    #[error("key derivation failed: {0}")]
    KdfFailed(String),

    #[error("unsupported hash: {0}")]
    UnsupportedHash(String),

    #[error("unsupported cipher: {0}")]
    UnsupportedCipher(String),

    #[error("unsupported digest type: {0}")]
    UnsupportedDigest(String),

    #[error("unsupported key length {0} (expected 32 or 64 bytes for AES-XTS)")]
    BadKeyLength(usize),

    /// No keyslot accepted the password. Deliberately indistinguishable from
    /// "this slot did not match" so nothing is leaked about which slot exists.
    #[error("wrong password")]
    WrongPassword,

    #[error("unsupported sector size {0} (expected 512, 1024, 2048 or 4096)")]
    BadSectorSize(u32),

    #[error("read past end of volume")]
    OutOfBounds,

    // --- filesystem layer ---
    #[error("not an ext4 filesystem: magic 0x{0:04x}, expected 0xef53")]
    NotExt4(u16),

    #[error("not a btrfs filesystem: magic {0:02x?}, expected _BHRfS_M")]
    NotBtrfs([u8; 8]),

    /// Nothing recognisable on an otherwise readable volume. Distinct from a
    /// wrong password, which produces plausible-looking noise — by the time
    /// this is reached the master key has already been verified against the
    /// keyslot digest, so the volume really is decrypted and really is
    /// something we cannot read.
    #[error("unrecognised filesystem: this reader understands ext4 and btrfs")]
    UnknownFs,

    /// A known filesystem format (such as FAT32, exFAT, NTFS, or XFS) was recognized
    /// by signature, but this reader cannot mount it.
    #[error("unsupported filesystem '{0}' is recognized but cannot be mounted")]
    UnsupportedFs(&'static str),

    /// Two filesystem signatures on one volume, usually a reformat that did not
    /// wipe the old one. Refused rather than resolved: one of the two answers
    /// is a filesystem that no longer exists, and there is no way to tell from
    /// the signatures alone which.
    #[error("this volume carries both ext4 and btrfs signatures — refusing to guess which is real")]
    AmbiguousFs,

    /// A metadata block did not match its stored checksum. btrfs checksums all
    /// of its metadata, so this is a genuine corruption signal rather than the
    /// advisory it would be on ext4.
    #[error("{0} failed its checksum — the drive is corrupt, or we parsed it wrong")]
    FsChecksumMismatch(&'static str),

    #[error("filesystem uses unsupported feature: {0}")]
    UnsupportedFsFeature(String),

    /// The journal has unreplayed transactions, so the on-disk state is stale.
    /// Reading it would show a filesystem that never existed.
    #[error("filesystem was not cleanly unmounted and needs journal recovery")]
    FsNeedsRecovery,

    #[error("invalid extent header (magic 0x{0:04x}, expected 0xf30a)")]
    BadExtentHeader(u16),

    #[error("extent tree is malformed: {0}")]
    BadExtentTree(&'static str),

    #[error("inode {0} is out of range")]
    BadInode(u64),

    #[error("no such file or directory: {0}")]
    NotFound(String),

    #[error("not a directory: {0}")]
    NotADirectory(String),

    #[error("is a directory: {0}")]
    IsADirectory(String),

    /// The low-level single-transaction delete refuses a directory that still
    /// has children — deleting only its own metadata while entries under it
    /// remain would orphan them (their storage never freed, their names
    /// unreachable). The recursive delete empties a directory before this is
    /// ever reached; surfacing here means recursion stopped partway — at a
    /// child type it doesn't delete yet, or a concurrent change — not that
    /// this call itself did anything wrong.
    #[error("directory is not empty: {0}")]
    DirectoryNotEmpty(String),

    #[error("too many levels of symbolic links resolving {0}")]
    SymlinkLoop(String),

    #[error("corrupt filesystem structure: {0}")]
    CorruptFs(&'static str),

    /// A name already taken in the directory being written to.
    ///
    /// Distinct from [`LuksError::CorruptFs`] on purpose: a caller creating a
    /// file whose name is already there has done something ordinary and
    /// recoverable — pick another name — and telling the user their filesystem
    /// is corrupt would be both alarming and false.
    ///
    /// The name is carried for a caller that wants it but deliberately kept
    /// **out of the message**. This text crosses into a Java exception and is
    /// a strong candidate for a log; filenames on an encrypted volume are
    /// among the things that volume exists to conceal, and the caller that
    /// chose the name already knows it.
    #[error("a file with that name already exists here")]
    AlreadyExists(String),

    /// Another unlocked volume on the same device already holds the write
    /// claim. Only one may allocate at a time: each unlock caches its own copy
    /// of the superblock counters and group descriptors, and two of those
    /// allocating against one disk produce overlapping files.
    ///
    /// Its own variant rather than a reused `UnsupportedFsFeature` because the
    /// caller's remedy is specific and actionable — close the other volume —
    /// and is nothing like the remedy for "this volume is btrfs", which the
    /// two used to be indistinguishable from at the JNI boundary.
    #[error("another unlocked volume on this device is already the writer — close it first")]
    WriterBusy,

    /// No group had a free block the allocator could reach. Note that on a
    /// filesystem with uninitialised groups this can mean "full" long before
    /// the drive is — see the note in `fs::ext4::alloc`.
    #[error("no space left on the filesystem")]
    FilesystemFull,

    /// A single btrfs item is larger than any tree node could ever hold, so no
    /// amount of splitting or free space can place it.
    ///
    /// Its own variant because sharing [`LuksError::FilesystemFull`] cost a
    /// day. On 2026-08-16 a 20 MB phone→stick write reported *"no space left
    /// on the filesystem"* on a drive with 676 MiB free: the real cause was
    /// one `EXTENT_CSUM` item covering a whole file and overflowing a 16 KiB
    /// leaf. The investigation went at the drive, the flashing and the free
    /// space, none of which were involved. `RULES.md` already required that an
    /// error name its own operation; one error meaning two unrelated things is
    /// the same defect wearing a different mask, so the two are now separate
    /// and this one says outright that space is not the problem.
    #[error(
        "a {item_bytes}-byte {what} does not fit in a {node_size}-byte btrfs tree node \
         — this is a node-capacity limit, not a full disk"
    )]
    BtrfsItemTooLarge {
        /// What was being written, so the message names its own operation
        /// rather than leaving the reader to supply one from context.
        what: &'static str,
        item_bytes: usize,
        node_size: u32,
    },

    #[error("no free inodes left on the filesystem")]
    NoFreeInodes,

    /// The device behind a write target's path is not the one the caller said
    /// it was. Almost always means the path now points at a different drive
    /// than when it was checked — see `FileDevice::open_writable`.
    #[error(
        "refusing to write to {path}: it is not the {expected}-byte device it \
         was confirmed as ({detail}) — the path points at something other than \
         what was checked"
    )]
    WrongWriteTarget {
        path: String,
        expected: u64,
        detail: &'static str,
    },

    /// The size of a write target could not be determined, so the caller's
    /// claim about it could not be checked. Refused rather than assumed.
    #[error("refusing to write to {path}: could not determine its size to confirm it")]
    UnverifiableWriteTarget { path: String },

    // --- USB / SCSI transport ---
    #[error("SCSI protocol error: {0}")]
    ScsiProtocol(&'static str),

    /// The device returned CSW status 1, carrying the sense data it gave when
    /// asked why. `None` means the REQUEST SENSE that follows a failure itself
    /// failed, which is worth distinguishing from a drive that stayed silent.
    ///
    /// This used to be a bare unit variant whose doc said "call REQUEST SENSE
    /// for the reason" — and nothing ever did. A real `WRITE(10)` refusal on
    /// hardware therefore reached the UI as the text "SCSI command failed",
    /// which is equally consistent with a write-protected drive, a CDB the
    /// drive would not accept, and failing media.
    #[error(
        "SCSI {} failed{}",
        crate::usb::scsi::opcode_name(*opcode),
        sense.map(|s| format!(": {s}")).unwrap_or_default()
    )]
    ScsiCommandFailed {
        /// The CDB's opcode byte. Carried because inferring which command
        /// failed from context got it wrong once already: a refused
        /// `SYNCHRONIZE CACHE` was read as a refused `WRITE(10)`, which sent
        /// the investigation at the CDB encoder instead of at the flush.
        opcode: u8,
        sense: Option<crate::usb::scsi::Sense>,
    },

    #[error("USB transfer failed: {0}")]
    UsbTransfer(String),

    /// A transfer or control request the kernel failed with `errno`.
    ///
    /// The errno is data, not prose: disconnect classification
    /// ([`LuksError::is_disconnected`]) used to be a substring match on the
    /// message, which depends on the C library's `strerror` wording and breaks
    /// the moment a message is reworded. `errno` is the Linux/Android value,
    /// the only kernel usbfs exists on. `reset_failed` records that the BOT
    /// reset that followed this error also failed, and `reset_gone` that the
    /// reset's own error said the device is gone (see `is_disconnected`).
    #[error("USB transfer failed: {detail}")]
    UsbErrno {
        errno: i32,
        reset_failed: bool,
        reset_gone: bool,
        detail: String,
    },

    // --- partition tables ---
    #[error("no MBR or GPT partition table found")]
    NoPartitionTable,

    #[error("GPT is invalid: {0}")]
    BadGpt(&'static str),

    // --- host I/O ---
    /// Reading a file or block device on the host failed. Only produced by
    /// `FileDevice`; the Android path never touches the host filesystem.
    #[error("I/O error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    // --- session lifecycle / cancellation ---
    /// A previous write operation panicked while holding the volume lock.
    /// Further writes are refused to prevent corrupting the on-disk state.
    #[error("write mutex poisoned: a previous operation panicked")]
    SessionPoisoned,

    /// The operation was cancelled via cancellation token.
    #[error("operation cancelled")]
    Cancelled,

    /// Every later write on this volume is refused because an earlier write
    /// failed in a way that leaves its on-device outcome unknown.
    ///
    /// This is the same policy the kernel's own btrfs takes on a failed
    /// transaction (`btrfs_abort_transaction`): stop, force read-only, and
    /// make the operator remount rather than continue writing on top of a
    /// filesystem whose last transaction may or may not have landed.
    ///
    /// It exists because the alternative was tried and produced the
    /// 2026-09-01 field corruption: a transport timeout mid-import was
    /// swallowed, the batch stayed live, and a later unrelated commit
    /// published a `BLOCK_GROUP_ITEM.used` the extent tree could not account
    /// for. Reads are deliberately still allowed — the point is to stop
    /// writing, not to strand the user's data behind an error screen.
    #[error("write session fenced after {0}; unlock the volume again to resume writing")]
    WriteSessionFenced(String),

    #[error("cannot write to read-only plain volume")]
    ReadOnlyVolume,

    /// A file that was open for reading is no longer the file it was opened
    /// as: deleted, replaced by another file that reused its inode number, or
    /// modified in place. The read is refused rather than answered with
    /// whatever now lives there (INV-4: never another file's bytes).
    ///
    /// Carries no path: the caller holds a handle, and error text reaches logs.
    #[error("the file changed or was replaced while it was open")]
    FileChanged,
}

impl LuksError {
    /// Whether this failure leaves the drive's state unknown, and so must end
    /// the write session rather than merely fail one operation.
    ///
    /// The distinction that matters is **who** failed. A transport error means
    /// the command may or may not have reached the medium, so nothing after it
    /// can be trusted to build on it. A refusal we generated ourselves —
    /// `FilesystemFull`, `NotFound`, `AlreadyExists`, an unsupported feature —
    /// means the write provably did not happen, the filesystem is exactly as
    /// it was, and the session is fine.
    ///
    /// [`Cancelled`](Self::Cancelled) is explicitly *not* fencing. A user who
    /// stops a transfer, or a source file that fails to read on the phone,
    /// says nothing about the destination drive; the files already completed
    /// are valid and committing them is correct.
    pub fn fences_write_session(&self) -> bool {
        matches!(
            self,
            // Transport: the command's outcome on the medium is unknown.
            Self::ScsiProtocol(_)
                | Self::ScsiCommandFailed { .. }
                | Self::UsbTransfer(_)
                | Self::UsbErrno { .. }
                | Self::Io { .. }
                // A panic already happened under the volume lock.
                | Self::SessionPoisoned
                // Already fenced; stay fenced.
                | Self::WriteSessionFenced(_)
        )
    }

    /// Stable numeric class for forensic events. The numbers are the JNI
    /// wire codes (`luks_jni::bridge::code`), so a native `ext4_error` line
    /// and the Kotlin exception it caused carry the same `code`. The match is
    /// exhaustive on purpose: a new variant must choose a number. A jni test
    /// pins this against `error_code`.
    pub fn forensic_code(&self) -> i32 {
        use LuksError::*;
        match self {
            KdfFailed(_) => 1,
            WrongPassword => 2,
            BadMagic { .. } | NoPartitionTable => 3,
            UnsupportedVersion(_)
            | Luks1NotImplemented
            | UnsupportedChecksumAlg(_)
            | UnsupportedHash(_)
            | UnsupportedCipher(_)
            | UnsupportedDigest(_)
            | BadKeyLength(_)
            | BadSectorSize(_)
            | UnsupportedFsFeature(_)
            | NotExt4(_)
            | NotBtrfs(_)
            | UnknownFs
            | UnsupportedFs(_)
            | AmbiguousFs => 4,
            ChecksumMismatch
            | Truncated { .. }
            | ImplausibleHeaderSize(_)
            | JsonNotUtf8
            | JsonParse(_)
            | Missing(_)
            | BadExtentHeader(_)
            | BadExtentTree(_)
            | CorruptFs(_)
            | FsChecksumMismatch(_)
            | SymlinkLoop(_)
            | BadGpt(_)
            | OutOfBounds => 5,
            NotFound(_) | NotADirectory(_) | IsADirectory(_) | BadInode(_) => 6,
            ScsiProtocol(_) | ScsiCommandFailed { .. } | UsbTransfer(_) | UsbErrno { .. } => 7,
            FsNeedsRecovery => 8,
            Io { .. } => 9,
            FilesystemFull | NoFreeInodes => 12,
            WrongWriteTarget { .. } => 13,
            UnverifiableWriteTarget { .. } => 14,
            AlreadyExists(_) => 15,
            WriterBusy => 16,
            BtrfsItemTooLarge { .. } => 17,
            SessionPoisoned => 18,
            Cancelled => 19,
            DirectoryNotEmpty(_) => 20,
            WriteSessionFenced(_) => 21,
            ReadOnlyVolume => 22,
            FileChanged => 23,
        }
    }

    /// A kernel transfer failure that kept its errno. See [`LuksError::UsbErrno`].
    pub fn usb_errno(errno: i32, detail: impl Into<String>) -> Self {
        Self::UsbErrno {
            errno,
            reset_failed: false,
            reset_gone: false,
            detail: detail.into(),
        }
    }

    /// Record that the Bulk-Only reset issued after this error failed with
    /// `reset_err`. The original error stays the primary; a
    /// [`UsbErrno`](Self::UsbErrno) carries the facts, so a timeout whose reset
    /// then fails with ENODEV is still reported as the timeout, and still
    /// classified as a disconnect. A variant that cannot carry them is
    /// returned unchanged unless the reset error shows the device is gone, in
    /// which case that error is returned instead (the more useful fact).
    pub fn note_reset_failed(self, reset_err: LuksError) -> Self {
        match self {
            Self::UsbErrno { errno, detail, .. } => Self::UsbErrno {
                errno,
                reset_failed: true,
                reset_gone: reset_err.is_disconnected(),
                detail,
            },
            _ if reset_err.is_disconnected() => reset_err,
            other => other,
        }
    }

    /// Returns true if this error means the physical USB device is gone, or its
    /// usbfs file descriptor was closed. Decided from the errno, never from
    /// message text.
    ///
    /// Linux errno values, each with the reason it counts:
    /// * `ENODEV` (19): usbfs fails every ioctl with it once the device is no
    ///   longer connected, and it is the URB status for a removed device.
    /// * `ESHUTDOWN` (108): URB completion status when the endpoint or host
    ///   controller was shut down underneath the transfer, i.e. an unplug.
    /// * `ENOENT` (2): as an *ioctl* result, the endpoint or interface is no
    ///   longer in the active configuration (usbfs endpoint lookup). The other
    ///   meaning of URB status ENOENT is our own DISCARDURB; usbfs filters
    ///   those out before they become an error, so one reaching here is not it.
    /// * `EBADF` (9): the fd was closed (Java's `UsbDeviceConnection.close()`
    ///   after an unplug).
    /// * `EPROTO` (71): **not** a disconnect on its own; it is also what a bad
    ///   cable or a glitching bridge produces and the next command can work.
    ///   It counts only when the Bulk-Only reset that followed it failed too
    ///   (`reset_failed`): the device then answers neither data nor control.
    ///
    /// Any error whose BOT reset failed with one of the disconnect errnos
    /// above (`reset_gone`) is also a disconnect.
    ///
    /// `ETIMEDOUT`, `EPIPE`, `EINVAL`, `EIO` and the rest are deliberately not
    /// disconnects: the device may be alive and the caller should reset, not
    /// give up.
    pub fn is_disconnected(&self) -> bool {
        const ENOENT: i32 = 2;
        const EBADF: i32 = 9;
        const ENODEV: i32 = 19;
        const EPROTO: i32 = 71;
        const ESHUTDOWN: i32 = 108;
        match self {
            Self::UsbErrno {
                errno,
                reset_failed,
                reset_gone,
                ..
            } => {
                matches!(*errno, ENOENT | EBADF | ENODEV | ESHUTDOWN)
                    || *reset_gone
                    || (*errno == EPROTO && *reset_failed)
            }
            Self::Io { source, .. } => {
                // File I/O, where ENOENT means a missing path, so it is not
                // in this list.
                matches!(source.raw_os_error(), Some(ENODEV | EBADF | ESHUTDOWN))
            }
            _ => false,
        }
    }
}

pub type Result<T> = std::result::Result<T, LuksError>;

#[cfg(test)]
mod fence_classification {
    use super::LuksError;

    /// Every fencing variant, named individually.
    ///
    /// A `matches!` arm is easy to widen by accident, and widening it turns an
    /// ordinary refusal into "unlock the drive again" for the user. The list is
    /// spelled out so adding a variant to the classifier without deciding about
    /// it here fails a test rather than silently changing behaviour.
    #[test]
    fn transport_and_panic_failures_fence_the_write_session() {
        let fencing: Vec<LuksError> = vec![
            LuksError::ScsiProtocol("no CSW"),
            LuksError::ScsiCommandFailed { opcode: 0x2A, sense: None },
            LuksError::UsbTransfer("timed out".into()),
            LuksError::usb_errno(110, "timed out"),
            LuksError::usb_errno(19, "gone"),
            LuksError::Io {
                path: "/dev/sda".into(),
                source: std::io::Error::new(std::io::ErrorKind::TimedOut, "etimedout"),
            },
            LuksError::SessionPoisoned,
            LuksError::WriteSessionFenced("already".into()),
        ];
        assert!(!fencing.is_empty(), "vacuity: the fencing list is empty");
        for e in &fencing {
            assert!(
                e.fences_write_session(),
                "{e:?} leaves the drive's state unknown and must fence the session"
            );
        }
    }

    /// The other half of the claim, and the more important one: a refusal we
    /// generated ourselves proves the write did *not* happen, so the session
    /// is still good. Fencing on these would make an out-of-space drive
    /// require a re-unlock.
    #[test]
    fn self_generated_refusals_do_not_fence_the_write_session() {
        let benign: Vec<LuksError> = vec![
            LuksError::FilesystemFull,
            LuksError::NoFreeInodes,
            LuksError::NotFound("/nope".into()),
            LuksError::AlreadyExists("dup".into()),
            LuksError::WriterBusy,
            LuksError::Cancelled,
            LuksError::OutOfBounds,
            LuksError::ReadOnlyVolume,
            LuksError::UnsupportedFs("test"),
        ];
        assert!(!benign.is_empty(), "vacuity: the benign list is empty");
        for e in &benign {
            assert!(
                !e.fences_write_session(),
                "{e:?} proves the write did not happen; fencing on it would strand the session"
            );
        }
    }

    /// Cancellation is the case Codex specifically called out: a user stopping
    /// a transfer, or a source file failing to read on the phone, says nothing
    /// about the destination drive, and the files already completed are valid.
    #[test]
    fn cancellation_never_fences() {
        assert!(!LuksError::Cancelled.fences_write_session());
    }
}

#[cfg(test)]
mod disconnect_classification {
    use super::LuksError;

    #[test]
    fn errno_decides_and_message_text_is_irrelevant() {
        // Message deliberately says "disconnected" for a live-device errno and
        // nothing useful for a gone one: only the errno may matter.
        for (errno, gone) in [(19, true), (108, true), (2, true), (9, true), (110, false), (32, false), (71, false), (22, false)] {
            let misleading = if gone { "all fine" } else { "device disconnected, No such device, dead" };
            let e = LuksError::usb_errno(errno, misleading);
            assert_eq!(e.is_disconnected(), gone, "errno {errno}");
        }
    }

    #[test]
    fn eproto_is_a_disconnect_only_after_the_reset_also_failed() {
        let e = LuksError::usb_errno(71, "bus error");
        assert!(!e.is_disconnected(), "EPROTO alone is a glitch, not an unplug");
        let reset_timeout = || LuksError::usb_errno(110, "reset timed out");
        assert!(e.note_reset_failed(reset_timeout()).is_disconnected());
        // The flag must not turn other errnos into disconnects.
        assert!(!LuksError::usb_errno(110, "t").note_reset_failed(reset_timeout()).is_disconnected());
        // And it is a no-op on variants that cannot carry it.
        assert!(!LuksError::UsbTransfer("x".into()).note_reset_failed(reset_timeout()).is_disconnected());
    }

    /// R3: a reset that fails because the device is gone makes the failure a
    /// disconnect, while the original error stays the primary one.
    #[test]
    fn a_reset_that_fails_with_a_disconnect_errno_marks_the_error_disconnected() {
        let gone = || LuksError::usb_errno(19, "reset: no such device");
        let timed_out = LuksError::usb_errno(110, "bulk timed out").note_reset_failed(gone());
        assert!(timed_out.is_disconnected());
        assert!(
            matches!(timed_out, LuksError::UsbErrno { errno: 110, .. }),
            "the original error must stay primary: {timed_out:?}"
        );
        // A variant that cannot carry the fact yields the reset error itself.
        let proto = LuksError::ScsiProtocol("no CSW").note_reset_failed(gone());
        assert!(proto.is_disconnected(), "{proto:?}");
    }

    #[test]
    fn a_missing_file_is_not_a_disconnect() {
        let e = LuksError::Io {
            path: "/nope".into(),
            source: std::io::Error::from_raw_os_error(2),
        };
        assert!(!e.is_disconnected(), "Io ENOENT is a missing path");
    }
}
