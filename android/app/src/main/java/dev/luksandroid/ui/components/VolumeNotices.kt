package dev.luksandroid.ui.components

import org.json.JSONObject

/** UI-side decisions for the "no flush" badge and the unreadable-entries listing notice. */
object VolumeNotices {
    const val COMMIT_FAILURE_BANNER = "Some changes may not be saved to the drive"

    /** Warn only when the bridge explicitly reports it cannot flush; unknown (null) shows nothing. */
    fun showNoFlush(flushSupported: Boolean?): Boolean = flushSupported == false

    fun noFlush(infoJson: String?): Boolean = showNoFlush(flushSupported(infoJson))

    fun flushSupported(infoJson: String?): Boolean? = parse(infoJson)?.let {
        if (it.has("flushSupported") && !it.isNull("flushSupported")) it.optBoolean("flushSupported") else null
    }

    /** Count from a listing JSON `"unreadable": N`; null when the field is absent. */
    fun unreadableCount(listingJson: String?): Int? = parse(listingJson)?.let {
        if (it.has("unreadable") && !it.isNull("unreadable")) it.optInt("unreadable") else null
    }

    fun unreadableNotice(count: Int?): String? = when {
        count == null || count <= 0 -> null
        count == 1 -> "1 entry couldn't be read"
        else -> "$count entries couldn't be read"
    }

    private fun parse(s: String?): JSONObject? =
        if (s == null) null else try { JSONObject(s) } catch (_: Exception) { null }
}

/** A fresh reading wins; an unknown (null) reading never erases what we already learned. */
fun mergeFlushSupported(current: Boolean?, fresh: Boolean?): Boolean? = fresh ?: current
