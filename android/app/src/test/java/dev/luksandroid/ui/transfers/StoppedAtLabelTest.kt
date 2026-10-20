package dev.luksandroid.ui.transfers

import dev.luksandroid.session.TransferItem
import dev.luksandroid.session.TransferState
import dev.luksandroid.session.TransferType
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class StoppedAtLabelTest {
    private fun item(state: TransferState, path: String? = "Photos/2024/a.jpg", total: Int = 10) =
        TransferItem(1, "Photos", TransferType.IMPORT, 10, 0, 0, 0, state, 0, "Import stopped",
            filesDone = 3, filesTotal = total, currentPath = path)

    @Test fun `failed and cancelled folder transfers name the file`() {
        assertEquals("Stopped at: Photos/2024/a.jpg (3 of 10 done)", StoppedAtLabel.label(item(TransferState.FAILED)))
        assertEquals("Stopped at: Photos/2024/a.jpg (3 of 10 done)", StoppedAtLabel.label(item(TransferState.CANCELLED)))
    }

    @Test fun `nothing for running, completed, single files, or no path`() {
        assertNull(StoppedAtLabel.label(item(TransferState.RUNNING)))
        assertNull(StoppedAtLabel.label(item(TransferState.COMPLETED)))
        assertNull(StoppedAtLabel.label(item(TransferState.FAILED, total = 0)))
        assertNull(StoppedAtLabel.label(item(TransferState.FAILED, path = null)))
    }
}
