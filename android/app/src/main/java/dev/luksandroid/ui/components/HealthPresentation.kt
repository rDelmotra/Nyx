package dev.luksandroid.ui.components

import dev.luksandroid.Health
import dev.luksandroid.HealthUnknownReason
import dev.luksandroid.ui.UiErrorMessage

/** Which light colour a health state gets. The composable maps each role to a theme colour. */
enum class HealthTone { OK, MINOR, ERROR, NEUTRAL }

/** The dot and its short label. [description] is the accessibility text. */
data class HealthBadge(val tone: HealthTone, val label: String, val description: String)

/** What the sheet shows: plain-language [lines] with counts, and the [action] to take. */
data class HealthSheet(val title: String, val lines: List<String>, val action: String)

/**
 * Pure decisions behind the health light, kept free of Compose so they are unit-tested on the JVM
 * (same split as `TypeBadgeColors`). Only finding kinds and counts reach this: no path, name or
 * block number exists to leak.
 */
object HealthPresentation {

    fun badge(h: Health): HealthBadge = when (h) {
        Health.Checking -> HealthBadge(HealthTone.NEUTRAL, "Checking", "Drive health: checking")
        Health.Clean -> HealthBadge(HealthTone.OK, "Healthy", "Drive health: no problems found")
        is Health.Leaks -> HealthBadge(HealthTone.MINOR, "Minor issues", "Drive health: minor issues found")
        is Health.Corrupt -> HealthBadge(HealthTone.ERROR, "Errors", "Drive health: errors found, writing is blocked")
        is Health.Unknown -> HealthBadge(HealthTone.NEUTRAL, unknownLabel(h.reason), "Drive health: ${unknownLabel(h.reason).lowercase()}")
    }

    fun sheet(h: Health): HealthSheet = when (h) {
        Health.Checking -> HealthSheet("Checking the drive", emptyList(), "This takes a moment. Transfers pause the check and it resumes afterwards.")
        Health.Clean -> HealthSheet("No problems found", emptyList(), "Nothing to do.")
        is Health.Leaks -> HealthSheet(
            "Minor issues",
            findingLines(h.findings),
            if (h.writeSafe) {
                "Safe to keep using. Run e2fsck -f (ext4) or btrfs check (btrfs) on Linux to tidy up."
            } else {
                UiErrorMessage.HEALTH_READ_ONLY_MESSAGE
            },
        )
        is Health.Corrupt -> HealthSheet("Errors found", findingLines(h.findings), UiErrorMessage.HEALTH_READ_ONLY_MESSAGE)
        is Health.Unknown -> HealthSheet(unknownLabel(h.reason), emptyList(), unknownAction(h.reason))
    }

    /** One line per finding, most numerous first; a kind this app version does not know is still counted. */
    fun findingLines(findings: Map<String, Long>): List<String> =
        findings.entries
            .filter { it.value > 0 }
            .sortedWith(compareByDescending<Map.Entry<String, Long>> { it.value }.thenBy { it.key })
            .map { findingLine(it.key, it.value) }

    fun findingLine(kind: String, count: Long): String {
        val text = LINES[kind] ?: return if (count == 1L) "1 problem of a kind this app version does not recognise" else "$count problems of a kind this app version does not recognise"
        return (if (count == 1L) text.first else text.second).replace("{n}", count.toString())
    }

    private fun unknownLabel(r: HealthUnknownReason) = when (r) {
        HealthUnknownReason.NOT_CHECKED -> "Not checked"
        HealthUnknownReason.STALE -> "Changed"
        HealthUnknownReason.CANCELLED -> "Paused"
        HealthUnknownReason.UNSUPPORTED -> "Can't check"
        HealthUnknownReason.TOO_LARGE -> "Too big to check"
        HealthUnknownReason.BUSY -> "Not checked"
        HealthUnknownReason.IO -> "Check failed"
        HealthUnknownReason.INTERNAL -> "Check failed"
    }

