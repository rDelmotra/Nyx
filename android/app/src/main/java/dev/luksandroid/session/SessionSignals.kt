package dev.luksandroid.session

/**
 * What the session needs to tell the user outside the app's own UI (notification
 * shade), because the failure happens after the screen that caused it is gone:
 * a SAF client already closed its descriptor, or the drive was pulled.
 *
 * Deliberately carries no file names or paths. These end up on the lock screen,
 * and the names inside an encrypted volume are exactly what the volume hides.
 */
sealed interface SessionNotice {
    /**
     * The final commit or close failed. [unsavedFiles] counts files reported
     * finished since the last successful commit (0 = unknown / none counted).
     */
    data class FilesMayNotBeSaved(val unsavedFiles: Int) : SessionNotice

    /** A SAF client's file could not be finished (the client saw a successful close). */
    data object SafFinishFailed : SessionNotice

    /** A SAF file was finished but the commit that makes it durable failed. */
    data object SafCommitFailed : SessionNotice
}

/** Sink for [SessionNotice]. The Android implementation posts a notification. */
fun interface SessionNotifier {
    fun notify(notice: SessionNotice)

    companion object {
        val None = SessionNotifier { }
    }
}

/**
 * Session-level banner state: the last teardown could not prove everything it had
 * reported as done is on the drive. Survives Locked/Detached so the UI can show it
 * after the volume is gone; cleared by [SessionController.clearCloseWarning] or the
 * next successful unlock.
 */
data class CloseWarning(
    /** Files reported finished since the last successful commit; 0 when unknown. */
    val unsavedFiles: Int,
    /** Native error code of the failure that caused the warning, or null. */
    val code: Int?,
)

/**
 * Handle for "a SAF write is open". While held, the session counts as busy (so a
 * destroyed last activity or a trim callback does not lock mid-write) and the
 * foreground service keeps running. [release] is idempotent.
 */
interface WriteHold {
    fun release()
}

/**
 * Where fatal native write errors are reported by code that catches them inside its
 * own lease block (which would otherwise hide them from [SessionController.withLease]).
 * Implemented by [SessionController]; E1's transfer code calls it before swallowing.
 */
fun interface WriteFailureSink {
    /** Returns true if [t] was fatal and the session is now write-dead. */
    fun reportWriteFailure(t: Throwable): Boolean
}
