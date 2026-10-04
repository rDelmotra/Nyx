package dev.luksandroid.session

import android.content.Context
import android.net.Uri
import dev.luksandroid.LuksException
import dev.luksandroid.LuksNative
import dev.luksandroid.LuksVolume
import dev.luksandroid.Trace
import dev.luksandroid.log.Ev
import dev.luksandroid.log.EventLog
import dev.luksandroid.log.NativeDrain
import dev.luksandroid.transfer.FileDone
import dev.luksandroid.ui.UiErrorMessage
import dev.luksandroid.withNativeCancel
import dev.luksandroid.zeroEntireCapacity
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.isActive
import kotlinx.coroutines.job
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeoutOrNull
import dev.luksandroid.transfer.CollisionMode
import dev.luksandroid.transfer.ContentResolverImportSource
import dev.luksandroid.transfer.ExportTarget
import dev.luksandroid.transfer.ImportSource
import dev.luksandroid.transfer.ReadBack
import dev.luksandroid.transfer.SafExportTarget
import dev.luksandroid.transfer.StatsRecorder
import dev.luksandroid.transfer.TransferCancelledException
import dev.luksandroid.transfer.TransferFailure
import dev.luksandroid.transfer.TransferProgress
import dev.luksandroid.transfer.VerifyLevel
import dev.luksandroid.transfer.formatThroughput
import dev.luksandroid.transfer.stoppedSummary
import dev.luksandroid.transfer.treeProgressLabel
import dev.luksandroid.transfer.verifyReadBack
import java.lang.ref.WeakReference
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.atomic.AtomicLong

enum class TransferType { IMPORT, EXPORT, HASH }

enum class TransferState { QUEUED, RUNNING, COMPLETED, CANCELLED, FAILED }

data class TransferItem(
    val id: Long,
    val name: String,
    val type: TransferType,
    val totalBytes: Long,
    val transferredBytes: Long,
    val speedBytesPerSec: Long,
    val etaSeconds: Long,
    val state: TransferState,
    val cancelToken: Long,
    val error: String?,
    /** Raw [LuksException.code] behind [error], when the failure came from one. Null otherwise. */
    val errorCode: Int? = null,
    /**
     * Tree-transfer progress. Zero and null for a single-file transfer, which
     * has no meaningful file count and no path below itself.
     *
     * A byte total alone is not enough for a directory copy: on a tree of
     * thousands of small files the bar barely moves per file, and "nothing is
     * happening" is precisely the reading that caused the 2026-08-23 incident.
     * [currentPath] is what makes it legible -- it changes constantly even when
     * the percentage does not.
     */
    val filesDone: Int = 0,
    val filesTotal: Int = 0,
    val currentPath: String? = null,
    /** True when [totalBytes] is a lower bound, so the UI must not claim a precise percentage. */
    val totalIsLowerBound: Boolean = false,
    /** Export only: how far the finished file was verified. Null until verification starts. */
    val verifyLevel: VerifyLevel? = null,
    /** Export only: true while the destination is being checked (after the last byte was written). [transferredBytes] then counts verified bytes. */
    val verifying: Boolean = false,
)


/** Creates, signals and frees the native cancel tokens a transfer hands to native calls. Seam for tests. */
internal interface CancelTokens {
    fun create(): Long
    fun signal(token: Long)
    fun close(token: Long)
}

internal object NativeCancelTokens : CancelTokens {
    override fun create(): Long = try {
        LuksNative.nativeCreateCancelToken()
    } catch (_: Throwable) {
        0L
    }

    override fun signal(token: Long) {
        try {
            LuksNative.nativeCancelOperation(token)
        } catch (t: Throwable) {
            Trace.e("TransferManager: nativeCancelOperation failed: ${Trace.throwableSummary(t)}")
        }
    }

    override fun close(token: Long) {
        try {
            LuksNative.nativeCloseCancelToken(token)
        } catch (t: Throwable) {
            Trace.e("TransferManager: nativeCloseCancelToken failed: ${Trace.throwableSummary(t)}")
        }
    }
}

/** Identity of whoever holds the transfer lock. [Mutex.unlock] refuses anyone else. */
private class LockToken(val label: String)

private fun TransferState.isTerminal() =
    this == TransferState.COMPLETED || this == TransferState.CANCELLED || this == TransferState.FAILED

/**
 * Process-wide singleton tracking active and completed file transfers.
 * Survives screen navigation and backgrounding.
 *
 * # Engine invariants (F5)
 *
 * * **One ordered worker.** Every transfer is a [Channel] entry consumed by a single coroutine,
 *   so jobs run strictly in start order and only one at a time (C4). The worker holds
 *   [transferMutex] for the whole job. A SAF write (see [tryAcquireForSafWrite]) takes the same
 *   mutex.
 * * **Owner tokens, no force-unlock.** Every lock/unlock carries a [LockToken]; releasing a lock
 *   you do not hold is a no-op, never an unlock (C1). [resetTransferLocks] cancels and *waits* for
 *   the holder to finish (bounded), and gives up rather than unlocking under it.
 * * **Terminal states are write-once.** Every terminal transition goes through [finishTransfer]
 *   (C5).
 * * **Commit policy (a)**: one `commitActiveBatch` when the queue drains after write work.
 *   [commitNow] and [withExclusiveWrite] cover callers outside the queue.
 * * **Fatal write errors** (fenced / poisoned / panic) are not swallowed: they are published on
 *   [fatalWriteErrors], the volume is marked write-dead, and later write jobs on it fail fast (C6).
 */
open class TransferController {

    private val nextTransferId = AtomicLong(1)
    private val _transfers = MutableStateFlow<List<TransferItem>>(emptyList())
    val transfers: StateFlow<List<TransferItem>> = _transfers.asStateFlow()

    private val activeJobs = ConcurrentHashMap<Long, Job>()
    private val transferMutex = Mutex()

    /** Who holds [transferMutex] right now (worker job, SAF write, or exclusive mutator). */
    @Volatile private var lockOwner: LockToken? = null
    @Volatile private var safToken: LockToken? = null

    internal var cancelTokens: CancelTokens = NativeCancelTokens

    /** F9: where native events are drained after a failure and after the queue empties. Replaceable by tests. */
    internal var nativeDrain: NativeDrain = NativeDrain.default

    // F9 log bookkeeping, keyed by transfer id: when it started (nanoTime) and the last phase it
    // entered, which is the stage it died in if it fails. Numbers and fixed words only.
    private val xferStartNs = ConcurrentHashMap<Long, Long>()
    private val xferStage = ConcurrentHashMap<Long, String>()

    private fun opOf(type: TransferType) = when (type) {
        TransferType.IMPORT -> "import"
        TransferType.EXPORT -> "export"
        TransferType.HASH -> "hash"
    }

    private fun noteStart(id: Long) {
        if (xferStartNs.putIfAbsent(id, System.nanoTime()) == null) EventLog.ev(Ev.XFER_START) { xfer(id) }
    }

    private fun msSinceStart(id: Long): Long? = xferStartNs[id]?.let { (System.nanoTime() - it) / 1_000_000L }

