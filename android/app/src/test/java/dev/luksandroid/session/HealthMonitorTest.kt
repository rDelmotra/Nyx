package dev.luksandroid.session

import dev.luksandroid.Health
import dev.luksandroid.HealthUnknownReason
import dev.luksandroid.LuksVolume
import dev.luksandroid.PartitionInfo
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
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicInteger

/** F8 monitor rules (A6-A8), against fakes: a scripted check, a recording token registry, a switchable idle probe. */
class HealthMonitorTest {
    private lateinit var scope: CoroutineScope

    @Before fun setUp() { scope = CoroutineScope(Dispatchers.Default + SupervisorJob()) }
    @After fun tearDown() { scope.cancel() }

    /** Tokens start at 7; remembers which were signalled and which are still open. */
    private class FakeTokens : CancelTokens {
        val signalled = CopyOnWriteArrayList<Long>()
        val open = CopyOnWriteArrayList<Long>()
        private val next = AtomicInteger(7)
        override fun create(): Long = next.getAndIncrement().toLong().also { open += it }
        override fun signal(token: Long) { signalled += token }
        override fun close(token: Long) { open -= token }
    }

    private class Harness(scope: CoroutineScope, debounceMs: Long = 30, stopWaitMs: Long = 500) {
        val tokens = FakeTokens()
        val idle = AtomicBoolean(true)
        val monitor = HealthMonitor(scope, Dispatchers.IO, tokens, { idle.get() }, debounceMs, stopWaitMs)
        val readOnlyPushes = CopyOnWriteArrayList<Boolean>()
        val checks = AtomicInteger(0)

        fun attach(check: (Long) -> Health) = monitor.attach(
            check = { t -> checks.incrementAndGet(); check(t) },
            setReadOnly = { readOnlyPushes += it },
        )

        /** A check that blocks until its token is signalled, like the native scan that polls it. */
        fun blockingCheck(entered: CountDownLatch, result: Health = Health.Clean): (Long) -> Health = { t ->
            entered.countDown()
            val deadline = System.nanoTime() + 5_000_000_000L
            while (!tokens.signalled.contains(t) && System.nanoTime() < deadline) Thread.sleep(2)
            if (tokens.signalled.contains(t)) Health.Unknown(HealthUnknownReason.CANCELLED) else result
        }
    }

    private suspend fun Harness.awaitState(timeoutMs: Long = 3000, pred: (Health) -> Boolean): Health =
        withTimeout(timeoutMs) {
            while (!pred(monitor.state.value)) delay(5)
            monitor.state.value
        }

    @Test
    fun runsAfterAttachWhenIdle_andShowsCheckingThenTheVerdict() = runBlocking {
        val h = Harness(scope)
        val release = CountDownLatch(1)
        h.attach { release.await(5, TimeUnit.SECONDS); Health.Clean }
        assertEquals(Health.Unknown(HealthUnknownReason.NOT_CHECKED), h.monitor.state.value)
        h.awaitState { it == Health.Checking }
        release.countDown()
        assertEquals(Health.Clean, h.awaitState { it == Health.Clean })
        assertTrue("token released after the scan", h.tokens.open.isEmpty())
    }

    @Test
    fun doesNotRunWhileBusy_andRunsAfterTheNextIdleSignal() = runBlocking {
        val h = Harness(scope)
        h.idle.set(false)
        h.attach { Health.Clean }
        delay(200)
        assertEquals("busy at the due time: no scan", 0, h.checks.get())
        assertTrue("and no token left behind", h.tokens.open.isEmpty())
        h.idle.set(true)
        h.monitor.noteIdle() // the queue drained
        h.awaitState { it == Health.Clean }
        assertEquals(1, h.checks.get())
    }

    @Test
    fun aDrainRechecksAndBurstsAreCoalesced() = runBlocking {
        val h = Harness(scope, debounceMs = 80)
        h.attach { Health.Clean }
        h.awaitState { it == Health.Clean }
        h.monitor.yieldToWriter() // a transfer starts: the verdict goes stale
        assertEquals(Health.Unknown(HealthUnknownReason.STALE), h.monitor.state.value)
        repeat(5) { h.monitor.noteIdle() } // five small jobs finish in a burst
        h.awaitState { it == Health.Clean }
        delay(250)
        assertEquals("one scan after attach, one after the burst", 2, h.checks.get())
    }

