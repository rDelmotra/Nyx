package dev.luksandroid.ui.browser

import dev.luksandroid.LuksException
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

class BatchDeleteTest {
    @Test fun `continues past a failed item and reports counts`() {
        val r = runBatchDelete(listOf("/a", "/b", "/c")) {
            if (it == "/b") throw LuksException("/b secret", LuksException.NOT_FOUND)
        }
        assertEquals(2, r.deleted); assertEquals(1, r.failed)
        assertTrue(r.message().startsWith("Deleted 2 of 3"))
        assertFalse(r.message().contains("/b"))
    }

    @Test fun `all ok message`() {
        assertEquals("Deleted 1 item", runBatchDelete(listOf(1)) {}.message())
        assertEquals("Deleted 2 items", runBatchDelete(listOf(1, 2)) {}.message())
    }

    @Test fun `fatal failure aborts the batch`() {
        var seen = 0
        try {
            runBatchDelete(listOf(1, 2, 3)) { seen++; if (it == 2) throw LuksException("x", LuksException.WRITE_SESSION_FENCED) }
            fail()
        } catch (e: LuksException) { assertEquals(2, seen) }
    }
}
