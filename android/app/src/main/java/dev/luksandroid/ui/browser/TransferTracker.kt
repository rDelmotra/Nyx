package dev.luksandroid.ui.browser

import dev.luksandroid.LuksException
import dev.luksandroid.session.TransferItem
import dev.luksandroid.session.TransferState
import dev.luksandroid.session.TransferType
import dev.luksandroid.ui.UiErrorMessage

/**
 * Remembers every transfer the browser started, so a multi-select import/export reports
 * each file instead of only the last one started. Pure logic, no Compose or coroutines:
 * the screen feeds it the latest transfer list and acts on the returned [Update].
 */
class TransferTracker {
    private val pending = LinkedHashSet<Long>()

    class Update(val finished: List<TransferItem>, val activeId: Long?)

    fun track(id: Long) {
        pending.add(id)
    }

    fun isTracking(id: Long): Boolean = id in pending

    /**
     * Returns tracked transfers that reached a terminal state in [list] (each exactly once),
     * and the first still-running tracked transfer, for the inline progress card.
     * Tracked ids missing from [list] (cleared from history) are dropped.
     */
    fun update(list: List<TransferItem>): Update {
        val byId = list.associateBy { it.id }
        val finished = ArrayList<TransferItem>()
        val it = pending.iterator()
        while (it.hasNext()) {
            val item = byId[it.next()]
            if (item == null) {
                it.remove()
            } else if (item.state != TransferState.RUNNING && item.state != TransferState.QUEUED) {
                finished.add(item)
                it.remove()
            }
        }
        val active = list.firstOrNull {
            it.id in pending && (it.state == TransferState.RUNNING || it.state == TransferState.QUEUED)
        }
        return Update(finished, active?.id)
    }
}

object TransferOutcomes {
    /** Snackbar text for a finished transfer, or null when none is wanted. Never includes paths. */
    fun message(item: TransferItem): String? = when (item.state) {
        TransferState.COMPLETED -> when (item.type) {
            TransferType.IMPORT -> "Copied \"${item.name}\" to drive"
            TransferType.EXPORT -> "Saved \"${item.name}\" to phone"
            TransferType.HASH -> null
        }
        TransferState.FAILED -> {
            val label = when (item.type) {
                TransferType.IMPORT -> "Copy to drive"
                TransferType.EXPORT -> "Save to phone"
                TransferType.HASH -> "Integrity check"
            }
            "$label failed: ${UiErrorMessage.getBaseMessage(item.errorCode ?: LuksException.GENERIC)}"
        }
        TransferState.CANCELLED -> "Transfer cancelled"
        else -> null
    }
}
