package dev.luksandroid

/**
 * The raw JNI surface. Nothing outside this file should touch it — use
 * [LuksDevice] and [LuksVolume], which own the handles and close them.
 *
 * Implemented in `jni/src/lib.rs`. Every function there is wrapped in
 * `catch_unwind`, so a malformed drive surfaces as a [LuksException] rather
 * than killing the process.
 *
 * Handles are opaque `Long`s. Passing the wrong one is caught on the Rust side
 * (every handle is the same tagged type), but it is still a bug: the wrappers
 * exist so it cannot happen by accident.
 *
 * Structured results come back as JSON strings rather than as constructed Java
 * objects. That keeps the JNI surface to twelve functions with no class lookups
 * or field IDs, and it costs nothing that matters: the payloads are metadata,
 * measured in kilobytes. File *contents* never go through JSON.
 */
internal object LuksNative {

    init {
        // Loads libluks_jni.so from the APK's lib/arm64-v8a/. Built by
        // tools/build-android-libs.sh, not by Gradle.
        try {
            System.loadLibrary("luks_jni")
        } catch (_: UnsatisfiedLinkError) {
            // Expected in host JVM unit test environment
        }
    }

    /** Version of the Rust core. The cheapest possible proof the .so loaded. */
    external fun nativeVersion(): String

    /** Dumps the in-memory bounded forensic ring buffer as formatted text. */
    external fun nativeDumpForensicLog(): String

    /** Clears the in-memory bounded forensic ring buffer. */
    external fun nativeClearForensicLog()

    /**
     * @param fd from `UsbDeviceConnection.getFileDescriptor()`. Stays owned by
     *   Java; Rust never closes it, and it must outlive the returned handle.
     * @param maxTransfer bytes per bulk transfer, or 0 for the safe default of
     *   16 KiB (the historical usbfs limit).
     */
    external fun nativeOpenDevice(
        fd: Int,
        epIn: Int,
        epOut: Int,
        iface: Int,
        maxTransfer: Int,
    ): Long

    external fun nativeCloseDevice(handle: Long)

    external fun nativeCloseVolume(handle: Long)

    /** JSON: vendor, product, block size/count, partition table. */
    external fun nativeDeviceInfo(handle: Long): String

    /**
     * Derives the master key and mounts the filesystem. Seconds of CPU and up
     * to a gigabyte of allocation — never on the main thread, and only with
     * [UnlockService] running.
     *
     * @param password direct [java.nio.ByteBuffer] containing the passphrase bytes.
     * @param length length of the passphrase in bytes.
     */
    external fun nativeUnlock(handle: Long, partitionOffset: Long, password: java.nio.ByteBuffer, length: Int): Long

    /** Opens an unencrypted (plain) partition volume and mounts the filesystem. */
    external fun nativeOpenPlain(handle: Long, partitionOffset: Long): Long

    /**
     * Records the user's explicit consent to write to an unencrypted volume.
     *
     * Until this is called, every write on a plain volume is refused with
     * [LuksException.READ_ONLY_VOLUME]. Call it only from a deliberate user
     * confirmation, never automatically on mount.
     */
    external fun nativeArmPlainWrites(handle: Long)

    /** JSON: filesystem label, UUID, block size. */
    external fun nativeVolumeInfo(handle: Long): String

    /** JSON: `{ path, entries: [{ name, inode, type }] }`, "." and ".." removed. */
    external fun nativeListDir(handle: Long, path: String): String

    /** JSON: `{ path, entries: [{ name, inode, type }] }` with pagination. */
    external fun nativeListDirPaged(handle: Long, path: String, offset: Long, limit: Long): String

    /** Creates a cancel token for interrupting long-running native operations. */
    external fun nativeCreateCancelToken(): Long

    /** Signals cancellation on the operation associated with [tokenId]. */
    external fun nativeCancelOperation(tokenId: Long)

    /** Frees the native cancel token handle. */
    external fun nativeCloseCancelToken(tokenId: Long)

    /** JSON: size, mode, uid, gid, links, type, times. */
    external fun nativeFileInfo(handle: Long, path: String): String

    /** Whole file. Throws rather than exceeding [maxBytes], which must fit the heap. */
    external fun nativeReadFile(handle: Long, path: String, maxBytes: Long): ByteArray

    /**
     * Resolves [path] once and returns a read handle that pins the file by identity (inode
     * number plus generation). Close it with [nativeCloseFile].
     */
    external fun nativeOpenFile(handle: Long, path: String): Long

    /**
     * Up to [len] bytes at [offset] of an open file, read by inode and never by path. A short
     * result means end of file. Throws [LuksException.FILE_CHANGED] if the file was deleted,
     * replaced or modified since [nativeOpenFile]; it never returns another file's bytes.
     */
    external fun nativeReadAt(volume: Long, file: Long, offset: Long, len: Int): ByteArray

    /**
     * Size of an open file, from the handle: identity-checked, no second path lookup. Throws
     * [LuksException.FILE_CHANGED] if the file is no longer the one that was opened.
     */
    external fun nativeFileSize(volume: Long, file: Long): Long

    /** `[verified, unverified]` bytes [nativeReadAt] has returned on [file] so far. */
    external fun nativeFileCoverage(file: Long): LongArray

