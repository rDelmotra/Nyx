package dev.luksandroid

import java.util.Locale
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class BiometricPartitionKeyTest {

    private fun key(
        vendorId: Int = 0x1234,
        productId: Int = 0x5678,
        serial: String? = "ABC123",
        deviceSizeBytes: Long = 128_000_000_000L,
        offsetBytes: Long = 1_048_576L,
        partitionSizeBytes: Long = 64_000_000_000L,
    ) = biometricPartitionKey(vendorId, productId, serial, deviceSizeBytes, offsetBytes, partitionSizeBytes)

    @Test
    fun `same inputs produce the same key`() {
        assertEquals(key(), key())
    }

    @Test
    fun `matches the v1 hex format`() {
        assertTrue(key().matches(Regex("^v1_[0-9a-f]{32}$")))
    }

    @Test
    fun `changing vendorId changes the key`() {
        assertNotEquals(key(), key(vendorId = 0x0001))
    }

    @Test
    fun `changing productId changes the key`() {
        assertNotEquals(key(), key(productId = 0x0001))
    }

    @Test
    fun `changing serial changes the key`() {
        assertNotEquals(key(), key(serial = "OTHER"))
    }

    @Test
    fun `changing device size changes the key`() {
        assertNotEquals(key(), key(deviceSizeBytes = 1L))
    }

    @Test
    fun `changing partition offset changes the key`() {
        assertNotEquals(key(), key(offsetBytes = 0L))
    }

    @Test
    fun `changing partition size changes the key`() {
        assertNotEquals(key(), key(partitionSizeBytes = 1L))
    }

    @Test
    fun `null serial and empty serial produce the same key`() {
        assertEquals(key(serial = null), key(serial = ""))
    }

    @Test
    fun `stable across locales with non-ASCII digits`() {
        val default = Locale.getDefault()
        try {
            Locale.setDefault(Locale.forLanguageTag("ar"))
            val arabicLocaleKey = key()
            Locale.setDefault(Locale.ROOT)
            val rootLocaleKey = key()
            assertEquals(rootLocaleKey, arabicLocaleKey)
        } finally {
            Locale.setDefault(default)
        }
    }
}
