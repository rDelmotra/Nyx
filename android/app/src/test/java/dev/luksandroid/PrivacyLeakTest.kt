package dev.luksandroid

import dev.luksandroid.ui.UiErrorMessage
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File
import java.io.FileNotFoundException
import java.io.IOException

/**
 * F6: a throwable's text can carry a drive path or file name (native errors,
 * SAF, OS). Neither the UI strings nor the debug log may ever contain it.
 */
class PrivacyLeakTest {

    private val secretPath = "/Volumes/secret-folder/tax-return.pdf"
    private val captured = mutableListOf<String>()

    @After
    fun detach() {
        Trace.testSink = null
    }

    private fun assertClean(what: String, text: String) {
        assertFalse("$what leaked a path: $text", text.contains("secret-folder"))
        assertFalse("$what leaked a name: $text", text.contains("tax-return"))
    }

    /** Every throwable family the mapper distinguishes, each carrying the path in its text. */
    @Test
    fun mappedMessageAndLogNeverCarryThrowableText() {
        Trace.testSink = { captured += it }
        val throwables = listOf(
            LuksException("no such file or directory: $secretPath", LuksException.NOT_FOUND),
            LuksException("I/O error on $secretPath", LuksException.IO),
            FileNotFoundException(secretPath),
            IOException("failed: $secretPath"),
            SecurityException(secretPath),
            IllegalStateException(secretPath),
            SessionClosedException(secretPath),
            RuntimeException(secretPath),
        )
        for (t in throwables) {
            for (op in listOf("Import", "")) {
                val msg = UiErrorMessage.getUserMessage(t, op)
                assertClean("user message for ${t.javaClass.simpleName}", msg)
            }
            assertClean("throwableSummary", Trace.throwableSummary(t))
        }
        // Vacuity: the sink really receives the mapper's log lines.
        assertTrue("sink saw nothing", captured.size >= throwables.size)
        for (line in captured) assertClean("log line", line)
        // Control: the raw text does contain the path, so the asserts above can fail.
        assertTrue(throwables.first().message!!.contains("secret-folder"))
    }

    /**
     * Source scan: no main-source code may read a caught throwable's text. The
     * UI maps by code/type ([UiErrorMessage]); logs use [Trace.throwableSummary].
     */
    @Test
    fun noMainSourceReadsAThrowableMessage() {
        var dir = File(System.getProperty("user.dir")).absoluteFile
        var root: File? = null
        repeat(8) {
            if (root == null) {
                listOf("src/main/java/dev/luksandroid", "android/app/src/main/java/dev/luksandroid")
                    .map { File(dir, it) }.firstOrNull { it.isDirectory }?.let { root = it }
                dir = dir.parentFile ?: dir
            }
        }
        val files = requireNotNull(root) { "main sources not found" }
            .walkTopDown().filter { it.isFile && it.extension == "kt" }.toList()
        assertTrue("scan is vacuous: ${files.size} files", files.size > 30)

        val names = "t|e|ex|err|failure|throwable|cause|thrown"
        val patterns = listOf(
            Regex("\\b($names)\\??\\.(message|localizedMessage)\\b"),
            Regex("\\$\\{($names)\\}"),
            Regex("\\$($names)\\b(?!\\.)"),
        )
        val hits = mutableListOf<String>()
        for (f in files) {
            f.readLines().forEachIndexed { i, line ->
                val code = line.trimStart()
                if (code.startsWith("*") || code.startsWith("//") || code.startsWith("/*")) return@forEachIndexed
                if (patterns.any { it.containsMatchIn(line) }) hits += "${f.name}:${i + 1}: ${code.take(100)}"
            }
        }
        assertEquals("throwable text reaches a string:\n" + hits.joinToString("\n"), emptyList<String>(), hits)
    }

    /** A failed cleanup is logged with its stage and class (never its text) and does not mask the caller. */
    @Test
    fun cleanupFailureIsLoggedByStageAndClassAndCancellationPropagates() {
        Trace.testSink = { captured += it }
        Trace.cleanup("close_writer") { throw IOException(secretPath) }
        val line = captured.single { it.contains("close_writer") }
        assertTrue(line, line.contains("IOException"))
        assertClean("cleanup log", line)

        var ran = false
        Trace.cleanup("noop") { ran = true }
        assertTrue(ran)
        assertEquals(1, captured.size)

        var propagated = false
        try {
            Trace.cleanup("cancelled") { throw kotlinx.coroutines.CancellationException("x") }
        } catch (_: kotlinx.coroutines.CancellationException) {
            propagated = true
        }
        assertTrue("cancellation must not be swallowed", propagated)
    }
}
