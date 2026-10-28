package dev.luksandroid.ui.components

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class VolumeNoticesTest {
    @Test
    fun `no flush badge only on explicit false`() {
        assertTrue(VolumeNotices.noFlush("""{"label":"x","flushSupported":false}"""))
        assertFalse(VolumeNotices.noFlush("""{"flushSupported":true}"""))
        assertFalse(VolumeNotices.noFlush("""{"label":"x"}"""))
        assertFalse(VolumeNotices.noFlush(null))
        assertFalse(VolumeNotices.noFlush("not json"))
        assertFalse(VolumeNotices.noFlush("""{"flushSupported":"false"}""".replace("\"false\"}", "null}")))
    }

    @Test
    fun `unreadable count parsing`() {
        assertEquals(3, VolumeNotices.unreadableCount("""{"entries":[],"unreadable":3}"""))
        assertNull(VolumeNotices.unreadableCount("""{"entries":[]}"""))
        assertNull(VolumeNotices.unreadableCount("garbage"))
        assertNull(VolumeNotices.unreadableCount(null))
    }

    @Test
    fun `unreadable notice text`() {
        assertNull(VolumeNotices.unreadableNotice(null))
        assertNull(VolumeNotices.unreadableNotice(0))
        assertEquals("1 entry couldn't be read", VolumeNotices.unreadableNotice(1))
        assertEquals("7 entries couldn't be read", VolumeNotices.unreadableNotice(7))
    }
}

class SessionWritingTest {
    @Test fun `write hold alone counts as writing`() {
        assertTrue(isSessionWriting(0, 1))
        assertTrue(isSessionWriting(1, 0))
        assertFalse(isSessionWriting(0, 0))
    }
}

class MergeFlushTest {
    @Test fun `fresh reading wins and null keeps old`() {
        assertEquals(false, mergeFlushSupported(null, false))
        assertEquals(true, mergeFlushSupported(false, true))
        assertEquals(false, mergeFlushSupported(false, null))
        assertNull(mergeFlushSupported(null, null))
    }
}

class WriteDurabilityNoticeTest {
    @Test fun `orange dot only when unverified`() {
        assertTrue(VolumeNotices.showUnverifiedDot("unverified"))
        for (m in listOf("flush", "write_through", "cache_off", null)) assertFalse(VolumeNotices.showUnverifiedDot(m))
    }

    @Test fun `safe slow note only when the cache was switched off`() {
        assertTrue(VolumeNotices.showSafeSlowNote("cache_off"))
        for (m in listOf("flush", "write_through", "unverified", null)) assertFalse(VolumeNotices.showSafeSlowNote(m))
    }

    @Test fun `old no-flush banner only when the native layer reports no mode`() {
        assertTrue(VolumeNotices.showNoFlush(false, null))
        assertFalse(VolumeNotices.showNoFlush(false, "cache_off"))
        assertFalse(VolumeNotices.showNoFlush(false, "unverified"))
        assertFalse(VolumeNotices.showNoFlush(null, null))
    }
}
