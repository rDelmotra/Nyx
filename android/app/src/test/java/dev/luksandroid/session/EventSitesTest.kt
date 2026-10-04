package dev.luksandroid.session

import android.content.Context
import android.content.ContextWrapper
import android.net.Uri
import dev.luksandroid.Health
import dev.luksandroid.HealthUnknownReason
import dev.luksandroid.LuksException
import dev.luksandroid.LuksVolume
import dev.luksandroid.StatFsInfo
import dev.luksandroid.VolumeInfo
import dev.luksandroid.log.Ev
import dev.luksandroid.log.EventLogger
import dev.luksandroid.log.NativeDrain
import dev.luksandroid.log.NativeEventSource
import dev.luksandroid.transfer.ExportTarget
import dev.luksandroid.transfer.ImportSource
import dev.luksandroid.transfer.ImportSourceInfo
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.InputStream
import java.io.OutputStream
import java.nio.ByteBuffer
import java.nio.channels.Channels
import java.nio.channels.ReadableByteChannel
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.atomic.AtomicInteger
import dev.luksandroid.log.EventLog as FLog

/**
 * F9: every Kotlin log site, driven through the real session / transfer / health code, with
 * path-bearing exceptions so a leak at any site shows up in the file.
 */
class EventSitesTest {

    @get:Rule val tmp = TemporaryFolder()
    private lateinit var logger: EventLogger
    private lateinit var scope: CoroutineScope

    private val secretPath = "/Volumes/secret-folder/tax-return.pdf"
    private fun pathExc(code: Int = LuksException.NOT_FOUND) = LuksException("no such file or directory: $secretPath", code)

    @Before
    fun setUp() {
        logger = EventLogger(File(tmp.root, "logs"), startThread = false)
        FLog.installForTest(logger)
        scope = CoroutineScope(Dispatchers.Default + SupervisorJob())
    }

    @After
    fun tearDown() {
        scope.cancel()
        runBlocking { LuksSession.lock() }
        FLog.installForTest(null)
        val field = TransferController::class.java.getDeclaredField("_transfers")
        field.isAccessible = true
        @Suppress("UNCHECKED_CAST")
        (field.get(TransferManager) as kotlinx.coroutines.flow.MutableStateFlow<List<TransferItem>>).value = emptyList()
    }

    private fun events(): List<JSONObject> {
        logger.pump()
        val f = File(tmp.root, "logs/events.0.jsonl")
        return if (f.exists()) f.readLines().map { JSONObject(it) } else emptyList()
    }

    private fun List<JSONObject>.ev(name: String) = filter { it.getString("ev") == name }

    private fun awaitEvents(timeoutMs: Long = 5_000, pred: (List<JSONObject>) -> Boolean): List<JSONObject> {
        val deadline = System.currentTimeMillis() + timeoutMs
        while (true) {
            val e = events()
            if (pred(e)) return e
            if (System.currentTimeMillis() > deadline) throw AssertionError("timed out waiting for events; have ${e.map { it.getString("ev") }}")
            Thread.sleep(5)
        }
    }

    private fun assertNoLeak(what: String) {
        val text = File(tmp.root, "logs").listFiles().orEmpty().joinToString("\n") { it.readText() }
        assertFalse("$what leaked a path: $text", text.contains("secret-folder"))
        assertFalse("$what leaked a name: $text", text.contains("tax-return"))
        assertFalse("$what leaked exception text: $text", text.contains("no such file"))
        // Every line any site wrote is valid JSON with the envelope and only allowed fields.
        val lines = File(tmp.root, "logs").listFiles().orEmpty().flatMap { it.readLines() }
        assertTrue("$what wrote nothing", lines.isNotEmpty())
        var lastSeq = 0L
        for (line in lines) {
            val o = JSONObject(line)
            for (k in listOf("t", "seq", "sid", "src", "ev")) assertTrue("$k missing in $line", o.has(k))
            assertTrue("seq not increasing in $line", o.getLong("seq") > lastSeq)
            lastSeq = o.getLong("seq")
            if (!o.getString("ev").startsWith("native")) {
                for (k in o.keys()) assertTrue("field $k not allowed in $line", k in FLog.ALLOWED_FIELDS)
            }
        }
    }

    private class Vol : LuksVolume(0L) {
        override val info = VolumeInfo("Test", "uuid-0000", 4096, 1024L, "ext4", emptyList())
        override fun commitActiveBatch() {}
    }

