package dev.luksandroid.session

import android.content.ContentResolver
import android.content.Context
import android.content.ContextWrapper
import android.database.Cursor
import android.database.MatrixCursor
import android.net.Uri
import android.os.ParcelFileDescriptor
import dev.luksandroid.LuksVolume
import dev.luksandroid.PartitionInfo
import dev.luksandroid.VolumeInfo
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.withTimeoutOrNull
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.nio.ByteBuffer
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger

/**
 * F0.7 tests for W5, W6, C1, C5 (notes/recon/transfer-issue-register.md). They were written
 * red against the pre-F5 engine and went green with F5 (E1); they now run in every build.
 *
 * JVM seam limits: libluks_jni is not loaded, so every LuksNative call throws
 * UnsatisfiedLinkError and native calls cannot be recorded. W5 therefore
 * observes the Kotlin side only (see its doc).
 */
class TransferIntegrityRedTest {

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

    private val info = VolumeInfo("Test", "uuid-0000", 4096, 1024L, "ext4", emptyList())

    // ---------------------------------------------------------------- W5

    private class W5Volume : LuksVolume(0L) {
        init {
            // The ctor parses native volume info for a non-zero handle, so open it
            // as handle 0 and then give it a live-looking handle.
            LuksVolume::class.java.getDeclaredField("handle").also { it.isAccessible = true }.setLong(this, 1L)
        }
        /** writerHandle at the moment each close() was entered. */
        val closeHandles = CopyOnWriteArrayList<Long>()

        inner class Writer : FileWriter(7L) {
            override fun close() {
                closeHandles += handleField().getLong(this)
                super.close()
            }
        }

        fun register(w: FileWriter) {
            val f = LuksVolume::class.java.getDeclaredField("activeWriters")
            f.isAccessible = true
            @Suppress("UNCHECKED_CAST")
            (f.get(this) as MutableSet<FileWriter>).add(w)
        }

        fun tracked(w: FileWriter): Boolean {
            val f = LuksVolume::class.java.getDeclaredField("activeWriters")
            f.isAccessible = true
            @Suppress("UNCHECKED_CAST")
            return (f.get(this) as Set<FileWriter>).contains(w)
        }
    }

    companion object {
        fun handleField() = LuksVolume.FileWriter::class.java.getDeclaredField("writerHandle").also { it.isAccessible = true }
    }

    /**
     * W5 (L5-O1): FileWriter.finish zeroes writerHandle and drops the writer from
     * activeWriters BEFORE the native finish. When native finish throws (here: the
     * unloaded JNI lib throws UnsatisfiedLinkError, a real throw from the real
     * nativeFinishFile call), nothing can ever issue nativeCloseWriter.
     *
     * Correct behaviour: after a failed finish the writer is still closable, i.e.
     * a close() with a non-zero handle was attempted by finish itself, or by the
     * volume teardown (writer still tracked with a non-zero handle). close() is
     * the only place that issues nativeCloseWriter, so this is the seam.
     */
    @Test
    fun w5_finishThatThrowsNativelyStillClosesTheNativeWriter() {
        val vol = W5Volume()
        val writer = vol.Writer()
        vol.register(writer)

        var threw = false
        try {
            writer.finish("/", "x")
        } catch (_: Throwable) {
            threw = true
        }
        assertTrue("native finish must have thrown for this test to mean anything", threw)

        vol.close() // session teardown: closes every writer it still tracks

        val handleNow = handleField().getLong(writer)
        val closableLater = vol.tracked(writer) && handleNow != 0L
        val closedWithHandle = vol.closeHandles.any { it != 0L }
        assertTrue(
            "after failed finish the native writer was never closed and can no longer be " +
                "(close() calls with handle=${vol.closeHandles}, writerHandle now=$handleNow, " +
                "tracked=${vol.tracked(writer)})",
            closedWithHandle || closableLater,
        )
    }