    @Test
    fun aTransferRequestSignalsTheNativeTokenAtOnce_andTheLightGoesGrey() = runBlocking {
        val h = Harness(scope)
        val entered = CountDownLatch(1)
        h.attach(h.blockingCheck(entered))
        assertTrue(entered.await(3, TimeUnit.SECONDS))
        val token = h.tokens.open.single()
        h.monitor.yieldToWriter()
        assertEquals("the scan's own token reaches the native cancel", listOf(token), h.tokens.signalled.toList())
        assertEquals(Health.Unknown(HealthUnknownReason.CANCELLED), h.monitor.state.value)
        h.awaitState { h.tokens.open.isEmpty() }
        assertEquals("a cancelled scan leaves no verdict", Health.Unknown(HealthUnknownReason.CANCELLED), h.monitor.state.value)
    }

    @Test
    fun aReadOnlyJobCancelsTheScanButKeepsAStandingVerdict() = runBlocking {
        val h = Harness(scope)
        h.attach { Health.Clean }
        h.awaitState { it == Health.Clean }
        h.monitor.yieldToWriter(writes = false)
        assertEquals("an export changes nothing on the drive", Health.Clean, h.monitor.state.value)
    }

    @Test
    fun corruptOrUnsafeLeaksLatchReadOnly_andUnknownNeverDoes() = runBlocking {
        val h = Harness(scope)
        h.attach { Health.Corrupt(mapOf("double_owned_block" to 2)) }
        h.awaitState { it is Health.Corrupt }
        assertTrue(h.monitor.readOnly.value)
        assertEquals(listOf(true), h.readOnlyPushes.toList())
        h.monitor.yieldToWriter()
        assertTrue("a red verdict is not dropped by a request", h.monitor.state.value is Health.Corrupt)

        val h2 = Harness(scope)
        h2.attach { Health.Leaks(writeSafe = false, findings = mapOf("marked_dirty" to 1)) }
        h2.awaitState { it is Health.Leaks }
        assertTrue(h2.monitor.readOnly.value)

        val h3 = Harness(scope)
        h3.attach { Health.Leaks(writeSafe = true, findings = mapOf("leaked_blocks" to 1869)) }
        h3.awaitState { it is Health.Leaks }
        assertFalse(h3.monitor.readOnly.value)
        assertTrue("never pushed", h3.readOnlyPushes.isEmpty())

        val h4 = Harness(scope)
        h4.attach { Health.Unknown(HealthUnknownReason.UNSUPPORTED) }
        h4.awaitState { it is Health.Unknown && it.reason == HealthUnknownReason.UNSUPPORTED }
        assertFalse(h4.monitor.readOnly.value)
    }

    @Test
    fun detachCancelsAtOnce_clearsTheGate_andAwaitStoppedWaitsForTheScan() = runBlocking {
        val h = Harness(scope)
        val entered = CountDownLatch(1)
        h.attach(h.blockingCheck(entered))
        assertTrue(entered.await(3, TimeUnit.SECONDS))
        val token = h.tokens.open.single()
        h.monitor.detach()
        assertTrue(h.tokens.signalled.contains(token))
        h.monitor.awaitStopped()
        assertTrue("scan left native code before the volume may close", h.tokens.open.isEmpty())
        assertEquals(Health.Unknown(HealthUnknownReason.NOT_CHECKED), h.monitor.state.value)
        delay(150)
        assertEquals("nothing reschedules after detach", 1, h.checks.get())
    }

    @Test
    fun awaitStoppedIsBoundedWhenTheDriveHangs() = runBlocking {
        val h = Harness(scope, stopWaitMs = 100)
        val entered = CountDownLatch(1)
        val hang = CountDownLatch(1)
        h.attach { entered.countDown(); hang.await(5, TimeUnit.SECONDS); Health.Clean }
        assertTrue(entered.await(3, TimeUnit.SECONDS))
        h.monitor.detach()
        val started = System.nanoTime()
        h.monitor.awaitStopped()
        assertTrue("gave up near the bound", (System.nanoTime() - started) / 1_000_000 < 1500)
        hang.countDown()
    }

    @Test
    fun aThrowingCheckIsUnknownInternal_andIsNotRetried() = runBlocking {
        val h = Harness(scope)
        h.attach { throw IllegalStateException("/secret/path") }
        h.awaitState { it is Health.Unknown && it.reason == HealthUnknownReason.INTERNAL }
        h.monitor.noteIdle()
        delay(150)
        assertEquals(1, h.checks.get())
    }

    // ------------------------------------------------------------ session wiring

    private class Vol : LuksVolume(0L)

