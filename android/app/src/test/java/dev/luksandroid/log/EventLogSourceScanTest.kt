package dev.luksandroid.log

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

/**
 * Structural backstop for the F9 call sites, in the style of TraceLeakTest: the typed builder already
 * has no setter that takes free text, so what is left to police is the few places text can enter.
 */
class EventLogSourceScanTest {

    private fun mainRoot(): File {
        var dir = File(System.getProperty("user.dir")!!).absoluteFile
        repeat(8) {
            listOf("src/main/java/dev/luksandroid", "android/app/src/main/java/dev/luksandroid")
                .map { File(dir, it) }.firstOrNull { it.isDirectory }?.let { return it }
            dir = dir.parentFile ?: dir
        }
        error("main sources not found")
    }

    private fun sources(): List<File> = mainRoot().walkTopDown().filter { it.isFile && it.extension == "kt" }.toList()

    @Test
    fun onlyTraceMayWriteFreeTextTraceLines() {
        val offenders = sources().filter { it.name != "Trace.kt" && it.name != "EventLog.kt" }
            .filter { Regex("EventLog\\.trace\\(").containsMatchIn(it.readText()) }
            .map { it.name }
        assertEquals("EventLog.trace takes free text; only Trace (whose callers are scanned by TraceLeakTest) may call it", emptyList<String>(), offenders)
    }

    @Test
    fun stringSettersOutsideTheLogPackageNeverTakeNameOrPathLikeValues() {
        val banned = Regex("(?i)(name|path|file|dir|entry|documentId|label|uuid|serial)")
        val setter = Regex("\\b(stage|state|reason|op|kind|verdict)\\(([^)]*)\\)")
        val violations = mutableListOf<String>()
        var scanned = 0
        for (f in sources()) {
            if (f.parentFile!!.name == "log") continue
            val text = f.readText()
            // Only inside EventLog.ev { ... } / nativeErr(...) neighbourhoods would be exact; the setter names are
            // distinctive enough in this codebase that scanning the whole file is stricter, not looser.
            for (m in setter.findAll(text)) {
                if (!text.substring(maxOf(0, m.range.first - 400), m.range.first).contains("EventLog.")) continue
                scanned++
                val arg = m.groupValues[2]
                // The two enum renderings (TransferState, HealthUnknownReason): `.name` there is the constant's name.
                if (Regex("^(state|h\\.reason)\\.name\\.lowercase\\(\\s*$").matches(arg.trim())) continue
                if (banned.containsMatchIn(arg)) {
                    violations += "${f.name}:${text.substring(0, m.range.first).count { it == '\n' } + 1} ${m.value}"
                }
            }
        }
        assertTrue("scan is vacuous: $scanned setter calls", scanned >= 10)
        assertEquals(emptyList<String>(), violations)
    }

    @Test
    fun nobodyBuildsFieldsByHandOutsideTheLogPackage() {
        val offenders = sources().filter { it.parentFile!!.name != "log" }
            .filter { Regex("\\bField\\(").containsMatchIn(it.readText()) }.map { it.name }
        assertEquals(emptyList<String>(), offenders)
    }
}
