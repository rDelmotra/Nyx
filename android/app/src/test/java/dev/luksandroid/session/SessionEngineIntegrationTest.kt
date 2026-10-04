package dev.luksandroid.session

import dev.luksandroid.LuksException
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

/** F5 integration: the session's commits go through the engine's one path, under its lock. */
class SessionEngineIntegrationTest {
    private lateinit var log: EventLog
    private lateinit var volume: RecordingVolume
    private lateinit var notifier: RecordingNotifier

    @Before
    fun setUp() {
        log = EventLog()
        volume = RecordingVolume(log)
        notifier = RecordingNotifier()
        LuksSession.notifier = notifier
        runBlocking { LuksSession.startUnlockedForTest(volume = volume) }
    }

    @After
    fun tearDown() {
        runBlocking { LuksSession.lock() }
        LuksSession.notifier = SessionNotifier.None
        LuksSession.noteBatchCommitted()
    }

    @Test
    fun luksSession_commitAndReport_goesThroughTheEnginesCommitNow() = runBlocking {
        LuksSession.noteFilesFinished(3)
        assertNull(LuksSession.commitAndReport(forFinish = false))
        assertEquals(listOf("VOLUME_COMMIT_START", "VOLUME_COMMIT"), log.snapshot())
        assertEquals("the engine's commit path resets the unsaved count", 0, LuksSession.unsavedFileCount)
        assertNull("a success clears the engine's failure flow", TransferManager.commitFailure.value)
    }

    @Test
    fun luksSession_commitFailure_isRecordedOnTheEnginesCommitFailureFlow_andReturned() = runBlocking {
        volume.commitFailure = LuksException("usb", LuksException.TRANSPORT)
        val failure = LuksSession.commitAndReport(forFinish = true)
        assertNotNull(failure)
        // The engine's flow, not a private copy: that is what proves the engine's path ran.
        assertEquals(LuksException.TRANSPORT, TransferManager.commitFailure.value?.code)
        volume.commitFailure = null
        assertNull(LuksSession.commitAndReport(forFinish = false))
        assertNull(TransferManager.commitFailure.value)
    }

    @Test
    fun commitFailureNotice_isPostedOncePerStreak() = runBlocking {
        volume.commitFailure = LuksException("usb", LuksException.TRANSPORT)
        LuksSession.commitAndReport(forFinish = true)
        LuksSession.commitAndReport(forFinish = true)
        LuksSession.commitAndReport(forFinish = true)
        assertEquals(listOf<SessionNotice>(SessionNotice.SafCommitFailed), notifier.notices.toList())

        volume.commitFailure = null
        LuksSession.commitAndReport(forFinish = true) // success re-arms
        volume.commitFailure = LuksException("usb", LuksException.TRANSPORT)
        LuksSession.commitAndReport(forFinish = true)
        assertEquals(2, notifier.notices.size)
    }

    @Test
    fun teardownCommit_runsHoldingTheTransferLock() = runBlocking {
        var busyDuringCommit: Boolean? = null
        volume.onCommit = { busyDuringCommit = TransferManager.busy.value }
        LuksSession.lock()
        assertEquals("the lock-time commit must hold the transfer lock", true, busyDuringCommit)
        assertEquals("and release it afterwards", false, TransferManager.busy.value)
    }

    @Test
    fun teardown_resetsTransferLocksOffTheCallersThread() {
        val callerThread = Thread.currentThread()
        var resetThread: Thread? = null
        val local = SessionController()
        local.transferReset = { resetThread = Thread.currentThread() }
        runBlocking { local.startUnlockedForTest(volume = RecordingVolume(EventLog())) }
        runBlocking { local.lock() }
        assertNotNull(resetThread)
        assertTrue("resetTransferLocks can block ~2 s; it must not run on the caller (main) thread",
            resetThread !== callerThread)
    }

    @Test
    fun detach_resetsTransferLocksOffTheCallersThread() {
        val callerThread = Thread.currentThread()
        var resetThread: Thread? = null
        val local = SessionController()
        local.transferReset = { resetThread = Thread.currentThread() }
        runBlocking { local.startUnlockedForTest(volume = RecordingVolume(EventLog())) }
        runBlocking { local.onDeviceDetached() }
        assertTrue(resetThread != null && resetThread !== callerThread)
    }
}
