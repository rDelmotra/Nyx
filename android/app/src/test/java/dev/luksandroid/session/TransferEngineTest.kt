package dev.luksandroid.session

import dev.luksandroid.LuksVolume
import dev.luksandroid.PartitionInfo
import dev.luksandroid.VolumeInfo
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.withTimeoutOrNull
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
 * F5 E1 engine tests that are not the four KNOWN-RED ones (those live in
 * [TransferIntegrityRedTest]). Each was written red against the pre-F5 engine.
 */
class TransferEngineTest {

    @After
    fun resetSingleton() {
        runBlocking { LuksSession.lock() }
        val field = TransferController::class.java.getDeclaredField("_transfers")
        field.isAccessible = true
        @Suppress("UNCHECKED_CAST")
        (field.get(TransferManager) as MutableStateFlow<List<TransferItem>>).value = emptyList()
    }

    private fun unlocked(volume: LuksVolume) = runBlocking {
        LuksSession.startUnlockedForTest(volume = volume, partition = PartitionInfo(0, "test", 0L, 1024L, true, 2))
    }

    private class OrderVolume(private val gatedPath: String) : LuksVolume(0L) {
        override val info = VolumeInfo("Test", "uuid-0000", 4096, 1024L, "ext4", emptyList())
        val gate = CountDownLatch(1)
        val entered = CountDownLatch(1)
        val order = CopyOnWriteArrayList<String>()
        val concurrent = AtomicInteger(0)
        val maxConcurrent = AtomicInteger(0)

        override fun fileSize(path: String): Long = 100L

        override fun sha256(path: String, chunkBytes: Int): Digest {
            val c = concurrent.incrementAndGet()
            maxConcurrent.updateAndGet { maxOf(it, c) }
            try {
                order += path
                if (path == gatedPath) {
                    entered.countDown()
                    gate.await(10, TimeUnit.SECONDS)
                }
                return Digest("h", 100L, 1L, 1L)
            } finally {
                concurrent.decrementAndGet()
            }
        }
    }

    // ---------------------------------------------------------------- C4

    /**
     * C4: jobs queued while one is running must start in start order. Pre-F5 every
     * startX launched its own coroutine on Dispatchers.IO and raced to the mutex.
     */
    @Test
    fun c4_queuedJobsRunInStartOrder() = runBlocking {
        val vol = OrderVolume("/p0")
        unlocked(vol)
        try {
            val ids = ArrayList<Long>()
            ids += TransferManager.startHash("/p0")
            assertTrue(vol.entered.await(5, TimeUnit.SECONDS))
            for (i in 1..40) ids += TransferManager.startHash("/p$i")
            vol.gate.countDown()
            withTimeout(15_000) {
                while (TransferManager.getTransfer(ids.last())?.state != TransferState.COMPLETED) delay(10)
            }
            assertEquals((0..40).map { "/p$it" }, vol.order.toList())
            assertEquals(1, vol.maxConcurrent.get())
        } finally {
            vol.gate.countDown()
        }
    }

    // ---------------------------------------------------------------- C1 (owner token)

    /**
     * C1: a stray releaseSafWriteLock() (no SAF write holds the lock) must not unlock the
     * mutex a running transfer owns. Pre-F5 it called Mutex.unlock() with no owner check.
     */
    @Test
    fun c1_strayReleaseDoesNotUnlockARunningTransfer() = runBlocking {
        val vol = OrderVolume("/a")
        unlocked(vol)
        try {
            TransferManager.startHash("/a")
            assertTrue(vol.entered.await(5, TimeUnit.SECONDS))
            TransferManager.releaseSafWriteLock() // not ours to release
            TransferManager.startHash("/b")
            withTimeoutOrNull(800) { while (vol.order.size < 2) delay(10) }
            assertEquals("second job entered while the first still owned the lock", listOf("/a"), vol.order.toList())
        } finally {
            vol.gate.countDown()
        }
    }