    private class RecordingSource(
        var drainJson: String = """{"dropped":0,"events":[]}""",
        var ring: String = """{"dropped":0,"events":[{"nseq":5,"ev":"usb_reset"}]}""",
    ) : NativeEventSource {
        val drains = AtomicInteger(0)
        override fun drain(): String { drains.incrementAndGet(); return drainJson }
        override fun ringJson(): String = ring
    }

    private fun syncDrain(src: NativeEventSource) = NativeDrain(src, executor = { it.run() })

    private fun unlockedSession() = runBlocking { LuksSession.startUnlockedForTest(volume = Vol()) }

    private fun awaitTerminal(c: TransferController, id: Long) = runBlocking {
        withTimeout(5_000) {
            while (c.getTransfer(id)?.state.let { it == TransferState.QUEUED || it == TransferState.RUNNING }) delay(5)
        }
    }

    // ------------------------------------------------------------------ transfers

    @Test
    fun aSuccessfulJobLogsQueuedStartEnd() {
        unlockedSession()
        val c = TransferController().also { it.nativeDrain = syncDrain(RecordingSource()) }
        val id = c.startJob("tax-return.pdf", TransferType.IMPORT, totalBytes = 2048L) { _, _ -> }
        awaitTerminal(c, id)
        val mine = awaitEvents { e -> e.any { it.optLong("xfer") == id && it.getString("ev") == "xfer_end" } }
            .filter { it.optLong("xfer", -1) == id }
        assertEquals(listOf("xfer_queued", "xfer_start", "xfer_end"), mine.map { it.getString("ev") })
        val q = mine[0]
        assertEquals("import", q.getString("op"))
        assertEquals(1L, q.getLong("n"))
        assertEquals(2048L, q.getLong("bytes"))
        val end = mine[2]
        assertEquals("completed", end.getString("state"))
        assertTrue(end.getLong("ms") >= 0)
        assertFalse(end.has("code"))
        assertNoLeak("successful job")
    }

    @Test
    fun aFailedJobLogsEndNativeErrAndRingWithoutAnyPath() {
        unlockedSession()
        val src = RecordingSource()
        val c = TransferController().also { it.nativeDrain = syncDrain(src) }
        val id = c.startJob("tax-return.pdf", TransferType.IMPORT, writes = false) { _, _ -> throw pathExc() }
        awaitTerminal(c, id)
        val e = awaitEvents { l -> l.ev("native_ring").isNotEmpty() && l.ev("xfer_end").isNotEmpty() }
        val end = e.ev("xfer_end").single()
        assertEquals("failed", end.getString("state"))
        assertEquals(LuksException.NOT_FOUND, end.getInt("code"))
        assertEquals("LuksException", end.getString("exc"))
        assertEquals(id, end.getLong("xfer"))
        val ne = e.ev("native_err").single()
        assertEquals(LuksException.NOT_FOUND, ne.getInt("code"))
        assertEquals("LuksException", ne.getString("exc"))
        assertEquals("transfer", ne.getString("op"))
        val ring = e.ev("native_ring").single()
        assertEquals(id, ring.getLong("xfer"))
        assertEquals("usb_reset", ring.getString("kind"))
        assertNoLeak("failed job")
    }

    @Test
    fun aCancelledQueuedItemLogsEndCancelled() {
        val c = TransferController()
        val id = c.enqueueForTest("tax-return.pdf", TransferType.IMPORT)
        c.cancelTransfer(id)
        val e = awaitEvents { l -> l.ev("xfer_end").isNotEmpty() }
        assertEquals("cancelled", e.ev("xfer_end").single().getString("state"))
        assertEquals("import", e.ev("xfer_queued").single().getString("op"))
        assertNoLeak("cancel")
    }

    @Test
    fun theQueueDrainRequestsANativeDrain() {
        unlockedSession()
        val src = RecordingSource()
        val c = TransferController().also { it.nativeDrain = syncDrain(src) }
        val id = c.startJob("x", TransferType.HASH) { _, _ -> }
        awaitTerminal(c, id)
        val deadline = System.currentTimeMillis() + 5_000
        while (src.drains.get() == 0 && System.currentTimeMillis() < deadline) Thread.sleep(5)
        assertTrue("no drain after the queue emptied", src.drains.get() >= 1)
    }