    /** Marks [stage] as the one [id] is in now, so a failure inside it is attributed to it. */
    private fun enter(id: Long, stage: String) {
        // Only for a transfer that is live: an entry for an ended one would never be removed.
        if (!xferStartNs.containsKey(id)) return
        xferStage[id] = stage
        // finish and commit are where a kill leaves the drive dirty: log entry as well as completion.
        if (stage == "finish" || stage == "commit") {
            EventLog.ev(Ev.XFER_PHASE) { xfer(id); stage(stage); state("enter") }
        }
    }

    /** One finished per-file phase line. Never per chunk. */
    private fun phase(id: Long, stage: String, ms: Long, bytes: Long? = null, ino: Long? = null) {
        EventLog.ev(Ev.XFER_PHASE) {
            xfer(id); stage(stage); ms(ms)
            if (bytes != null) bytes(bytes)
            if (ino != null) ino(ino)
        }
        // Pull the native timeline right after the phases a kill would otherwise erase it from.
        if (stage == "finish" || stage == "commit") nativeDrain.requestDrain()
    }

    // ---- logged operations that are not queue jobs (browser mutators, SAF provider writes) ----

    /** Allocates a log id for an operation outside the queue and logs `xfer_queued` + `xfer_start`. */
    fun beginLoggedOp(op: String, bytes: Long = -1L): Long {
        val id = nextTransferId.getAndIncrement()
        EventLog.ev(Ev.XFER_QUEUED) { xfer(id); op(op); n(1); if (bytes >= 0L) bytes(bytes) }
        noteStart(id)
        enter(id, op)
        return id
    }

    /** A per-file phase of a logged operation, with entry lines for finish/commit. */
    fun enterLoggedPhase(id: Long, stage: String) = enter(id, stage)

    fun phaseLogged(id: Long, stage: String, ms: Long, bytes: Long? = null, ino: Long? = null) =
        phase(id, stage, ms, bytes, ino)

    /**
     * Ends a logged operation: `native_err` for a LuksException, `xfer_end`, and on failure the native ring
     * dump tagged with [id]. Safe to call once per id; a second call logs a second `xfer_end` without a duration.
     */
    fun endLoggedOp(id: Long, state: TransferState, cause: Throwable? = null, bytes: Long? = null) {
        if (cause != null && state == TransferState.FAILED) nativeErr(xferStage[id] ?: "op", cause, id)
        logEnd(id, state, null, cause, bytes)
        if (state == TransferState.FAILED) nativeDrain.onTransferFailure(id)
    }

    /** Runs [block] as phase [stage] of [id]: marks it entered, logs `xfer_phase` with its duration when it completes. */
    private inline fun <T> timedPhase(id: Long, stage: String, bytes: Long? = null, block: () -> T): T {
        enter(id, stage)
        val t0 = System.nanoTime()
        val r = block()
        phase(id, stage, (System.nanoTime() - t0) / 1_000_000L, bytes)
        return r
    }

    private fun nativeErr(op: String, t: Throwable, id: Long? = null) =
        EventLog.nativeErr(op, t, xfer = id, stage = id?.let { xferStage[it] })

    /** How long [withExclusiveWrite] waits for the transfer lock before reporting the drive busy. */
    internal var exclusiveWaitMs: Long = EXCLUSIVE_WAIT_MS

    /** How long [resetTransferLocks] waits for the lock holder to finish before giving up. */
    internal var resetWaitMs: Long = RESET_WAIT_MS

    /**
     * The manager's own coroutine scope. Transfers started via [startImport] /
     * [startExport] / [startHash] run here, NOT on a caller-supplied (e.g.
     * `rememberCoroutineScope()`) scope, so navigating away from the screen that
     * started a transfer does not cancel it: this scope is process-wide, tied to
     * [TransferManager] itself, not to any Composable's lifecycle.
     */
    private val managerScope = CoroutineScope(SupervisorJob() + Dispatchers.IO)

    companion object {
        const val CHUNK_SIZE = 1 shl 20 // 1 MiB

        /** Bound on [tryAcquireForSafWrite] -- see its doc comment for why this is finite. */
        private const val SAF_WRITE_LOCK_TIMEOUT_MS = 10_000L

        private const val RESET_WAIT_MS = 2_000L

        private const val EXCLUSIVE_WAIT_MS = 10_000L

        /** Longest the queue-drain commit waits for a SAF write that is holding the lock. */
        private const val DRAIN_COMMIT_LOCK_WAIT_MS = 15_000L

        private const val WRITE_DEAD_MESSAGE =
            "Writes are blocked after a write failure. Lock and unlock the volume again to continue."
    }

    // ------------------------------------------------------------------ observable engine state

    private val queueLock = Any()
    private var outstanding = 0
    private val _queueSize = MutableStateFlow(0)

    /** Transfers that are queued or running (including a drain commit in progress). For the foreground service. */
    val queueSize: StateFlow<Int> = _queueSize.asStateFlow()

    private val _busy = MutableStateFlow(false)

    /**
     * True while the queue is non-empty or a SAF write holds the transfer lock: the signal for the
     * `dataSync` foreground service to run (N1). Goes false only when there is nothing left to lose.
     */
    val busy: StateFlow<Boolean> = _busy.asStateFlow()

    private fun refreshBusy() {
        _busy.value = _queueSize.value > 0 || safToken != null
    }

    /**
     * True when no job is queued or running, no SAF write holds the lock and no exclusive mutator
     * does. What the health monitor waits for. Reads volatile fields only, never [queueLock]:
     * the monitor calls it under its own lock while this class calls the monitor, so taking
     * [queueLock] here could deadlock.
     */
    internal fun writesIdle(): Boolean = _queueSize.value == 0 && safToken == null && lockOwner == null

    /** Refuses with [LuksException.HEALTH_READ_ONLY] while the health check says writes must stop. */
    private fun requireHealthWritable() {
        if (LuksSession.healthReadOnly.value) {
            throw LuksException(UiErrorMessage.HEALTH_READ_ONLY_MESSAGE, LuksException.HEALTH_READ_ONLY)
        }
    }

    private val fatalChannel = Channel<LuksException>(Channel.UNLIMITED)

    /**
     * Fenced / poisoned / panic failures seen by a transfer, commit or exclusive mutator (C6). One
     * consumer (the session): buffered until collected, so an event is never lost to a late
     * subscriber. The consumer should move the session to its write-dead state
     * ([SessionController.onWriteSessionFenced] / [SessionController.onWritePoison]).
     */
    val fatalWriteErrors: Flow<LuksException> = fatalChannel.receiveAsFlow()

    private var faultedVolume: WeakReference<LuksVolume>? = null

    /** Whether a fatal write failure was seen on [volume]. Later write jobs on it fail fast; reads still run. */
    fun isWriteDead(volume: LuksVolume): Boolean = faultedVolume?.get() === volume

    private val _commitFailure = MutableStateFlow<LuksException?>(null)

    /** The last failed queue-drain / [commitNow] commit, cleared by the next successful one. Files reported done may not be durable while set. */
    val commitFailure: StateFlow<LuksException?> = _commitFailure.asStateFlow()

    private fun noteFatal(volume: LuksVolume?, t: Throwable?) {
        val e = t as? LuksException ?: return
        if (!(e.isWriteSessionDead || e.isPanic)) return
        if (volume != null) {
            if (faultedVolume?.get() === volume) return // already reported for this volume
            faultedVolume = WeakReference(volume)
        }
        fatalChannel.trySend(e)
    }

