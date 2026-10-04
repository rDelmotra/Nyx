package dev.luksandroid.session

import dev.luksandroid.Health
import dev.luksandroid.HealthUnknownReason
import dev.luksandroid.Trace
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineDispatcher
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withTimeoutOrNull

/**
 * Session-scoped runner for the drive-health check (F8). One instance per [SessionController];
 * [attach] binds it to the unlocked volume, [detach] unbinds it.
 *
 * # Rules, each with its reason
 *
 * * **Only when idle.** A scan holds the native filesystem lock for its whole run (A6), so it
 *   starts only when [writesIdle] says no transfer, SAF write or browser mutation is active. If
 *   something is active at the due time the check is skipped, not polled for: whoever was active
 *   calls [noteIdle] when it finishes.
 * * **Yields at once.** [yieldToWriter] signals the scan's native cancel token synchronously, so
 *   the requester waits at most one native poll interval for the lock. Callers set their own busy
 *   flag first and call this second; the scan registers its token first and re-reads idleness
 *   second. Either order then leaves no window where both proceed.
 * * **No lease, no idle-timer touch (A8).** The check never goes through `withLease` and never
 *   calls `recordActivity`, so a long scan cannot keep an idle drive unlocked. Idle-lock,
 *   lock and detach reach it through [detach], which signals the token and then lets
 *   [awaitStopped] wait a bounded time for the native call to return before the volume closes.
 * * **A cancelled scan shows grey, and a write drops a stale verdict.** A cancelled scan has no
 *   verdict. A write request also turns a standing Clean / safe-leaks verdict into
 *   `Unknown(STALE)`: the drive is about to change, so a blue dot would claim something nobody
 *   has checked. The re-check after the work finishes restores it. A blocking verdict (red) is
 *   never dropped: writes are already refused, so nothing is about to change.
 * * **Unknown never gates writes**; only `Corrupt` and `Leaks(writeSafe = false)` do ([Health.blocksWrites]).
 */
