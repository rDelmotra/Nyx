package dev.luksandroid.ui.browser

import dev.luksandroid.LuksException
import dev.luksandroid.ui.UiErrorMessage

/** Per-item outcome of a batch delete run inside one exclusive write. Holds codes only, never paths. */
class BatchDeleteResult(val total: Int, val deleted: Int, val failureCodes: List<Int>) {
    val failed: Int get() = failureCodes.size

    fun message(): String = when {
        failed == 0 -> if (deleted == 1) "Deleted 1 item" else "Deleted $deleted items"
        else -> "Deleted $deleted of $total. ${failed} failed: " +
            UiErrorMessage.getBaseMessage(failureCodes.first())
    }
}

private fun isFatal(code: Int) = code == LuksException.MUTEX_POISONED ||
    code == LuksException.PANIC || code == LuksException.WRITE_SESSION_FENCED

/**
 * Deletes every item, recording per-item failures and carrying on. Fatal write failures and
 * cancellation propagate immediately (the engine fences the session on them).
 */
fun <T> runBatchDelete(items: List<T>, delete: (T) -> Unit): BatchDeleteResult {
    var deleted = 0
    val codes = ArrayList<Int>()
    for (item in items) {
        try {
            delete(item)
            deleted++
        } catch (e: LuksException) {
            if (isFatal(e.code) || e.code == LuksException.CANCELLED) throw e
            codes.add(e.code)
        }
    }
    return BatchDeleteResult(items.size, deleted, codes)
}