    /** A: the check never resets the idle timer, and the idle lock cancels a running scan. */
    @Test
    fun idleLockCancelsARunningCheck_whichNeverHeldALeaseOrResetTheTimer() = runBlocking {
        val session = SessionController(scope = scope)
        val tokens = FakeTokens()
        session.healthMonitor.tokens = tokens
        session.healthMonitor.debounceMs = 10
        val vol = Vol()
        session.startUnlockedForTest(volume = vol, partition = PartitionInfo(0, "t", 0L, 1024L, true, 2))
        val entered = CountDownLatch(1)
        session.healthMonitor.attach(
            check = { t ->
                entered.countDown()
                while (!tokens.signalled.contains(t)) Thread.sleep(2)
                Health.Unknown(HealthUnknownReason.CANCELLED)
            },
            setReadOnly = {},
        )
        assertTrue(entered.await(3, TimeUnit.SECONDS))
        session.setIdleTimeout(300)
        assertEquals("the scan holds no lease", 0, session.activeLeases)
        withTimeout(5000) { while (session.state.value !is SessionState.Locked) delay(10) }
        assertEquals("idle-lock signalled the scan's token", 1, tokens.signalled.size)
        withTimeout(3000) { while (tokens.open.isNotEmpty()) delay(10) }
    }

    @Test
    fun aRedVerdictRefusesWritesOnTheVolume_withoutClosingTheSession() = runBlocking {
        val session = SessionController(scope = scope)
        session.healthMonitor.tokens = FakeTokens()
        val vol = Vol()
        session.startUnlockedForTest(volume = vol, partition = PartitionInfo(0, "t", 0L, 1024L, true, 2))
        session.healthMonitor.attach(
            check = { Health.Corrupt(mapOf("double_owned_block" to 1)) },
            setReadOnly = { vol.healthReadOnly = it },
        )
        withTimeout(3000) { while (!session.healthReadOnly.value) delay(5) }
        val refused = listOf<() -> Any?>(
            { vol.writeFile("/", "a", ByteArray(1)) },
            { vol.deleteFile("/a") },
            { vol.createDirectory("/", "d") },
            { vol.rename("/", "a", "/", "b") },
            { vol.beginFile(1) },
            { vol.beginFileStreaming() },
        ).map { op -> runCatching { op() }.exceptionOrNull() }
        refused.forEach {
            val e = it as? dev.luksandroid.LuksException
            assertEquals("every write path refuses with the health code", dev.luksandroid.LuksException.HEALTH_READ_ONLY, e?.code)
        }
        assertTrue(session.state.value is SessionState.Unlocked)
        assertFalse("not the write-dead teardown path", session.isWriteDead.value)
    }

    private class ListingVol : LuksVolume(0L) {
        override fun listDir(path: String) = emptyList<dev.luksandroid.Entry>()
    }

    /** Lead follow-up: a read through the lease cancels the scan instead of waiting on its fs lock. */
    @Test
    fun aListingDuringAScanReturnsPromptly_cancelsTheToken_andTheMonitorRerunsAfterwards() = runBlocking {
        val session = SessionController(scope = scope)
        val tokens = FakeTokens()
        session.healthMonitor.tokens = tokens
        session.healthMonitor.debounceMs = 20
        val vol = ListingVol()
        session.startUnlockedForTest(volume = vol, partition = PartitionInfo(0, "t", 0L, 1024L, true, 2))
        val calls = AtomicInteger(0)
        val firstEntered = CountDownLatch(1)
        val secondEntered = CountDownLatch(1)
        session.healthMonitor.attach(
            check = { t ->
                if (calls.incrementAndGet() == 1) {
                    firstEntered.countDown()
                    // Like the native scan: holds "the lock" until its token is signalled.
                    while (!tokens.signalled.contains(t)) Thread.sleep(2)
                    Health.Unknown(HealthUnknownReason.CANCELLED)
                } else {
                    secondEntered.countDown()
                    Health.Clean
                }
            },
            setReadOnly = {},
        )
        assertTrue(firstEntered.await(3, TimeUnit.SECONDS))
        val firstToken = tokens.open.single()
        val started = System.nanoTime()
        withTimeout(1000) { session.withLease { it.listDir("/") } }
        assertTrue("the read did not wait out the scan", (System.nanoTime() - started) / 1_000_000 < 800)
        assertEquals(listOf(firstToken), tokens.signalled.toList())
        assertTrue("re-runs once the session is idle again", secondEntered.await(5, TimeUnit.SECONDS))
        withTimeout(3000) { while (session.health.value != Health.Clean) delay(5) }
    }
}
