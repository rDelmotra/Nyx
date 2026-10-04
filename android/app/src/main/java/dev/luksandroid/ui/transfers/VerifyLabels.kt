package dev.luksandroid.ui.transfers

import dev.luksandroid.session.TransferItem
import dev.luksandroid.session.TransferState
import dev.luksandroid.session.TransferType
import dev.luksandroid.transfer.VerifyLevel

object VerifyLabels {
    /** Label for an export's verification, or null when there is nothing to say. */
    fun label(item: TransferItem): String? {
        if (item.type != TransferType.EXPORT) return null
        if (item.verifying && item.state == TransferState.RUNNING) return "Verifying…"
        if (item.state != TransferState.COMPLETED) return null
        return when (item.verifyLevel) {
            VerifyLevel.FULL -> "Verified (hash)"
            VerifyLevel.SIZE -> "Size checked"
            null -> null
        }
    }
}
