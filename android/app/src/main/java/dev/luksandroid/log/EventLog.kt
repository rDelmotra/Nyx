package dev.luksandroid.log

import android.content.Context
import android.util.Log
import dev.luksandroid.BuildConfig
import dev.luksandroid.LuksException
import org.json.JSONObject
import java.io.File
import java.security.SecureRandom

/** The fixed event vocabulary (C2). A new event is a new entry here, never a free-form string. */
enum class Ev(val wire: String, internal val native: Boolean = false) {
    PROCESS_START("process_start"),
    UNCAUGHT("uncaught"),
    TRACE("trace"),
    DROPPED("dropped"),
    SESSION_HEADER("session_header"),
    UNLOCK("unlock"),
    UNLOCK_FAIL("unlock_fail"),
    LOCK("lock"),
    DETACH("detach"),
    IDLE_LOCK("idle_lock"),
    TRIM_MEMORY("trim_memory"),
    SERVICE_START("service_start"),
    SERVICE_STOP("service_stop"),
    XFER_QUEUED("xfer_queued"),
    XFER_START("xfer_start"),
    XFER_PHASE("xfer_phase"),
    XFER_END("xfer_end"),
    HEALTH("health"),
    HEALTH_FINDING("health_finding"),
    NATIVE_ERR("native_err"),
    NATIVE("native", native = true),
    NATIVE_RING("native_ring", native = true),
    NATIVE_DROPPED("native_dropped", native = true),
}

/**
 * Typed setters for one event. There is deliberately no setter that takes arbitrary text:
 * a path or file name has nowhere to go. String-valued setters accept only the stage grammar
 * and turn anything else into `"invalid"`, counted and reported by the writer.
 */
class EventBuilder @PublishedApi internal constructor() {
    @PublishedApi internal val fields = ArrayList<Field>(8)
    @PublishedApi internal var urgent = false

    /** Last write wins: a line never carries the same key twice, so it is always valid JSON. */
    private fun put(name: String, value: Any) {
        fields.removeAll { it.name == name }
        fields.add(Field(name, value))
    }

    private fun safe(v: String): String =
        if (EventLog.SAFE.matches(v)) v else { EventLog.noteInvalid(); "invalid" }

    private fun header(v: String): String =
        safe(v.lowercase().map { if (it.isLetterOrDigit() || it in "_.:-") it else '_' }.joinToString("").trim('_').take(48))

    fun xfer(v: Long) = put("xfer", v)
    fun ino(v: Long) = put("ino", v)
    fun parent(v: Long) = put("parent", v)
    fun bytes(v: Long) = put("bytes", v)
    fun ms(v: Long) = put("ms", v)
    fun lba(v: Long) = put("lba", v)
    fun blocks(v: Long) = put("blocks", v)
    fun n(v: Long) = put("n", v)
    fun n(v: Int) = put("n", v.toLong())
    fun nseq(v: Long) = put("nseq", v)
    fun code(v: Int) = put("code", v)
    fun errno(v: Int) = put("errno", v)

    fun stage(v: String) = put("stage", safe(v))
    fun state(v: String) { val s = safe(v); if (s == "failed") urgent = true; put("state", s) }
    fun verdict(v: String) = put("verdict", safe(v))
    fun reason(v: String) = put("reason", safe(v))
    fun kind(v: String) = put("kind", safe(v))
    fun op(v: String) = put("op", safe(v))

    fun lvl(error: Boolean) { if (error) urgent = true; put("lvl", if (error) "E" else "I") }

    /** Exception class only. Never the message, never the cause chain's text. */
    fun exc(t: Throwable) {
        val name = t.javaClass.name.substringAfterLast('.')
        put("exc", if (EventLog.CLASS.matches(name)) name else { EventLog.noteInvalid(); "invalid" })
        if (t is LuksException) put("code", t.code)
    }

    fun exc(simpleName: String) =
        put("exc", if (EventLog.CLASS.matches(simpleName)) simpleName else { EventLog.noteInvalid(); "invalid" })

    /** Our own frames only (`dev.luksandroid.*`), at most 12, class + method + line. The exception's text is never read. */
    fun frames(t: Throwable) {
        val out = ArrayList<String>(12)
        var cur: Throwable? = t
        var depth = 0
        while (cur != null && out.size < EventLog.MAX_FRAMES && depth < 8) {
            for (f in cur.stackTrace) {
                if (out.size >= EventLog.MAX_FRAMES) break
                if (!f.className.startsWith("dev.luksandroid.")) continue
                val s = "${f.className.substringAfterLast('.')}.${f.methodName}:${f.lineNumber}"
                if (EventLog.FRAME.matches(s)) out.add(s) else EventLog.noteInvalid()
            }
            cur = cur.cause?.takeIf { it !== cur }
            depth++
        }
        put("frames", out)
    }