    // ------------------------------------------------------------------ transfer lock

    /**
     * Acquires the transfer lock for the lifetime of a single SAF streaming write (see
     * `LuksProxyCallback` in the documents package), so a write coming in through the
     * platform Files app cannot interleave with an app-initiated import/export/hash queued
     * through [startImport]/[startExport]/[startHash] -- both paths serialize through this
     * one mutex, never two independent ones.
     *
     * Bounded wait, not a suspend-forever [Mutex.lock]: the Rust writer's `claim_writer()`
     * guard returns `WriterBusy` if it is already held, and that arriving mid-stream (after
     * this mutex was already acquired) is unrecoverable -- so contention has to be resolved
     * here, before the native writer is ever claimed, not discovered afterward. The risk this
     * bounds is real, not theoretical: an app that opens a write `ParcelFileDescriptor` and is
     * killed, crashes, or otherwise never closes it would hold this lock until whatever bound
     * applies, or forever with a plain [Mutex.lock]. There is no way to detect "this caller
     * vanished" from inside a suspend function; the timeout is the entire mitigation.
     */
    suspend fun tryAcquireForSafWrite(timeoutMs: Long = SAF_WRITE_LOCK_TIMEOUT_MS): Boolean {
        val token = LockToken("saf")
        if (!acquireBounded(token, timeoutMs)) return false
        lockOwner = token
        safToken = token
        refreshBusy()
        // After the busy flag, before the caller's first native call (see HealthMonitor).
        LuksSession.healthMonitor.yieldToWriter()
        return true
    }

    /**
     * Takes the transfer lock for [token] within [timeoutMs], or not at all. Polls `tryLock`
     * instead of wrapping `lock()` in a timeout: a timeout that fires just after `lock()` returned
     * would report "not acquired" while the mutex stays held, and nothing would ever unlock it. Here
     * the lock is either held by [token] and `true` is returned, or it is not held and `false` is.
     */
    private suspend fun acquireBounded(token: LockToken, timeoutMs: Long): Boolean {
        val deadline = System.nanoTime() + timeoutMs * 1_000_000L
        while (true) {
            if (transferMutex.tryLock(token)) return true
            if (System.nanoTime() >= deadline) return false
            kotlinx.coroutines.delay(5)
        }
    }

    /**
     * Releases the lock taken by [tryAcquireForSafWrite]. Exactly one release per successful
     * acquire; callers hold it across error paths with try/finally. Releasing when no SAF write
     * holds the lock is a no-op: it can never unlock a lock a transfer owns (C1).
     */
    fun releaseSafWriteLock() {
        val token = safToken ?: return
        safToken = null
        if (lockOwner === token) lockOwner = null
        if (transferMutex.holdsLock(token)) transferMutex.unlock(token)
        refreshBusy()
        LuksSession.healthMonitor.noteIdle()
    }

    /**
     * Cancels every queued and running transfer when the session is torn down, then waits (bounded
     * by [resetWaitMs]) for whoever holds the transfer lock to finish. It never unlocks the lock
     * itself: a cancelled job is still inside its blocking native call until that returns, and
     * letting the next job in alongside it is exactly what C1 was (the cancel token makes native
     * calls return promptly). If the holder does not finish in time the caller tears the session
     * down anyway.
     */
    fun resetTransferLocks() {
        val holder = lockOwner
        // Weakly consistent iteration: toList() sizes first and then iterates, which throws
        // NoSuchElementException when a finishing job removes its entry in between.
        for (id in activeJobs.keys) cancelTransfer(id)
        if (holder == null || !transferMutex.holdsLock(holder)) return
        val deadline = System.nanoTime() + resetWaitMs * 1_000_000L
        while (transferMutex.holdsLock(holder)) {
            if (System.nanoTime() >= deadline) {
                Trace.e("TransferManager: lock holder did not finish within ${resetWaitMs}ms of reset; continuing teardown")
                return
            }
            Thread.sleep(10)
        }
    }

    /**
     * Runs [block] with the transfer lock and a session lease, then commits (policy (e)). For
     * browser mutators (mkdir / rename / delete) that must not interleave with a transfer's open
     * writer (N8). A fatal write failure inside [block] is published on [fatalWriteErrors] and
     * rethrown after the lock is released.
     */
    suspend fun <T> withExclusiveWrite(op: String? = null, block: suspend (LuksVolume) -> T): T {
        if (op == null) {
            // Unlabelled mutator: no id, but a failure still gets its native_err and the ring dump.
            try {
                return exclusiveWrite(null, block)
            } catch (t: Throwable) {
                if (t !is CancellationException) nativeDrain.onTransferFailure(null)
                throw t
            }
        }
        // F9: a browser mutation has no queue entry, so it gets a log id of its own.
        val id = beginLoggedOp(op)
        try {
            val r = exclusiveWrite(id, block)
            endLoggedOp(id, TransferState.COMPLETED)
            return r
        } catch (t: Throwable) {
            endLoggedOp(id, if (t is CancellationException) TransferState.CANCELLED else TransferState.FAILED, t)
            throw t
        }
    }

    private suspend fun <T> exclusiveWrite(id: Long?, block: suspend (LuksVolume) -> T): T {
        requireHealthWritable()
        val token = LockToken("mutator")
        // Bounded, like a SAF write: behind a long import the UI gets an answer, not a hang.
        if (!acquireBounded(token, exclusiveWaitMs)) {
            throw LuksException("the drive is busy with a transfer", LuksException.WRITER_BUSY)
        }
        lockOwner = token
        LuksSession.healthMonitor.yieldToWriter()
        var thrown: Throwable? = null
        var result: T? = null
        try {
            try {
                LuksSession.withLease { volume ->
                    // Caught inside the lease so a fatal failure does not make the session tear
                    // itself down while this call still owns the lock.
                    try {
                        if (isWriteDead(volume)) throw LuksException(WRITE_DEAD_MESSAGE, LuksException.WRITE_SESSION_FENCED)
                        result = block(volume)
                        commitOn(volume)
                    } catch (t: Throwable) {
                        thrown = t
                        noteFatal(volume, t)
                    }
                }
            } catch (t: Throwable) {
                thrown = thrown ?: t
            }
        } finally {
            if (lockOwner === token) lockOwner = null
            transferMutex.unlock(token)
            LuksSession.healthMonitor.noteIdle()
        }
        thrown?.let {
            // Labelled mutators get their native_err from endLoggedOp; this covers the unlabelled ones.
            if (id == null) nativeErr("mutator", it, null)
            throw it
        }
        @Suppress("UNCHECKED_CAST")
        return result as T
    }

    /**
     * Commits the active batch now (btrfs publishes the in-memory batch; a no-op where there is
     * none). For callers outside the queue: SAF finish / fsync, browser mutators, lock. Returns
     * false, without throwing, when there is no unlocked session or the commit failed; a failure
     * is published on [commitFailure] and, if fatal, on [fatalWriteErrors].
     */
    suspend fun commitNow(): Boolean {
        var ok = false
        try {
            LuksSession.withLease { volume ->
                if (isWriteDead(volume)) return@withLease
                ok = commitOn(volume)
            }
        } catch (e: CancellationException) {
            throw e
        } catch (t: Throwable) {
            // No unlocked session: teardown commits natively.
            Trace.e("TransferManager: drain commit lease failed: ${Trace.throwableSummary(t)}")
        }
        return ok
    }

