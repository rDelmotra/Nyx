package dev.luksandroid.log

/**
 * Uncaught-exception hook (F9 C2): writes `uncaught{exc, frames}` (class name, code if it is a
 * `LuksException`, and our own `dev.luksandroid.*` frames; never the message or any cause text),
 * waits briefly for it to reach the disk, then hands the throwable to the previous handler so
 * the platform's crash handling is unchanged.
 */
class CrashLogger(
    private val previous: Thread.UncaughtExceptionHandler?,
    private val flush: () -> Unit = { EventLog.flushSync(1_000) },
) : Thread.UncaughtExceptionHandler {

    override fun uncaughtException(t: Thread, e: Throwable) {
        try {
            EventLog.ev(Ev.UNCAUGHT) { exc(e); frames(e) }
            flush()
        } catch (_: Throwable) {
            // A failure of the logger must not replace the crash it was describing: the original
            // is still delivered to the previous handler below, which is the record that matters.
        }
        previous?.uncaughtException(t, e)
    }

    companion object {
        /** Installs itself in front of the current default handler. Idempotent per process. */
        fun install() {
            val current = Thread.getDefaultUncaughtExceptionHandler()
            if (current is CrashLogger) return
            Thread.setDefaultUncaughtExceptionHandler(CrashLogger(current))
        }
    }
}
