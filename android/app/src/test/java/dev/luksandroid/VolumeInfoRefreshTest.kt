package dev.luksandroid

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class VolumeInfoRefreshTest {
    private class V : LuksVolume(0L) {
        var flush: String = "null"
        var infoCalls = 0
        init {
            LuksVolume::class.java.getDeclaredField("handle").also { it.isAccessible = true }.setLong(this, 1L)
            LuksVolume::class.java.getDeclaredField("cachedInfo").also { it.isAccessible = true }
                .set(this, parseVolumeInfo(readInfoJson()))
        }
        override fun readInfoJson(): String { infoCalls++; return """{"label":"x","flushSupported":$flush}""" }
        override fun nativeCommit() { flush = "false" }
    }

    @Test
    fun `flushSupported flips after the first commit, with one info read per commit`() {
        val v = V()
        assertNull(v.info.flushSupported)
        val before = v.infoCalls
        v.commitActiveBatch()
        assertEquals(false, v.info.flushSupported)
        assertEquals(1, v.infoCalls - before)
    }

    @Test
    fun `refresh stops once flushSupported is known`() {
        val v = V()
        v.commitActiveBatch() // null -> false
        val after = v.infoCalls
        v.commitActiveBatch()
        v.commitActiveBatch()
        assertEquals(after, v.infoCalls)
    }
}

class WriteDurabilityRefreshTest {
    private class V : LuksVolume(0L) {
        var mode: String = "\"cache_off\""
        var infoCalls = 0
        init {
            LuksVolume::class.java.getDeclaredField("handle").also { it.isAccessible = true }.setLong(this, 1L)
            LuksVolume::class.java.getDeclaredField("cachedInfo").also { it.isAccessible = true }
                .set(this, parseVolumeInfo(readInfoJson()))
        }
        override fun readInfoJson(): String { infoCalls++; return """{"label":"x","flushSupported":false,"writeDurability":$mode}""" }
        override fun nativeCommit() {}
    }

    @Test
    fun `cache_off keeps being re-read so a mid-session downgrade shows up`() {
        val v = V()
        assertEquals("cache_off", v.info.writeDurability)
        v.mode = "\"unverified\""
        v.commitActiveBatch()
        assertEquals("unverified", v.info.writeDurability)
        val after = v.infoCalls
        v.commitActiveBatch()
        assertEquals(after, v.infoCalls)
    }
}