    /** Commit on [volume]; never throws (a failure is recorded, and fatal ones published). */
    private fun commitOn(volume: LuksVolume): Boolean = try {
        volume.commitActiveBatch()
        _commitFailure.value = null
        LuksSession.noteBatchCommitted()
        true
    } catch (t: Throwable) {
        val e = (t as? LuksException) ?: LuksException("commit failed", LuksException.GENERIC)
        Trace.err(e.code, "commit")
        nativeErr("commit", t)
        nativeDrain.onTransferFailure(null)
        _commitFailure.value = e
        noteFatal(volume, t)
        false
    }

    // ------------------------------------------------------------------ the queue

    private class Queued(val id: Long, val job: Job, val writes: Boolean)

    private val queue = Channel<Queued>(Channel.UNLIMITED)
    private var workerStarted = false
    @Volatile private var dirty = false

    private fun enqueue(id: Long, writes: Boolean, run: suspend () -> Unit) {
        val job = managerScope.launch(start = CoroutineStart.LAZY) { run() }
        activeJobs[id] = job
        synchronized(queueLock) {
            outstanding++
            _queueSize.value = outstanding
            refreshBusy()
            if (!workerStarted) {
                workerStarted = true
                managerScope.launch { for (q in queue) process(q) }
            }
        }
        // After refreshBusy above (the monitor re-reads idleness after registering its token).
        // Reads yield too: a scan holds the filesystem lock, so an export would wait on it.
        LuksSession.healthMonitor.yieldToWriter(writes)
        queue.trySend(Queued(id, job, writes))
    }

    private suspend fun process(q: Queued) {
        var ran = false
        try {
            if (!q.job.isCancelled) {
                val token = LockToken("transfer-${q.id}")
                transferMutex.lock(token)
                lockOwner = token
                try {
                    if (!q.job.isCancelled) {
                        ran = true
                        q.job.start()
                        q.job.join()
                    }
                } finally {
                    if (lockOwner === token) lockOwner = null
                    transferMutex.unlock(token)
                }
            }
        } catch (t: Throwable) {
            Trace.e("TransferManager: worker error: ${Trace.throwableSummary(t)}")
        } finally {
            activeJobs.remove(q.id)
            if (ran && q.writes) dirty = true
            try {
                // Policy (a): the queue drained after write work.
                val lastOne = synchronized(queueLock) { outstanding } == 1
                if (dirty && lastOne) drainCommit()
                // F9: the queue emptied; pull the native timeline of everything it just did.
                if (lastOne) nativeDrain.requestDrain()
            } catch (t: Throwable) {
                Trace.e("TransferManager: drain commit error: ${Trace.throwableSummary(t)}")
            }
            synchronized(queueLock) {
                outstanding--
                _queueSize.value = outstanding
                refreshBusy()
            }
            LuksSession.healthMonitor.noteIdle()
        }
    }

    private suspend fun drainCommit() {
        // A fatal error was already published: the session is being torn down, teardown commits
        // natively, and taking a lease here would only queue behind it (while it waits on us).
        if (LuksSession.volume?.let { isWriteDead(it) } == true) {
            dirty = false
            return
        }
        val token = LockToken("drain")
        if (!acquireBounded(token, DRAIN_COMMIT_LOCK_WAIT_MS)) return // a SAF write holds the lock; the next drain commits
        lockOwner = token
        try {
            dirty = false
            var ok = commitNow()
            // A failed commit is retried once. Still failing: commitFailure stays set (the banner)
            // until a later commit succeeds, and the work stays dirty so the next drain tries again.
            if (!ok && _commitFailure.value != null) ok = commitNow()
            if (!ok && _commitFailure.value != null) dirty = true
        } finally {
            if (lockOwner === token) lockOwner = null
            transferMutex.unlock(token)
        }
    }

    // ------------------------------------------------------------------ transfer bookkeeping

    private fun addQueued(item: TransferItem, op: String = opOf(item.type)) {
        _transfers.update { listOf(item) + it }
        EventLog.ev(Ev.XFER_QUEUED) {
            xfer(item.id); op(op); n(maxOf(item.filesTotal, 1))
            if (item.totalBytes >= 0L) bytes(item.totalBytes)
        }
    }

    private fun queuedItem(id: Long, name: String, type: TransferType, totalBytes: Long, extra: (TransferItem) -> TransferItem = { it }) =
        extra(
            TransferItem(
                id = id,
                name = name,
                type = type,
                totalBytes = totalBytes,
                transferredBytes = 0L,
                speedBytesPerSec = 0L,
                etaSeconds = 0L,
                state = TransferState.QUEUED,
                cancelToken = 0L,
                error = null,
            ),
        )

    /** Test seam: a QUEUED item with no job behind it. */
    internal fun enqueueForTest(name: String, type: TransferType): Long {
        val id = nextTransferId.getAndIncrement()
        addQueued(queuedItem(id, name, type, -1L))
        return id
    }

    /**
     * THE only way a transfer reaches COMPLETED, CANCELLED or FAILED. Applies only while the item
     * is QUEUED or RUNNING, so a terminal state is write-once: a late cancel cannot undo a
     * completion, and a late completion cannot undo a cancel (C5). Returns whether it applied.
     */
    internal fun finishTransfer(
        id: Long,
        state: TransferState,
        error: String? = null,
        errorCode: Int? = null,
        cause: Throwable? = null,
        adjust: (TransferItem) -> TransferItem = { it },
    ): Boolean {
        require(state.isTerminal()) { "finishTransfer needs a terminal state" }
        var applied = false
        var final: TransferItem? = null
        _transfers.update { list ->
            applied = false
            list.map {
                if (it.id == id && !it.state.isTerminal()) {
                    applied = true
                    adjust(it).copy(
                        state = state,
                        error = error,
                        errorCode = errorCode,
                        etaSeconds = 0L,
                        verifying = false,
                        speedBytesPerSec = if (state == TransferState.COMPLETED) adjust(it).speedBytesPerSec else 0L,
                    ).also { done -> final = done }
                } else it
            }
        }
        // Logged after the update, never inside it: the update lambda can be retried.
        if (applied) logEnd(id, state, final, cause)
        return applied
    }

    /** `xfer_end`: state, code, stage it was in, exception class, duration, bytes and files. Never a name or a message. */
    private fun logEnd(id: Long, state: TransferState, item: TransferItem?, cause: Throwable?, bytesOverride: Long? = null) {
        val ms = msSinceStart(id)
        val stage = xferStage.remove(id)
        xferStartNs.remove(id)
        EventLog.ev(Ev.XFER_END) {
            xfer(id)
            state(state.name.lowercase())
            val code = item?.errorCode ?: (cause as? LuksException)?.code
            if (code != null) code(code)
            if (state != TransferState.COMPLETED && stage != null) stage(stage)
            if (cause != null) exc(cause)
            if (ms != null) ms(ms)
            if (bytesOverride != null) bytes(bytesOverride)
            if (item != null) {
                bytes(item.transferredBytes)
                if (item.filesTotal > 0 || item.filesDone > 0) n(item.filesDone)
            }
        }
    }