    /** Releases an open-file handle. Idempotent; safe from a `finally`. */
    external fun nativeCloseFile(file: Long)

    /**
     * Streams the file through SHA-256 without materialising it, returning JSON
     * with the digest, byte count and elapsed time. The only way to check a
     * multi-gigabyte file from a phone.
     *
     * @param chunkBytes 0 for the default (1 MiB).
     */
    external fun nativeSha256(handle: Long, path: String, chunkBytes: Int): String

    /**
     * [nativeSha256] that polls [cancelToken] (from [nativeCreateCancelToken]) between chunks and
     * throws [LuksException.CANCELLED] once it is signalled. A token <= 0 means "none".
     * Matches `nativeSha256WithCancel` in `jni/src/lib.rs`.
     */
    external fun nativeSha256WithCancel(handle: Long, path: String, chunkBytes: Int, cancelToken: Long): String

    /**
     * Reads the whole file at [path] through the native pipeline into a sink and returns the byte
     * count, polling [cancelToken] between chunks (<= 0 means none). Nothing is returned to the
     * JVM, so this is a read-throughput probe, not a way to obtain the bytes.
     * Matches `nativeReadFileStream` in `jni/src/lib.rs`.
     */
    external fun nativeReadFileStream(handle: Long, path: String, chunkBytes: Int, cancelToken: Long): Long

    /**
     * Times a raw block read — no decryption, no filesystem. JSON result also
     * reports the transfer size the kernel actually allowed, which is the only
     * way to tell a self-tuned ceiling from a silent fallback.
     */
    external fun nativeBenchmarkRead(handle: Long, bytes: Long, chunkBytes: Int): String

    /**
     * Times a raw block write — no encryption, no filesystem, no ByteArray.
     * Writes past the end of every partition, never into the backup GPT.
     *
     * Only present in a write-enabled build; throws UnsatisfiedLinkError
     * otherwise, which is why the caller checks [LuksDevice.canWrite] first.
     */
    external fun nativeBenchmarkWrite(handle: Long, bytes: Long, chunkBytes: Int): String

    /**
     * Measures AES-XTS and SHA-256 throughput in memory. No USB, no drive —
     * these are the CPU-side ceilings the read pipeline can never exceed.
     */
    external fun nativeSelfTest(mib: Int): String

    // ------------------------------------------------------------ write path

    /**
     * Whether this `.so` was built with the write path linked in.
     *
     * Present in **every** build and answers honestly in each — ask this
     * rather than assuming the `.so` matches the Gradle build type. It is
     * built by `tools/build-android-libs.sh`, a separate tool that can be run
     * with different flags than the APK around it, so the two can disagree.
     *
     * Call this before [nativeWriteFile]. A `.so` without the write path does
     * not export that symbol at all — calling it there is not a caught
     * [LuksException], it is an `UnsatisfiedLinkError`.
     */
    external fun nativeWriteSupported(): Boolean

    /**
     * Creates a file in the volume's root directory holding [data], returning
     * its inode number.
     *
     * **Only linkable when the loaded `.so` was built with
     * `dangerous-write-support`** (`tools/build-android-libs.sh --debug
     * --write`). Every call site must check [nativeWriteSupported] first — see
     * that doc for what happens if it does not.
     *
     * Slow and blocking: it allocates every block, writes them, and flushes
     * all the way through the USB bridge before returning. Never on the main
     * thread.
     */
    external fun nativeWriteFile(handle: Long, parentPath: String, name: String, data: ByteArray): Long

    external fun nativeBeginFile(handle: Long, sizeBytes: Long): Long

    /**
     * Sibling of [nativeBeginFile] with no upfront size, for a streaming write whose
     * total length is not known before the first chunk arrives -- the shape the SAF
     * write proxy needs, since the kernel VFS delivers writes without ever declaring a
     * length. Same gating as every other write primitive: only linkable when the
     * loaded `.so` was built with `dangerous-write-support`. Call [nativeWriteSupported]
     * first.
     */
    external fun nativeBeginFileStreaming(handle: Long): Long
    external fun nativeWriteChunk(handle: Long, writer: Long, data: java.nio.ByteBuffer, len: Int)

    /**
     * [nativeWriteChunk] that refuses with [LuksException.CANCELLED] when [cancelToken] is already
     * signalled (<= 0 means none). Same write-support gating. Matches `nativeWriteChunkWithCancel`
     * in `jni/src/lib.rs`.
     */
    external fun nativeWriteChunkWithCancel(handle: Long, writer: Long, data: java.nio.ByteBuffer, len: Int, cancelToken: Long)
    external fun nativeFinishFile(handle: Long, writer: Long, parentPath: String, name: String): Long
    external fun nativeCommitActiveBatch(handle: Long)
    external fun nativeCloseWriter(handle: Long, writer: Long)
    external fun nativeDeleteFile(handle: Long, path: String)
    external fun nativeCreateDirectory(handle: Long, parentPath: String, name: String): Long
    external fun nativeRename(handle: Long, oldParent: String, oldName: String, newParent: String, newName: String)
    external fun nativeStatFs(handle: Long): String
}
