package dev.luksandroid.ui.transfers

import dev.luksandroid.session.TransferItem
import dev.luksandroid.session.TransferState

/**
 * Where a folder transfer stopped, shown on its failed or cancelled card.
 * Screen only: the path is the user's own tree and never goes into a log or
 * [TransferItem.error].
 */
object StoppedAtLabel {
    fun label(item: TransferItem): String? {
        if (item.state != TransferState.FAILED && item.state != TransferState.CANCELLED) return null
        if (item.filesTotal == 0) return null
        val path = item.currentPath?.takeIf { it.isNotBlank() } ?: return null
        return "Stopped at: $path (${item.filesDone} of ${item.filesTotal} done)"
    }
}