    private class Src(val name: String, val declared: Long, val bytes: ByteArray) : ImportSource {
        override fun describe() = ImportSourceInfo(name, declared)
        override fun open(): ReadableByteChannel {
            val inner = Channels.newChannel(ByteArrayInputStream(bytes))
            return object : ReadableByteChannel {
                override fun read(dst: ByteBuffer) = inner.read(dst)
                override fun isOpen() = inner.isOpen
                override fun close() = inner.close()
            }
        }
    }

    private class DstVolume(private val failFinish: Boolean = false) : LuksVolume(0L) {
        override val info = VolumeInfo("Test", "uuid-0000", 4096, 1 shl 30, "ext4", emptyList())
        override fun statFs() = StatFsInfo(1 shl 30, Long.MAX_VALUE, Long.MAX_VALUE, 100, 100, 4096)
        override fun commitActiveBatch() {}
        override fun beginFile(sizeBytes: Long): FileWriter = W()
        inner class W : FileWriter(0L) {
            override fun write(data: ByteBuffer, len: Int) { data.position(data.position() + len) }
            override fun finish(parentPath: String, name: String): Long {
                if (failFinish) throw pathExc2()
                return 9L
            }
            override fun close() = super.close()
        }
        private fun pathExc2() = LuksException("finish failed for /Volumes/secret-folder/tax-return.pdf", LuksException.IO)
    }

    private val ctx: Context = object : ContextWrapper(null) {}
    private val uri: Uri? = Uri.parse("content://t/x")

    @Test
    fun anImportLogsItsPerFilePhasesWithInodeAndBytes() {
        val c = TransferController().also { it.nativeDrain = syncDrain(RecordingSource()) }
        val data = ByteArray(5000) { 1 }
        val id = runBlocking { c.importFileWithProgress(ctx, DstVolume(), "/", uri!!, source = Src("tax-return.pdf", data.size.toLong(), data)) }
        val e = awaitEvents { l -> l.ev("xfer_end").isNotEmpty() }.filter { it.optLong("xfer", -1) == id }
        val phases = e.ev("xfer_phase")
        assertEquals(listOf("begin", "write", "finish", "commit"), phases.map { it.getString("stage") })
        assertEquals(5000L, phases[1].getLong("bytes"))
        assertEquals(9L, phases[2].getLong("ino"))
        assertEquals(5000L, phases[2].getLong("bytes"))
        for (p in phases) assertTrue(p.getLong("ms") >= 0)
        val end = e.ev("xfer_end").single()
        assertEquals("completed", end.getString("state"))
        assertEquals(5000L, end.getLong("bytes"))
        assertEquals(1, e.ev("xfer_start").size)
        assertNoLeak("import")
    }

    @Test
    fun aFailedImportNamesTheStageItDiedIn() {
        val c = TransferController().also { it.nativeDrain = syncDrain(RecordingSource()) }
        val data = ByteArray(100) { 1 }
        val id = runBlocking { c.importFileWithProgress(ctx, DstVolume(failFinish = true), "/", uri!!, source = Src("tax-return.pdf", 100, data)) }
        val e = awaitEvents { l -> l.ev("xfer_end").isNotEmpty() }.filter { it.optLong("xfer", -1) == id }
        val end = e.ev("xfer_end").single()
        assertEquals("failed", end.getString("state"))
        assertEquals("finish", end.getString("stage"))
        assertEquals(LuksException.IO, end.getInt("code"))
        assertEquals("finish", e.ev("native_err").single().getString("stage"))
        assertNoLeak("failed import")
    }

    private class SrcVolume(private val data: ByteArray) : LuksVolume(0L) {
        override val info = VolumeInfo("Test", "uuid-0000", 4096, 1024L, "ext4", emptyList())
        override fun openFile(path: String): Long = 42L
        override fun fileSizeOf(file: Long): Long = data.size.toLong()
        override fun readAt(file: Long, offset: Long, len: Int): ByteArray =
            data.copyOfRange(offset.toInt(), minOf(data.size.toLong(), offset + len).toInt())
        override fun closeFile(file: Long) {}
    }

    private class Target : ExportTarget {
        val out = ByteArrayOutputStream()
        override var verifyLevel = dev.luksandroid.transfer.VerifyLevel.FULL
        override fun sizeBytes(): Long? = null
        override fun availableBytes(): Long? = null
        override fun openOutput(): OutputStream = out
        override fun openRead(): InputStream? = ByteArrayInputStream(out.toByteArray())
        override fun commit() {}
        override fun discard() {}
    }