    /**
     * The one sanctioned exception to write-once: a single import whose file was published while a
     * cancel was being recorded. The file exists, so the honest terminal state is COMPLETED. Never
     * overrides FAILED or an earlier COMPLETED.
     */
    private fun confirmPublished(id: Long, adjust: (TransferItem) -> TransferItem) {
        var flipped: TransferItem? = null
        _transfers.update { list ->
            flipped = null
            list.map {
                if (it.id == id && (it.state == TransferState.CANCELLED || !it.state.isTerminal())) {
                    adjust(it).copy(state = TransferState.COMPLETED, error = null, errorCode = null, etaSeconds = 0L, verifying = false)
                        .also { done -> flipped = done }
                } else it
            }
        }
        flipped?.let { logEnd(id, TransferState.COMPLETED, it, null) }
    }

    /** Progress and other non-terminal updates; ignored once the item is terminal. */
    private fun updateActive(id: Long, transform: (TransferItem) -> TransferItem) {
        _transfers.update { list ->
            list.map { if (it.id == id && !it.state.isTerminal()) transform(it) else it }
        }
    }

    /** QUEUED -> RUNNING. Returns false when the item is already terminal (cancelled while queued). */
    private fun markRunning(id: Long): Boolean {
        var ok = false
        _transfers.update { list ->
            ok = false
            list.map {
                if (it.id == id && !it.state.isTerminal()) {
                    ok = true
                    it.copy(state = TransferState.RUNNING)
                } else it
            }
        }
        if (ok) noteStart(id)
        return ok
    }

    /**
     * Makes sure an item for [transferId] exists and is RUNNING with the given facts. A direct call
     * of a `*WithProgress` function (no queue) creates the item here.
     */
    private fun registerRunning(transferId: Long, name: String, type: TransferType, totalBytes: Long) {
        noteStart(transferId)
        _transfers.update { list ->
            if (list.any { it.id == transferId }) {
                list.map {
                    if (it.id == transferId && !it.state.isTerminal()) {
                        it.copy(name = name, totalBytes = if (totalBytes >= 0L || it.totalBytes < 0L) totalBytes else it.totalBytes, state = TransferState.RUNNING)
                    } else it
                }
            } else {
                listOf(queuedItem(transferId, name, type, totalBytes).copy(state = TransferState.RUNNING)) + list
            }
        }
    }

    internal fun attachCancelToken(id: Long, token: Long) {
        updateActive(id) { it.copy(cancelToken = token) }
    }

    private fun clearCancelToken(id: Long) {
        _transfers.update { list -> list.map { if (it.id == id) it.copy(cancelToken = 0L) else it } }
    }

    /** Creates a native cancel token for [transferId], runs [block] with it, and always frees it. */
    private inline fun <T> withTransferCancelToken(transferId: Long, block: (Long) -> T): T {
        val token = cancelTokens.create()
        if (token > 0L) attachCancelToken(transferId, token)
        try {
            return block(token)
        } finally {
            if (token > 0L) {
                clearCancelToken(transferId)
                cancelTokens.close(token)
            }
        }
    }

    /** The single translation from "what was thrown" to a terminal state. Write-once, so a no-op if already finished. */
    private fun failFrom(id: Long, t: Throwable, operation: String, volume: LuksVolume? = null) {
        noteFatal(volume, t)
        val cancelled = t is CancellationException ||
            (t is LuksException && t.isCancelled) ||
            isTransferCancelled(id)
        if (cancelled) {
            if (finishTransfer(id, TransferState.CANCELLED)) Trace.err(LuksException.CANCELLED, operation.lowercase())
            return
        }
        val code = (t as? LuksException)?.code
        val message = if (t is TransferFailure) t.userMessage else UiErrorMessage.getUserMessage(t, operation)
        // F9: the native error is recorded before the terminal state, so the end line follows its cause.
        nativeErr(operation.lowercase(), t, id)
        val applied = finishTransfer(id, TransferState.FAILED, error = message, errorCode = code ?: (t as? TransferFailure)?.code, cause = t)
        Trace.err(code ?: LuksException.GENERIC, operation.lowercase())
        // Off this thread: pulls the native events that led here, then the whole ring, tagged with this transfer.
        if (applied) nativeDrain.onTransferFailure(id)
    }

    /**
     * Runs one queued job: lease, write-dead gate, [body], and the terminal state. Everything that
     * can go wrong lands in [finishTransfer]; the throwable (if any) is returned for callers, such
     * as [startHash], that report it themselves.
     */
    private suspend fun runJob(
        id: Long,
        operation: String,
        writes: Boolean,
        body: suspend (LuksVolume, Long) -> Unit,
    ): Throwable? {
        if (isTransferCancelled(id) || !markRunning(id)) {
            return CancellationException("Transfer cancelled")
        }
        var thrown: Throwable? = null
        try {
            LuksSession.withLease { volume ->
                // Everything is caught INSIDE the lease: a fatal failure must reach the session
                // through [fatalWriteErrors] after the lock is released, not through withLease's
                // own handler, which would tear the session down while this job still owns it.
                try {
                    if (writes && isWriteDead(volume)) {
                        finishTransfer(id, TransferState.FAILED, error = WRITE_DEAD_MESSAGE, errorCode = LuksException.WRITE_SESSION_FENCED)
                        return@withLease
                    }
                    // Not write-dead: the session stays open and reads keep working (A8).
                    if (writes && LuksSession.healthReadOnly.value) {
                        finishTransfer(id, TransferState.FAILED, error = UiErrorMessage.HEALTH_READ_ONLY_MESSAGE, errorCode = LuksException.HEALTH_READ_ONLY)
                        return@withLease
                    }
                    body(volume, id)
                } catch (t: Throwable) {
                    thrown = t
                    failFrom(id, t, operation, volume)
                }
            }
        } catch (t: Throwable) {
            thrown = thrown ?: t
            failFrom(id, t, operation)
        }
        // A body that returned without finishing its own item is a success.
        finishTransfer(id, TransferState.COMPLETED)
        return thrown
    }

    /**
     * Queues a generic job. [body] runs under the transfer lock and a session lease, in start
     * order; throwing fails (or, for cancellation, cancels) the transfer. A returned job that did
     * not finish its own item completes it. [writes] marks work that dirties the volume, which
     * earns a commit when the queue drains.
     */
    internal fun startJob(
        name: String,
        type: TransferType,
        totalBytes: Long = -1L,
        writes: Boolean = false,
        body: suspend (LuksVolume, Long) -> Unit,
    ): Long {
        val id = nextTransferId.getAndIncrement()
        addQueued(queuedItem(id, name, type, totalBytes))
        enqueue(id, writes) { runJob(id, "Transfer", writes, body) }
        return id
    }

    // ------------------------------------------------------------------ public start* API

    /**
     * Starts an export on the manager's own scope, holding a [LuksSession] lease
     * for the whole transfer (so the idle timer cannot lock the session out from
     * under it). Returns the transfer id immediately; observe [transfers] for
     * progress. Safe to call from a Composable without tying the transfer's
     * lifetime to that Composable's.
     */
    fun startExport(context: Context, path: String, targetUri: Uri): Long {
        val id = nextTransferId.getAndIncrement()
        val fileName = path.substringAfterLast('/').ifEmpty { "export_${System.currentTimeMillis()}" }
        addQueued(queuedItem(id, fileName, TransferType.EXPORT, -1L))
        enqueue(id, writes = false) {
            runJob(id, "Export", writes = false) { volume, tid ->
                exportFileWithProgress(context, volume, path, targetUri, tid)
            }
        }
        return id
    }

