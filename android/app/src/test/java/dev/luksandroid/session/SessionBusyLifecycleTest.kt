package dev.luksandroid.session

import android.app.Activity
import android.content.ComponentCallbacks2
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

/** C9: an open SAF write (a write hold) keeps the session alive; it does not outlive its release. */
class SessionBusyLifecycleTest {
    private lateinit var scope: CoroutineScope
    private lateinit var session: SessionController
    private lateinit var lifecycle: LuksSessionLifecycle
    private val activity = Activity()

    @Before
    fun setUp() {
        scope = CoroutineScope(Dispatchers.Default + SupervisorJob())
        session = SessionController(scope = scope)
        lifecycle = LuksSessionLifecycle(session = session, scope = scope)
    }

    @After
    fun tearDown() { scope.cancel() }

    private suspend fun awaitLocked() = withTimeout(2000) {
        while (session.state.value !is SessionState.Locked) delay(10)
    }

    @Test
    fun writeHold_countsAsBusy_andReleaseIsIdempotent() {
        assertFalse(session.isBusy)
        val hold = session.acquireWriteHold()
        assertEquals(1, session.activeWriteHolds)
        assertTrue(session.isBusy)
        hold.release()
        hold.release()
        assertEquals("a double release must not underflow", 0, session.activeWriteHolds)
        assertFalse(session.isBusy)
    }

    @Test
    fun lastActivityDestroyed_withAWriteOpen_doesNotLock() = runBlocking {
        session.startUnlockedForTest()
        lifecycle.onActivityCreated(activity, null)
        val hold = session.acquireWriteHold()

        lifecycle.onActivityDestroyed(activity)
        delay(300)

        assertTrue("must not lock mid-write", session.state.value is SessionState.Unlocked)
        hold.release()
    }

    @Test
    fun lastActivityDestroyed_thenWriteEnds_locksOnceIdle() = runBlocking {
        session.startUnlockedForTest()
        lifecycle.onActivityCreated(activity, null)
        val hold = session.acquireWriteHold()
        lifecycle.onActivityDestroyed(activity)
        delay(100)

        hold.release()

        awaitLocked()
    }

    @Test
    fun lastActivityDestroyed_thenNewActivity_doesNotLockWhenWriteEnds() = runBlocking {
        session.startUnlockedForTest()
        lifecycle.onActivityCreated(activity, null)
        val hold = session.acquireWriteHold()
        lifecycle.onActivityDestroyed(activity)
        lifecycle.onActivityCreated(Activity(), null)

        hold.release()
        delay(300)

        assertTrue(session.state.value is SessionState.Unlocked)
    }

    @Test
    fun lastActivityDestroyed_whenIdle_stillLocks() = runBlocking {
        session.startUnlockedForTest()
        lifecycle.onActivityCreated(activity, null)
        lifecycle.onActivityDestroyed(activity)
        awaitLocked()
    }

    @Test
    fun criticalTrim_withAWriteOpen_doesNotLock_atEitherEntryPoint() = runBlocking {
        session.startUnlockedForTest()
        val hold = session.acquireWriteHold()

        lifecycle.onTrimMemory(ComponentCallbacks2.TRIM_MEMORY_RUNNING_CRITICAL)
        session.onTrimMemory(ComponentCallbacks2.TRIM_MEMORY_RUNNING_CRITICAL)
        @Suppress("DEPRECATION") lifecycle.onLowMemory()
        delay(300)

        assertTrue(session.state.value is SessionState.Unlocked)
        hold.release()
    }
}