    @Test
    fun anExportLogsReadVerifyCommitPhases() {
        val c = TransferController().also { it.nativeDrain = syncDrain(RecordingSource()) }
        val id = runBlocking { c.exportFileWithProgress(ctx, SrcVolume(ByteArray(3000) { 2 }), "/tax-return.pdf", uri!!, target = Target()) }
        val e = awaitEvents { l -> l.ev("xfer_end").isNotEmpty() }.filter { it.optLong("xfer", -1) == id }
        assertEquals(listOf("read", "verify", "commit"), e.ev("xfer_phase").map { it.getString("stage") })
        assertEquals(3000L, e.ev("xfer_phase")[0].getLong("bytes"))
        assertEquals("completed", e.ev("xfer_end").single().getString("state"))
        assertNoLeak("export")
    }

    @Test
    fun exclusiveWritesLogTheirOpAndAFailureWithoutAnyPath() {
        unlockedSession()
        val c = TransferController().also { it.nativeDrain = syncDrain(RecordingSource()) }
        runBlocking { c.withExclusiveWrite(op = "mkdir") { } }
        val failure = runCatching { runBlocking { c.withExclusiveWrite(op = "delete") { throw pathExc() } } }.exceptionOrNull()
        assertTrue(failure is LuksException)
        val e = awaitEvents { l -> l.ev("xfer_end").size >= 2 }
        assertEquals(listOf("mkdir", "delete"), e.ev("xfer_queued").map { it.getString("op") })
        assertEquals(listOf("completed", "failed"), e.ev("xfer_end").map { it.getString("state") })
        assertEquals("delete", e.ev("native_err").single().getString("op"))
        assertNoLeak("exclusive write")
    }

    @Test
    fun aFailedCommitIsALoggedNativeErrWithoutPath() {
        unlockedSession()
        val vol = object : LuksVolume(0L) {
            override val info = VolumeInfo("Test", "uuid-0000", 4096, 1024L, "ext4", emptyList())
            override fun commitActiveBatch() = throw pathExc(LuksException.IO)
        }
        runBlocking { LuksSession.startUnlockedForTest(volume = vol) }
        val c = TransferController().also { it.nativeDrain = syncDrain(RecordingSource()) }
        assertFalse(runBlocking { c.commitNow() })
        val ne = events().ev("native_err").single()
        assertEquals("commit", ne.getString("op"))
        assertEquals(LuksException.IO, ne.getInt("code"))
        assertNoLeak("commit")
    }

    // ------------------------------------------------------------------ session

    @Test
    fun unlockWritesHeaderAndUnlockUnderANewSessionIdThenLockEndsIt() = runBlocking {
        val session = SessionController(scope = scope)
        session.nativeDrain = syncDrain(RecordingSource())
        val p = testPartition()
        val volume = RecordingVolume(dev.luksandroid.session.EventLog(), "V1")
        val state = session.unlock(fakeContext(), FakeDevice(p) { volume }, p, passphrase())
        assertTrue(state is SessionState.Unlocked)
        val sid = FLog.sid
        assertTrue(sid.matches(Regex("[0-9a-f]{8}")))
        session.lock()
        assertEquals("-", FLog.sid)
        val e = events()
        val names = e.map { it.getString("ev") }
        assertTrue(names.toString(), names.indexOf("session_header") < names.indexOf("unlock"))
        assertTrue(names.indexOf("unlock") < names.indexOf("lock"))
        val h = e.ev("session_header").single()
        assertEquals(sid, h.getString("sid"))
        assertEquals(2L shl 30, h.getLong("dev_bytes"))
        assertEquals(512, h.getInt("sector"))
        assertEquals("btrfs", h.getString("fs"))
        assertEquals(true, h.getBoolean("write"))
        assertTrue(h.getString("app").isNotEmpty())
        assertTrue(h.getString("git").isNotEmpty())
        assertFalse("no usb device in a fake", h.has("vid"))
        assertEquals(sid, e.ev("unlock").single().getString("sid"))
        assertTrue(e.ev("unlock").single().getLong("ms") >= 0)
        assertEquals("user", e.ev("lock").single().getString("reason"))
        assertEquals(sid, e.ev("lock").single().getString("sid"))
        assertNoLeak("unlock/lock")
    }

