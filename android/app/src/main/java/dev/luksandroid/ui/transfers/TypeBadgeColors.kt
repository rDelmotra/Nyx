package dev.luksandroid.ui.transfers

import dev.luksandroid.session.TransferType

enum class BadgeRole { TERTIARY, SECONDARY, SURFACE_VARIANT }

/** Container and on-colour roles for a transfer type badge; content must be the on-colour of container. */
data class BadgeColors(val container: BadgeRole, val content: BadgeRole)

object TypeBadgeColors {
    fun of(type: TransferType): BadgeColors = when (type) {
        TransferType.IMPORT -> BadgeColors(BadgeRole.TERTIARY, BadgeRole.TERTIARY)
        TransferType.EXPORT -> BadgeColors(BadgeRole.SECONDARY, BadgeRole.SECONDARY)
        TransferType.HASH -> BadgeColors(BadgeRole.SURFACE_VARIANT, BadgeRole.SURFACE_VARIANT)
    }
}