    private fun unknownAction(r: HealthUnknownReason) = when (r) {
        HealthUnknownReason.NOT_CHECKED -> "The drive is checked shortly after unlocking, when nothing is being copied."
        HealthUnknownReason.STALE -> "Files were changed since the last check. It runs again when the work finishes."
        HealthUnknownReason.CANCELLED -> "The check stopped so a transfer or edit could use the drive. It runs again afterwards."
        HealthUnknownReason.UNSUPPORTED -> "This drive uses a feature the checker does not cover. Writing is not blocked."
        HealthUnknownReason.TOO_LARGE -> "This drive is bigger than the checker can handle. Writing is not blocked."
        HealthUnknownReason.BUSY -> "Unsaved changes were pending. The check runs again once they are saved."
        HealthUnknownReason.IO -> "A read failed during the check. Check the cable; it runs again after the next transfer."
        HealthUnknownReason.INTERNAL -> "The checker failed on this drive. Writing is not blocked."
    }

    /** (singular, plural) wording per native finding kind; `{n}` is the count. */
    private val LINES: Map<String, Pair<String, String>> = mapOf(
        "marked_dirty" to ("The drive was not closed cleanly last time" to "The drive was not closed cleanly last time"),
        "leaked_blocks" to ("1 block is reserved but unused" to "{n} blocks are reserved but unused"),
        "leaked_inodes" to ("1 file record is reserved but unused" to "{n} file records are reserved but unused"),
        "counter_mismatch" to ("1 free-space or link counter is off" to "{n} free-space or link counters are off"),
        "unattached_inode" to ("1 file has data but no name in any folder" to "{n} files have data but no name in any folder"),
        "bitmap_checksum" to ("1 allocation map fails its checksum" to "{n} allocation maps fail their checksums"),
        "group_desc_checksum" to ("1 group header fails its checksum" to "{n} group headers fail their checksums"),
        "leaked_extent_items" to ("1 space record points at nothing" to "{n} space records point at nothing"),
        "mirror_invalid" to ("1 backup copy of the drive header is damaged" to "{n} backup copies of the drive header are damaged"),
        "link_count_low" to ("1 file is linked more often than it says" to "{n} files are linked more often than they say"),
        "block_in_use_but_free" to ("1 block holds file data but is marked free" to "{n} blocks hold file data but are marked free"),
        "inode_in_use_but_free" to ("1 file record is in use but marked free" to "{n} file records are in use but marked free"),
        "double_owned_block" to ("1 block belongs to more than one file" to "{n} blocks belong to more than one file"),
        "dirent_to_free_inode" to ("1 folder entry points at a deleted file" to "{n} folder entries point at deleted files"),
        "inode_checksum" to ("1 file record fails its checksum" to "{n} file records fail their checksums"),
        "extent_checksum" to ("1 file map block fails its checksum" to "{n} file map blocks fail their checksums"),
        "dir_checksum" to ("1 folder block fails its checksum" to "{n} folder blocks fail their checksums"),
        "bad_structure" to ("1 filesystem structure is malformed" to "{n} filesystem structures are malformed"),
        "marked_errors" to ("The filesystem has flagged errors itself" to "The filesystem has flagged errors itself"),
        "empty_leaf" to ("1 metadata node is empty" to "{n} metadata nodes are empty"),
        "block_group_used_mismatch" to ("1 space total is wrong" to "{n} space totals are wrong"),
        "free_space_overlaps_extent" to ("1 free-space range overlaps file data" to "{n} free-space ranges overlap file data"),
        "overlapping_extents" to ("1 data range overlaps another" to "{n} data ranges overlap others"),
        "overlapping_dev_extents" to ("1 device range overlaps another" to "{n} device ranges overlap others"),
        "mirror_mismatch" to ("1 copy of the drive header disagrees with the others" to "{n} copies of the drive header disagree with the others"),
        "unowned_tree_block" to ("1 metadata block is not tracked" to "{n} metadata blocks are not tracked"),
    )
}
