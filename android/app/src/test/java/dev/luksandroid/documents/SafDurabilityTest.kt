package dev.luksandroid.documents

import android.system.ErrnoException
import android.system.OsConstants
import dev.luksandroid.LuksException
import dev.luksandroid.session.EventLog
import dev.luksandroid.session.RecordingNotifier
import dev.luksandroid.session.RecordingVolume
import dev.luksandroid.session.SessionController
import dev.luksandroid.session.SessionNotice
import dev.luksandroid.session.SessionState
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Before
import org.junit.Test

/**
 * F5 E2: SAF write durability and honesty. Commit after each finish (c), commit on
 * fsync (d), finish failures surfaced (W12), write hold for the stream's lifetime (C9).
 *
 * D-d (lead decision, F5-design.md): a killed client's partial stream is still published
 * on onRelease. [onRelease_afterPartialStream_stillPublishesWhatArrived] pins that.
 */
class SafDurabilityTest {
    private class SafVolume(log: EventLog) : RecordingVolume(log) {
        @Volatile var finishFailure: Throwable? = null
        val finished = mutableListOf<String>()

        override fun beginFileStreaming(): FileWriter = Writer()
        override fun writeChunk(writer: FileWriter, data: ByteArray, offset: Int, length: Int) {
            log.add("WRITE")
        }
        override fun finishFile(writer: FileWriter, parentPath: String, name: String): Long {
            log.add("FINISH")
            finishFailure?.let { throw it }
            finished.add("$parentPath/$name")
            return 7L
        }
        override fun abandonFile(writer: FileWriter) { log.add("ABANDON") }

        inner class Writer : FileWriter(0L) {
            override fun write(bytes: ByteArray, offset: Int, length: Int) {}
            override fun close() {}
        }
    }

    private lateinit var scope: CoroutineScope
    private lateinit var session: SessionController
    private lateinit var log: EventLog
    private lateinit var volume: SafVolume
    private lateinit var notifier: RecordingNotifier

    @Before
    fun setUp() {
        scope = CoroutineScope(Dispatchers.Default + SupervisorJob())
        session = SessionController(scope = scope)
        log = EventLog()
        volume = SafVolume(log)
        notifier = RecordingNotifier()
        session.notifier = notifier
        runBlocking { session.startUnlockedForTest(volume = volume) }
        PendingDocuments.clear()
    }

    @After
    fun tearDown() {
        scope.cancel()
        PendingDocuments.clear()
    }

    private fun newWriteCallback(name: String = "f.bin"): LuksProxyCallback {
        val id = PendingDocuments.register("/", name)
        return LuksWriteProxyCallback(session = session, documentId = id)
    }

    @Test
    fun finish_commitsAfterEachFile() {
        val cb = newWriteCallback()
        cb.onWrite(0, 4, ByteArray(4))
        cb.onRelease()
        assertEquals(listOf("WRITE", "FINISH", "VOLUME_COMMIT_START", "VOLUME_COMMIT"), log.snapshot())
        assertEquals(0, session.unsavedFileCount)
        assertTrue(notifier.notices.isEmpty())
    }

    @Test
    fun emptyFile_commitsToo() {
        val cb = newWriteCallback()
        cb.onRelease()
        assertTrue("FINISH" in log.snapshot())
        assertTrue("an empty file is a file: ${log.snapshot()}", "VOLUME_COMMIT" in log.snapshot())
    }

    @Test
    fun finishFailure_isSurfacedToTheUser_andTheWriterIsAbandoned() {
        volume.finishFailure = LuksException("no space", LuksException.NO_SPACE)
        val cb = newWriteCallback()
        cb.onWrite(0, 4, ByteArray(4))
        cb.onRelease()
        assertEquals(listOf<SessionNotice>(SessionNotice.SafFinishFailed), notifier.notices.toList())
        assertTrue("ABANDON" in log.snapshot())
        assertTrue("a failed finish must not be followed by a commit", "VOLUME_COMMIT_START" !in log.snapshot())
        assertEquals(0, session.activeWriteHolds)
    }

    @Test
    fun commitFailureAfterFinish_isSurfaced_andCountedAsUnsaved() {
        volume.commitFailure = LuksException("usb", LuksException.TRANSPORT)
        val cb = newWriteCallback()
        cb.onWrite(0, 4, ByteArray(4))
        cb.onRelease()
        assertEquals(listOf<SessionNotice>(SessionNotice.SafCommitFailed), notifier.notices.toList())
        assertEquals("the finished-but-uncommitted file stays counted", 1, session.unsavedFileCount)
    }