class HealthMonitor internal constructor(
    private val scope: CoroutineScope,
    private val dispatcher: CoroutineDispatcher,
    tokens: CancelTokens,
    private val writesIdle: () -> Boolean,
    debounceMs: Long = DEBOUNCE_MS,
    private val stopWaitMs: Long = STOP_WAIT_MS,
) {
    /** Replaceable by tests; production uses the native registry. */
    @Volatile internal var tokens: CancelTokens = tokens

    /** Replaceable by tests. */
    @Volatile internal var debounceMs: Long = debounceMs

    private val _state = MutableStateFlow<Health>(Health.Unknown(HealthUnknownReason.NOT_CHECKED))
    val state: StateFlow<Health> = _state.asStateFlow()

    private val _readOnly = MutableStateFlow(false)

    /** True while the verdict says writes must be refused. Distinct from the session's write-dead flag. */
    val readOnly: StateFlow<Boolean> = _readOnly.asStateFlow()

    private class Scan(val generation: Long, val token: Long) {
        @Volatile var cancelRequested = false
    }

    private class Binding(val check: (Long) -> Health, val setReadOnly: (Boolean) -> Unit)

    private val lock = Any()
    private var generation = 0L
    private var binding: Binding? = null
    private var pending: Job? = null
    private var current: Scan? = null
    private var scanJob: Job? = null

    /**
     * Binds to a freshly unlocked volume. [check] runs the blocking native check with the given
     * cancel token; [setReadOnly] pushes the write gate onto that volume. Schedules the first check.
     */
    fun attach(check: (Long) -> Health, setReadOnly: (Boolean) -> Unit) {
        synchronized(lock) {
            stopLocked()
            generation++
            binding = Binding(check, setReadOnly)
            setStateLocked(Health.Unknown(HealthUnknownReason.NOT_CHECKED))
            scheduleLocked()
        }
    }

    /** Lock, detach, fatal close, idle-lock: cancel everything now. Safe to call repeatedly. */
    fun detach() {
        synchronized(lock) {
            stopLocked()
            generation++
            setStateLocked(Health.Unknown(HealthUnknownReason.NOT_CHECKED))
            binding = null
        }
    }

    /**
     * Waits, bounded, for a scan cancelled by [detach] to leave native code, so the volume and
     * device are not closed under it. Never waits longer than the bound: a hung drive must not
     * hold up a lock.
     */
    suspend fun awaitStopped() {
        val job = synchronized(lock) { scanJob } ?: return
        if (withTimeoutOrNull(stopWaitMs) { job.join() } == null) {
            Trace.e("HealthMonitor: scan still in native code after ${stopWaitMs}ms, closing anyway")
        }
    }

    /**
     * A transfer, browser mutation or SAF write is being requested (or, with [writes] false, a
     * read job that would otherwise wait on the scan). Cancels a running scan and the pending
     * debounce, synchronously and without blocking. See the class note for ordering.
     */
    fun yieldToWriter(writes: Boolean = true) {
        synchronized(lock) {
            if (binding == null) return
            pending?.cancel()
            pending = null
            val scan = current
            if (scan != null) {
                // Idempotent: a request and the lease it takes both land here.
                if (!scan.cancelRequested) {
                    scan.cancelRequested = true
                    tokens.signal(scan.token)
                }
                setStateLocked(Health.Unknown(HealthUnknownReason.CANCELLED))
            } else if (writes && !_state.value.blocksWrites && _state.value !is Health.Unknown) {
                setStateLocked(Health.Unknown(HealthUnknownReason.STALE))
            }
        }
    }

    /** The work that called [yieldToWriter] has finished: schedule a debounced re-check if one is useful. */
    fun noteIdle() {
        synchronized(lock) {
            if (binding == null || current != null) return
            val s = _state.value
            if (s is Health.Unknown && s.reason.retry) scheduleLocked()
        }
    }

    private fun scheduleLocked() {
        pending?.cancel()
        val gen = generation
        pending = scope.launch {
            delay(debounceMs)
            start(gen)
        }
    }

    private fun start(gen: Long) {
        val chk: Binding
        val scan: Scan
        synchronized(lock) {
            val b = binding
            if (b == null || gen != generation || current != null) return
            // Idle is read once the token is registered, after the requester's own busy flag
            // (see the class note), but the token needs creating first.
            val token = tokens.create()
            if (token <= 0L) {
                // Without a token the scan could not be stopped for a writer: do not run it.
                Trace.e("HealthMonitor: no cancel token, check not run")
                setStateLocked(Health.Unknown(HealthUnknownReason.INTERNAL))
                return
            }
            scan = Scan(gen, token)
            current = scan
            chk = b
            if (!writesIdle()) {
                current = null
                tokens.close(token)
                return
            }
            setStateLocked(Health.Checking)
            scanJob = scope.launch(dispatcher) { execute(scan, chk) }
        }
    }

    private fun execute(scan: Scan, b: Binding) {
        var result: Health = Health.Unknown(HealthUnknownReason.INTERNAL)
        try {
            result = b.check(scan.token)
        } catch (c: CancellationException) {
            result = Health.Unknown(HealthUnknownReason.CANCELLED)
            throw c
        } catch (t: Throwable) {
            Trace.e("HealthMonitor: check failed: ${Trace.throwableSummary(t)}")
        } finally {
            synchronized(lock) {
                tokens.close(scan.token)
                if (current === scan) {
                    current = null
                    setStateLocked(if (scan.cancelRequested) Health.Unknown(HealthUnknownReason.CANCELLED) else result)
                    // The request that cancelled us may have called noteIdle() while this scan was
                    // still leaving native code (it returned early then): re-arm here. start()
                    // re-checks idleness, so a still-busy session just skips.
                    if (scan.cancelRequested) scheduleLocked()
                }
            }
        }
    }

    private fun stopLocked() {
        pending?.cancel()
        pending = null
        current?.let {
            it.cancelRequested = true
            tokens.signal(it.token)
        }
        current = null
    }

    private fun setStateLocked(h: Health) {
        _state.value = h
        val ro = h.blocksWrites
        if (ro != _readOnly.value) {
            _readOnly.value = ro
            binding?.setReadOnly?.invoke(ro)
        }
    }

    companion object {
        /** Coalesces the burst of "finished" signals a queue of small jobs produces. */
        const val DEBOUNCE_MS = 2_000L
        const val STOP_WAIT_MS = 2_000L
    }
}