    /** A SAF write holds the lock; the release pairs with exactly that acquire. */
    @Test
    fun c1_safAcquireAndReleasePairUp() = runBlocking {
        val vol = OrderVolume("/none")
        unlocked(vol)
        assertTrue(TransferManager.tryAcquireForSafWrite(1_000))
        val id = TransferManager.startHash("/x")
        delay(300)
        assertEquals("a transfer must wait for the SAF write", TransferState.QUEUED, TransferManager.getTransfer(id)?.state)
        TransferManager.releaseSafWriteLock()
        withTimeout(5_000) { while (TransferManager.getTransfer(id)?.state != TransferState.COMPLETED) delay(10) }
    }

    @Test
    fun c1_safAcquireTimesOutWhileATransferOwnsTheLockAndLeavesItIntact() = runBlocking {
        val vol = OrderVolume("/a")
        unlocked(vol)
        try {
            TransferManager.startHash("/a")
            assertTrue(vol.entered.await(5, TimeUnit.SECONDS))
            assertFalse(TransferManager.tryAcquireForSafWrite(200))
            TransferManager.releaseSafWriteLock() // would have been a force-unlock before
            TransferManager.startHash("/b")
            withTimeoutOrNull(500) { while (vol.order.size < 2) delay(10) }
            assertEquals(listOf("/a"), vol.order.toList())
        } finally {
            vol.gate.countDown()
        }
    }

    // ---------------------------------------------------------------- C5

    /** C5 (L5-O17): a lease failure must end the transfer, not leave it RUNNING forever. */
    @Test
    fun c5_hashOnALockedSessionFailsInsteadOfStayingRunning() = runBlocking {
        // no unlocked(): the session is Locked
        val done = CompletableDeferred<Result<LuksVolume.Digest>>()
        val id = TransferManager.startHash("/a") { done.complete(it) }
        withTimeout(5_000) { done.await() }
        assertEquals(TransferState.FAILED, TransferManager.getTransfer(id)?.state)
    }

    /** C5: a transfer in a terminal state never changes again, whichever writer is late. */
    @Test
    fun c5_finishTransferIsWriteOnce() {
        val c = TransferController()
        val id = c.enqueueForTest("x", TransferType.IMPORT)
        assertTrue(c.finishTransfer(id, TransferState.FAILED, error = "boom"))
        assertFalse(c.finishTransfer(id, TransferState.COMPLETED))
        assertFalse(c.finishTransfer(id, TransferState.CANCELLED))
        val item = c.getTransfer(id)!!
        assertEquals(TransferState.FAILED, item.state)
        assertEquals("boom", item.error)
    }

    // ---------------------------------------------------------------- policy (a): drain commit

    private class CountingVolume : LuksVolume(0L) {
        override val info = VolumeInfo("Test", "uuid-0000", 4096, 1024L, "ext4", emptyList())
        val commits = AtomicInteger(0)
        override fun commitActiveBatch() { commits.incrementAndGet() }
    }

    private suspend fun awaitState(id: Long, state: TransferState) =
        withTimeout(10_000) { while (TransferManager.getTransfer(id)?.state != state) delay(10) }

    @Test
    fun policyA_oneCommitWhenTheQueueDrains() = runBlocking {
        val vol = CountingVolume()
        unlocked(vol)
        val gate = CompletableDeferred<Unit>()
        val a = TransferManager.startJob("a", TransferType.IMPORT, writes = true) { _, _ -> gate.await() }
        val b = TransferManager.startJob("b", TransferType.IMPORT, writes = true) { _, _ -> }
        val c = TransferManager.startJob("c", TransferType.IMPORT, writes = true) { _, _ -> }
        delay(100)
        assertEquals("no commit while work is still queued", 0, vol.commits.get())
        gate.complete(Unit)
        awaitState(c, TransferState.COMPLETED)
        withTimeout(5_000) { while (TransferManager.queueSize.value != 0) delay(10) }
        assertEquals("exactly one commit, after the last job", 1, vol.commits.get())
        assertEquals(TransferState.COMPLETED, TransferManager.getTransfer(a)?.state)
        assertEquals(TransferState.COMPLETED, TransferManager.getTransfer(b)?.state)
    }

