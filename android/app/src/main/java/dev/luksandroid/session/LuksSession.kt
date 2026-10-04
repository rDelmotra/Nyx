package dev.luksandroid.session

import android.content.Context
import android.hardware.usb.UsbDevice
import dev.luksandroid.Entry
import dev.luksandroid.LuksDevice
import dev.luksandroid.LuksException
import dev.luksandroid.LuksVolume
import dev.luksandroid.PartitionInfo
import dev.luksandroid.SessionClosedException
import dev.luksandroid.Trace
import dev.luksandroid.UnlockService
import dev.luksandroid.security.SecurePassphraseBuffer
import dev.luksandroid.ui.UiErrorMessage
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger

/**
 * State machine for the process-scoped LUKS session.
 */
sealed interface SessionState {
    data object Locked : SessionState
    data class Unlocking(val partition: PartitionInfo) : SessionState
    data class Unlocked(
        val volume: LuksVolume,
        val partition: PartitionInfo,
        val entries: List<Entry> = emptyList(),
    ) : SessionState
    data class Detached(val message: String = "USB drive was disconnected") : SessionState
    data class Failed(val message: String, val partition: PartitionInfo? = null) : SessionState
}

/**
 * Process-scoped, thread-safe, reference-counted owner of the unlocked LUKS volume.
 */
