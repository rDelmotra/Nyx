package dev.luksandroid.log

import dev.luksandroid.LuksNative
import org.json.JSONArray
import org.json.JSONObject
import java.util.concurrent.Executors
import java.util.concurrent.atomic.AtomicInteger

/** Where native events come from. A seam: production is [LuksNativeEventSource], tests script it. */
interface NativeEventSource {
    fun drain(): String
    fun ringJson(): String
}

internal object LuksNativeEventSource : NativeEventSource {
    override fun drain(): String = LuksNative.nativeDrainEvents()
    override fun ringJson(): String = LuksNative.nativeRingJson()
}

/**
 * Moves Rust's event timeline into the event log (F9 C4).
 *
 * Never throws and never stalls a caller: [requestDrain] / [onTransferFailure] hand the native
 * calls to a single background thread (so drains also stay ordered), and everything that can go
 * wrong is logged as `native_err` instead. An `.so` that predates F9 has no such symbols: that
 * `UnsatisfiedLinkError` is logged once and drains are switched off for the process. Output
 * that is not the documented JSON is logged (class name only) and, after
 * [MAX_CONSECUTIVE_FAILURES] in a row, also switches drains off, so a broken native side cannot
 * flood the log every two seconds.
 */
class NativeDrain internal constructor(
    private val source: NativeEventSource,
    private val executor: (Runnable) -> Unit,
    /** Longest the drain thread waits for room in the writer queue, per line, before giving up on the rest of a batch. */
    private val offerTimeoutMs: Long = OFFER_TIMEOUT_MS,
) {
    companion object {
        const val MAX_CONSECUTIVE_FAILURES = 10
        const val PERIOD_MS = 2_000L
        const val OFFER_TIMEOUT_MS = 250L

        private val pool by lazy {
            Executors.newSingleThreadExecutor { r -> Thread(r, "luks-native-drain").apply { isDaemon = true } }
        }

        /** The process-wide instance, backed by the real JNI functions. */
        val default: NativeDrain by lazy { NativeDrain(LuksNativeEventSource, { pool.execute(it) }) }
    }

    @Volatile var disabled = false
        private set
    private val consecutiveFailures = AtomicInteger(0)

    /** Drains the notable queue on the executor thread. [sid] defaults to the session current *now*, at request time. */
    fun requestDrain(xfer: Long? = null) {
        if (!enabled()) return
        // Coalesced: a burst of requests (one per finished file) is one queued drain, not hundreds.
        if (!drainQueued.compareAndSet(false, true)) return
        val sid = EventLog.sid
        executor(Runnable {
            drainQueued.set(false)
            drainNow(xfer, sid)
        })
    }

    private val drainQueued = java.util.concurrent.atomic.AtomicBoolean(false)

    /**
     * A transfer failed: drain what is queued, then write the whole ring tagged with [xfer] (the ring is
     * not cleared, so the failing moment is still there even if it was never "notable").
     */
    fun onTransferFailure(xfer: Long?) {
        if (!enabled()) return
        val sid = EventLog.sid
        executor(Runnable {
            drainNow(null, sid)
            dumpRing(xfer, sid)
        })
    }

    private fun enabled() = !disabled && EventLog.enabled

    fun drainNow(xfer: Long? = null, sid: String? = null) {
        if (!enabled()) return
        val json = guarded("drain") { source.drain() } ?: return
        parse(json, Ev.NATIVE, xfer, sid, reportDropped = true)
    }

    fun dumpRing(xfer: Long?, sid: String? = null) {
        if (!enabled()) return
        val json = guarded("ring") { source.ringJson() } ?: return
        parse(json, Ev.NATIVE_RING, xfer, sid, reportDropped = false)
    }

    private inline fun <T> guarded(what: String, call: () -> T): T? {
        return try {
            call()
        } catch (t: UnsatisfiedLinkError) {
            disabled = true
            EventLog.ev(Ev.NATIVE_ERR) { op(what); exc(t); stage("no_native_symbol") }
            null
        } catch (t: Throwable) {
            fail(what, t)
            null
        }
    }

    private fun fail(what: String, t: Throwable) {
        val n = consecutiveFailures.incrementAndGet()
        val stop = n >= MAX_CONSECUTIVE_FAILURES
        if (stop) disabled = true
        EventLog.ev(Ev.NATIVE_ERR) { op(what); exc(t); stage(if (stop) "disabled" else "parse") }
    }

    private fun parse(json: String, ev: Ev, xfer: Long?, sid: String?, reportDropped: Boolean) {
        try {
            val root = JSONObject(json)
            val events: JSONArray = root.getJSONArray("events")
            val dropped = root.optLong("dropped", 0L)
            // Parse everything before emitting anything: a half-valid payload is one error, not half a timeline.
            val parsed = ArrayList<JSONObject>(events.length())
            for (i in 0 until events.length()) parsed.add(events.getJSONObject(i))
            consecutiveFailures.set(0)
            if (reportDropped && dropped > 0) EventLog.ev(Ev.NATIVE_DROPPED) { n(dropped) }
            // Backpressure, on this (drain) thread only: wait for room per line; on a timeout give up on the rest
            // of the batch and count it, so the writer queue is never flooded and nothing is lost silently.
            for ((i, o) in parsed.withIndex()) {
                if (!EventLog.nativeEvent(ev, o, xfer, sid, offerTimeoutMs)) {
                    EventLog.noteDropped((parsed.size - i - 1).toLong())
                    break
                }
            }
        } catch (t: Throwable) {
            fail(if (reportDropped) "drain" else "ring", t)
        }
    }
}