    @Test
    fun policyA_readOnlyJobsDoNotCommit() = runBlocking {
        val vol = CountingVolume()
        unlocked(vol)
        val id = TransferManager.startJob("r", TransferType.HASH, writes = false) { _, _ -> }
        awaitState(id, TransferState.COMPLETED)
        withTimeout(5_000) { while (TransferManager.queueSize.value != 0) delay(10) }
        assertEquals(0, vol.commits.get())
    }

    @Test
    fun commitNowCommitsOnTheLiveVolumeAndReportsAnUnlockedSessionOnly() = runBlocking {
        assertFalse("locked session: nothing to commit, no throw", TransferManager.commitNow())
        val vol = CountingVolume()
        unlocked(vol)
        assertTrue(TransferManager.commitNow())
        assertEquals(1, vol.commits.get())
    }

    @Test
    fun withExclusiveWriteHoldsTheTransferLockAndCommitsAfter() = runBlocking {
        val vol = OrderVolume("/a")
        unlocked(vol)
        try {
            TransferManager.startHash("/a")
            assertTrue(vol.entered.await(5, TimeUnit.SECONDS))
            val ran = CompletableDeferred<Unit>()
            val job = launch(kotlinx.coroutines.Dispatchers.IO) {
                TransferManager.withExclusiveWrite { ran.complete(Unit) }
            }
            delay(300)
            assertFalse("mutator must wait for the running transfer", ran.isCompleted)
            vol.gate.countDown()
            withTimeout(5_000) { ran.await() }
            job.join()
        } finally {
            vol.gate.countDown()
        }
    }

    // ---------------------------------------------------------------- C6: fatal errors reach the session

    private class FatalVolume : LuksVolume(0L) {
        override val info = VolumeInfo("Test", "uuid-0000", 4096, 1024L, "ext4", emptyList())
    }

    @Test
    fun c6_fencedWriteFailureIsPublishedAndStopsLaterWriteJobsButNotReads() = runBlocking {
        val vol = FatalVolume()
        unlocked(vol)
        val seen = java.util.concurrent.CopyOnWriteArrayList<Int>()
        val collector = launch(kotlinx.coroutines.Dispatchers.Default) {
            TransferManager.fatalWriteErrors.collect { seen += it.code }
        }
        try {
            val bad = TransferManager.startJob("bad", TransferType.IMPORT, writes = true) { _, _ ->
                throw dev.luksandroid.LuksException("fenced", dev.luksandroid.LuksException.WRITE_SESSION_FENCED)
            }
            awaitState(bad, TransferState.FAILED)
            assertEquals(dev.luksandroid.LuksException.WRITE_SESSION_FENCED, TransferManager.getTransfer(bad)?.errorCode)
            withTimeout(5_000) { while (seen.isEmpty()) delay(10) }
            assertEquals(listOf(dev.luksandroid.LuksException.WRITE_SESSION_FENCED), seen.toList())
            assertTrue(TransferManager.isWriteDead(vol))

            val ranWrite = AtomicInteger(0)
            val w = TransferManager.startJob("w", TransferType.IMPORT, writes = true) { _, _ -> ranWrite.incrementAndGet() }
            awaitState(w, TransferState.FAILED)
            assertEquals("a write job must not run on a fenced volume", 0, ranWrite.get())

            val ranRead = AtomicInteger(0)
            val r = TransferManager.startJob("r", TransferType.EXPORT, writes = false) { _, _ -> ranRead.incrementAndGet() }
            awaitState(r, TransferState.COMPLETED)
            assertEquals(1, ranRead.get())
        } finally {
            collector.cancel()
        }
    }

    @Test
    fun c6_aFreshVolumeIsWritableAgain() = runBlocking {
        val dead = FatalVolume()
        unlocked(dead)
        val bad = TransferManager.startJob("bad", TransferType.IMPORT, writes = true) { _, _ ->
            throw dev.luksandroid.LuksException("poisoned", dev.luksandroid.LuksException.MUTEX_POISONED)
        }
        awaitState(bad, TransferState.FAILED)
        assertTrue(TransferManager.isWriteDead(dead))

        LuksSession.lock()
        val fresh = FatalVolume()
        unlocked(fresh)
        assertFalse(TransferManager.isWriteDead(fresh))
        val ran = AtomicInteger(0)
        val w = TransferManager.startJob("w", TransferType.IMPORT, writes = true) { _, _ -> ran.incrementAndGet() }
        awaitState(w, TransferState.COMPLETED)
        assertEquals(1, ran.get())
    }