open class SessionController(
    val scope: CoroutineScope = CoroutineScope(Dispatchers.Default + SupervisorJob()),
    private val timeProvider: () -> Long = { System.currentTimeMillis() },
) : WriteFailureSink {
    private val _state = MutableStateFlow<SessionState>(SessionState.Locked)
    val state: StateFlow<SessionState> = _state.asStateFlow()

    private val mutex = Mutex()
    private val _activeLeases = MutableStateFlow(0)
    val activeLeases: Int get() = _activeLeases.value
    val activeLeaseCount: Int get() = _activeLeases.value
    val activeLeasesFlow: StateFlow<Int> = _activeLeases.asStateFlow()

    private val isLocking = AtomicBoolean(false)

    /**
     * Where user-visible notices go (notification shade). The Application installs the
     * Android implementation; the default drops them so tests and a bare controller are silent.
     */
    @Volatile var notifier: SessionNotifier = SessionNotifier.None

    private val _closeWarning = MutableStateFlow<CloseWarning?>(null)

    /**
     * Non-null after a teardown that could not prove everything reported as done is on the
     * drive ("N files reported done may not be on the drive"). Outlives Locked/Detached.
     */
    val closeWarning: StateFlow<CloseWarning?> = _closeWarning.asStateFlow()

    fun clearCloseWarning() {
        _closeWarning.value = null
    }

    private val _isWriteDead = MutableStateFlow(false)

    /** True once a fatal native write error (fenced / poisoned) was seen; cleared by the next unlock. */
    val isWriteDead: StateFlow<Boolean> = _isWriteDead.asStateFlow()

    private val _writeHolds = MutableStateFlow(0)
    val activeWriteHolds: Int get() = _writeHolds.value
    val writeHoldsFlow: StateFlow<Int> = _writeHolds.asStateFlow()

    /**
     * Whether something is using the volume right now: a lease, or an open SAF write that
     * holds no lease between its chunks. Lifecycle and trim locks must not fire while busy.
     */
    open val isBusy: Boolean get() = _activeLeases.value > 0 || _writeHolds.value > 0

    /** Suspends until no lease and no write hold remains. */
    open suspend fun awaitNotBusy() {
        combine(_activeLeases, _writeHolds) { leases, holds -> leases + holds }.first { it == 0 }
    }

    /**
     * Marks a SAF write as open (C9). Unlike a lease it does not delay [lock] (an explicit
     * lock must win) and it does not defer the idle timer: every chunk is a lease that
     * resets the idle clock, so a write that stalls for the whole idle timeout still locks.
     */
    fun acquireWriteHold(): WriteHold {
        _writeHolds.update { it + 1 }
        recordActivity()
        val released = AtomicBoolean(false)
        return object : WriteHold {
            override fun release() {
                if (released.compareAndSet(false, true)) {
                    _writeHolds.update { it - 1 }
                    recordActivity()
                }
            }
        }
    }

    private val unsavedFiles = AtomicInteger(0)

    /** Files reported finished since the last commit known to have succeeded. */
    val unsavedFileCount: Int get() = unsavedFiles.get()

    /** Call when a file was reported finished to its client but is not yet known to be committed. */
    fun noteFilesFinished(count: Int = 1) {
        if (count > 0) unsavedFiles.addAndGet(count)
    }

    /** Call after any commit that succeeded. */
    fun noteBatchCommitted() {
        unsavedFiles.set(0)
        commitNoticeLatched.set(false)
    }

    /**
     * Commits the active batch under a lease (commit policy (c) and (d)).
     *
     * Returns the failure, or null on success. A failed commit keeps the batch natively
     * (it is retryable), so [forFinish] failures are surfaced to the user at once rather than
     * waiting for a teardown that may never get to retry.
     */
    suspend fun commitAndReport(forFinish: Boolean): Throwable? {
        val failure = try {
            commitBatch()
        } catch (c: CancellationException) {
            throw c
        } catch (t: Throwable) {
            t
        }
        if (failure == null) {
            noteBatchCommitted()
            return null
        }
        Trace.e("LuksSession: commit failed: ${Trace.throwableSummary(failure)}")
        // One notice per failure streak: ten finished files behind one dead drive are one
        // problem, not ten notifications. A successful commit re-arms it.
        if (forFinish && commitNoticeLatched.compareAndSet(false, true)) {
            notifier.notify(SessionNotice.SafCommitFailed)
        }
        return failure
    }

    /**
     * The commit itself; null on success. The default commits under this controller's own
     * lease. [LuksSession] overrides it to go through the transfer engine's `commitNow`, the
     * one commit path that is serialised with the queue's writers and records `commitFailure`.
     */
    protected open suspend fun commitBatch(): Throwable? = try {
        withLease { v -> if (v.canWrite) v.commitActiveBatch() }
        null
    } catch (c: CancellationException) {
        throw c
    } catch (t: Throwable) {
        t
    }

    private val commitNoticeLatched = AtomicBoolean(false)

    /** Test seam for item "teardown resets transfer locks off the caller's thread". */
    internal var transferReset: () -> Unit = { TransferManager.resetTransferLocks() }

    override fun reportWriteFailure(t: Throwable): Boolean {
        if (!isFatalWriteFailure(t)) return false
        _isWriteDead.value = true
        val code = (t as LuksException).code
        scope.launch { failFatal(code, awaitClose = true) }
        return true
    }

    var device: AutoCloseable? = null
        private set
    var volume: LuksVolume? = null
        private set
    var activeUsbDevice: UsbDevice? = null
        private set

    internal var volumeCloseable: AutoCloseable? = null
    internal var deviceCloseable: AutoCloseable? = null

    private var idleTimeoutMs: Long = DEFAULT_IDLE_TIMEOUT_MS
    private var idleJob: Job? = null
    private var lastActivityMs: Long = 0L

    private val _remainingIdleMs = MutableStateFlow(DEFAULT_IDLE_TIMEOUT_MS)
    val remainingIdleMs: StateFlow<Long> = _remainingIdleMs.asStateFlow()

    companion object {
        // Idle-lock value is a trade-off against re-unlock cost, not a fixed security
        // number: hardware run 2026-08-18 measured a single unlock (Argon2 KDF) at
        // 78s. At the old 5-minute value, any pause near that length (reading a file
        // list, switching to a file manager and back) forces the user through another
        // 78s KDF just to keep working — that is what "locking in 5 mins what is this"
        // was about. 15 minutes keeps a real idle-security backstop while making
        // relock rare during actual use; it is not configurable (out of scope here),
        // so it has to be a number that is defensible on its own.
        const val DEFAULT_IDLE_TIMEOUT_MS = 15 * 60 * 1000L // 15 minutes

        private const val COMMIT_LOCK_WAIT_MS = 2_000L
    }

    fun setIdleTimeout(timeoutMs: Long) {
        idleTimeoutMs = timeoutMs
        _remainingIdleMs.value = timeoutMs
        if (_state.value is SessionState.Unlocked) {
            restartIdleTimer()
        }
    }

    private fun recordActivity() {
        lastActivityMs = timeProvider()
        _remainingIdleMs.value = idleTimeoutMs
    }

    private fun restartIdleTimer() {
        idleJob?.cancel()
        if (idleTimeoutMs <= 0) return
        recordActivity()
        idleJob = scope.launch {
            while (isActive) {
                if (_activeLeases.value > 0) {
                    recordActivity()
                    delay(100)
                    continue
                }
                val elapsed = timeProvider() - lastActivityMs
                val remaining = idleTimeoutMs - elapsed
                _remainingIdleMs.value = remaining.coerceAtLeast(0L)
                if (remaining <= 0) {
                    if (_activeLeases.value == 0) {
                        Trace.i("LuksSession: idle timeout expired, locking session")
                        lock()
                        break
                    } else {
                        delay(100)
                    }
                } else {
                    delay(minOf(250L, remaining.coerceAtLeast(1L)))
                }
            }
        }
    }

    private fun cancelIdleTimer() {
        idleJob?.cancel()
        idleJob = null
        _remainingIdleMs.value = idleTimeoutMs
    }

    /**
     * Executes [block] with reference-counted access to the active [LuksVolume].
     *
     * Multiple readers can execute concurrently. [lock] and teardown operations
     * wait for all active leases to drain.
     */
    suspend fun <T> withLease(block: suspend (LuksVolume) -> T): T {
        if (isLocking.get()) {
            throw SessionClosedException("Session is currently locking")
        }
        val currentVol = mutex.withLock {
            if (isLocking.get()) {
                throw SessionClosedException("Session is currently locking")
            }
            val s = _state.value
            if (s !is SessionState.Unlocked) {
                // The state's *type*, never the state itself. `Failed` and `Detached`
                // carry a free-text message that may quote a native error, and those
                // routinely name a path inside the encrypted volume ("... at
                // /private/passwords_database.kdbx"). This exception is handed to
                // whichever third-party app made the SAF call, so interpolating "$s"
                // published plaintext filenames from a locked volume to an app that
                // was just refused access to it. Callers that legitimately need the
                // reason read `state` directly; a refusal only needs to say it was
                // refused.
                throw SessionClosedException("Session is not unlocked (state: ${s::class.simpleName})")
            }
            val v = volume ?: s.volume
            _activeLeases.update { it + 1 }
            recordActivity()
            v
        }

        try {
            return block(currentVol)
        } catch (t: Throwable) {
            if (isFatalWriteFailure(t)) {
                // Failed is published now; the close is left to run in the background. This
                // caller may itself own the transfer lock (a SAF write), which the close's
                // reset waits for, so awaiting it here would wait on ourselves.
                failFatal((t as LuksException).code, awaitClose = false)
            }
            throw t
        } finally {
            _activeLeases.update { it - 1 }
            recordActivity()
        }
    }

    /**
     * Unlocks [partition] on [device] using [password] within [UnlockService.holding].
     */
    suspend fun unlock(
        context: Context,
        device: LuksDevice,
        partition: PartitionInfo,
        password: SecurePassphraseBuffer,
    ): SessionState = mutex.withLock {
        Trace.i("LuksSession: unlocking partition at offset ${partition.offsetBytes}")
        retirePriorSession(incoming = device)
        _state.value = SessionState.Unlocking(partition)
        this.activeUsbDevice = device.usbDevice
        val started = timeProvider()
        var producedVolume: LuksVolume? = null
        try {
            val vol = UnlockService.holding(context) {
                withContext(Dispatchers.IO) {
                    val v = device.unlock(partition.offsetBytes, password)
                    producedVolume = v
                    v
                }
            }
            val kdfMs = timeProvider() - started
            val info = vol.info
            Trace.i(
                "LuksSession: unlocked in $kdfMs ms · fs=${info.fsType} " +
                    "block=${info.blockSize} size=${info.sizeBytes} " +
                    "subvolumes=${info.subvolumes.size}"
            )
            val entries = withContext(Dispatchers.IO) { vol.listDir("/") }
            Trace.i("LuksSession: root listed, ${entries.size} entries")

            this.device = device
            this.volume = vol
            this.volumeCloseable = vol
            this.deviceCloseable = device
            this.activeUsbDevice = device.usbDevice
            onSessionAdopted()

            val unlocked = SessionState.Unlocked(vol, partition, entries)
            _state.value = unlocked
            restartIdleTimer()
            unlocked
        } catch (e: CancellationException) {
            this.activeUsbDevice = null
            if (this.volume !== producedVolume) {
                closeProduced(producedVolume)
            }
            if (_state.value is SessionState.Unlocking) {
                _state.value = SessionState.Locked
            }
            throw e
        } catch (e: LuksException) {
            this.activeUsbDevice = null
            closeProduced(producedVolume)
            Trace.err(e.code, "unlock")
            Trace.e("LuksSession: unlock failed: ${Trace.throwableSummary(e)}")
            // "wrong passphrase" is matched by the devices screen; everything else is code-only.
            val msg = if (e.isWrongPassword) "wrong passphrase" else codeOnlyMessage(e.code)
            val failed = SessionState.Failed(msg, partition)
            _state.value = failed
            failed
        } catch (e: Exception) {
            this.activeUsbDevice = null
            closeProduced(producedVolume)
            Trace.err(-1, "unlock")
            Trace.e("LuksSession: unlock failed: ${Trace.throwableSummary(e)}")
            val failed = SessionState.Failed(unexpectedFailureMessage(), partition)
            _state.value = failed
            failed
        }
    }

    /**
     * Unlocks the first LUKS partition found on [device] using [password].
     */
    suspend fun unlock(
        context: Context,
        device: LuksDevice,
        password: SecurePassphraseBuffer,
    ): SessionState {
        val partition = device.luksPartitions.firstOrNull()
            ?: throw IllegalStateException("No LUKS partition found on device")
        return unlock(context, device, partition, password)
    }

    /**
     * Opens an unencrypted (plain) partition volume on [device] and mounts the filesystem.
     * Mount is instantaneous (~2ms) on Dispatchers.IO; no UnlockService foreground lock required.
     */
    suspend fun openPlain(
        context: Context,
        device: LuksDevice,
        partition: PartitionInfo,
    ): SessionState = mutex.withLock {
        Trace.i("LuksSession: opening plain partition at offset ${partition.offsetBytes}")
        retirePriorSession(incoming = device)
        _state.value = SessionState.Unlocking(partition)
        this.activeUsbDevice = device.usbDevice
        val started = timeProvider()
        var producedVolume: LuksVolume? = null
        try {
            val vol = withContext(Dispatchers.IO) {
                val v = device.openPlain(partition.offsetBytes)
                producedVolume = v
                v
            }
            val mountMs = timeProvider() - started
            val info = vol.info
            Trace.i(
                "LuksSession: plain volume opened in $mountMs ms · fs=${info.fsType} " +
                    "block=${info.blockSize} size=${info.sizeBytes} " +
                    "subvolumes=${info.subvolumes.size}"
            )
            val entries = withContext(Dispatchers.IO) { vol.listDir("/") }
            Trace.i("LuksSession: root listed, ${entries.size} entries")

            this.device = device
            this.volume = vol
            this.volumeCloseable = vol
            this.deviceCloseable = device
            this.activeUsbDevice = device.usbDevice
            onSessionAdopted()

            val unlocked = SessionState.Unlocked(vol, partition, entries)
            _state.value = unlocked
            restartIdleTimer()
            unlocked
        } catch (e: CancellationException) {
            this.activeUsbDevice = null
            if (this.volume !== producedVolume) {
                closeProduced(producedVolume)
            }
            if (_state.value is SessionState.Unlocking) {
                _state.value = SessionState.Locked
            }
            throw e
        } catch (e: LuksException) {
            this.activeUsbDevice = null
            closeProduced(producedVolume)
            Trace.err(e.code, "open_plain")
            Trace.e("LuksSession: openPlain failed: ${Trace.throwableSummary(e)}")
            val failed = SessionState.Failed(codeOnlyMessage(e.code), partition)
            _state.value = failed
            failed
        } catch (e: Exception) {
            this.activeUsbDevice = null
            closeProduced(producedVolume)
            Trace.err(-1, "open_plain")
            Trace.e("LuksSession: openPlain failed: ${Trace.throwableSummary(e)}")
            val failed = SessionState.Failed(unexpectedFailureMessage(), partition)
            _state.value = failed
            failed
        }
    }

    /**
     * Opens the first plain partition found on [device].
     */
    suspend fun openPlain(
        context: Context,
        device: LuksDevice,
    ): SessionState {
        val partition = device.info.partitions.firstOrNull { it.isPlain }
            ?: throw IllegalStateException("No plain partition found on device")
        return openPlain(context, device, partition)
    }

    /**
     * Waits for all active leases to drain, then commits and tears down the volume and device handles.
     */
    suspend fun lock(): Unit {
        if (_state.value is SessionState.Locked && !isLocking.get()) {
            return
        }
        var startState: SessionState? = null
        val shouldProceed = mutex.withLock {
            if (_state.value is SessionState.Locked || isLocking.get()) {
                false
            } else {
                isLocking.set(true)
                startState = _state.value
                true
            }
        }
        if (!shouldProceed) return

        try {
            Trace.i("LuksSession: locking session, waiting for leases to drain")
            if (_activeLeases.value > 0) {
                withTimeoutOrNull(5000L) {
                    _activeLeases.first { it == 0 }
                } ?: Trace.i("LuksSession: active leases did not drain within 5s, proceeding with teardown")
            }
            withContext(kotlinx.coroutines.NonCancellable) {
                mutex.withLock {
                    // A detach or fatal error during the lease wait already moved the state and
                    // took the handles; its banner must not be overwritten with Locked.
                    if (_state.value !== startState) return@withLock
                    // The drive is alive on this path, so the commit is quick and the state
                    // flips after it: Locked must mean the key is gone and the batch is out.
                    closeHandles(takeHandles(), commit = true)
                    _state.value = SessionState.Locked
                }
            }
        } finally {
            isLocking.set(false)
        }
    }

    /**
     * Handles USB device detachment. Marks the session [SessionState.Detached] first, under
     * the mutex, then commits and closes **outside** it (M2): after a cable pull the commit
     * and close can each cost a full USB timeout, and nothing else may queue behind that
     * while the UI still claims the drive is unlocked. A detach with nothing open is ignored (C7).
     */
    suspend fun onDeviceDetached(message: String = "USB drive was disconnected") {
        val handles = mutex.withLock {
            if (_state.value is SessionState.Locked) {
                Trace.i("LuksSession: detach ignored, nothing is open")
                return
            }
            Trace.i("LuksSession: device detached")
            isLocking.set(false)
            val h = takeHandles()
            _state.value = SessionState.Detached(message)
            h
        }
        closeOutsideMutex(handles, commit = true)
    }

    fun notifyDeviceDetached(message: String = "USB drive was disconnected") {
        scope.launch {
            onDeviceDetached(message)
        }
    }

    /**
     * Moves session into [SessionState.Failed] because a previous write left the
     * volume lock poisoned ([LuksException.MUTEX_POISONED] or [LuksException.PANIC]):
     * a write panicked while holding it, so no further operation on this volume
     * can be trusted. No commit is attempted: the volume is not trustworthy.
     *
     * Takes the error *code*, never its text: native messages can name paths inside the
     * volume, and this state is shown in the UI and the log is written to disk.
     */
    suspend fun onWritePoison(code: Int = LuksException.MUTEX_POISONED) = failFatal(code, awaitClose = true)

    /**
     * Moves session into [SessionState.Failed] because a transport failure left
     * the drive's on-disk state unknown ([LuksException.WRITE_SESSION_FENCED]).
     * Unlike [onWritePoison], nothing panicked and the volume lock is intact --
     * so the message must not say "panic" or "poison". It names the actual
     * remedy (unlock the volume again) instead of implying the app is broken.
     * No commit is attempted: the native side already discarded the batch.
     */
    suspend fun onWriteSessionFenced(code: Int = LuksException.WRITE_SESSION_FENCED) = failFatal(code, awaitClose = true)

    /**
     * Fatal write failure: under the mutex only mark Failed and take the handles (fast); the
     * reset of transfer locks (up to ~2 s) and the close run outside it, like a detach, and a
     * re-unlock waits for them via [pendingClose]. When [awaitClose] is false the close is
     * launched on [scope] instead of awaited.
     */
    private suspend fun failFatal(code: Int, awaitClose: Boolean) {
        val fenced = code == LuksException.WRITE_SESSION_FENCED
        val (handles, done) = mutex.withLock {
            _isWriteDead.value = true
            Trace.err(code, if (fenced) "write_session_fenced" else "write_poison")
            Trace.e("LuksSession: fatal write failure [code=$code], session failed")
            isLocking.set(false)
            val h = takeHandles()
            val d = CompletableDeferred<Unit>().also { pendingClose = it }
            _state.value = SessionState.Failed(
                if (fenced) {
                    "The drive stopped responding during a write, so its contents are uncertain. " +
                        "Unlock the volume again to continue."
                } else {
                    "A write failed inside the app, so the volume was closed. " +
                        "Unlock the volume again to continue."
                } + " (code $code)",
            )
            h to d
        }
        if (awaitClose) {
            closeOutsideMutex(handles, commit = false, done = done)
        } else {
            scope.launch { closeOutsideMutex(handles, commit = false, done = done) }
        }
    }

    /**
     * Resets any detached or failed session state back to [SessionState.Locked].
     */
    suspend fun reset() = mutex.withLock {
        Trace.i("LuksSession: reset")
        isLocking.set(false)
        closeHandles(takeHandles(), commit = true)
        _isWriteDead.value = false
        _state.value = SessionState.Locked
    }

    /**
     * Called directly by [dev.luksandroid.MainActivity]'s own `onTrimMemory` override, in
     * addition to [LuksSessionLifecycle] (registered at the Application level via
     * `registerComponentCallbacks`) receiving the same system events — both paths land here
     * or in [LuksSessionLifecycle], so the policy has to agree in both places.
     *
     * Previously this locked on any `level >= TRIM_MEMORY_RUNNING_CRITICAL (15)`, which
     * silently swept in `TRIM_MEMORY_UI_HIDDEN (20)`, `BACKGROUND (40)`, `MODERATE (60)` and
     * `COMPLETE (80)` — i.e. plain backgrounding, not memory pressure. The logcat evidence
     * for the "backgrounding destroys the session" bug (`LuksSession: onTrimMemory level=20`
     * immediately followed by `LuksSession: locking session...`) is this method's own
     * `Trace.i` format, not [LuksSessionLifecycle]'s — this was the actual live trigger, not
     * just [LuksSessionLifecycle]. Only [android.content.ComponentCallbacks2.TRIM_MEMORY_COMPLETE]
     * and [android.content.ComponentCallbacks2.TRIM_MEMORY_RUNNING_CRITICAL] mean the process is
     * genuinely about to be killed; those are the only levels allowed to lock here.
     */
    @Suppress("DEPRECATION")
    fun onTrimMemory(level: Int) {
        Trace.i("LuksSession: onTrimMemory level=$level")
        val criticalLevel = level == android.content.ComponentCallbacks2.TRIM_MEMORY_COMPLETE ||
            level == android.content.ComponentCallbacks2.TRIM_MEMORY_RUNNING_CRITICAL
        if (criticalLevel && !isBusy) {
            scope.launch { lock() }
        }
    }

    /**
     * Helper for tests to initialize the session in Unlocked state.
     */
    suspend fun startUnlockedForTest(
        volume: LuksVolume? = null,
        device: AutoCloseable? = null,
        volumeCloseable: AutoCloseable? = volume,
        deviceCloseable: AutoCloseable? = device,
        partition: PartitionInfo = PartitionInfo(0, "test", 0L, 1024L * 1024L, true, 2),
        entries: List<Entry> = emptyList(),
    ): SessionState.Unlocked = mutex.withLock {
        this.volume = volume
        this.device = device
        this.volumeCloseable = volumeCloseable
        this.deviceCloseable = deviceCloseable
        this.activeUsbDevice = (device as? LuksDevice)?.usbDevice
        val dummyVol = volume ?: LuksVolume(0L)
        val s = SessionState.Unlocked(dummyVol, partition, entries)
        _state.value = s
        restartIdleTimer()
        s
    }

    /** Everything a teardown has to close, detached from the controller's fields. */
    private class Handles(
        val volume: LuksVolume?,
        val volumeCloseable: AutoCloseable?,
        var deviceCloseable: AutoCloseable?,
    )

    /**
     * Detaches the current handles from the controller. Fast and non-suspending: the caller
     * holds [mutex] and decides whether the slow [closeHandles] runs under it.
     */
    private fun takeHandles(): Handles {
        cancelIdleTimer()
        val h = Handles(
            volume = volume,
            volumeCloseable = volumeCloseable ?: volume,
            deviceCloseable = deviceCloseable ?: device,
        )
        volume = null
        device = null
        volumeCloseable = null
        deviceCloseable = null
        activeUsbDevice = null
        return h
    }

    /** Runs [closeHandles] without [mutex], publishing itself so a re-unlock can wait for it. */
    private suspend fun closeOutsideMutex(
        handles: Handles,
        commit: Boolean,
        done: CompletableDeferred<Unit> = CompletableDeferred<Unit>().also { pendingClose = it },
    ) {
        try {
            withContext(kotlinx.coroutines.NonCancellable) { closeHandles(handles, commit) }
        } finally {
            done.complete(Unit)
            if (pendingClose === done) pendingClose = null
        }
    }

    @Volatile private var pendingClose: CompletableDeferred<Unit>? = null

    /**
     * Commit (when [commit]), then close the volume, then the device. Never throws.
     *
     * A failed explicit commit alone is not reported: it can be a transient error or a
     * refusal because a writer is still open, and the volume's native close abandons writers
     * and commits once more. The user is told only when that final attempt fails, because
     * only then are files reported done known not to be on the drive (M3).
     */
    private suspend fun closeHandles(h: Handles, commit: Boolean) = withContext(Dispatchers.IO) {
        // Blocks up to the engine's bounded wait (~2 s). Safe here: this whole function runs on
        // Dispatchers.IO, never on the caller's (possibly main) thread.
        transferReset()

        var commitFailure: Throwable? = null
        val vol = h.volume
        // An open SAF writer makes the native commit refuse; close() abandons it first.
        if (commit && vol != null && activeWriteHolds == 0 && volumeCanWrite(vol)) {
            // The commit must not interleave with another job's writer, so it runs holding the
            // transfer lock. If the lock cannot be had (a stuck holder), skip: the volume's
            // native close abandons writers and commits once more.
            if (TransferManager.tryAcquireForSafWrite(COMMIT_LOCK_WAIT_MS)) {
                try {
                    vol.commitActiveBatch()
                } catch (t: Throwable) {
                    commitFailure = t
                    Trace.e("LuksSession: commit before close failed: ${Trace.throwableSummary(t)}")
                } finally {
                    TransferManager.releaseSafWriteLock()
                }
            } else {
                Trace.e("LuksSession: transfer lock busy, leaving the commit to the volume close")
            }
        }

        // 1. Volume must close FIRST
        var closeFailure: Throwable? = null
        try {
            h.volumeCloseable?.close()
        } catch (t: Throwable) {
            closeFailure = t
            Trace.e("LuksSession: error closing volume during teardown: ${Trace.throwableSummary(t)}")
        }

        // 2. Device must close SECOND
        try {
            h.deviceCloseable?.close()
        } catch (t: Throwable) {
            Trace.e("LuksSession: error closing device during teardown: ${Trace.throwableSummary(t)}")
        }

        if (closeFailure != null && closeFailure !is UnsatisfiedLinkError) {
            val unsaved = unsavedFiles.get()
            val code = ((closeFailure as? LuksException) ?: (commitFailure as? LuksException)
                ?: TransferManager.commitFailure.value)?.code
            Trace.e("LuksSession: final close failed, $unsaved file(s) may not be on the drive")
            _closeWarning.value = CloseWarning(unsaved, code)
            notifier.notify(SessionNotice.FilesMayNotBeSaved(unsaved))
        } else if (commit && h.volumeCloseable != null) {
            // The close's own commit succeeded, so everything reported done is on the drive.
            noteBatchCommitted()
        }
    }

    private fun volumeCanWrite(v: LuksVolume): Boolean =
        try { v.canWrite } catch (_: LinkageError) { false }

    /**
     * Before a new unlock: wait for a detach's background close, then retire whatever the
     * previous session still holds (C8). [incoming] is never closed, since a caller may reuse
     * one [LuksDevice] across unlocks.
     */
    private suspend fun retirePriorSession(incoming: LuksDevice) {
        pendingClose?.await()
        if (volumeCloseable != null || deviceCloseable != null || volume != null || device != null) {
            Trace.i("LuksSession: re-unlock, closing the previous session first")
            val h = takeHandles()
            if (h.deviceCloseable === incoming) h.deviceCloseable = null
            closeHandles(h, commit = true)
        }
    }

    private fun onSessionAdopted() {
        _closeWarning.value = null
        _isWriteDead.value = false
        unsavedFiles.set(0)
    }

    /**
     * State text for a native failure: the mapped user message plus the code. Never the
     * native message, which can name paths inside the volume (this state is shown in the UI).
     */
    private fun codeOnlyMessage(code: Int) = "${UiErrorMessage.getBaseMessage(code)} (code $code)"

    private fun unexpectedFailureMessage() = "Unlocking failed unexpectedly. Reconnect the drive and try again."

    /** Closes a volume an unlock produced but the session never adopted. */
    private fun closeProduced(v: LuksVolume?) {
        if (v == null) return
        try {
            v.close()
        } catch (t: Throwable) {
            Trace.e("LuksSession: error closing an unadopted volume: ${Trace.throwableSummary(t)}")
        }
    }

    /**
     * Whether [t] means no further write on this volume can be trusted, so the
     * session must be torn down rather than letting the caller retry.
     *
     * Matches on the error *code*, never on the message text. An earlier
     * version substring-matched "poison", "panic" and "corrupt" against
     * `t.message`, which tore the whole session down on a read that returned
     * `CorruptFs` — code [LuksException.CORRUPT], from
     * `Cursor::retreat_leaf`, raised while merely *listing* a btrfs volume
     * through SAF. Browsing the drive in a file manager therefore killed the
     * session and reported it as "Write poison", which was false twice over:
     * nothing had been written, and nothing was poisoned. A corrupt structure
     * found during a read is a failure of that one operation. Do not widen
     * this check to any other code, and never back to message text.
     *
     * Three codes genuinely invalidate the volume, and all three arrive as
     * codes rather than prose: [LuksException.MUTEX_POISONED] and
     * [LuksException.PANIC] mean a previous write panicked while holding the
     * volume lock; [LuksException.WRITE_SESSION_FENCED] means a previous write
     * failed for an unrelated reason (a transport failure, say) that left the
     * on-disk state unknown, so the write session was fenced even though
     * nothing panicked. "Poison" used to be the whole category; it is now one
     * of three causes of the same fatal outcome.
     */
    private fun isFatalWriteFailure(t: Throwable): Boolean {
        val e = t as? LuksException ?: return false
        return e.code == LuksException.MUTEX_POISONED ||
            e.code == LuksException.PANIC ||
            e.code == LuksException.WRITE_SESSION_FENCED
    }
}

/** Default singleton instance used across the application. */
object LuksSession : SessionController() {
    /** Busy also while the engine has queued work or a SAF write holds its lock, which no lease covers between jobs. */
    override val isBusy: Boolean get() = super.isBusy || TransferManager.busy.value

    override suspend fun awaitNotBusy() {
        while (isBusy) {
            super.awaitNotBusy()
            TransferManager.busy.first { !it }
        }
    }

    /** One commit path (F5 integration): the engine's, so a commit never interleaves with a job's write. */
    override suspend fun commitBatch(): Throwable? =
        if (TransferManager.commitNow()) {
            null
        } else {
            TransferManager.commitFailure.value
                ?: IllegalStateException("commit refused: no unlocked session, or the volume is write-dead")
        }
}
