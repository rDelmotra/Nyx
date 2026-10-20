package dev.luksandroid.ui.transfers

import dev.luksandroid.session.TransferItem
import dev.luksandroid.session.TransferState
import dev.luksandroid.session.TransferType
import dev.luksandroid.transfer.VerifyLevel
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class VerifyLabelsTest {
    private fun item(state: TransferState, type: TransferType = TransferType.EXPORT, level: VerifyLevel? = null, verifying: Boolean = false) =
        TransferItem(1, "f", type, 10, 0, 0, 0, state, 0, null, verifyLevel = level, verifying = verifying)

    @Test fun `verifying while running`() =
        assertEquals("Verifying…", VerifyLabels.label(item(TransferState.RUNNING, level = VerifyLevel.FULL, verifying = true)))

    @Test fun `completed labels by level`() {
        assertEquals("Verified (hash)", VerifyLabels.label(item(TransferState.COMPLETED, level = VerifyLevel.FULL)))
        assertEquals("Size checked", VerifyLabels.label(item(TransferState.COMPLETED, level = VerifyLevel.SIZE)))
        assertNull(VerifyLabels.label(item(TransferState.COMPLETED, level = VerifyLevel.NONE)))
        assertNull(VerifyLabels.label(item(TransferState.COMPLETED)))
    }

    @Test fun `no label for failed, cancelled, or non-export`() {
        assertNull(VerifyLabels.label(item(TransferState.FAILED, level = VerifyLevel.FULL)))
        assertNull(VerifyLabels.label(item(TransferState.CANCELLED, level = VerifyLevel.SIZE)))
        assertNull(VerifyLabels.label(item(TransferState.COMPLETED, TransferType.IMPORT, VerifyLevel.FULL)))
    }
}
