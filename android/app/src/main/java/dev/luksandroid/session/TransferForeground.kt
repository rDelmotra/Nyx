package dev.luksandroid.session

import dev.luksandroid.Trace
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.launch

/** Starts, updates and stops the transfer foreground service. */
interface ForegroundHost {
    /** Called with the current work count (> 0) on every change while work exists. */
    fun show(count: Int)

    /** Called once when the work count returns to 0. */
    fun hide()
}

/**
 * Keeps a foreground service up exactly while there is work to protect (N1). [busy] is the
 * transfer engine's own signal (queue non-empty, or a SAF write holding the transfer lock),
 * so there is one source of truth; [queueCount] only supplies the number for the notification
 * (at least 1 while busy).
 *
 * Start is immediate; stop is debounced by [stopDelayMs] of continuous idleness. A
 * `startForegroundService()` followed quickly by `stopService()` (a tiny job, or one cancelled
 * while queued) crashes the app when the service has not yet reached `startForeground()`, so
 * the stop must never chase a start. The service additionally stops itself if it finds the
 * engine idle when it starts.
 *
 * A refused start (API 31+ forbids starting a foreground service from the background in
 * most cases, which a SAF write from another app can hit) is logged and survived: the work
 * itself must not depend on the notification.
 */
class TransferForegroundController(
    private val queueCount: Flow<Int>,
    private val busy: Flow<Boolean>,
    private val host: ForegroundHost,
    private val stopDelayMs: Long = DEFAULT_STOP_DELAY_MS,
) {
    fun start(scope: CoroutineScope): Job = scope.launch {
        var active = false
        combine(queueCount, busy) { queued, isBusy -> if (isBusy) maxOf(queued, 1) else 0 }
            .distinctUntilChanged()
            .collectLatest { total ->
                try {
                    if (total > 0) {
                        active = true
                        host.show(total)
                    } else if (active) {
                        // Cancelled by the next emission, so a flap inside the window never stops it.
                        delay(stopDelayMs)
                        active = false
                        host.hide()
                    }
                } catch (c: CancellationException) {
                    throw c
                } catch (t: Throwable) {
                    Trace.e("TransferForeground: host refused (${Trace.throwableSummary(t)})")
                }
            }
    }

    companion object {
        const val DEFAULT_STOP_DELAY_MS = 1_500L
    }
}