    // ---------------------------------------------------------------- W6

    private class W6Volume : LuksVolume(0L) {
        override val info = VolumeInfo("Test", "uuid-0000", 4096, 1024L, "ext4", emptyList())
        var beginCalls = 0
        val closeCalls = AtomicInteger(0)

        inner class Writer : FileWriter(0L) {
            override fun close() {
                closeCalls.incrementAndGet()
                super.close()
            }
        }

        override fun beginFile(sizeBytes: Long): FileWriter {
            beginCalls++
            return Writer()
        }
    }

    private class SizedResolver(ctx: Context?) : ContentResolver(ctx) {
        override fun query(uri: Uri, projection: Array<String>?, selection: String?, selectionArgs: Array<String>?, sortOrder: String?): Cursor {
            return MatrixCursor(arrayOf("_display_name", "_size")).also { it.addRow(arrayOf<Any>("big.bin", 4096L)) }
        }
        override fun openFileDescriptor(uri: Uri, mode: String): ParcelFileDescriptor? = null
    }

    private fun fakeContext(): Context = object : ContextWrapper(null) {
        private val resolver = SizedResolver(null)
        override fun getContentResolver(): ContentResolver = resolver
    }

    /** Pins direct-buffer memory until fewer than CHUNK_SIZE bytes remain. */
    private fun hogDirectMemory(sink: MutableList<ByteBuffer>) {
        for (size in intArrayOf(1 shl 20, 1 shl 16, 1 shl 12)) {
            try {
                while (true) sink += ByteBuffer.allocateDirect(size)
            } catch (_: OutOfMemoryError) {
            }
        }
    }

    private fun freeDirect(bufs: List<ByteBuffer>) {
        val uf = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe").also { it.isAccessible = true }.get(null)
        val m = uf.javaClass.getMethod("invokeCleaner", ByteBuffer::class.java)
        bufs.forEach { m.invoke(uf, it) }
    }

    /**
     * W6 (register W6, L5-O4): importFileWithProgress calls volume.beginFile and then
     * ByteBuffer.allocateDirect(CHUNK_SIZE), both outside the try whose finally closes
     * the writer. If the allocation throws, the writer is never closed.
     *
     * Injection without product changes: exhaust the JVM's direct-buffer budget so
     * the product's own allocateDirect(1 MiB) throws OutOfMemoryError.
     */
    @Test
    fun w6_allocationFailureAfterBeginFileStillClosesWriter() = runBlocking {
        val vol = W6Volume()
        val hog = ArrayList<ByteBuffer>()
        try {
            hogDirectMemory(hog)
            // Precondition: the product's exact allocation really fails now.
            var oom = false
            try {
                ByteBuffer.allocateDirect(TransferController.CHUNK_SIZE)
            } catch (_: OutOfMemoryError) {
                oom = true
            }
            assertTrue("could not exhaust direct memory; test would be vacuous", oom)

            val id = TransferManager.importFileWithProgress(fakeContext(), vol, "/", Uri.parse("content://t/1"))

            assertEquals("beginFile must have run (the OOM must be the allocation after it)", 1, vol.beginCalls)
            assertEquals(TransferState.FAILED, TransferManager.getTransfer(id)?.state)
            assertEquals("writer opened by beginFile was never closed", 1, vol.closeCalls.get())
        } finally {
            freeDirect(hog)
        }
    }

    // ---------------------------------------------------------------- C1

    private class GatedHashVolume(private val gatedPath: String) : LuksVolume(0L) {
        override val info = VolumeInfo("Test", "uuid-0000", 4096, 1024L, "ext4", emptyList())
        val gate = CountDownLatch(1)
        val entered = CountDownLatch(1)
        val concurrent = AtomicInteger(0)
        val maxConcurrent = AtomicInteger(0)

        override fun fileSize(path: String): Long = 100L

