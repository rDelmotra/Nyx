package dev.luksandroid.ui.components

import dev.luksandroid.Health
import dev.luksandroid.HealthUnknownReason
import dev.luksandroid.ui.UiErrorMessage
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class HealthPresentationTest {
    @Test
    fun verdictToColourAndLabel() {
        fun tone(h: Health) = HealthPresentation.badge(h).tone
        assertEquals(HealthTone.OK, tone(Health.Clean))
        assertEquals(HealthTone.MINOR, tone(Health.Leaks(true, mapOf("leaked_blocks" to 1))))
        assertEquals(HealthTone.MINOR, tone(Health.Leaks(false, mapOf("marked_dirty" to 1))))
        assertEquals(HealthTone.ERROR, tone(Health.Corrupt(mapOf("double_owned_block" to 1))))
        assertEquals(HealthTone.NEUTRAL, tone(Health.Checking))
        for (r in HealthUnknownReason.values()) assertEquals(HealthTone.NEUTRAL, tone(Health.Unknown(r)))
        assertEquals("Healthy", HealthPresentation.badge(Health.Clean).label)
        assertEquals("Errors", HealthPresentation.badge(Health.Corrupt(emptyMap())).label)
    }

    @Test
    fun everyStateHasADistinctAccessibleDescription() {
        val states = listOf<Health>(Health.Checking, Health.Clean, Health.Leaks(true, emptyMap()), Health.Corrupt(emptyMap())) +
            HealthUnknownReason.values().map { Health.Unknown(it) }
        val descriptions = states.map { HealthPresentation.badge(it).description }
        assertTrue(descriptions.all { it.startsWith("Drive health: ") && it.length > "Drive health: ".length })
        // BUSY/NOT_CHECKED and IO/INTERNAL share a label by design; the rest must differ.
        assertTrue(descriptions.toSet().size >= states.size - 2)
    }

    @Test
    fun theSheetNamesTheActionAndCountsTheFindings() {
        val red = HealthPresentation.sheet(Health.Corrupt(mapOf("double_owned_block" to 2, "dirent_to_free_inode" to 1)))
        assertEquals(UiErrorMessage.HEALTH_READ_ONLY_MESSAGE, red.action)
        assertEquals(listOf("2 blocks belong to more than one file", "1 folder entry points at a deleted file"), red.lines)

        val safe = HealthPresentation.sheet(Health.Leaks(true, mapOf("leaked_blocks" to 1869, "leaked_inodes" to 1)))
        assertEquals(listOf("1869 blocks are reserved but unused", "1 file record is reserved but unused"), safe.lines)
        assertTrue("write-safe leaks do not say writing is refused", safe.action.startsWith("Safe to keep using"))

        val unsafe = HealthPresentation.sheet(Health.Leaks(false, mapOf("marked_dirty" to 1)))
        assertEquals(UiErrorMessage.HEALTH_READ_ONLY_MESSAGE, unsafe.action)
    }

    @Test
    fun anUnknownKindIsStillCounted_andZeroCountsAreHidden() {
        assertEquals(
            listOf("3 problems of a kind this app version does not recognise"),
            HealthPresentation.findingLines(mapOf("future_kind" to 3, "leaked_blocks" to 0)),
        )
        assertFalse(HealthPresentation.sheet(Health.Clean).lines.isNotEmpty())
        assertEquals(
            "This drive has errors. Repair it on Linux (e2fsck -f / btrfs check) before writing.",
            dev.luksandroid.ui.UiErrorMessage.getBaseMessage(dev.luksandroid.LuksException.HEALTH_READ_ONLY),
        )
    }
}
