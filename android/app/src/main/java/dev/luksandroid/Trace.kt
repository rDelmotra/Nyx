package dev.luksandroid

import android.util.Log

/**
 * Production-safe logging.
 *
 * `BuildConfig.DEBUG` gates the lot, so a release build logs nothing at all
 * rather than relying on callers staying disciplined.
 */
object Trace {
    const val TAG = "luks"
    const val TAG_ERR = "luks_err"

    /** Test seam: sees every line the debug build would write, after formatting. */
    @Volatile
    internal var testSink: ((String) -> Unit)? = null

    /**
     * The on-disk copy is a `trace{lvl,msg}` line in the structured event log (F9), the same
     * file as every other event; there is no separate text log any more. The text is exactly
     * what was formatted for logcat, so the privacy rules on callers are unchanged.
     */
    private fun appendToFile(level: String, tag: String, text: String) {
        if (!BuildConfig.DEBUG) return
        testSink?.invoke(text)
        dev.luksandroid.log.EventLog.trace(isError = level == "E", msg = if (tag == TAG) text else "$tag: $text")
    }

    /** Newest part of the on-disk event log, for the diagnostics screen. */
    fun readSessionLog(): String = dev.luksandroid.log.EventLog.readTail(SESSION_LOG_VIEW_BYTES)

    fun clearSessionLog() {
        dev.luksandroid.log.EventLog.clear()
    }

    private const val SESSION_LOG_VIEW_BYTES = 192 * 1024

    @Suppress("DEPRECATION")
    private fun currentThreadTag(): String = "tid=${Thread.currentThread().id}"

    fun i(msg: String) {
        if (BuildConfig.DEBUG) {
            val formatted = "[${currentThreadTag()}] $msg"
            runCatching { Log.i(TAG, formatted) }
            appendToFile("I", TAG, formatted)
        }
    }

    fun i(tag: String, msg: String) {
        if (BuildConfig.DEBUG) {
            val formatted = "[${currentThreadTag()}] $msg"
            runCatching { Log.i(tag, formatted) }
            appendToFile("I", tag, formatted)
        }
    }

    /**
     * Deliberately has no `Throwable`-accepting overload: `Log.e(tag, msg, t)`
     * prints `t`'s own message and stack trace, and `LuksException`/`LuksError`
     * messages can embed drive paths (core/src/error.rs). Passing a throwable
     * used to be a call-site discipline problem; now it's a compile error.
     * Callers that need to note *what* failed should log `t.javaClass.simpleName`
     * and/or a `LuksException.code`, never `t` or `t.message`.
     */
    fun e(msg: String) {
        if (BuildConfig.DEBUG) {
            val formatted = "[${currentThreadTag()}] $msg"
            runCatching { Log.e(TAG, formatted) }
            appendToFile("E", TAG, formatted)
        }
    }

    fun e(tag: String, msg: String) {
        if (BuildConfig.DEBUG) {
            val formatted = "[${currentThreadTag()}] $msg"
            runCatching { Log.e(tag, formatted) }
            appendToFile("E", tag, formatted)
        }
    }

    /**
     * Shape-only detail for [err]. Deliberately has no `String` variant: a
     * filename or path cannot be constructed as an [ErrDetail], so it cannot
     * be passed into [err] — this is enforced by the compiler, not a rule
     * callers have to remember.
     */
    sealed interface ErrDetail {
        data class Bytes(val n: Long) : ErrDetail
        data class Count(val n: Int) : ErrDetail
        data class Offset(val n: Long) : ErrDetail
        data object None : ErrDetail
    }

    private fun renderDetail(detail: ErrDetail): String = when (detail) {
        is ErrDetail.Bytes -> "bytes=${detail.n}"
        is ErrDetail.Count -> "count=${detail.n}"
        is ErrDetail.Offset -> "offset=${detail.n}"
        ErrDetail.None -> ""
    }

    fun formatErr(code: Int, operation: String, detail: ErrDetail = ErrDetail.None): String =
        "code=$code op=$operation ${renderDetail(detail)}".trimEnd()

    /**
     * Production-safe error logging: logs error codes, operations, opcodes, sizes.
     * NEVER logs passphrases, filenames, or directory paths from the encrypted drive.
     *
     * [detail] is structurally shape-only (see [ErrDetail]) so a filename or
     * path cannot be passed here even by mistake.
     */
    fun err(code: Int, operation: String, detail: ErrDetail = ErrDetail.None) {
        if (!BuildConfig.DEBUG) return
        val formatted = "[${currentThreadTag()}] ${formatErr(code, operation, detail)}"
        runCatching { Log.e(TAG_ERR, formatted) }
        appendToFile("E", TAG_ERR, formatted)
    }

    /**
     * Non-identifying summary of a [Throwable] safe to pass into [e]: the
     * exception's class name and, if it's a `LuksException`, its error code.
     * Never the throwable itself, never `.message` — both can embed drive
     * paths (see `LuksError::{NotFound,NotADirectory,IsADirectory,
     * DirectoryNotEmpty}` in core/src/error.rs, surfaced via `Fail::parts()`
     * in jni/src/lib.rs).
     */
    fun throwableSummary(t: Throwable): String {
        val code = (t as? LuksException)?.code
        return if (code != null) "${t.javaClass.simpleName}[code=$code]" else t.javaClass.simpleName
    }

    /**
     * Runs best-effort cleanup (close, delete, unregister). A failure is logged
     * with its [stage] and exception class, never rethrown, never its text:
     * cleanup must not mask the failure the caller is already handling, and
     * must not vanish either. Cancellation always propagates.
     */
    inline fun cleanup(stage: String, block: () -> Unit) {
        try {
            block()
        } catch (c: kotlinx.coroutines.CancellationException) {
            throw c
        } catch (t: Throwable) {
            e("$stage: cleanup failed: ${throwableSummary(t)}")
        }
    }

    /**
     * Retrieve the in-memory bounded forensic log from the native layer.
     */
    fun dumpForensicLog(): String {
        return runCatching { LuksNative.nativeDumpForensicLog() }.getOrDefault("")
    }

    /**
     * Clear the in-memory bounded forensic log.
     */
    fun clearForensicLog() {
        runCatching { LuksNative.nativeClearForensicLog() }
    }
}