    /**
     * Starts an import on the manager's own scope, holding a [LuksSession] lease
     * for the whole transfer. Returns the transfer id immediately; observe
     * [transfers] for progress. Safe to call from a Composable without tying the
     * transfer's lifetime to that Composable's.
     */
    fun startImport(context: Context, parentPath: String, sourceUri: Uri): Long {
        val id = nextTransferId.getAndIncrement()
        val source = ContentResolverImportSource(context, sourceUri)
        val info = source.describe()
        addQueued(queuedItem(id, info.name, TransferType.IMPORT, info.sizeBytes))
        enqueue(id, writes = true) {
            runJob(id, "Import", writes = true) { volume, tid ->
                importFileWithProgress(context, volume, parentPath, sourceUri, tid, source)
            }
        }
        return id
    }

    /**
     * Enumerates and prechecks a directory transfer without starting it.
     *
     * Runs under a session lease but deliberately *not* under the transfer lock:
     * this phase only reads, and holding the write lock across a user-facing
     * dialog would block every other transfer for as long as someone leaves the
     * prompt on screen.
     */
    suspend fun prepareDirectoryTransfer(
        context: Context,
        direction: TransferType,
        path: String,
        treeUri: Uri,
    ): DirectoryTransferRequest = withContext(Dispatchers.IO) {
        LuksSession.withLease { volume ->
            if (direction == TransferType.IMPORT) {
                prepareDirectoryImport(context, volume, path, treeUri)
            } else {
                prepareDirectoryExport(context, volume, path, treeUri)
            }
        }
    }

    /**
     * Runs an already-prechecked directory transfer on the manager's own scope.
     *
     * [request] must have come from [prepareDirectoryTransfer] and [mode] must
     * be an option its prompt actually allowed -- offering keep-both where the
     * precheck blocked it is how a transfer fails halfway, which is the whole
     * thing this design exists to avoid.
     */
    fun startDirectoryTransfer(
        context: Context,
        direction: TransferType,
        request: DirectoryTransferRequest,
        treeUri: Uri,
        mode: CollisionMode,
    ): Long {
        val id = nextTransferId.getAndIncrement()
        addQueued(
            queuedItem(id, request.rootName, direction, request.plan.totalBytes) {
                it.copy(filesTotal = request.plan.fileCount, totalIsLowerBound = request.plan.hasUnknownSizes)
            },
            op = if (direction == TransferType.IMPORT) "dir_import" else "dir_export",
        )
        val writes = direction == TransferType.IMPORT
        enqueue(id, writes) {
            runJob(id, "Transfer", writes) { volume, tid ->
                val started = System.currentTimeMillis()
                val onProgress: (TransferProgress) -> Unit = { p ->
                    val elapsed = System.currentTimeMillis() - started
                    val speed = if (elapsed > 0) (p.bytesDone * 1000L) / elapsed else 0L
                    val label = treeProgressLabel(p, elapsed)
                    updateActive(tid) {
                        it.copy(
                            transferredBytes = p.bytesDone,
                            speedBytesPerSec = speed,
                            etaSeconds = label.etaSeconds ?: 0L,
                            filesDone = p.filesDone,
                            currentPath = p.currentPath,
                        )
                    }
                }
                val cancelled = { isTransferCancelled(tid) }
                // F9: one line per finished file (never per chunk); numbers only, no name.
                val onFileDone: (FileDone) -> Unit = { f -> phase(tid, "finish", f.ms, bytes = f.bytes, ino = f.ino) }
                enter(tid, "tree")
                val outcome = withTransferCancelToken(tid) { token ->
                    // No suspension inside: the thread-local token holds for the whole tree.
                    withNativeCancel(token) {
                        if (direction == TransferType.IMPORT) {
                            runDirectoryImport(context, volume, request, treeUri, mode, onProgress, cancelled, onFileDone)
                        } else {
                            runDirectoryExport(context, volume, request, treeUri, mode, onProgress, cancelled, onFileDone)
                        }
                    }
                }
                noteFatal(volume, outcome.failure)
                outcome.failure?.let { f ->
                    nativeErr("transfer", f, tid)
                    if (f !is TransferCancelledException) nativeDrain.onTransferFailure(tid)
                }

                // A tree transfer reports its own outcome rather than throwing (§5.2: what
                // already landed is kept), so the terminal state is set from that outcome, not
                // from whether an exception escaped.
                finishTransfer(
                    tid,
                    when {
                        outcome.succeeded -> TransferState.COMPLETED
                        outcome.failure is TransferCancelledException -> TransferState.CANCELLED
                        else -> TransferState.FAILED
                    },
                    error = if (outcome.succeeded) null else stoppedSummary(outcome, request.plan.fileCount),
                    errorCode = (outcome.failure as? LuksException)?.code,
                    cause = outcome.failure,
                ) {
                    it.copy(
                        filesDone = outcome.filesCopied + outcome.filesSkipped,
                        transferredBytes = outcome.bytesCopied,
                        currentPath = outcome.stoppedAtPath ?: it.currentPath,
                    )
                }
            }
        }
        return id
    }

    /**
     * Starts a SHA-256 checksum on the manager's own scope, tracked as a HASH
     * transfer like imports/exports and holding a [LuksSession] lease for the
     * duration. [onResult] is invoked with the outcome once finished; it may run
     * after any originating Composable has left composition, so callers must
     * tolerate a no-op update in that case.
     */
    fun startHash(
        path: String,
        onResult: (Result<LuksVolume.Digest>) -> Unit = {},
    ): Long {
        val id = nextTransferId.getAndIncrement()
        val fileName = path.substringAfterLast('/').ifEmpty { "hash_${System.currentTimeMillis()}" }
        addQueued(queuedItem(id, fileName, TransferType.HASH, -1L))
        enqueue(id, writes = false) {
            var digest: LuksVolume.Digest? = null
            val failure = runJob(id, "Checksum", writes = false) { volume, tid ->
                digest = hashFileWithProgress(volume, path, tid)
            }
            val result = digest?.let { Result.success(it) }
                ?: Result.failure(failure ?: IllegalStateException("checksum did not run"))
            try {
                onResult(result)
            } catch (t: Throwable) {
                Trace.e("TransferManager: hash callback failed: ${Trace.throwableSummary(t)}")
            }
        }
        return id
    }

    fun getTransfer(id: Long): TransferItem? =
        _transfers.value.find { it.id == id }

    fun clearCompleted() {
        _transfers.update { list ->
            list.filter { it.state == TransferState.RUNNING || it.state == TransferState.QUEUED }
        }
        pruneLogMaps()
    }

    /** Drops log bookkeeping for ids that are no longer tracked as live items (belt and braces for [logEnd]). */
    private fun pruneLogMaps() {
        val live = _transfers.value.filter { !it.state.isTerminal() }.map { it.id }.toSet()
        for (id in xferStartNs.keys.toList()) if (id !in live && !activeJobs.containsKey(id)) { xferStartNs.remove(id); xferStage.remove(id) }
    }

