package dev.luksandroid.session

import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import java.util.Collections

/** N1: the foreground service runs while the queue is non-empty or a SAF write is open. */
class TransferForegroundControllerTest {
    private class FakeHost : ForegroundHost {
        val calls: MutableList<String> = Collections.synchronizedList(mutableListOf())
        @Volatile var failShow: Boolean = false
        override fun show(count: Int) {
            calls.add("show($count)")
            if (failShow) throw IllegalStateException("ForegroundServiceStartNotAllowed")
        }
        override fun hide() { calls.add("hide") }
        fun snapshot() = synchronized(calls) { calls.toList() }
    }

    private lateinit var scope: CoroutineScope
    private val queue = MutableStateFlow(0)
    private val busy = MutableStateFlow(false)
    private val host = FakeHost()

    @Before
    fun setUp() {
        scope = CoroutineScope(Dispatchers.Default + SupervisorJob())
        TransferForegroundController(queue, busy, host).start(scope)
    }

    @After
    fun tearDown() { scope.cancel() }

    private suspend fun awaitCalls(expected: List<String>) {
        withTimeout(2000) { while (host.snapshot() != expected) delay(5) }
        assertEquals(expected, host.snapshot())
    }

    @Test
    fun idle_doesNothing() = runBlocking {
        delay(100)
        assertEquals(emptyList<String>(), host.snapshot())
    }

    @Test
    fun queueNonEmpty_shows_updatesCount_andHidesWhenDrained() = runBlocking {
        // queue before busy: (2, not busy) maps to 0 and is suppressed, so the first emission is show(2).
        // The other order let the collector see (0, busy) -> show(1) first.
        queue.value = 2
        busy.value = true
        awaitCalls(listOf("show(2)"))
        queue.value = 1
        awaitCalls(listOf("show(2)", "show(1)"))
        queue.value = 0
        busy.value = false
        awaitCalls(listOf("show(2)", "show(1)", "hide"))
    }

    @Test
    fun busyWithAnEmptyQueue_aSafWrite_keepsItRunningAsOne() = runBlocking {
        busy.value = true
        awaitCalls(listOf("show(1)"))
        busy.value = false
        awaitCalls(listOf("show(1)", "hide"))
    }

    @Test
    fun aStaleQueueCount_withoutBusy_doesNotKeepItRunning() = runBlocking {
        queue.value = 3
        delay(100)
        assertEquals(emptyList<String>(), host.snapshot())
    }

    @Test
    fun aRefusedStart_doesNotKillTheController() = runBlocking {
        host.failShow = true
        busy.value = true
        queue.value = 1
        awaitCalls(listOf("show(1)"))
        host.failShow = false
        queue.value = 2
        awaitCalls(listOf("show(1)", "show(2)"))
        assertTrue(true)
    }
}

/** M3: a stop must never chase a start that has not been confirmed. */
class TransferForegroundSequencingTest {
    private class Host : ForegroundHost {
        val calls: MutableList<String> = Collections.synchronizedList(mutableListOf())
        override fun show(count: Int) { calls.add("show($count)") }
        override fun hide() { calls.add("hide") }
        fun snapshot() = synchronized(calls) { calls.toList() }
    }

    private val scope = CoroutineScope(Dispatchers.Default + SupervisorJob())
    private val queue = MutableStateFlow(0)
    private val busy = MutableStateFlow(false)
    private val host = Host()

    @After
    fun tearDown() { scope.cancel() }

    @Test
    fun aFastTrueFalse_neverStopsTheService_beforeTheDebounce() = runBlocking {
        TransferForegroundController(queue, busy, host, stopDelayMs = 400).start(scope)
        busy.value = true
        withTimeout(2000) { while (host.snapshot().isEmpty()) delay(5) }
        busy.value = false
        delay(150)
        assertEquals("hide must wait out the debounce", listOf("show(1)"), host.snapshot())
        delay(500)
        assertEquals(listOf("show(1)", "hide"), host.snapshot())
    }

    @Test
    fun aFlap_withinTheDebounce_neverHides() = runBlocking {
        TransferForegroundController(queue, busy, host, stopDelayMs = 400).start(scope)
        busy.value = true
        withTimeout(2000) { while (host.snapshot().isEmpty()) delay(5) }
        busy.value = false
        delay(100)
        busy.value = true
        delay(600)
        assertTrue("no hide while work resumed: ${host.snapshot()}", "hide" !in host.snapshot())
    }
}