    @Test
    fun c6_aNonFatalFailureLeavesWritesAlive() = runBlocking {
        val vol = FatalVolume()
        unlocked(vol)
        val bad = TransferManager.startJob("bad", TransferType.IMPORT, writes = true) { _, _ ->
            throw dev.luksandroid.LuksException("no space", dev.luksandroid.LuksException.NO_SPACE)
        }
        awaitState(bad, TransferState.FAILED)
        assertFalse(TransferManager.isWriteDead(vol))
    }

    // ---------------------------------------------------------------- queue size (foreground service)

    @Test
    fun queueSizeCountsQueuedAndRunningThenReturnsToZero() = runBlocking {
        unlocked(FatalVolume())
        val gate = CompletableDeferred<Unit>()
        TransferManager.startJob("a", TransferType.HASH) { _, _ -> gate.await() }
        TransferManager.startJob("b", TransferType.HASH) { _, _ -> }
        withTimeout(5_000) { while (TransferManager.queueSize.value != 2) delay(10) }
        assertTrue(TransferManager.busy.value)
        gate.complete(Unit)
        withTimeout(5_000) { while (TransferManager.queueSize.value != 0) delay(10) }
        assertFalse(TransferManager.busy.value)
    }

    @Test
    fun aSafWriteCountsAsBusy() = runBlocking {
        assertTrue(TransferManager.tryAcquireForSafWrite(1_000))
        try {
            assertTrue(TransferManager.busy.value)
        } finally {
            TransferManager.releaseSafWriteLock()
        }
        assertFalse(TransferManager.busy.value)
    }

    // ---------------------------------------------------------------- C3: the token reaches native

    private class TokenVolume : LuksVolume(0L) {
        override val info = VolumeInfo("Test", "uuid-0000", 4096, 1024L, "ext4", emptyList())
        val tokenSeenBySha = AtomicInteger(-1)
        override fun fileSize(path: String): Long = 10L
        override fun sha256(path: String, chunkBytes: Int): Digest {
            tokenSeenBySha.set(dev.luksandroid.currentNativeCancelToken().toInt())
            return Digest("h", 10L, 1L, 1L)
        }
    }

    private class FakeTokens : CancelTokens {
        val signalled = CopyOnWriteArrayList<Long>()
        val closed = CopyOnWriteArrayList<Long>()
        override fun create(): Long = 77L
        override fun signal(token: Long) { signalled += token }
        override fun close(token: Long) { closed += token }
    }

    @Test
    fun c3_hashRunsWithItsCancelTokenAndClosesIt() = runBlocking {
        val c = TransferController()
        val tokens = FakeTokens()
        c.cancelTokens = tokens
        val vol = TokenVolume()
        unlocked(vol)
        val done = CompletableDeferred<Unit>()
        val id = c.startHash("/a") { done.complete(Unit) }
        withTimeout(5_000) { done.await() }
        assertEquals("sha256 must see the transfer's token", 77, vol.tokenSeenBySha.get())
        assertEquals(listOf(77L), tokens.closed.toList())
        assertEquals(TransferState.COMPLETED, c.getTransfer(id)?.state)
    }

    @Test
    fun c3_cancelSignalsTheNativeToken() = runBlocking {
        val c = TransferController()
        val tokens = FakeTokens()
        c.cancelTokens = tokens
        unlocked(FatalVolume())
        val gate = CompletableDeferred<Unit>()
        val id = c.startJob("a", TransferType.HASH, writes = false) { _, tid ->
            c.attachCancelToken(tid, tokens.create())
            gate.await()
        }
        withTimeout(5_000) { while (c.getTransfer(id)?.cancelToken != 77L) delay(10) }
        c.cancelTransfer(id)
        assertEquals(listOf(77L), tokens.signalled.toList())
    }

