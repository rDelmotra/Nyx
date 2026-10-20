package dev.luksandroid.session

import dev.luksandroid.LuksException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.withTimeoutOrNull
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

/** F5 review fixes (E2): review-F5.md M1, M2, M3 (sequencing), and the native-text leak. */
class SessionReviewFixesTest {
    private lateinit var scope: CoroutineScope
    private lateinit var session: SessionController
    private lateinit var log: EventLog

    @Before
    fun setUp() {
        scope = CoroutineScope(Dispatchers.Default + SupervisorJob())
        session = SessionController(scope = scope)
        log = EventLog()
    }

    @After
    fun tearDown() { scope.cancel() }

    // ---- M1: isBusy sees the engine

    @Test
    fun luksSession_isBusyWhileTheEngineHoldsTheLock_evenWithNoLeaseAndNoHold() = runBlocking {
        LuksSession.startUnlockedForTest(volume = RecordingVolume(log))
        try {
            assertFalse(LuksSession.isBusy)
            assertTrue(TransferManager.tryAcquireForSafWrite(1000))
            try {
                assertEquals(0, LuksSession.activeLeases)
                assertEquals(0, LuksSession.activeWriteHolds)
                assertTrue("engine busy (between queued jobs) must count", LuksSession.isBusy)
                val waiter = GlobalScope.launch { LuksSession.awaitNotBusy() }
                kotlinx.coroutines.delay(150)
                assertFalse("awaitNotBusy must wait for the engine", waiter.isCompleted)
            } finally {
                TransferManager.releaseSafWriteLock()
            }
            withTimeout(2000) { while (LuksSession.isBusy) kotlinx.coroutines.delay(10) }
        } finally {
            LuksSession.lock()
        }
    }

    // ---- M2: the transfer-lock wait is outside LuksSession.mutex

    private fun mutexFreeProbe(): () -> Boolean = {
        runBlocking {
            withTimeoutOrNull(500) { runCatching { session.withLease { } }; true } == true
        }
    }

    @Test
    fun fencedTeardown_doesNotHoldTheSessionMutexWhileResettingTransferLocks() = runBlocking {
        session.startUnlockedForTest(volume = RecordingVolume(log))
        var free: Boolean? = null
        val probe = mutexFreeProbe()
        session.transferReset = { free = probe() }
        session.onWriteSessionFenced()
        assertEquals("mutex must be free during the slow reset", true, free)
        assertTrue(session.state.value is SessionState.Failed)
    }

    @Test
    fun poisonTeardown_doesNotHoldTheSessionMutexWhileResettingTransferLocks() = runBlocking {
        session.startUnlockedForTest(volume = RecordingVolume(log))
        var free: Boolean? = null
        val probe = mutexFreeProbe()
        session.transferReset = { free = probe() }
        session.onWritePoison()
        assertEquals(true, free)
    }

    @Test
    fun fatalErrorInsideALease_doesNotWaitForTheTeardownToFinish() = runBlocking {
        // The SAF path: the caller owns the transfer lock the reset waits for.
        session.startUnlockedForTest(volume = RecordingVolume(log))
        session.transferReset = { Thread.sleep(1500) }
        val started = System.nanoTime()
        runCatching { session.withLease { throw LuksException("x", LuksException.WRITE_SESSION_FENCED) } }
        val ms = (System.nanoTime() - started) / 1_000_000
        assertTrue("withLease must return before the slow teardown ($ms ms)", ms < 800)
        assertTrue("state is Failed immediately", session.state.value is SessionState.Failed)
        // and the teardown still happens, later
        withTimeout(4000) { while ("VOLUME_CLOSE" !in log.snapshot()) kotlinx.coroutines.delay(20) }
    }

    @Test
    fun aUnlockAfterAFatalError_waitsForTheBackgroundCloseBeforeStarting() = runBlocking {
        val v1 = RecordingVolume(log, "V1")
        session.startUnlockedForTest(volume = v1)
        session.transferReset = { Thread.sleep(300) }
        runCatching { session.withLease { throw LuksException("x", LuksException.WRITE_SESSION_FENCED) } }
        val p = testPartition()
        session.unlock(fakeContext(), FakeDevice(p, log) { log.add("V2_PRODUCED"); RecordingVolume(log, "V2") }, p, passphrase())
        val ev = log.snapshot()
        assertTrue("old volume closed before the new unlock produced: $ev", ev.indexOf("V1_CLOSE") in 0 until ev.indexOf("V2_PRODUCED"))
    }

