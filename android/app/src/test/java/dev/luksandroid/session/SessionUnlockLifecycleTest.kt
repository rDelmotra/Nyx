package dev.luksandroid.session

import dev.luksandroid.LuksException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test

/** C8: a failed unlock closes what it produced; a re-unlock tears the old session down first. */
class SessionUnlockLifecycleTest {
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

    @Test
    fun unlock_listDirFailure_closesTheProducedVolume() = runBlocking {
        val produced = RecordingVolume(log, "V1").apply {
            listFailure = LuksException("corrupt root", LuksException.CORRUPT)
        }
        val p = testPartition()
        val state = session.unlock(fakeContext(), FakeDevice(p) { produced }, p, passphrase())

        assertTrue("expected Failed, was $state", state is SessionState.Failed)
        assertTrue("C8: produced volume must be closed: ${log.snapshot()}", "V1_CLOSE" in log.snapshot())
        assertFalse("a volume that never became the session's must not be committed", "V1_COMMIT" in log.snapshot())
    }

    @Test
    fun unlock_nonNativeFailure_closesTheProducedVolume() = runBlocking {
        val produced = RecordingVolume(log, "V1").apply { listFailure = IllegalStateException("boom") }
        val p = testPartition()
        val state = session.unlock(fakeContext(), FakeDevice(p) { produced }, p, passphrase())
        assertTrue(state is SessionState.Failed)
        assertTrue("V1_CLOSE" in log.snapshot())
    }

    @Test
    fun openPlain_failure_closesTheProducedVolume() = runBlocking {
        val produced = RecordingVolume(log, "V1").apply {
            listFailure = LuksException("corrupt root", LuksException.CORRUPT)
        }
        val p = testPartition()
        val state = session.openPlain(fakeContext(), FakeDevice(p) { produced }, p)
        assertTrue(state is SessionState.Failed)
        assertTrue("V1_CLOSE" in log.snapshot())
    }

    @Test
    fun unlock_failureOfTheCloseItself_doesNotMaskTheUnlockFailure() = runBlocking {
        val produced = RecordingVolume(log, "V1").apply {
            listFailure = LuksException("corrupt root", LuksException.CORRUPT)
            closeFailure = LuksException("close failed", LuksException.IO)
        }
        val p = testPartition()
        val state = session.unlock(fakeContext(), FakeDevice(p) { produced }, p, passphrase())
        assertTrue("unlock result must still be Failed, was $state", state is SessionState.Failed)
    }

    @Test
    fun reUnlock_tearsDownThePriorSessionFirst_commitThenCloseVolumeThenDevice() = runBlocking {
        val v1 = RecordingVolume(log, "V1")
        session.startUnlockedForTest(volume = v1, device = AutoCloseable { log.add("D1_CLOSE") })

        val v2 = RecordingVolume(log, "V2")
        val p = testPartition()
        val d2 = FakeDevice(p, log) { log.add("V2_PRODUCED"); v2 }
        val state = session.unlock(fakeContext(), d2, p, passphrase())

        assertTrue("expected Unlocked, was $state", state is SessionState.Unlocked)
        val ev = log.snapshot()
        assertEquals(
            listOf("V1_COMMIT_START", "V1_COMMIT", "V1_CLOSE", "D1_CLOSE", "V2_PRODUCED"),
            ev.take(5),
        )
        assertSame(v2, session.volume)
    }

    @Test
    fun reUnlock_onTheSameDeviceObject_doesNotCloseTheDeviceItIsAboutToUse() = runBlocking {
        val p = testPartition()
        val v2 = RecordingVolume(log, "V2")
        val dev = FakeDevice(p, log) { v2 }
        val v1 = RecordingVolume(log, "V1")
        session.startUnlockedForTest(volume = v1, device = dev)

        val state = session.unlock(fakeContext(), dev, p, passphrase())

        assertTrue(state is SessionState.Unlocked)
        assertTrue("old volume must be closed", "V1_CLOSE" in log.snapshot())
        assertFalse("the incoming device must not be closed by the teardown: ${log.snapshot()}", "DEVICE_CLOSE" in log.snapshot())
    }

    @Test
    fun successfulUnlock_clearsAPreviousCloseWarning() = runBlocking {
        val v1 = RecordingVolume(log, "V1").apply { commitFailure = LuksException("x", LuksException.TRANSPORT) }
        session.startUnlockedForTest(volume = v1)
        session.lock()
        assertTrue(session.closeWarning.value != null)

        val p = testPartition()
        session.unlock(fakeContext(), FakeDevice(p) { RecordingVolume(log, "V2") }, p, passphrase())
        assertEquals(null, session.closeWarning.value)
    }
}