    // ---------------------------------------------------------------- review fixes

    @Test
    fun r1_withExclusiveWriteGivesUpWithABusyErrorInsteadOfWaitingForever() = runBlocking {
        val c = TransferController()
        c.exclusiveWaitMs = 300
        val vol = OrderVolume("/a")
        unlocked(vol)
        try {
            c.startHash("/a")
            assertTrue(vol.entered.await(5, TimeUnit.SECONDS))
            val e = try {
                c.withExclusiveWrite { }
                null
            } catch (e: dev.luksandroid.LuksException) {
                e
            }
            assertEquals(dev.luksandroid.LuksException.WRITER_BUSY, e?.code)
        } finally {
            vol.gate.countDown()
        }
    }

    @Test
    fun r1_aDeadVolumeRefusesWithTheFencedCodeNotAnUnexpectedError() = runBlocking {
        val vol = FatalVolume()
        unlocked(vol)
        val bad = TransferManager.startJob("bad", TransferType.IMPORT, writes = true) { _, _ ->
            throw dev.luksandroid.LuksException("fenced", dev.luksandroid.LuksException.WRITE_SESSION_FENCED)
        }
        awaitState(bad, TransferState.FAILED)
        val e = try {
            TransferManager.withExclusiveWrite { }
            null
        } catch (e: dev.luksandroid.LuksException) {
            e
        }
        assertEquals(dev.luksandroid.LuksException.WRITE_SESSION_FENCED, e?.code)
    }

    private class FlakyCommitVolume(var failures: Int) : LuksVolume(0L) {
        override val info = VolumeInfo("Test", "uuid-0000", 4096, 1024L, "ext4", emptyList())
        val attempts = AtomicInteger(0)
        override fun commitActiveBatch() {
            attempts.incrementAndGet()
            if (failures > 0) { failures--; throw dev.luksandroid.LuksException("io", dev.luksandroid.LuksException.IO) }
        }
    }

    @Test
    fun r2_aFailedDrainCommitIsRetriedOnce() = runBlocking {
        val c = TransferController()
        val vol = FlakyCommitVolume(failures = 1)
        unlocked(vol)
        val id = c.startJob("a", TransferType.IMPORT, writes = true) { _, _ -> }
        withTimeout(10_000) { while (c.queueSize.value != 0 || c.getTransfer(id)?.state != TransferState.COMPLETED) delay(10) }
        assertEquals(2, vol.attempts.get())
        assertEquals(null, c.commitFailure.value)
    }

    @Test
    fun r2_aDrainCommitThatFailsTwiceSetsTheBannerAndItStaysUntilALaterCommitSucceeds() = runBlocking {
        val c = TransferController()
        val vol = FlakyCommitVolume(failures = 2)
        unlocked(vol)
        val id = c.startJob("a", TransferType.IMPORT, writes = true) { _, _ -> }
        withTimeout(10_000) { while (c.queueSize.value != 0 || c.getTransfer(id)?.state != TransferState.COMPLETED) delay(10) }
        assertEquals(2, vol.attempts.get())
        assertEquals(dev.luksandroid.LuksException.IO, c.commitFailure.value?.code)
        assertTrue("a later commit succeeds and clears it", c.commitNow())
        assertEquals(null, c.commitFailure.value)
    }

    @Test
    fun r9_aFatalErrorSkipsTheDrainCommit() = runBlocking {
        val c = TransferController()
        val vol = CountingVolume()
        unlocked(vol)
        val id = c.startJob("bad", TransferType.IMPORT, writes = true) { _, _ ->
            throw dev.luksandroid.LuksException("fenced", dev.luksandroid.LuksException.WRITE_SESSION_FENCED)
        }
        awaitStateOn(c, id, TransferState.FAILED)
        withTimeout(5_000) { while (c.queueSize.value != 0) delay(10) }
        assertEquals(0, vol.commits.get())
    }

    private suspend fun awaitStateOn(c: TransferController, id: Long, state: TransferState) =
        withTimeout(10_000) { while (c.getTransfer(id)?.state != state) delay(10) }
}
