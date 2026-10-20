package dev.luksandroid.documents

import dev.luksandroid.LuksException
import dev.luksandroid.PartitionInfo
import dev.luksandroid.log.EventLogger
import dev.luksandroid.log.NativeDrain
import dev.luksandroid.log.NativeEventSource
import dev.luksandroid.session.SessionController
import dev.luksandroid.session.TransferManager
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.runBlocking
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File
import dev.luksandroid.log.EventLog as FLog

/** F9 S3: the DocumentsProvider write path (other apps writing into the volume) is logged like any transfer. */
class SafWriteLogTest {
    @get:Rule val tmp = TemporaryFolder()
    private lateinit var logger: EventLogger
    private lateinit var scope: CoroutineScope
    private lateinit var session: SessionController
    private lateinit var vol: Vol
    private val originalDrain = TransferManager.nativeDrain

    private val ringSource = object : NativeEventSource {
        override fun drain() = """{"dropped":0,"events":[]}"""
        override fun ringJson() = """{"dropped":0,"events":[{"nseq":3,"ev":"usb_timeout"}]}"""
    }

    private class Vol : dev.luksandroid.LuksVolume(0L) {
        var throwOnWrite: Throwable? = null
        var throwOnFinish: Throwable? = null
        override val info = dev.luksandroid.VolumeInfo("t", "u", 4096, 1L shl 20, "ext4", emptyList())
        override val canWrite: Boolean get() = true
        override fun commitActiveBatch() {}
        override fun beginFileStreaming(): FileWriter = W()
        override fun writeChunk(writer: FileWriter, data: ByteArray, offset: Int, length: Int) {
            throwOnWrite?.let { throw it }
        }
        override fun finishFile(writer: FileWriter, parentPath: String, name: String): Long {
            throwOnFinish?.let { throw it }
            return 200L
        }
        override fun abandonFile(writer: FileWriter) {}
        inner class W : FileWriter(0L) {
            override fun close() {}
        }
    }

    @Before
    fun setUp() {
        logger = EventLogger(File(tmp.root, "logs"), startThread = false)
        FLog.installForTest(logger)
        TransferManager.nativeDrain = NativeDrain(ringSource, executor = { it.run() })
        scope = CoroutineScope(Dispatchers.Default + SupervisorJob())
        session = SessionController(scope = scope)
        vol = Vol()
        runBlocking { session.startUnlockedForTest(volume = vol, partition = PartitionInfo(0, "p", 0L, 1L shl 20, true, 2)) }
    }

    @After
    fun tearDown() {
        scope.cancel()
        PendingDocuments.clear()
        TransferManager.nativeDrain = originalDrain
        FLog.installForTest(null)
    }

    private fun events(): List<JSONObject> {
        logger.pump()
        return File(tmp.root, "logs/events.0.jsonl").readLines().map { JSONObject(it) }
    }

    private fun noLeak() {
        val text = File(tmp.root, "logs").listFiles()!!.joinToString { it.readText() }
        assertFalse(text, text.contains("secret-folder") || text.contains("tax-return") || text.contains("failed on"))
    }

    @Test
    fun aSuccessfulSafWriteHasQueuedStartPhasesAndCompletedEnd() {
        val doc = PendingDocuments.register("/", "tax-return.pdf")
        val cb = LuksWriteProxyCallback(session = session, documentId = doc)
        cb.onWrite(0, 100, ByteArray(100))
        cb.onWrite(100, 50, ByteArray(50))
        cb.onRelease()
        val e = events()
        assertEquals("saf_write", e.first { it.getString("ev") == "xfer_queued" }.getString("op"))
        assertEquals(1, e.count { it.getString("ev") == "xfer_start" })
        val trail = e.filter { it.getString("ev") == "xfer_phase" }.map { it.getString("stage") + (if (it.has("state")) ":enter" else "") }
        assertEquals(listOf("begin", "finish:enter", "finish", "commit:enter", "commit"), trail)
        val finish = e.first { it.getString("ev") == "xfer_phase" && it.getString("stage") == "finish" && !it.has("state") }
        assertEquals(200L, finish.getLong("ino"))
        assertEquals(150L, finish.getLong("bytes"))
        val end = e.single { it.getString("ev") == "xfer_end" }
        assertEquals("completed", end.getString("state"))
        assertEquals(150L, end.getLong("bytes"))
        assertEquals(0, e.count { it.getString("ev") == "native_ring" })
        noLeak()
    }

    @Test
    fun aFailedChunkLogsNativeErrEndAndTheRingWithoutAPath() {
        val doc = PendingDocuments.register("/", "tax-return.pdf")
        val cb = LuksWriteProxyCallback(session = session, documentId = doc)
        cb.onWrite(0, 10, ByteArray(10))
        vol.throwOnWrite = LuksException("no space left writing /Volumes/secret-folder/tax-return.pdf", LuksException.NO_SPACE)
        runCatching { cb.onWrite(10, 10, ByteArray(10)) }
        val e = events()
        val end = e.single { it.getString("ev") == "xfer_end" }
        assertEquals("failed", end.getString("state"))
        assertEquals(LuksException.NO_SPACE, end.getInt("code"))
        assertEquals("write", end.getString("stage"))
        assertEquals(LuksException.NO_SPACE, e.single { it.getString("ev") == "native_err" }.getInt("code"))
        assertEquals(end.getLong("xfer"), e.single { it.getString("ev") == "native_ring" }.getLong("xfer"))
        noLeak()
    }

    @Test
    fun aFailedFinishLogsStageFinishAndTheRing() {
        val doc = PendingDocuments.register("/", "tax-return.pdf")
        val cb = LuksWriteProxyCallback(session = session, documentId = doc)
        cb.onWrite(0, 10, ByteArray(10))
        vol.throwOnFinish = LuksException("collision at /Volumes/secret-folder/tax-return.pdf", LuksException.ALREADY_EXISTS)
        cb.onRelease()
        val e = events()
        val end = e.single { it.getString("ev") == "xfer_end" }
        assertEquals("failed", end.getString("state"))
        assertEquals("finish", end.getString("stage"))
        assertEquals(1, e.count { it.getString("ev") == "native_ring" })
        assertTrue(e.any { it.getString("ev") == "native_err" })
        noLeak()
    }

    @Test
    fun theMapsAreEmptyAfterSafWrites() {
        val doc = PendingDocuments.register("/", "a.bin")
        LuksWriteProxyCallback(session = session, documentId = doc).also { it.onWrite(0, 1, ByteArray(1)); it.onRelease() }
        assertEquals(0 to 0, TransferManager.logMapSizes())
    }
}
