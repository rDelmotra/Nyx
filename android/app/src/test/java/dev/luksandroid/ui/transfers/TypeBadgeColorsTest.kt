package dev.luksandroid.ui.transfers

import dev.luksandroid.session.TransferType
import org.junit.Assert.assertEquals
import org.junit.Test

class TypeBadgeColorsTest {
    @Test
    fun `on-colour pairs with its container`() {
        assertEquals(BadgeRole.TERTIARY, TypeBadgeColors.of(TransferType.IMPORT).container)
        assertEquals(BadgeRole.TERTIARY, TypeBadgeColors.of(TransferType.IMPORT).content)
        assertEquals(BadgeRole.SECONDARY, TypeBadgeColors.of(TransferType.EXPORT).container)
        assertEquals(BadgeRole.SECONDARY, TypeBadgeColors.of(TransferType.EXPORT).content)
        assertEquals(BadgeRole.SURFACE_VARIANT, TypeBadgeColors.of(TransferType.HASH).content)
    }
}