        override fun sha256(path: String, chunkBytes: Int): Digest {
            val c = concurrent.incrementAndGet()
            maxConcurrent.updateAndGet { maxOf(it, c) }
            try {
                if (path == gatedPath) {
                    entered.countDown()
                    gate.await(10, TimeUnit.SECONDS) // blocking native-style call: not interruptible by cancel
                }
                return Digest("h", 100L, 1L, 1L)
            } finally {
                concurrent.decrementAndGet()
            }
        }
    }

    /**
     * C1 (L5-O6, L6-O6): resetTransferLocks force-unlocks transferMutex although job A
     * is still inside withLock (cancel is cooperative; A is in a blocking call). Queued
     * job B then acquires the mutex and enters the critical section concurrently.
     * Correct behaviour: mutual exclusion holds, max concurrency stays 1.
     */
    @Test
    fun c1_resetTransferLocksDoesNotLetSecondJobOverlapFirst() = runBlocking {
        val vol = GatedHashVolume("/a.bin")
        unlocked(vol)
        try {
            val a = TransferManager.startHash("/a.bin")
            assertTrue("A must be inside the critical section", vol.entered.await(5, TimeUnit.SECONDS))
            TransferManager.resetTransferLocks() // session teardown: cancels A, force-unlocks while A is still in sha256

            // A new job after the reset (reset also cancels everything already queued).
            val b = TransferManager.startHash("/b.bin")

            // Give B ample time to (wrongly) enter while A is still blocked.
            withTimeoutOrNull(1_000) {
                while (vol.maxConcurrent.get() < 2) delay(10)
            }
            assertEquals("two jobs inside transferMutex.withLock at once", 1, vol.maxConcurrent.get())
            assertNotNull(TransferManager.getTransfer(a))
        } finally {
            vol.gate.countDown()
        }
    }

    // ---------------------------------------------------------------- C5

    /**
     * C5 (L5-O9), part 1: a cancel that is recorded (item shows CANCELLED) while the
     * worker is mid-hash must stay the terminal state. hashFileWithProgress writes
     * state = COMPLETED unguarded when sha256 returns, so CANCELLED flips to COMPLETED:
     * one transfer, two terminal states.
     */
    @Test
    fun c5_cancelRecordedMidFlightIsNotOverwrittenByCompleted() = runBlocking {
        val vol = GatedHashVolume("/a.bin")
        unlocked(vol)
        try {
            val done = CompletableDeferred<Unit>()
            val id = TransferManager.startHash("/a.bin") { done.complete(Unit) }
            assertTrue(vol.entered.await(5, TimeUnit.SECONDS))

            TransferManager.cancelTransfer(id)
            assertEquals(TransferState.CANCELLED, TransferManager.getTransfer(id)?.state)

            vol.gate.countDown() // native hash "finishes" after the cancel was recorded
            withTimeout(5_000) { done.await() }

            assertEquals("terminal state changed after CANCELLED was recorded", TransferState.CANCELLED, TransferManager.getTransfer(id)?.state)
        } finally {
            vol.gate.countDown()
        }
    }

    /**
     * C5, part 2 (the other direction): a late cancel after COMPLETED must not
     * overwrite it. cancelTransfer already guards on RUNNING/QUEUED, so this is
     * expected GREEN today: it challenges the "late cancel" half of L5-O9. Not ignored.
     */
    @Test
    fun c5_lateCancelAfterCompletedDoesNotOverwrite() = runBlocking {
        val vol = GatedHashVolume("/none")
        unlocked(vol)
        val done = CompletableDeferred<Unit>()
        val id = TransferManager.startHash("/a.bin") { done.complete(Unit) }
        withTimeout(5_000) { done.await() }
        assertEquals(TransferState.COMPLETED, TransferManager.getTransfer(id)?.state)

        TransferManager.cancelTransfer(id)

        assertEquals(TransferState.COMPLETED, TransferManager.getTransfer(id)?.state)
    }
}
