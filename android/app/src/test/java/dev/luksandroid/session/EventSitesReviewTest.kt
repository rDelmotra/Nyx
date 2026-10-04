package dev.luksandroid.session

import android.content.Context
import android.content.ContextWrapper
import android.net.Uri
import dev.luksandroid.LuksException
import dev.luksandroid.LuksVolume
import dev.luksandroid.StatFsInfo
import dev.luksandroid.VolumeInfo
import dev.luksandroid.log.EventLogger
import dev.luksandroid.log.NativeDrain
import dev.luksandroid.log.NativeEventSource
import dev.luksandroid.transfer.ImportSource
import dev.luksandroid.transfer.ImportSourceInfo
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.async
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
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
import java.io.File
import java.nio.ByteBuffer
import java.nio.channels.Channels
import java.nio.channels.ReadableByteChannel
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import dev.luksandroid.log.EventLog as FLog

/** F9 review fixes S3, S4, S9 and the leak nit, driven through the real engine and session. */
class EventSitesReviewTest {

    @get:Rule val tmp = TemporaryFolder()
    private lateinit var logger: EventLogger
    private lateinit var scope: CoroutineScope
    private val secret = "/Volumes/secret-folder/tax-return.pdf"
    private fun pathExc(code: Int) = LuksException("failed on $secret", code)

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
            if (System.currentTimeMillis() > deadline) throw AssertionError("timed out; have ${e.map { it.getString("ev") }}")
            Thread.sleep(5)
        }
    }

    private class Src(
        val onDrain: () -> Unit = {},
        val ring: String = """{"dropped":0,"events":[{"nseq":5,"ev":"usb_reset"}]}""",
    ) : NativeEventSource {
        val drains = AtomicInteger(0)
        override fun drain(): String { drains.incrementAndGet(); onDrain(); return """{"dropped":0,"events":[]}""" }
        override fun ringJson(): String = ring
    }

    private fun sync(src: NativeEventSource) = NativeDrain(src, executor = { it.run() })

    private open class Vol : LuksVolume(0L) {
        override val info = VolumeInfo("Test", "uuid-0000", 4096, 1024L, "ext4", emptyList())
        override fun commitActiveBatch() {}
    }

    // ------------------------------------------------------------------ S3: ring dumps

    @Test
    fun aLabelledMutatorFailureDumpsTheRingTaggedWithItsId() {
        runBlocking { LuksSession.startUnlockedForTest(volume = Vol()) }
        val c = TransferController().also { it.nativeDrain = sync(Src()) }
        runCatching { runBlocking { c.withExclusiveWrite(op = "delete") { throw pathExc(LuksException.CORRUPT) } } }
        val e = awaitEvents { it.ev("native_ring").isNotEmpty() }
        val id = e.ev("xfer_queued").single().getLong("xfer")
        assertEquals(id, e.ev("native_ring").single().getLong("xfer"))
        assertFalse(File(tmp.root, "logs").listFiles()!!.joinToString { it.readText() }.contains("secret-folder"))
    }

    @Test
    fun anUnlabelledMutatorFailureDumpsTheRingWithoutAnId() {
        runBlocking { LuksSession.startUnlockedForTest(volume = Vol()) }
        val c = TransferController().also { it.nativeDrain = sync(Src()) }
        runCatching { runBlocking { c.withExclusiveWrite { throw pathExc(LuksException.IO) } } }
        val ring = awaitEvents { it.ev("native_ring").isNotEmpty() }.ev("native_ring").single()
        assertFalse(ring.has("xfer"))
    }

    @Test
    fun aFailedCommitDumpsTheRing() {
        runBlocking {
            LuksSession.startUnlockedForTest(volume = object : Vol() { override fun commitActiveBatch() = throw pathExc(LuksException.IO) })
        }
        val c = TransferController().also { it.nativeDrain = sync(Src()) }
        assertFalse(runBlocking { c.commitNow() })
        assertEquals(1, awaitEvents { it.ev("native_ring").isNotEmpty() }.ev("native_ring").size)
    }

    @Test
    fun aFatalWriteFailureDumpsTheRing() = runBlocking {
        val session = SessionController(scope = scope)
        session.nativeDrain = sync(Src())
        session.startUnlockedForTest(volume = RecordingVolume(EventLog(), "V"))
        session.onWriteSessionFenced()
        assertEquals(1, events().ev("native_ring").size)
    }

    // ------------------------------------------------------------------ S4: drains after finish/commit, entry lines

    private class DstVolume : LuksVolume(0L) {
        override val info = VolumeInfo("Test", "uuid-0000", 4096, 1 shl 30, "ext4", emptyList())
        override fun statFs() = StatFsInfo(1 shl 30, Long.MAX_VALUE, Long.MAX_VALUE, 100, 100, 4096)
        override fun commitActiveBatch() {}
        override fun beginFile(sizeBytes: Long): FileWriter = W()
        inner class W : FileWriter(0L) {
            override fun write(data: ByteBuffer, len: Int) { data.position(data.position() + len) }
            override fun finish(parentPath: String, name: String): Long = 9L
            override fun close() = super.close()
        }
    }

    private class FileSrc(val bytes: ByteArray) : ImportSource {
        override fun describe() = ImportSourceInfo("tax-return.pdf", bytes.size.toLong())
        override fun open(): ReadableByteChannel {
            val inner = Channels.newChannel(ByteArrayInputStream(bytes))
            return object : ReadableByteChannel {
                override fun read(dst: ByteBuffer) = inner.read(dst)
                override fun isOpen() = inner.isOpen
                override fun close() = inner.close()
            }
        }
    }

    private val ctx: Context = object : ContextWrapper(null) {}
    private val uri: Uri? = Uri.parse("content://t/x")

    private fun phaseTrail(e: List<JSONObject>) =
        e.ev("xfer_phase").map { it.getString("stage") + (if (it.has("state")) ":" + it.getString("state") else "") }

    @Test
    fun finishAndCommitLogEntryLinesAndEachIsFollowedByANativeDrain() {
        val seenAtDrain = mutableListOf<List<String>>()
        val src = Src(onDrain = { seenAtDrain += phaseTrail(events()) })
        val c = TransferController().also { it.nativeDrain = sync(src) }
        runBlocking { c.importFileWithProgress(ctx, DstVolume(), "/", uri!!, source = FileSrc(ByteArray(10) { 1 })) }
        val trail = phaseTrail(awaitEvents { it.ev("xfer_end").isNotEmpty() })
        assertEquals(listOf("begin", "write", "finish:enter", "finish", "commit:enter", "commit"), trail)
        assertTrue("a drain right after the finish line: $seenAtDrain", seenAtDrain.any { it.lastOrNull() == "finish" })
        assertTrue("a drain right after the commit line: $seenAtDrain", seenAtDrain.any { it.lastOrNull() == "commit" })
    }

    private class Active : LuksVolume(0L)

    @Test
    fun theTickerRunsEvery250msWhileAWriteIsActiveAndEvery2sOtherwise() = runBlocking {
        val p = testPartition()
        fun session(idle: Boolean, src: Src) = SessionController(scope = scope).also {
            it.nativeDrain = sync(src)
            it.drainPeriodMs = 60_000
            it.activeDrainPeriodMs = 20
            it.transferWritesIdle = { idle }
        }
        val busySrc = Src()
        val s1 = session(false, busySrc)
        s1.unlock(fakeContext(), FakeDevice(p) { RecordingVolume(EventLog(), "A") }, p, passphrase())
        withTimeout(5_000) { while (busySrc.drains.get() < 3) delay(5) }
        s1.lock()
        val idleSrc = Src()
        val s2 = session(true, idleSrc)
        s2.unlock(fakeContext(), FakeDevice(p) { RecordingVolume(EventLog(), "B") }, p, passphrase())
        delay(300)
        assertEquals("idle sessions tick on the slow period", 0, idleSrc.drains.get())
        s2.lock()
    }

    @Test
    fun theDefaultActivePeriodIs250ms() {
        assertEquals(250L, SessionController(scope = scope).activeDrainPeriodMs)
    }

    // ------------------------------------------------------------------ S9

    @Test
    fun aFailedUnlockEndsTheSessionIdSoLaterLinesAreNotAttributedToIt() = runBlocking {
        val session = SessionController(scope = scope)
        session.nativeDrain = sync(Src())
        val p = testPartition()
        val bad = RecordingVolume(EventLog(), "V").apply { listFailure = pathExc(LuksException.CORRUPT) }
        session.unlock(fakeContext(), FakeDevice(p) { bad }, p, passphrase())
        assertEquals("-", FLog.sid)
        val fail = events().ev("unlock_fail").single()
        assertNotEquals("the failure itself belongs to the attempt's session", "-", fail.getString("sid"))
        session.openPlain(fakeContext(), FakeDevice(p) { bad }, p)
        assertEquals("-", FLog.sid)
    }

    @Test
    fun aDetachPublishesItsPendingCloseBeforeTheStateIsVisible() = runBlocking {
        repeat(300) { i ->
            val session = SessionController(scope = scope)
            session.nativeDrain = sync(Src())
            session.startUnlockedForTest(volume = RecordingVolume(EventLog(), "V$i"))
            val detach = scope.launch { session.onDeviceDetached() }
            while (session.state.value !is SessionState.Detached) Thread.onSpinWait()
            assertTrue("iteration $i: Detached was visible before pendingClose existed", session.hasPendingClose)
            detach.join()
        }
    }

    @Test
    fun anUnlockDuringADetachClosesWaitsForItAndKeepsItsOwnSessionId() = runBlocking {
        val session = SessionController(scope = scope)
        session.nativeDrain = sync(Src())
        val gate = CountDownLatch(1)
        val entered = CountDownLatch(1)
        val v1 = RecordingVolume(EventLog(), "V1").apply { commitGate = gate; commitEntered = entered }
        session.startUnlockedForTest(volume = v1)
        val detach = scope.launch { session.onDeviceDetached() }
        assertTrue(entered.await(5, TimeUnit.SECONDS))
        val p = testPartition()
        val unlock = scope.async { session.unlock(fakeContext(), FakeDevice(p) { RecordingVolume(EventLog(), "V2") }, p, passphrase()) }
        delay(200)
        assertFalse("unlock must wait for the detach's close", unlock.isCompleted)
        gate.countDown()
        withTimeout(5_000) { unlock.await(); detach.join() }
        val sid = events().ev("session_header").single().getString("sid")
        assertEquals("the new session keeps its id after the old close ended", sid, FLog.sid)
    }

    // ------------------------------------------------------------------ nit: map leaks

    @Test
    fun logBookkeepingMapsAreEmptyAfterEveryKindOfEnd() {
        runBlocking { LuksSession.startUnlockedForTest(volume = Vol()) }
        val c = TransferController().also { it.nativeDrain = sync(Src()) }
        val ok = c.startJob("a", TransferType.HASH) { _, _ -> }
        val bad = c.startJob("b", TransferType.HASH) { _, _ -> throw pathExc(LuksException.IO) }
        for (id in listOf(ok, bad)) runBlocking { withTimeout(5_000) { while (c.getTransfer(id)?.state.let { it == TransferState.QUEUED || it == TransferState.RUNNING }) delay(5) } }
        val q = c.enqueueForTest("c", TransferType.IMPORT)
        c.cancelTransfer(q)
        runBlocking { c.withExclusiveWrite(op = "mkdir") { } }
        runCatching { runBlocking { c.withExclusiveWrite(op = "delete") { throw pathExc(LuksException.IO) } } }
        val b = runBlocking { c.importFileWithProgress(ctx, DstVolume(), "/", uri!!, source = FileSrc(ByteArray(3))) }
        assertTrue(b > 0)
        val deadline = System.currentTimeMillis() + 5_000
        while (c.logMapSizes() != (0 to 0) && System.currentTimeMillis() < deadline) Thread.sleep(5)
        assertEquals(0 to 0, c.logMapSizes())
    }

    // ------------------------------------------------------------------ nit: teardown drain

    @Test
    fun theTeardownDrainHappensAfterTheCloseNotJustAsAFourthTick() = runBlocking {
        val log = EventLog()
        val src = Src(onDrain = { log.add("DRAIN") })
        val session = SessionController(scope = scope)
        session.nativeDrain = sync(src)
        session.drainPeriodMs = 60_000 // no periodic tick can masquerade as the final one
        val p = testPartition()
        session.unlock(fakeContext(), FakeDevice(p, log) { RecordingVolume(log, "V") }, p, passphrase())
        session.lock()
        val trail = log.snapshot()
        val lastClose = trail.lastIndexOf("DEVICE_CLOSE")
        assertTrue("no drain after the device closed: $trail", lastClose >= 0 && trail.subList(lastClose, trail.size).contains("DRAIN"))
    }
}
