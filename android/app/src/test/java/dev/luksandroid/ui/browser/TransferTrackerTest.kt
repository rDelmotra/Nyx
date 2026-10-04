package dev.luksandroid.ui.browser

import dev.luksandroid.LuksException
import dev.luksandroid.session.TransferItem
import dev.luksandroid.session.TransferState
import dev.luksandroid.session.TransferType
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class TransferTrackerTest {
    private fun item(id: Long, state: TransferState, type: TransferType = TransferType.IMPORT, code: Int? = null, err: String? = null) =
        TransferItem(id, "f$id", type, 10, 0, 0, 0, state, 0, err, code)

    @Test
    fun `every started transfer reports its own terminal outcome`() {
        val t = TransferTracker()
        t.track(1); t.track(2); t.track(3)
        val u = t.update(listOf(item(1, TransferState.COMPLETED), item(2, TransferState.RUNNING), item(3, TransferState.QUEUED)))
        assertEquals(listOf(1L), u.finished.map { it.id })
        assertEquals(2L, u.activeId)
        val u2 = t.update(listOf(item(1, TransferState.COMPLETED), item(2, TransferState.COMPLETED), item(3, TransferState.FAILED)))
        assertEquals(listOf(2L, 3L), u2.finished.map { it.id })
        assertNull(u2.activeId)
        assertTrue(t.update(listOf(item(2, TransferState.COMPLETED))).finished.isEmpty())
    }

    @Test
    fun `terminal transfer is reported once`() {
        val t = TransferTracker()
        t.track(1)
        assertEquals(1, t.update(listOf(item(1, TransferState.CANCELLED))).finished.size)
        assertEquals(0, t.update(listOf(item(1, TransferState.CANCELLED))).finished.size)
    }

    @Test
    fun `untracked transfers are ignored and vanished ones dropped`() {
        val t = TransferTracker()
        t.track(5)
        val u = t.update(listOf(item(9, TransferState.COMPLETED)))
        assertTrue(u.finished.isEmpty())
        assertNull(u.activeId)
        assertFalse(t.isTracking(5))
    }

    @Test
    fun `transfer already terminal when tracked is still reported`() {
        val t = TransferTracker()
        val snapshot = listOf(item(1, TransferState.COMPLETED))
        t.track(1)
        assertEquals(1, t.update(snapshot).finished.size)
    }

    @Test
    fun `failure message uses code mapping and never raw error text`() {
        val m = TransferOutcomes.message(item(1, TransferState.FAILED, code = LuksException.FILE_CHANGED, err = "/secret/path.txt"))!!
        assertTrue(m.startsWith("Copy to drive failed"))
        assertFalse(m.contains("/secret"))
        val m2 = TransferOutcomes.message(item(1, TransferState.FAILED, TransferType.EXPORT, err = "/secret/x"))!!
        assertFalse(m2.contains("/secret"))
        assertTrue(m2.startsWith("Save to phone failed"))
    }

    @Test
    fun `completed hash has no snackbar`() {
        assertNull(TransferOutcomes.message(item(1, TransferState.COMPLETED, TransferType.HASH)))
        assertEquals("Transfer cancelled", TransferOutcomes.message(item(1, TransferState.CANCELLED)))
    }
}