    // ---- item 4: native text never reaches state or log

    @Test
    fun fatalErrorText_isNotStoredInTheSessionState() = runBlocking {
        session.startUnlockedForTest(volume = RecordingVolume(log))
        runCatching {
            session.withLease { throw LuksException("io at /private/passwords_database.kdbx", LuksException.WRITE_SESSION_FENCED) }
        }
        val m = (session.state.value as SessionState.Failed).message
        assertFalse(m, m.contains("passwords_database") || m.contains("/private"))
        assertTrue("states the remedy: $m", m.contains("Unlock the volume again"))
    }

    @Test
    fun reportWriteFailure_textIsNotStoredInTheSessionState() = runBlocking {
        session.startUnlockedForTest(volume = RecordingVolume(log))
        session.reportWriteFailure(LuksException("panic at /secret/dir/file", LuksException.PANIC))
        withTimeout(2000) { while (session.state.value !is SessionState.Failed) kotlinx.coroutines.delay(10) }
        val m = (session.state.value as SessionState.Failed).message
        assertFalse(m, m.contains("/secret") || m.contains("panic at"))
    }

    // ---- leftovers: unlock failure text, lock() overwriting a newer state

    private suspend fun failUnlock(plain: Boolean, t: Throwable): SessionState.Failed {
        val v = RecordingVolume(log).apply { listFailure = t }
        val p = testPartition()
        val d = FakeDevice(p) { v }
        val st = if (plain) session.openPlain(fakeContext(), d, p) else session.unlock(fakeContext(), d, p, passphrase())
        return st as SessionState.Failed
    }

    @Test
    fun unlockFailure_storesCodeOnly_notNativeText() = runBlocking {
        for (plain in listOf(false, true)) {
            val m = failUnlock(plain, LuksException("bad node at /private/passwords.kdbx", LuksException.CORRUPT)).message
            assertFalse(m, m.contains("/private") || m.contains("passwords"))
            assertTrue(m, m.contains("code ${LuksException.CORRUPT}"))
            val g = failUnlock(plain, RuntimeException("secret /private/x leaked")).message
            assertFalse(g, g.contains("secret") || g.contains("/private"))
        }
    }

    @Test
    fun wrongPassphrase_keepsItsExactMessageForTheDevicesScreen() = runBlocking {
        val m = failUnlock(false, LuksException("nope", LuksException.WRONG_PASSWORD)).message
        assertEquals("wrong passphrase", m)
    }

    @Test
    fun lock_doesNotOverwriteADetachedStateReachedDuringTheLeaseWait() = runBlocking {
        session.startUnlockedForTest(volume = RecordingVolume(log))
        val hold = CompletableDeferred<Unit>()
        val leaseIn = CompletableDeferred<Unit>()
        val leaseJob = GlobalScope.launch { session.withLease { leaseIn.complete(Unit); hold.await() } }
        leaseIn.await()
        val lockJob = GlobalScope.launch { session.lock() }
        kotlinx.coroutines.delay(100) // lock() is now waiting for the lease
        session.onDeviceDetached("pulled")
        hold.complete(Unit)
        leaseJob.join(); lockJob.join()
        assertTrue("Detached must survive lock(): ${session.state.value}", session.state.value is SessionState.Detached)
    }

    @Test
    fun lock_doesNotOverwriteAFailedStateReachedDuringTheLeaseWait() = runBlocking {
        session.startUnlockedForTest(volume = RecordingVolume(log))
        val hold = CompletableDeferred<Unit>()
        val leaseIn = CompletableDeferred<Unit>()
        val leaseJob = GlobalScope.launch { session.withLease { leaseIn.complete(Unit); hold.await() } }
        leaseIn.await()
        val lockJob = GlobalScope.launch { session.lock() }
        kotlinx.coroutines.delay(100)
        session.onWriteSessionFenced()
        hold.complete(Unit)
        leaseJob.join(); lockJob.join()
        assertTrue(session.state.value is SessionState.Failed)
    }
}
