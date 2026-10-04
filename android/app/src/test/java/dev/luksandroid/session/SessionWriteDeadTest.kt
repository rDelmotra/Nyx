package dev.luksandroid.session

import dev.luksandroid.LuksException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

/** C6: fatal write errors caught inside a lease block still reach the session. */
class SessionWriteDeadTest {
    private lateinit var scope: CoroutineScope
    private lateinit var session: SessionController

    @Before
    fun setUp() {
        scope = CoroutineScope(Dispatchers.Default + SupervisorJob())
        session = SessionController(scope = scope)
    }

    @After
    fun tearDown() { scope.cancel() }

    private suspend fun awaitFailed() = withTimeout(2000) {
        while (session.state.value !is SessionState.Failed) delay(10)
    }

    @Test
    fun reportWriteFailure_fenced_marksWriteDeadAtOnce_andFailsTheSession() = runBlocking {
        session.startUnlockedForTest()
        val fatal = session.reportWriteFailure(LuksException("fenced", LuksException.WRITE_SESSION_FENCED))
        assertTrue(fatal)
        assertTrue("write-dead must be visible synchronously so the queue can stop", session.isWriteDead.value)
        awaitFailed()
        assertTrue((session.state.value as SessionState.Failed).message.contains("code ${LuksException.WRITE_SESSION_FENCED}"))
    }

    @Test
    fun reportWriteFailure_poisoned_marksWriteDead() = runBlocking {
        session.startUnlockedForTest()
        assertTrue(session.reportWriteFailure(LuksException("poison", LuksException.MUTEX_POISONED)))
        assertTrue(session.isWriteDead.value)
        awaitFailed()
    }

    @Test
    fun reportWriteFailure_nonFatal_isIgnored() = runBlocking {
        session.startUnlockedForTest()
        assertFalse(session.reportWriteFailure(LuksException("corrupt read", LuksException.CORRUPT)))
        assertFalse(session.reportWriteFailure(RuntimeException("x")))
        assertFalse(session.isWriteDead.value)
        delay(100)
        assertTrue(session.state.value is SessionState.Unlocked)
    }

    @Test
    fun withLease_fatal_alsoMarksWriteDead() = runBlocking {
        session.startUnlockedForTest()
        runCatching { session.withLease { throw LuksException("fenced", LuksException.WRITE_SESSION_FENCED) } }
        assertTrue(session.isWriteDead.value)
    }

    @Test
    fun writeDead_clearsOnNextSuccessfulUnlock() = runBlocking {
        session.startUnlockedForTest()
        session.reportWriteFailure(LuksException("fenced", LuksException.WRITE_SESSION_FENCED))
        awaitFailed()
        assertTrue(session.isWriteDead.value)

        val p = testPartition()
        session.unlock(fakeContext(), FakeDevice(p) { RecordingVolume(EventLog()) }, p, passphrase())
        assertFalse(session.isWriteDead.value)
    }
}
