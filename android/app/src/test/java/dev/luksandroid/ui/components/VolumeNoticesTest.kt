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