    /** Test seam: sizes of the per-transfer log maps. */
    internal fun logMapSizes(): Pair<Int, Int> = xferStartNs.size to xferStage.size

    fun clearHistory() {
        clearCompleted()
    }

    fun removeTransfer(id: Long) {
        _transfers.update { list ->
            list.filterNot { it.id == id && it.state != TransferState.RUNNING && it.state != TransferState.QUEUED }
        }
        if (_transfers.value.none { it.id == id }) { xferStartNs.remove(id); xferStage.remove(id) }
    }

    fun isTransferCancelled(id: Long): Boolean {
        return _transfers.value.find { it.id == id }?.state == TransferState.CANCELLED
    }

    /**
     * Signals cancellation for the transfer identified by [id]: the native token (so a blocking
     * native call returns promptly), the coroutine, and the recorded state. The state is
     * write-once, so a transfer that already finished stays finished.
     */
    fun cancelTransfer(id: Long) {
        val item = _transfers.value.find { it.id == id } ?: return
        if (item.state == TransferState.RUNNING || item.state == TransferState.QUEUED) {
            if (item.cancelToken > 0L) cancelTokens.signal(item.cancelToken)
            activeJobs[id]?.cancel()
            finishTransfer(id, TransferState.CANCELLED)
        }
    }

    // ------------------------------------------------------------------ progress meter

    private inner class Meter(private val id: Long, private val total: Long) {
        private val started = System.currentTimeMillis()
        private var lastMs = started
        private var lastBytes = 0L

        fun tick(done: Long) {
            val now = System.currentTimeMillis()
            val dt = now - lastMs
            if (dt < 200 && done < total) return
            val speed = if (dt > 0) ((done - lastBytes) * 1000L) / dt else 0L
            val totalElapsedSec = (now - started) / 1000.0
            val avgSpeed = if (totalElapsedSec > 0) (done / totalElapsedSec).toLong() else speed
            val currentSpeed = if (speed > 0) speed else avgSpeed
            val remaining = (total - done).coerceAtLeast(0L)
            val eta = if (currentSpeed > 0) remaining / currentSpeed else 0L
            lastMs = now
            lastBytes = done
            updateActive(id) { it.copy(transferredBytes = done, speedBytesPerSec = currentSpeed, etaSeconds = eta) }
        }

        fun finalSpeed(done: Long): Long {
            val sec = (System.currentTimeMillis() - started).coerceAtLeast(1) / 1000.0
            return (done / sec).toLong()
        }
    }

    // ------------------------------------------------------------------ export

    /**
     * Exports a file from the encrypted [volume] at [path] to [target] (by default the SAF
     * document at [targetUri]) with real-time speed, ETA, and cancellation tracking.
     *
     * The source is resolved once and read by handle; the total comes from the handle too
     * ([LuksVolume.fileSizeOf]). The destination is written under a pending name where the
     * provider allows it, and only on COMPLETED has it been verified (size and SHA-256 re-read
     * from the destination) and given its final name. On ANY failure, cancel or FILE_CHANGED the
     * destination document is deleted: a partial file is never left behind (R5r).
     */
    suspend fun exportFileWithProgress(
        context: Context,
        volume: LuksVolume,
        path: String,
        targetUri: Uri,
        transferId: Long = nextTransferId.getAndIncrement(),
        target: ExportTarget = SafExportTarget(context, targetUri),
    ): Long = withContext(Dispatchers.IO) {
        val fileName = path.substringAfterLast('/').ifEmpty { "export_${System.currentTimeMillis()}" }
        registerRunning(transferId, fileName, TransferType.EXPORT, -1L)
        val ownsJob = activeJobs.putIfAbsent(transferId, currentCoroutineContext().job) == null

        var succeeded = false
        var file = 0L
        var output: java.io.OutputStream? = null
        try {
            withTransferCancelToken(transferId) { _ ->
                enter(transferId, "begin")
                file = volume.openFile(path)
                val totalBytes = volume.fileSizeOf(file)
                updateActive(transferId) { it.copy(totalBytes = totalBytes) }

                // V3: free space at the destination, when the provider will say. Unknown is not full.
                target.availableBytes()?.let { free ->
                    if (totalBytes > free) throw LuksException("not enough space at the destination", LuksException.NO_SPACE)
                }

                val meter = Meter(transferId, totalBytes)
                val digest = java.security.MessageDigest.getInstance("SHA-256")
                var done = 0L
                val out = target.openOutput()
                output = out
                enter(transferId, "read")
                val readStartNs = System.nanoTime()
                while (done < totalBytes) {
                    currentCoroutineContext().ensureActive()
                    if (isTransferCancelled(transferId)) throw CancellationException("Transfer cancelled")

                    val toRead = (totalBytes - done).coerceAtMost(CHUNK_SIZE.toLong()).toInt()
                    val chunk = volume.readAt(file, done, toRead)
                    if (chunk.isEmpty()) break
                    try {
                        out.write(chunk)
                        digest.update(chunk)
                    } finally {
                        java.util.Arrays.fill(chunk, 0.toByte())
                    }
                    done += chunk.size
                    meter.tick(done)
                }
                out.flush()
                out.close() // a close that fails means the bytes may not have landed
                output = null
                phase(transferId, "read", (System.nanoTime() - readStartNs) / 1_000_000L, bytes = done)

                currentCoroutineContext().ensureActive()
                if (isTransferCancelled(transferId)) throw CancellationException("Transfer cancelled")
                if (done < totalBytes) throw TransferFailure("The file ended early, so the copy was stopped.")

                val level = target.verifyLevel
                updateActive(transferId) { it.copy(verifyLevel = level, verifying = true, transferredBytes = 0L, speedBytesPerSec = 0L) }
                enter(transferId, "verify")
                val verifyStartNs = System.nanoTime()
                when (level) {
                    VerifyLevel.FULL -> {
                        val verifyMeter = Meter(transferId, done)
                        val callJob = currentCoroutineContext().job
                        val outcome = verifyReadBack(
                            open = { target.openRead() },
                            expectedBytes = done,
                            expectedSha256 = digest.digest(),
                            isCancelled = { isTransferCancelled(transferId) || !callJob.isActive },
                            onProgress = { verifyMeter.tick(it) },
                        )
                        when (outcome) {
                            ReadBack.Verified -> Unit
                            is ReadBack.SizeMismatch, ReadBack.HashMismatch ->
                                throw TransferFailure("The saved file did not match the original, so it was removed.")
                            ReadBack.Unreadable ->
                                throw TransferFailure("The saved file could not be read back to verify it, so it was removed.")
                        }
                    }
                    VerifyLevel.SIZE -> {
                        // A cloud-backed provider: re-reading would download the file again.
                        if (target.sizeBytes() != done) {
                            throw TransferFailure("The saved file's size did not match the original, so it was removed.")
                        }
                    }
                }
                phase(transferId, "verify", (System.nanoTime() - verifyStartNs) / 1_000_000L, bytes = done)
                timedPhase(transferId, "commit") { target.commit() }

                succeeded = finishTransfer(transferId, TransferState.COMPLETED) {
                    it.copy(transferredBytes = done, speedBytesPerSec = meter.finalSpeed(done))
                }
                Trace.i("TransferManager", "Export completed: $transferId")
            }
        } catch (t: Throwable) {
            failFrom(transferId, t, "Export", volume)
        } finally {
            Trace.cleanup("export_close_output") { output?.close() }
            // The destination is deleted unless the export finished AND stayed finished: a cancel
            // that raced a completion keeps the cancelled state, and then so must the delete.
            if (!succeeded) target.discard()
            if (file != 0L) Trace.cleanup("export_close_file") { volume.closeFile(file) }
            if (ownsJob) activeJobs.remove(transferId)
        }
        transferId
    }

