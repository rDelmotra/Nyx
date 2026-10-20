package dev.luksandroid.session

import dev.luksandroid.LuksException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * F5 E2: commit policy (b) and teardown honesty (M2, M3).
 * Notes: notes/recon/F5-design.md "Commit policy", "Teardown honesty".
 */
class SessionTeardownHonestyTest {
    private lateinit var scope: CoroutineScope
    private lateinit var session: SessionController
    private lateinit var log: EventLog
    private lateinit var volume: RecordingVolume
    private lateinit var notifier: RecordingNotifier

    @Before
    fun setUp() {
        scope = CoroutineScope(Dispatchers.Default + SupervisorJob())
        session = SessionController(scope = scope)
        log = EventLog()
        volume = RecordingVolume(log)
        notifier = RecordingNotifier()
        session.notifier = notifier
    }

    @After
    fun tearDown() { scope.cancel() }

    private suspend fun unlockedWithDevice() {
        session.startUnlockedForTest(
            volume = volume,
            device = AutoCloseable { log.add("DEVICE_CLOSE") },
        )
    }

    @Test
    fun lock_commitsExplicitlyBeforeClosingVolumeAndDevice() = runBlocking {
        unlockedWithDevice()
        session.lock()
        assertEquals(
            listOf("VOLUME_COMMIT_START", "VOLUME_COMMIT", "VOLUME_CLOSE", "DEVICE_CLOSE"),
            log.snapshot(),
        )
        assertNull("a clean teardown raises no warning", session.closeWarning.value)
        assertTrue(notifier.notices.isEmpty())
    }

    @Test
    fun detach_commitsExplicitlyBeforeClose() = runBlocking {
        unlockedWithDevice()
        session.onDeviceDetached()
        assertEquals(
            listOf("VOLUME_COMMIT_START", "VOLUME_COMMIT", "VOLUME_CLOSE", "DEVICE_CLOSE"),
            log.snapshot(),
        )
    }

    @Test
    fun lock_commitFailure_stillClosesEverything_andReportsFilesMayNotBeSaved() = runBlocking {
        volume.commitFailure = LuksException("usb transfer failed", LuksException.TRANSPORT)
        unlockedWithDevice()
        session.noteFilesFinished(3)

        session.lock()

        assertEquals(SessionState.Locked, session.state.value)
        assertTrue("volume must still be closed", "VOLUME_CLOSE" in log.snapshot())
        assertTrue("device must still be closed", "DEVICE_CLOSE" in log.snapshot())
        val warning = session.closeWarning.value
        assertNotNull("failed final commit must leave a session warning", warning)
        assertEquals(3, warning!!.unsavedFiles)
        assertEquals(LuksException.TRANSPORT, warning.code)
        assertEquals(listOf<SessionNotice>(SessionNotice.FilesMayNotBeSaved(3)), notifier.notices.toList())
    }

    @Test
    fun lock_transientCommitFailure_thatTheFinalCloseRecovers_raisesNoWarning() = runBlocking {
        volume.commitFailure = LuksException("usb hiccup", LuksException.TRANSPORT)
        volume.closeRetriesCommit = false // the close's own commit succeeds
        unlockedWithDevice()
        session.noteFilesFinished(2)

        session.lock()

        assertNull("the final attempt succeeded, so nothing is unsaved", session.closeWarning.value)
        assertTrue(notifier.notices.isEmpty())
        assertEquals(0, session.unsavedFileCount)
    }

    @Test
    fun lock_withASafWriteOpen_skipsTheExplicitCommit_whichWouldRefuse() = runBlocking {
        unlockedWithDevice()
        val hold = session.acquireWriteHold()
        session.lock()
        hold.release()
        assertEquals(listOf("VOLUME_CLOSE", "DEVICE_CLOSE"), log.snapshot())
    }

    @Test
    fun lock_closeFailure_isReportedToo() = runBlocking {
        volume.closeFailure = LuksException("final commit failed", LuksException.IO)
        unlockedWithDevice()
        session.noteFilesFinished(2)

        session.lock()

        assertEquals(SessionState.Locked, session.state.value)
        assertTrue("device close must still run", "DEVICE_CLOSE" in log.snapshot())
        assertEquals(2, session.closeWarning.value?.unsavedFiles)
        assertEquals(listOf<SessionNotice>(SessionNotice.FilesMayNotBeSaved(2)), notifier.notices.toList())
    }

    @Test
    fun successfulCommit_resetsTheUnsavedCount() = runBlocking {
        unlockedWithDevice()
        session.noteFilesFinished(5)
        assertEquals(5, session.unsavedFileCount)
        session.lock()
        assertEquals(0, session.unsavedFileCount)
        assertNull(session.closeWarning.value)
    }

    @Test
    fun clearCloseWarning_dismissesIt() = runBlocking {
        volume.commitFailure = LuksException("x", LuksException.TRANSPORT)
        unlockedWithDevice()
        session.lock()
        assertNotNull(session.closeWarning.value)
        session.clearCloseWarning()
        assertNull(session.closeWarning.value)
    }

    @Test
    fun detach_marksDetachedBeforeTheSlowCommit_andDoesNotHoldTheSessionMutex() {
        volume.commitGate = CountDownLatch(1)
        volume.commitEntered = CountDownLatch(1)
        runBlocking { unlockedWithDevice() }

        val detach = scope.launch { session.onDeviceDetached("pulled") }

        assertTrue("commit never started", volume.commitEntered!!.await(2, TimeUnit.SECONDS))
        // The native commit is now blocked (dead device). The UI must already be told.
        assertTrue(
            "state must be Detached while the slow close is still running, was ${session.state.value}",
            session.state.value is SessionState.Detached,
        )
        // And nothing that needs the session mutex may queue behind the close.
        val refused = runBlocking {
            withTimeout(1000) { runCatching { session.withLease { } }.exceptionOrNull() }
        }
        assertTrue("withLease must be refused promptly, not hang: $refused", refused is IllegalStateException)
        assertTrue("close must not have run yet", "VOLUME_CLOSE" !in log.snapshot())

        volume.commitGate!!.countDown()
        runBlocking { withTimeout(2000) { detach.join() } }
        assertEquals(
            listOf("VOLUME_COMMIT_START", "VOLUME_COMMIT", "VOLUME_CLOSE", "DEVICE_CLOSE"),
            log.snapshot(),
        )
    }

    @Test
    fun detach_commitFailure_isReported() = runBlocking {
        volume.commitFailure = LuksException("device gone", LuksException.TRANSPORT)
        unlockedWithDevice()
        session.noteFilesFinished(1)
        session.onDeviceDetached()
        assertTrue(session.state.value is SessionState.Detached)
        assertEquals(1, session.closeWarning.value?.unsavedFiles)
        assertEquals(listOf<SessionNotice>(SessionNotice.FilesMayNotBeSaved(1)), notifier.notices.toList())
    }

    @Test
    fun fenced_and_poisoned_neverCommit() = runBlocking {
        unlockedWithDevice()
        session.onWriteSessionFenced()
        assertTrue("a fenced session must not be committed: ${log.snapshot()}", log.snapshot().none { "COMMIT" in it })

        log.events.clear()
        unlockedWithDevice()
        session.onWritePoison()
        assertTrue("a poisoned session must not be committed: ${log.snapshot()}", log.snapshot().none { "COMMIT" in it })
    }

    @Test
    fun detachOnALockedSession_isIgnored() = runBlocking {
        assertEquals(SessionState.Locked, session.state.value)
        session.onDeviceDetached()
        assertEquals("C7: a detach with nothing open must not change state", SessionState.Locked, session.state.value)
    }
}
