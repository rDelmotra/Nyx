package dev.luksandroid

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertThrows
import org.junit.Assert.assertTrue
import org.junit.Test

/** The JSON shapes are copied from `bridge::health_json` output (jni unit tests). */
class HealthParseTest {
    @Test
    fun clean() {
        assertEquals(
            Health.Clean,
            parseHealth("""{"elapsedMs":3,"findings":{},"unknown":null,"verdict":"clean","writeSafe":true}"""),
        )
    }

    @Test
    fun leaksCarryWriteSafeAndCounts() {
        val safe = parseHealth("""{"elapsedMs":0,"findings":{"leaked_blocks":1869,"leaked_inodes":1},"unknown":null,"verdict":"leaks","writeSafe":true}""")
        assertEquals(Health.Leaks(true, mapOf("leaked_blocks" to 1869L, "leaked_inodes" to 1L)), safe)
        assertFalse(safe.blocksWrites)
        val unsafe = parseHealth("""{"elapsedMs":0,"findings":{"marked_dirty":1},"unknown":null,"verdict":"leaks","writeSafe":false}""")
        assertTrue(unsafe.blocksWrites)
    }

    @Test
    fun aLeaksVerdictWithoutWriteSafeIsTreatedAsNotSafe() {
        assertTrue(parseHealth("""{"verdict":"leaks","findings":{"leaked_blocks":1}}""").blocksWrites)
    }

    @Test
    fun corruptBlocksWrites() {
        val h = parseHealth("""{"elapsedMs":9,"findings":{"double_owned_block":2},"unknown":null,"verdict":"corrupt","writeSafe":false}""")
        assertEquals(Health.Corrupt(mapOf("double_owned_block" to 2L)), h)
        assertTrue(h.blocksWrites)
    }

    @Test
    fun everyNativeUnknownReasonMapsAndNoneGatesWrites() {
        val expected = mapOf(
            "cancelled" to HealthUnknownReason.CANCELLED,
            "unsupported" to HealthUnknownReason.UNSUPPORTED,
            "too_large" to HealthUnknownReason.TOO_LARGE,
            "busy" to HealthUnknownReason.BUSY,
            "io" to HealthUnknownReason.IO,
            "internal" to HealthUnknownReason.INTERNAL,
        )
        for ((name, reason) in expected) {
            val h = parseHealth("""{"verdict":"unknown","unknown":"$name","writeSafe":false,"findings":{},"elapsedMs":1}""")
            assertEquals(Health.Unknown(reason), h)
            assertFalse(h.blocksWrites)
        }
        // A reason this build has never heard of is INTERNAL, not a friendlier guess.
        assertEquals(Health.Unknown(HealthUnknownReason.INTERNAL), parseHealth("""{"verdict":"unknown","unknown":"new_reason"}"""))
    }

    @Test
    fun garbageThrowsInsteadOfGuessing() {
        assertThrows(Exception::class.java) { parseHealth("not json") }
        assertThrows(IllegalArgumentException::class.java) { parseHealth("""{"verdict":"fine"}""") }
    }
}
