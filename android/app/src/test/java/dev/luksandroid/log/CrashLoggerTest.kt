package dev.luksandroid.log

import dev.luksandroid.LuksException
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

class CrashLoggerTest {
    @get:Rule val tmp = TemporaryFolder()
    private lateinit var log: EventLogger

    @Before
    fun setUp() {
        log = EventLogger(File(tmp.root, "logs"), startThread = false)
        EventLog.installForTest(log)
    }

    @After
    fun tearDown() = EventLog.installForTest(null)

    private fun lines(): List<JSONObject> {
        log.pump()
        return File(tmp.root, "logs/events.0.jsonl").readLines().map { JSONObject(it) }
    }

    private fun deep(depth: Int, path: String): Nothing =
        if (depth == 0) throw IllegalStateException("cannot open $path") else deep(depth - 1, path)

    @Test
    fun logsClassAndOurFramesNeverTheMessageAndChains() {
        val secret = "/Volumes/secret-folder/tax-return.pdf"
        val boom = try { deep(20, secret) } catch (e: IllegalStateException) { e }
        var chainedWith: Throwable? = null
        var flushed = false
        val handler = CrashLogger(previous = { _, e -> chainedWith = e }, flush = { flushed = true })

        handler.uncaughtException(Thread.currentThread(), boom)

        val line = lines().single()
        assertEquals("uncaught", line.getString("ev"))
        assertEquals("IllegalStateException", line.getString("exc"))
        val frames = line.getJSONArray("frames")
        assertTrue(frames.length() in 1..12)
        assertEquals(12, frames.length()) // the recursion alone exceeds the cap
        for (i in 0 until frames.length()) {
            val f = frames.getString(i)
            assertTrue(f, f.matches(Regex("[A-Za-z0-9_$<>.]+:\\d+")))
        }
        assertTrue(frames.getString(0).startsWith("CrashLoggerTest.deep:"))
        val text = line.toString()
        assertFalse(text, text.contains("secret"))
        assertFalse(text, text.contains("tax-return"))
        assertFalse(text, text.contains("cannot open"))
        assertSame(boom, chainedWith)
        assertTrue("flushed before chaining", flushed)
    }

    @Test
    fun onlyDevLuksandroidFramesAreKept() {
        val e = RuntimeException("x").apply {
            stackTrace = arrayOf(
                StackTraceElement("java.util.ArrayList", "get", "ArrayList.java", 427),
                StackTraceElement("dev.luksandroid.session.TransferController\$importFile\$1", "invokeSuspend", "TransferManager.kt", 812),
                StackTraceElement("kotlinx.coroutines.DispatchedTask", "run", "DispatchedTask.kt", 100),
                StackTraceElement("dev.luksandroid.LuksVolume", "<init>", "LuksHandles.kt", 5),
            )
        }
        CrashLogger(previous = null, flush = {}).uncaughtException(Thread.currentThread(), e)
        val frames = lines().single().getJSONArray("frames")
        assertEquals(2, frames.length())
        assertEquals("TransferController\$importFile\$1.invokeSuspend:812", frames.getString(0))
        assertEquals("LuksVolume.<init>:5", frames.getString(1))
    }

    @Test
    fun luksExceptionCarriesItsCodeButNotItsPath() {
        val e = LuksException("no such file /Volumes/secret-folder/x", LuksException.NOT_FOUND)
        CrashLogger(previous = null, flush = {}).uncaughtException(Thread.currentThread(), e)
        val l = lines().single()
        assertEquals("LuksException", l.getString("exc"))
        assertEquals(LuksException.NOT_FOUND, l.getInt("code"))
        assertFalse(l.toString().contains("secret"))
    }

    @Test
    fun aFailingLoggerStillChainsToThePreviousHandler() {
        EventLog.installForTest(null)
        var chained = false
        CrashLogger(previous = { _, _ -> chained = true }, flush = { throw IllegalStateException("flush broke") })
            .uncaughtException(Thread.currentThread(), RuntimeException("x"))
        assertTrue(chained)
    }
}