    // ------------------------------------------------------------------ import

    /**
     * Imports a file from [source] (by default the content at [sourceUri]) into the encrypted
     * [volume] under [parentPath] with real-time speed, ETA, and cancellation tracking.
     *
     * The declared size is a contract (V2): fewer or more bytes than declared fails the transfer
     * cleanly, nothing published. Free space is checked up front against the volume's own
     * statfs (V3). The writer has ONE owner, the `finally` below: whatever happens after
     * `beginFile` (an allocation failure, a failed finish, a cancel), it is closed there, and
     * native close is idempotent on a writer a successful finish already consumed (W5, W6).
     */
    suspend fun importFileWithProgress(
        context: Context,
        volume: LuksVolume,
        parentPath: String,
        sourceUri: Uri,
        transferId: Long = nextTransferId.getAndIncrement(),
        source: ImportSource = ContentResolverImportSource(context, sourceUri),
    ): Long = withContext(Dispatchers.IO) {
        val info = source.describe()
        val targetName = info.name
        val totalBytes = info.sizeBytes
        registerRunning(transferId, targetName, TransferType.IMPORT, totalBytes)
        val ownsJob = activeJobs.putIfAbsent(transferId, currentCoroutineContext().job) == null

        var writer: LuksVolume.FileWriter? = null
        var buffer: java.nio.ByteBuffer? = null
        try {
            withTransferCancelToken(transferId) { token ->
                if (totalBytes < 0L) {
                    throw TransferFailure("The file's size could not be determined, so it was not copied.")
                }
                requireFreeSpace(volume, totalBytes)

                val stats = StatsRecorder()
                val meter = Meter(transferId, totalBytes)
                var done = 0L

                val w = timedPhase(transferId, "begin") { volume.beginFile(totalBytes) }
                writer = w
                val buf = java.nio.ByteBuffer.allocateDirect(CHUNK_SIZE)
                buffer = buf

                enter(transferId, "write")
                val writeStartNs = System.nanoTime()
                source.open().use { channel ->
                    while (done < totalBytes) {
                        currentCoroutineContext().ensureActive()
                        if (isTransferCancelled(transferId)) throw CancellationException("Transfer cancelled")

                        buf.clear()
                        buf.limit((totalBytes - done).coerceAtMost(CHUNK_SIZE.toLong()).toInt())

                        // Timed with the same recorder the directory paths use, so the two are
                        // directly comparable. The whole refill counts as one read.
                        var read = 0
                        stats.read {
                            while (buf.hasRemaining()) {
                                val r = channel.read(buf)
                                if (r <= 0) break
                                read += r
                            }
                        }
                        if (read <= 0) break

                        buf.flip()
                        stats.write { withNativeCancel(token) { w.write(buf, read) } }
                        done += read
                        meter.tick(done)
                    }
                    currentCoroutineContext().ensureActive()
                    if (isTransferCancelled(transferId)) throw CancellationException("Transfer cancelled")

                    if (done < totalBytes) {
                        throw TransferFailure("The file is smaller than it reported, so it was not copied.")
                    }
                    // V2: anything past the declared size means the size lied; refuse, don't truncate.
                    buf.clear()
                    buf.limit(1)
                    if (channel.read(buf) > 0) {
                        throw TransferFailure("The file is larger than it reported, so it was not copied.")
                    }
                }
                phase(transferId, "write", (System.nanoTime() - writeStartNs) / 1_000_000L, bytes = done)

                enter(transferId, "finish")
                val finishStartNs = System.nanoTime()
                val ino = stats.commit { w.finish(parentPath, targetName) }
                phase(transferId, "finish", (System.nanoTime() - finishStartNs) / 1_000_000L, bytes = done, ino = ino)
                LuksSession.noteFilesFinished()
                timedPhase(transferId, "commit") { stats.commit { volume.commitActiveBatch() } }
                LuksSession.noteBatchCommitted()
                confirmPublished(transferId) {
                    it.copy(transferredBytes = done, speedBytesPerSec = meter.finalSpeed(done))
                }
                Trace.i("TransferManager", "Import completed: $transferId (inode $ino)")
                Trace.i(formatThroughput("import-single", done, stats.snapshot()))
            }
        } catch (t: Throwable) {
            failFrom(transferId, t, "Import", volume)
        } finally {
            try {
                buffer?.zeroEntireCapacity()
            } finally {
                Trace.cleanup("import_close_writer") { writer?.close() }
                if (ownsJob) activeJobs.remove(transferId)
            }
        }
        transferId
    }

    /**
     * Refuses an import that cannot fit, using the volume's own statfs (V3). A statfs that cannot
     * be read means "unknown", not "full": the write itself will still report NO_SPACE.
     */
    private fun requireFreeSpace(volume: LuksVolume, bytes: Long) {
        val fs = try {
            volume.statFs()
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            Trace.e("TransferManager: statFs before import failed: ${Trace.throwableSummary(e)}")
            return
        }
        val block = fs.blockSize.toLong().coerceAtLeast(1L)
        val needed = ((bytes + block - 1) / block) * block
        if (needed > fs.availableBytes) {
            throw LuksException("not enough free space on the drive", LuksException.NO_SPACE)
        }
    }

    // ------------------------------------------------------------------ hash

    /**
     * Computes a SHA-256 digest of the file at [path] in [volume], tracked as a
     * HASH transfer in [transfers] like an import or export. Cancellable down into the
     * native hash loop.
     */
    suspend fun hashFileWithProgress(
        volume: LuksVolume,
        path: String,
        transferId: Long = nextTransferId.getAndIncrement(),
    ): LuksVolume.Digest = withContext(Dispatchers.IO) {
        val fileName = path.substringAfterLast('/').ifEmpty { "hash_${System.currentTimeMillis()}" }
        val totalBytes = try {
            volume.fileSize(path)
        } catch (e: Exception) {
            -1L
        }
        registerRunning(transferId, fileName, TransferType.HASH, totalBytes)
        val ownsJob = activeJobs.putIfAbsent(transferId, currentCoroutineContext().job) == null

        try {
            val digest = withTransferCancelToken(transferId) { token ->
                timedPhase(transferId, "read") { withNativeCancel(token) { volume.sha256(path) } }
            }
            finishTransfer(transferId, TransferState.COMPLETED) {
                it.copy(
                    transferredBytes = digest.bytes,
                    totalBytes = if (it.totalBytes >= 0L) it.totalBytes else digest.bytes,
                    speedBytesPerSec = digest.bytesPerSec,
                )
            }
            Trace.i("TransferManager", "Hash completed: $transferId")
            digest
        } catch (t: Throwable) {
            failFrom(transferId, t, "Checksum", volume)
            throw t
        } finally {
            if (ownsJob) activeJobs.remove(transferId)
        }
    }
}

/** Process-wide singleton instance. */
object TransferManager : TransferController()