    // ---- session_header (C3) ----
    fun app(v: String) = put("app", header(v))
    fun git(v: String) = put("git", header(v))
    fun model(v: String) = put("model", header(v))
    fun sdk(v: Int) = put("sdk", v)
    fun vid(v: Int) = put("vid", v)
    fun pid(v: Int) = put("pid", v)
    fun devBytes(v: Long) = put("dev_bytes", v)
    fun sector(v: Int) = put("sector", v)
    fun flush(v: Boolean) = put("flush", v)
    fun maxXfer(v: Int) = put("max_xfer", v)
    fun fs(v: String) = put("fs", header(v))
    fun cipher(v: String) = put("cipher", header(v))
    fun keyBits(v: Int) = put("key_bits", v)
    fun write(v: Boolean) = put("write", v)

    @PublishedApi internal fun addAll(more: List<Field>) { fields.addAll(more) }
}

/**
 * The structured debug log (F9). Debug builds only: every entry point is a no-op unless
 * [BuildConfig.DEBUG] and a logger is installed ([init], or [installForTest]).
 */
object EventLog {
    const val TAG = "luks"
    const val MAX_FRAMES = 12

    @PublishedApi internal val SAFE = Regex("^[a-z0-9_.:-]{1,48}$")
    @PublishedApi internal val CLASS = Regex("^[A-Za-z0-9_$]{1,64}$")
    @PublishedApi internal val FRAME = Regex("^[A-Za-z0-9_$<>.:-]{1,128}$")
    private val NATIVE_KEY = Regex("^[a-z0-9_]{1,32}$")

    /** Field names a Kotlin-side line may carry, and, with t/seq/sid/src/ev, the pinned C1 set. */
    val ALLOWED_FIELDS: Set<String> = setOf(
        "t", "seq", "sid", "src", "ev",
        "xfer", "ino", "parent", "bytes", "ms", "lba", "blocks", "n", "code", "errno",
        "stage", "exc", "state", "verdict", "reason", "kind", "op", "nseq", "lvl", "msg", "frames",
        "app", "git", "model", "sdk", "vid", "pid", "dev_bytes", "sector", "flush", "max_xfer",
        "fs", "cipher", "key_bits", "write",
    )

    private enum class T { LONG, INT, STR, BOOL, STRL }
    private val FIXED_TYPES = mapOf(
        "xfer" to T.LONG, "ino" to T.LONG, "parent" to T.LONG, "bytes" to T.LONG, "ms" to T.LONG, "lba" to T.LONG,
        "blocks" to T.LONG, "n" to T.LONG, "nseq" to T.LONG, "code" to T.INT, "errno" to T.INT,
        "stage" to T.STR, "state" to T.STR, "verdict" to T.STR, "reason" to T.STR, "kind" to T.STR, "op" to T.STR,
    )
    private val RESERVED = setOf("seq", "sid", "src", "msg", "frames", "lvl", "exc")

    /** Native keys that read as identifying data are refused even when the value fits the grammar. */
    private val BANNED_KEY_WORDS = setOf("name", "path", "file", "filename", "dir", "label", "uuid", "serial", "entry")

    @PublishedApi @Volatile internal var logger: EventLogger? = null
    @Volatile private var sidValue = "-"

    val sid: String get() = sidValue
    val enabled: Boolean get() = BuildConfig.DEBUG && logger != null

    private val rng = SecureRandom()

    /** New session id (8 hex chars) for an unlock. */
    fun newSession(): String {
        val id = "%08x".format(rng.nextInt())
        sidValue = id
        return id
    }

    /** Back to `"-"` once a session has been torn down and its last lines written. */
    fun endSession() { sidValue = "-" }

    fun init(context: Context) {
        if (!BuildConfig.DEBUG) return
        if (logger != null) return
        logger = EventLogger(
            dir = File(context.filesDir, "logs"),
            mirror = { isError, line -> mirrorToLogcat(isError, line) },
            sidProvider = { sidValue },
        )
    }

    private fun mirrorToLogcat(isError: Boolean, line: String) {
        try {
            if (isError) Log.e(TAG, line) else Log.i(TAG, line)
        } catch (_: Throwable) {
            // Logcat is a convenience copy; the file is the record.
        }
    }

    internal fun installForTest(l: EventLogger?) {
        logger = l
        sidValue = "-"
    }

    /** Deterministic session id for fixture generation. */
    internal fun setSidForTest(id: String) { sidValue = id }

    fun invalidCount(): Long = logger?.invalidTotal?.get() ?: 0L

    @PublishedApi internal fun noteInvalid() {
        logger?.let { it.invalidPending.incrementAndGet(); it.invalidTotal.incrementAndGet() }
    }

    /** Emits [ev] with typed fields. Costs a null check when logging is off. */
    inline fun ev(ev: Ev, build: EventBuilder.() -> Unit = {}) {
        if (!BuildConfig.DEBUG) return
        val l = logger ?: return
        val b = EventBuilder()
        b.build()
        submit(l, ev, b.fields, b.urgent)
    }