    @Test
    fun aNewUnlockGetsANewSessionId() = runBlocking {
        val session = SessionController(scope = scope)
        session.nativeDrain = syncDrain(RecordingSource())
        val p = testPartition()
        val v = { RecordingVolume(dev.luksandroid.session.EventLog(), "V") }
        session.unlock(fakeContext(), FakeDevice(p) { v() }, p, passphrase())
        val first = FLog.sid
        session.lock()
        session.unlock(fakeContext(), FakeDevice(p) { v() }, p, passphrase())
        assertNotEquals(first, FLog.sid)
        val sids = events().ev("session_header").map { it.getString("sid") }
        assertEquals(2, sids.toSet().size)
    }

    @Test
    fun aFailedUnlockLogsClassCodeAndStageWithoutThePath() = runBlocking {
        val session = SessionController(scope = scope)
        session.nativeDrain = syncDrain(RecordingSource())
        val p = testPartition()
        val bad = RecordingVolume(dev.luksandroid.session.EventLog(), "V1").apply { listFailure = pathExc(LuksException.CORRUPT) }
        session.unlock(fakeContext(), FakeDevice(p) { bad }, p, passphrase())
        val e = events()
        val f = e.ev("unlock_fail").single()
        assertEquals("LuksException", f.getString("exc"))
        assertEquals(LuksException.CORRUPT, f.getInt("code"))
        assertTrue(f.getString("stage").isNotEmpty())
        assertEquals(1, e.ev("native_err").size)
        assertEquals(0, e.ev("session_header").size)
        assertNoLeak("failed unlock")
        // Non-native failures are logged too (class only).
        val bad2 = RecordingVolume(dev.luksandroid.session.EventLog(), "V2").apply { listFailure = IllegalStateException(secretPath) }
        session.unlock(fakeContext(), FakeDevice(p) { bad2 }, p, passphrase())
        assertEquals("IllegalStateException", events().ev("unlock_fail").last().getString("exc"))
        assertNoLeak("failed unlock 2")
    }

    @Test
    fun aFailedOpenPlainLogsUnlockFailToo() = runBlocking {
        val session = SessionController(scope = scope)
        val p = testPartition()
        val bad = RecordingVolume(dev.luksandroid.session.EventLog(), "V1").apply { listFailure = pathExc(LuksException.CORRUPT) }
        session.openPlain(fakeContext(), FakeDevice(p) { bad }, p)
        assertEquals("open_plain", events().ev("unlock_fail").single().getString("stage"))
        assertNoLeak("failed plain open")
    }

    @Test
    fun detachLogsReasonAndEndsTheSession() = runBlocking {
        val session = SessionController(scope = scope)
        session.nativeDrain = syncDrain(RecordingSource())
        val p = testPartition()
        session.unlock(fakeContext(), FakeDevice(p) { RecordingVolume(dev.luksandroid.session.EventLog(), "V") }, p, passphrase())
        val sid = FLog.sid
        session.onDeviceDetached()
        val e = events()
        assertEquals("usb_detach", e.ev("detach").single().getString("reason"))
        assertEquals(sid, e.ev("detach").single().getString("sid"))
        assertEquals("-", FLog.sid)
    }

    @Test
    fun idleLockLogsIdleLockThenLockWithIdleReason() = runBlocking {
        val session = SessionController(scope = scope)
        session.nativeDrain = syncDrain(RecordingSource())
        session.startUnlockedForTest(volume = RecordingVolume(dev.luksandroid.session.EventLog(), "V"))
        session.setIdleTimeout(40)
        withTimeout(5_000) { while (session.state.value !is SessionState.Locked) delay(10) }
        val e = events()
        assertEquals(1, e.ev("idle_lock").size)
        assertEquals("idle", e.ev("lock").single().getString("reason"))
    }

    @Test
    fun trimMemoryIsLogged() {
        val session = SessionController(scope = scope)
        session.onTrimMemory(20)
        assertEquals(20L, events().ev("trim_memory").single().getLong("n"))
    }

    @Test
    fun aFatalWriteFailureIsLoggedAsALockWithItsReason() = runBlocking {
        val session = SessionController(scope = scope)
        session.nativeDrain = syncDrain(RecordingSource())
        session.startUnlockedForTest(volume = RecordingVolume(dev.luksandroid.session.EventLog(), "V"))
        session.onWriteSessionFenced()
        val e = events()
        assertEquals("write_session_fenced", e.ev("lock").single().getString("reason"))
        assertNoLeak("fatal")
    }

