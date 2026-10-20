package dev.luksandroid

import org.json.JSONObject

/**
 * Why the drive-health light is grey. The native reasons come from `nativeHealthCheck`; the first
 * two are the app's own.
 */
enum class HealthUnknownReason(val retry: Boolean) {
    /** No check has finished yet in this session. */
    NOT_CHECKED(retry = true),

    /** A write may have changed the drive since the last verdict, so that verdict is no longer shown. */
    STALE(retry = true),

    /** The check was stopped (a transfer, edit or lock needed the drive). */
    CANCELLED(retry = true),

    /** A filesystem feature the checker has not been proven against. Deterministic: do not retry. */
    UNSUPPORTED(retry = false),

    /** The drive is larger than the checker's memory budget. Deterministic: do not retry. */
    TOO_LARGE(retry = false),

    /** Uncommitted writes were pending natively. */
    BUSY(retry = true),

    /** A read failed during the check. */
    IO(retry = true),

    /** The checker itself failed (a bug). Not retried: it would fail the same way. */
    INTERNAL(retry = false);

    companion object {
        /** An unrecognised native reason is [INTERNAL]: never guess a friendlier one. */
        fun fromNative(name: String?): HealthUnknownReason = when (name) {
            "cancelled" -> CANCELLED
            "unsupported" -> UNSUPPORTED
            "too_large" -> TOO_LARGE
            "busy" -> BUSY
            "io" -> IO
            else -> INTERNAL
        }
    }
}

/** What the health check currently says about the unlocked drive. */
sealed interface Health {
    data object Checking : Health
    data object Clean : Health

    /** Only leak-class findings. [writeSafe] false means writing on top of them is refused. */
    data class Leaks(val writeSafe: Boolean, val findings: Map<String, Long>) : Health

    /** Something that can lose or mix data. */
    data class Corrupt(val findings: Map<String, Long>) : Health

    data class Unknown(val reason: HealthUnknownReason) : Health

    /** Whether new writes must be refused. Unknown and Checking never block by themselves. */
    val blocksWrites: Boolean
        get() = this is Corrupt || (this is Leaks && !writeSafe)
}

/**
 * Parses `nativeHealthCheck` JSON. Throws on anything that is not that shape rather than
 * guessing: the caller turns a throw into `Unknown(INTERNAL)` and logs it. A leaks verdict
 * without `writeSafe` is treated as not safe (refuse), the cautious reading.
 */
internal fun parseHealth(json: String): Health {
    val o = JSONObject(json)
    val findings = o.optJSONObject("findings")?.let { f ->
        f.keys().asSequence().associateWith { f.getLong(it) }
    } ?: emptyMap()
    return when (val verdict = o.getString("verdict")) {
        "clean" -> Health.Clean
        "leaks" -> Health.Leaks(o.optBoolean("writeSafe", false), findings)
        "corrupt" -> Health.Corrupt(findings)
        "unknown" -> Health.Unknown(HealthUnknownReason.fromNative(o.optString("unknown", "")))
        else -> throw IllegalArgumentException("unknown health verdict (${verdict.length} chars)")
    }
}
