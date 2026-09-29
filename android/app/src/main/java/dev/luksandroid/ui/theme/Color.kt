package dev.luksandroid.ui.theme

import androidx.compose.ui.graphics.Color

// Calm Security Base Palette
val DarkBackground = Color(0xFF0E1114)
val DarkSurface = Color(0xFF161A1E)
val DarkSurfaceVariant = Color(0xFF1E2328)
val DarkOutline = Color(0xFF262D35)
val DarkOutlineVariant = Color(0xFF38424D)

val TextPrimary = Color(0xFFE6E4DF)
val TextMuted = Color(0xFF8D939B)

// Two States, Two Colors
val StateLocked = Color(0xFF7FA3C4)      // Cool slate: At rest. Nothing is exposed.
val StateUnlocked = Color(0xFFE0B062)    // Warm brass / amber: Awake and open. Pay attention.
val DestructiveRed = Color(0xFFE05D5D)   // Destructive / danger: Rare, keeps its meaning.

// Material Design compatibility tokens
val DarkPrimary = StateLocked
val DarkOnPrimary = Color(0xFF0E1114)
val DarkPrimaryContainer = Color(0xFF1E2328)
val DarkOnPrimaryContainer = StateLocked

val DarkSecondary = StateUnlocked
val DarkOnSecondary = Color(0xFF0E1114)
val DarkSecondaryContainer = Color(0xFF2A2418)
val DarkOnSecondaryContainer = StateUnlocked

val DarkTertiary = Color(0xFF94A3B8)
val DarkOnTertiary = Color(0xFF0E1114)
val DarkTertiaryContainer = Color(0xFF1E2328)
val DarkOnTertiaryContainer = Color(0xFFCBD5E1)

val DarkOnBackground = TextPrimary
val DarkOnSurface = TextPrimary
val DarkOnSurfaceVariant = TextMuted

val DarkError = DestructiveRed
val DarkOnError = Color(0xFF0E1114)
val DarkErrorContainer = Color(0xFF3A1818)
val DarkOnErrorContainer = DestructiveRed

// Semantic compatibility aliases
val SuccessGreen = Color(0xFF4CAF50)
val WarningAmber = StateUnlocked
val InfoCyan = StateLocked