    @Test
    fun aFailedCommitBeforeCloseIsLoggedWithoutPath() = runBlocking {
        val session = SessionController(scope = scope)
        session.nativeDrain = syncDrain(RecordingSource())
        val vol = RecordingVolume(dev.luksandroid.session.EventLog(), "V").apply { commitFailure = pathExc(LuksException.IO) }
        session.startUnlockedForTest(volume = vol)
        session.lock()
        val e = events()
        assertTrue(e.ev("native_err").any { it.getString("op") == "commit_before_close" })
        assertNoLeak("commit before close")
    }

    @Test
    fun theSessionDrainsNativeEventsPeriodicallyAndOnceMoreOnLockThenStops() = runBlocking {
        val src = RecordingSource()
        val session = SessionController(scope = scope)
        session.nativeDrain = syncDrain(src)
        session.drainPeriodMs = 20
        val p = testPartition()
        session.unlock(fakeContext(), FakeDevice(p) { RecordingVolume(dev.luksandroid.session.EventLog(), "V") }, p, passphrase())
        withTimeout(5_000) { while (src.drains.get() < 3) delay(5) }
        session.lock()
        val afterLock = src.drains.get()
        delay(120)
        assertEquals("drain loop kept running after lock", afterLock, src.drains.get())
        // The final drain happened after the close: at least one drain is attributable to the teardown.
        assertTrue(afterLock >= 4)
    }

    // ------------------------------------------------------------------ health

    private class FakeTokens : CancelTokens {
        private val next = AtomicInteger(10)
        override fun create(): Long = next.getAndIncrement().toLong()
        override fun signal(token: Long) {}
        override fun close(token: Long) {}
    }

    private fun runHealth(check: (Long) -> Health): List<JSONObject> {
        val hm = HealthMonitor(scope, Dispatchers.Default, FakeTokens(), writesIdle = { true }, debounceMs = 5)
        hm.attach(check = check, setReadOnly = {})
        val e = awaitEvents { l -> l.ev("health").isNotEmpty() }
        hm.detach()
        return e
    }

    @Test
    fun healthVerdictsAndFindingsAreLogged() {
        val e = runHealth { Health.Corrupt(mapOf("double_owned_block" to 3L, "orphan_inode" to 1L)) }
        val h = e.ev("health").single()
        assertEquals("corrupt", h.getString("verdict"))
        assertTrue(h.getLong("ms") >= 0)
        val findings = e.ev("health_finding").associate { it.getString("kind") to it.getLong("n") }
        assertEquals(mapOf("double_owned_block" to 3L, "orphan_inode" to 1L), findings)
    }

    @Test
    fun anUnknownVerdictCarriesItsReason() {
        val e = runHealth { Health.Unknown(HealthUnknownReason.TOO_LARGE) }
        val h = e.ev("health").single()
        assertEquals("unknown", h.getString("verdict"))
        assertEquals("too_large", h.getString("reason"))
    }

    @Test
    fun aThrowingCheckIsLoggedWithClassOnly() {
        val e = runHealth { throw pathExc(LuksException.IO) }
        val h = e.ev("health").single()
        assertEquals("unknown", h.getString("verdict"))
        assertEquals("internal", h.getString("reason"))
        assertEquals("LuksException", h.getString("exc"))
        assertNoLeak("health")
    }

    // ------------------------------------------------------------------ foreground service

    @Test
    fun theForegroundControllerLogsServiceStartAndStop() = runBlocking {
        val count = kotlinx.coroutines.flow.MutableStateFlow(0)
        val busy = kotlinx.coroutines.flow.MutableStateFlow(false)
        val shows = CopyOnWriteArrayList<Int>()
        val host = object : ForegroundHost {
            override fun show(count: Int) { shows += count }
            override fun hide() { shows += -1 }
        }
        val job = TransferForegroundController(count, busy, host, stopDelayMs = 10).start(scope)
        count.value = 2
        busy.value = true
        withTimeout(5_000) { while (shows.isEmpty()) delay(5) }
        busy.value = false
        count.value = 0
        withTimeout(5_000) { while (-1 !in shows) delay(5) }
        job.cancel()
        val e = awaitEvents { l -> l.ev("service_stop").isNotEmpty() }
        assertEquals(2L, e.ev("service_start").first().getLong("n"))
        assertEquals("idle", e.ev("service_stop").single().getString("reason"))
    }
}