    @PublishedApi internal fun submit(l: EventLogger, ev: Ev, fields: List<Field>, urgent: Boolean) {
        val urgentEv = urgent || ev == Ev.UNCAUGHT || ev == Ev.UNLOCK_FAIL || ev == Ev.NATIVE_ERR
        l.offer(Entry(l.now(), sidValue, if (ev.native) "rs" else "kt", ev.wire, fields, mirror = true, urgent = urgentEv))
    }

    /** The legacy [dev.luksandroid.Trace] text as a `trace` line. Not mirrored: Trace already wrote logcat. */
    fun trace(isError: Boolean, msg: String) {
        if (!BuildConfig.DEBUG) return
        val l = logger ?: return
        var m = if (msg.length > 512 - 112) msg.substring(0, 512 - 112) else msg
        if (m.isNotEmpty() && Character.isHighSurrogate(m.last())) m = m.dropLast(1)
        val fields = listOf(Field("lvl", if (isError) "E" else "I"), Field("msg", m))
        l.offer(Entry(l.now(), sidValue, "kt", Ev.TRACE.wire, fields, mirror = false, urgent = isError))
    }

    /**
     * A native event drained from Rust (C4). Only numbers, booleans and stage-grammar strings
     * survive; anything else is dropped and counted. `ev` becomes `kind`, native time `t` becomes `nt`.
     */
    fun nativeEvent(ev: Ev, raw: JSONObject, xfer: Long? = null, sid: String? = null, offerTimeoutMs: Long = 0): Boolean {
        if (!BuildConfig.DEBUG) return true
        val l = logger ?: return true
        require(ev.native) { "not a native event" }
        val fields = ArrayList<Field>(raw.length() + 1)
        val seen = HashSet<String>()
        var bad = 0L
        if (xfer != null) { fields.add(Field("xfer", xfer)); seen.add("xfer") }
        val it = raw.keys()
        while (it.hasNext()) {
            val k0 = it.next()
            val k = when (k0) { "ev" -> "kind"; "t" -> "nt"; else -> k0 }
            val v = raw.opt(k0)
            val value: Any? = when {
                k in RESERVED || k == "xfer" && xfer != null -> null
                !NATIVE_KEY.matches(k) || k.split('_').any { it in BANNED_KEY_WORDS } -> null
                !seen.add(k) -> null
                else -> sanitize(k, v)
            }
            if (value == null) bad++ else fields.add(Field(k, value))
            if (fields.size >= 32) { bad += raw.length(); break }
        }
        if (bad > 0) { l.invalidPending.addAndGet(bad); l.invalidTotal.addAndGet(bad) }
        val e = Entry(l.now(), sid ?: sidValue, "rs", ev.wire, fields, mirror = true, urgent = false)
        // The drain thread may wait for queue room (backpressure); callers pass 0 to never wait.
        if (offerTimeoutMs > 0) return l.offerBlocking(e, offerTimeoutMs)
        l.offer(e)
        return true
    }

    /** Lines a producer gave up on without offering (reported by the writer as `dropped{n}`). */
    fun noteDropped(n: Long) { logger?.noteDropped(n) }

    private fun sanitize(key: String, v: Any?): Any? {
        val type = FIXED_TYPES[key]
        return when (v) {
            is Boolean -> if (type == null) v else null
            is String -> if ((type == null || type == T.STR) && SAFE.matches(v)) v else null
            is Int, is Long, is Short, is Byte, is java.math.BigInteger -> {
                val l = (v as Number).let { if (it is java.math.BigInteger) (if (it.bitLength() < 64) it.toLong() else return null) else it.toLong() }
                when (type) {
                    null, T.LONG -> l
                    T.INT -> if (l in Int.MIN_VALUE..Int.MAX_VALUE) l.toInt() else null
                    else -> null
                }
            }
            // org.json yields Double on Android and BigDecimal in the desktop artifact the tests use.
            is Double, is Float, is java.math.BigDecimal -> {
                val d = (v as Number).toDouble()
                if (type == null && d.isFinite()) d else null
            }
            else -> null
        }
    }

    /**
     * `native_err`: a [LuksException] that came out of a JNI call and was caught. Class and code only
     * (the exception's text can name a path). Anything that is not a native error is not logged here;
     * its class already appears on the surrounding `xfer_end` / `unlock_fail`.
     */
    fun nativeErr(op: String, t: Throwable, xfer: Long? = null, stage: String? = null) {
        if (t !is LuksException) return
        ev(Ev.NATIVE_ERR) {
            op(op); exc(t)
            if (xfer != null) xfer(xfer)
            if (stage != null) stage(stage)
        }
    }

    /** Waits (bounded) for queued lines to reach the disk. Used by the crash handler. */
    fun flushSync(timeoutMs: Long = 1_000): Boolean = logger?.flushBlocking(timeoutMs) ?: false

    fun readTail(maxBytes: Int): String = logger?.readTail(maxBytes).orEmpty()

    fun clear(): Boolean = logger?.clearBlocking(2_000) ?: false
}