    @Test
    fun onRelease_afterPartialStream_stillPublishesWhatArrived() {
        // Lead decision D-d: POSIX semantics, a killed writer leaves a truncated file.
        val cb = newWriteCallback("partial.bin")
        cb.onWrite(0, 4, ByteArray(4))
        cb.onRelease()
        assertEquals(listOf("//partial.bin"), volume.finished)
    }

    @Test
    fun fsync_withNoStreamOpen_commits() {
        val cb = newWriteCallback()
        cb.onFsync()
        assertEquals(listOf("VOLUME_COMMIT_START", "VOLUME_COMMIT"), log.snapshot())
    }

    @Test
    fun fsync_commitFailure_returnsEioToTheClient() {
        volume.commitFailure = LuksException("usb", LuksException.TRANSPORT)
        val cb = newWriteCallback()
        try {
            cb.onFsync()
            fail("a failed commit must not be reported as a successful fsync")
        } catch (e: ErrnoException) {
            assertEquals(OsConstants.EIO, e.errno)
        }
    }

    @Test
    fun fsync_withANativeWriterOpen_doesNotTryToCommit() {
        // A btrfs commit refuses while a writer is open; the in-flight file becomes
        // durable at finish+commit on release, where a failure is surfaced (W12).
        val cb = newWriteCallback()
        cb.onWrite(0, 4, ByteArray(4))
        log.events.clear()
        cb.onFsync()
        assertEquals(emptyList<String>(), log.snapshot())
        cb.onRelease()
    }

    @Test
    fun fsync_fatalFence_marksSessionWriteDead() {
        volume.commitFailure = LuksException("fenced", LuksException.WRITE_SESSION_FENCED)
        val cb = newWriteCallback()
        try { cb.onFsync() } catch (_: ErrnoException) {}
        assertTrue(session.isWriteDead.value)
    }

    @Test
    fun writeStream_holdsTheSessionBusyFromFirstWriteUntilRelease() {
        val cb = newWriteCallback()
        assertEquals(0, session.activeWriteHolds)
        cb.onWrite(0, 4, ByteArray(4))
        assertEquals(1, session.activeWriteHolds)
        assertTrue(session.isBusy)
        cb.onWrite(4, 4, ByteArray(4))
        assertEquals("still one hold per stream", 1, session.activeWriteHolds)
        cb.onRelease()
        assertEquals(0, session.activeWriteHolds)
    }

    @Test
    fun writeHold_isReleasedOnAnAbandonedStream() {
        val cb = newWriteCallback()
        cb.onWrite(0, 4, ByteArray(4))
        try { cb.onWrite(99, 4, ByteArray(4)) } catch (_: ErrnoException) {}
        assertEquals(0, session.activeWriteHolds)
    }

    @Test
    fun finishFatal_marksSessionWriteDead_andReleasesTheHold() {
        volume.finishFailure = LuksException("fenced", LuksException.WRITE_SESSION_FENCED)
        val cb = newWriteCallback()
        cb.onWrite(0, 4, ByteArray(4))
        cb.onRelease()
        assertTrue(session.isWriteDead.value)
        assertEquals(0, session.activeWriteHolds)
        assertFalse(session.state.value is SessionState.Locked)
    }

    @Test
    fun fsync_commitRunsHoldingTheTransferLock() {
        var busyDuringCommit: Boolean? = null
        volume.onCommit = { busyDuringCommit = dev.luksandroid.session.TransferManager.busy.value }
        newWriteCallback().onFsync()
        assertEquals(true, busyDuringCommit)
        assertEquals("lock released after fsync", false, dev.luksandroid.session.TransferManager.busy.value)
    }

    @Test
    fun finish_commitRunsWhileTheSafWriteStillHoldsTheTransferLock() {
        var busyDuringCommit: Boolean? = null
        volume.onCommit = { busyDuringCommit = dev.luksandroid.session.TransferManager.busy.value }
        val cb = newWriteCallback()
        cb.onWrite(0, 4, ByteArray(4))
        cb.onRelease()
        assertEquals(true, busyDuringCommit)
        assertEquals(false, dev.luksandroid.session.TransferManager.busy.value)
    }
}
