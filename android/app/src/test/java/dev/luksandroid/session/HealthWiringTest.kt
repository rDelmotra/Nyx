package dev.luksandroid.session

import dev.luksandroid.Health
import dev.luksandroid.HealthUnknownReason
import dev.luksandroid.LuksException
import dev.luksandroid.LuksVolume
import dev.luksandroid.PartitionInfo
import dev.luksandroid.VolumeInfo
import dev.luksandroid.ui.UiErrorMessage
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

/**
 * F8: the health monitor as wired into the transfer engine and the session singleton. Real
 * [TransferManager] and [LuksSession]; fake volume, fake tokens, scripted check.
 */
class HealthWiringTest {
    private val tokens = object : CancelTokens {
        val signalled = CopyOnWriteArrayList<Long>()
        val open = CopyOnWriteArrayList<Long>()
        private val next = AtomicInteger(100)
        override fun create(): Long = next.getAndIncrement().toLong().also { open += it }
        override fun signal(token: Long) { signalled += token }
        override fun close(token: Long) { open -= token }
    }
    private val originalTokens = LuksSession.healthMonitor.tokens

    @After
    fun reset() {
        runBlocking { LuksSession.lock() }
        LuksSession.healthMonitor.tokens = originalTokens
        val field = TransferController::class.java.getDeclaredField("_transfers")
        field.isAccessible = true
        @Suppress("UNCHECKED_CAST")
        (field.get(TransferManager) as MutableStateFlow<List<TransferItem>>).value = emptyList()
    }

    private class Vol : LuksVolume(0L) {
        override val info = VolumeInfo("Test", "uuid-0000", 4096, 1024L, "ext4", emptyList())
        override fun fileSize(path: String): Long = 1L
        override fun sha256(path: String, chunkBytes: Int) = Digest("h", 1L, 1L, 1L)

        // A real commit on a handle-0 volume throws, which would leave the engine's dirty flag
        // set for whichever test runs next in this JVM.
        override fun commitActiveBatch() {}
    }

    private fun unlocked(vol: LuksVolume) = runBlocking {
        LuksSession.healthMonitor.tokens = tokens
        LuksSession.startUnlockedForTest(volume = vol, partition = PartitionInfo(0, "t", 0L, 1024L, true, 2))
    }

    private fun attach(vol: Vol, check: (Long) -> Health) =
        LuksSession.healthMonitor.attach(check = check, setReadOnly = { vol.healthReadOnly = it })

    private fun blocking(entered: CountDownLatch): (Long) -> Health = { t ->
        entered.countDown()
        while (!tokens.signalled.contains(t)) Thread.sleep(2)
        Health.Unknown(HealthUnknownReason.CANCELLED)
    }

    /** A transfer being queued reaches the scan's native token, read-only jobs included. */
    @Test
    fun queueingAJobSignalsTheRunningScan() = runBlocking {
        val vol = Vol()
        unlocked(vol)
        val entered = CountDownLatch(1)
        attach(vol, blocking(entered))
        assertTrue(entered.await(5, TimeUnit.SECONDS))
        val token = tokens.open.single()
        TransferManager.startHash("/p")
        assertEquals(listOf(token), tokens.signalled.toList())
    }

    @Test
    fun aSafWriteReachesTheScanToo_andAMutatorDoes() = runBlocking {
        val vol = Vol()
        unlocked(vol)
        val entered = CountDownLatch(1)
        attach(vol, blocking(entered))
        assertTrue(entered.await(5, TimeUnit.SECONDS))
        assertTrue(TransferManager.tryAcquireForSafWrite(1000))
        try {
            assertEquals("SAF acquire cancelled the scan", 1, tokens.signalled.size)
        } finally {
            TransferManager.releaseSafWriteLock()
        }
        val entered2 = CountDownLatch(1)
        attach(vol, blocking(entered2))
        assertTrue("rechecks once the SAF write released", entered2.await(5, TimeUnit.SECONDS))
        val before = tokens.signalled.size
        TransferManager.withExclusiveWrite { }
        assertEquals("a browser mutation cancelled the second scan", before + 1, tokens.signalled.size)
    }

    @Test
    fun theScanDoesNotStartWhileATransferIsQueued_andRunsAfterTheDrain() = runBlocking {
        val vol = Vol()
        unlocked(vol)
        val gate = CountDownLatch(1)
        val job = TransferManager.startJob("j", TransferType.HASH) { _, _ -> gate.await(5, TimeUnit.SECONDS) }
        val ran = AtomicInteger(0)
        attach(vol) { ran.incrementAndGet(); Health.Clean }
        delay(2500) // past the default debounce, with the job still holding the queue
        assertEquals(0, ran.get())
        gate.countDown()
        withTimeout(8000) { while (LuksSession.health.value != Health.Clean) delay(20) }
        assertEquals(1, ran.get())
        assertEquals(TransferState.COMPLETED, TransferManager.getTransfer(job)?.state)
    }

    @Test
    fun healthReadOnlyRefusesImportsAndMutators_keepsReads_andKeepsTheSessionOpen() = runBlocking {
        val vol = Vol()
        unlocked(vol)
        attach(vol) { Health.Corrupt(mapOf("double_owned_block" to 1)) }
        withTimeout(8000) { while (!LuksSession.healthReadOnly.value) delay(20) }

        val ran = AtomicInteger(0)
        val import = TransferManager.startJob("i", TransferType.IMPORT, writes = true) { _, _ -> ran.incrementAndGet() }
        withTimeout(5000) { while (TransferManager.getTransfer(import)?.state?.let { it == TransferState.QUEUED || it == TransferState.RUNNING } != false) delay(10) }
        val item = TransferManager.getTransfer(import)!!
        assertEquals(TransferState.FAILED, item.state)
        assertEquals(LuksException.HEALTH_READ_ONLY, item.errorCode)
        assertEquals(UiErrorMessage.HEALTH_READ_ONLY_MESSAGE, item.error)
        assertEquals("the write body never ran", 0, ran.get())

        val mutator = runCatching { TransferManager.withExclusiveWrite { } }.exceptionOrNull() as LuksException
        assertEquals(LuksException.HEALTH_READ_ONLY, mutator.code)

        val hash = TransferManager.startHash("/p")
        withTimeout(5000) { while (TransferManager.getTransfer(hash)?.state != TransferState.COMPLETED) delay(10) }

        assertTrue(LuksSession.state.value is SessionState.Unlocked)
        assertFalse(LuksSession.isWriteDead.value)
        assertFalse(TransferManager.isWriteDead(vol))
    }

    @Test
    fun cleanAndSafeLeaksDoNotGateWrites() = runBlocking {
        val vol = Vol()
        unlocked(vol)
        attach(vol) { Health.Leaks(writeSafe = true, findings = mapOf("leaked_blocks" to 3)) }
        withTimeout(8000) { while (LuksSession.health.value !is Health.Leaks) delay(20) }
        assertFalse(LuksSession.healthReadOnly.value)
        val id = TransferManager.startJob("i", TransferType.IMPORT, writes = true) { _, _ -> }
        withTimeout(5000) { while (TransferManager.getTransfer(id)?.state != TransferState.COMPLETED) delay(10) }
    }
}
