package dev.luksandroid.session

import android.content.Context
import android.content.ContextWrapper
import android.net.Uri
import dev.luksandroid.LuksException
import dev.luksandroid.LuksVolume
import dev.luksandroid.StatFsInfo
import dev.luksandroid.VolumeInfo
import dev.luksandroid.transfer.ExportTarget
import dev.luksandroid.transfer.ImportSource
import dev.luksandroid.transfer.ImportSourceInfo
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.ByteArrayInputStream
import java.io.ByteArrayOutputStream
import java.io.InputStream
import java.io.OutputStream
import java.nio.ByteBuffer
import java.nio.channels.Channels
import java.nio.channels.ReadableByteChannel
import java.util.concurrent.atomic.AtomicInteger

/**
 * F5 E1: export policy (R5r, P4: pending name, delete on failure/cancel/FILE_CHANGED, size+sha256
 * verification, total from the handle) and import policy (V2 size, V3 free space, W5/W6 single-owner
 * writer cleanup), against fakes of the two seams ([ExportTarget], [ImportSource]).
 */
class TransferIoIntegrityTest {

    private val ctx: Context = object : ContextWrapper(null) {}
    private val uri: Uri? = Uri.parse("content://t/x") // null under the JVM stubs; the fakes ignore it

    private val originalDrain = TransferManager.nativeDrain

    @org.junit.Before
    fun setUpDrain() {
        TransferManager.nativeDrain = dev.luksandroid.log.NativeDrain(object : dev.luksandroid.log.NativeEventSource {
            override fun drain() = """{"dropped":0,"events":[]}"""
            override fun ringJson() = """{"dropped":0,"events":[]}"""
        }, executor = { it.run() })
    }

    @After
    fun clear() {
        TransferManager.nativeDrain = originalDrain
        val field = TransferController::class.java.getDeclaredField("_transfers")
        field.isAccessible = true
        @Suppress("UNCHECKED_CAST")
        (field.get(TransferManager) as kotlinx.coroutines.flow.MutableStateFlow<List<TransferItem>>).value = emptyList()
    }

    // ------------------------------------------------------------------ export fakes

    private open class SrcVolume(private val data: ByteArray) : LuksVolume(0L) {
        override val info = VolumeInfo("Test", "uuid-0000", 4096, 1024L, "ext4", emptyList())
        var changeAtRead = 0
        var reads = 0
        var closed = 0
        var onRead: (Int) -> Unit = {}

        override fun fileSize(path: String): Long = throw IllegalStateException("total must come from the handle, not the path")
        override fun openFile(path: String): Long = 42L
        override fun fileSizeOf(file: Long): Long = data.size.toLong()
        override fun readAt(file: Long, offset: Long, len: Int): ByteArray {
            reads++
            onRead(reads)
            if (changeAtRead != 0 && reads == changeAtRead) throw LuksException("changed", LuksException.FILE_CHANGED)
            val end = minOf(data.size.toLong(), offset + len).toInt()
            return data.copyOfRange(offset.toInt(), end)
        }
        override fun closeFile(file: Long) { closed++ }
    }

    private class Target(
        var available: Long? = null,
        var failCommit: Boolean = false,
        var failWriteAfter: Int = Int.MAX_VALUE,
    ) : ExportTarget {
        val out = ByteArrayOutputStream()
        var opened = 0
        var committed = 0
        var discarded = 0
        var outClosed = false
        /** Override what a read-back returns. Null = what was written. */
        var readBack: (() -> ByteArray)? = null
        var readable = true

        override var verifyLevel: dev.luksandroid.transfer.VerifyLevel = dev.luksandroid.transfer.VerifyLevel.FULL
        var reportedSize: Long? = null
        var reads = 0
        var onOpenRead: () -> Unit = {}

        override fun sizeBytes(): Long? = reportedSize
        override fun availableBytes(): Long? = available
        override fun openOutput(): OutputStream {
            opened++
            return object : OutputStream() {
                override fun write(b: Int) = write(byteArrayOf(b.toByte()), 0, 1)
                override fun write(b: ByteArray, off: Int, len: Int) {
                    if (out.size() + len > failWriteAfter) throw java.io.IOException("disk full")
                    out.write(b, off, len)
                }
                override fun close() { outClosed = true }
            }
        }
        override fun openRead(): InputStream? {
            reads++
            onOpenRead()
            return if (!readable) null else ByteArrayInputStream((readBack?.invoke()) ?: out.toByteArray())
        }
        override fun commit() {
            if (failCommit) throw java.io.IOException("rename refused")
            committed++
        }
        override fun discard() { discarded++ }
    }

    private fun payload(n: Int) = ByteArray(n) { (it * 31 % 251).toByte() }

    private fun export(vol: LuksVolume, target: Target): Long = runBlocking {
        TransferManager.exportFileWithProgress(ctx, vol, "/f.bin", uri!!, target = target)
    }

    @Test
    fun `export success verifies, commits and never discards`() {
        val data = payload(TransferController.CHUNK_SIZE + 5)
        val vol = SrcVolume(data)
        val t = Target()
        val id = export(vol, t)
        val item = TransferManager.getTransfer(id)!!
        assertEquals(TransferState.COMPLETED, item.state)
        assertEquals("total comes from fileSizeOf(handle)", data.size.toLong(), item.totalBytes)
        assertArrayEquals(data, t.out.toByteArray())
        assertEquals(1, t.committed)
        assertEquals(0, t.discarded)
        assertEquals(1, vol.closed)
    }

    @Test
    fun `export FILE_CHANGED deletes the partial destination`() {
        val vol = SrcVolume(payload(TransferController.CHUNK_SIZE * 3)).also { it.changeAtRead = 2 }
        val t = Target()
        val id = export(vol, t)
        val item = TransferManager.getTransfer(id)!!
        assertEquals(TransferState.FAILED, item.state)
        assertEquals(LuksException.FILE_CHANGED, item.errorCode)
        assertEquals(1, t.discarded)
        assertEquals(0, t.committed)
        assertTrue("stream closed before the delete", t.outClosed)
    }

    @Test
    fun `export destination write failure deletes the partial destination`() {
        val vol = SrcVolume(payload(TransferController.CHUNK_SIZE * 2))
        val t = Target(failWriteAfter = TransferController.CHUNK_SIZE)
        val id = export(vol, t)
        assertEquals(TransferState.FAILED, TransferManager.getTransfer(id)?.state)
        assertEquals(1, t.discarded)
        assertEquals(0, t.committed)
    }

    @Test
    fun `export cancelled mid-flight deletes the partial destination`() {
        val vol = SrcVolume(payload(TransferController.CHUNK_SIZE * 3))
        val t = Target()
        var id = 0L
        vol.onRead = { n -> if (n == 2) TransferManager.cancelTransfer(id) }
        // the id is only known after the call starts, so pre-seed it the way the queue does
        id = TransferManager.enqueueForTest("f.bin", TransferType.EXPORT)
        // Cancelled from inside the call it is running in, so the call itself ends cancelled too.
        try {
            runBlocking { TransferManager.exportFileWithProgress(ctx, vol, "/f.bin", uri!!, id, t) }
        } catch (_: kotlinx.coroutines.CancellationException) {
        }
        assertEquals(TransferState.CANCELLED, TransferManager.getTransfer(id)?.state)
        assertEquals(1, t.discarded)
        assertEquals(0, t.committed)
    }

    @Test
    fun `export whose read-back differs is failed and deleted`() {
        val data = payload(1000)
        val t = Target().also { it.readBack = { data.copyOf().also { b -> b[500] = (b[500] + 1).toByte() } } }
        val id = export(SrcVolume(data), t)
        assertEquals(TransferState.FAILED, TransferManager.getTransfer(id)?.state)
        assertEquals(1, t.discarded)
        assertEquals(0, t.committed)
    }

    @Test
    fun `export whose read-back is short is failed and deleted`() {
        val data = payload(1000)
        val t = Target().also { it.readBack = { data.copyOf(999) } }
        val id = export(SrcVolume(data), t)
        assertEquals(TransferState.FAILED, TransferManager.getTransfer(id)?.state)
        assertEquals(1, t.discarded)
    }

    @Test
    fun `export that cannot be read back cannot be verified, so it fails and is deleted`() {
        val t = Target().also { it.readable = false }
        val id = export(SrcVolume(payload(10)), t)
        assertEquals(TransferState.FAILED, TransferManager.getTransfer(id)?.state)
        assertEquals(1, t.discarded)
    }

    @Test
    fun `export commit refusal fails and deletes`() {
        val t = Target(failCommit = true)
        val id = export(SrcVolume(payload(10)), t)
        assertEquals(TransferState.FAILED, TransferManager.getTransfer(id)?.state)
        assertEquals(1, t.discarded)
    }

    @Test
    fun `export prechecks free space before writing a byte`() {
        val t = Target(available = 100L)
        val vol = SrcVolume(payload(1000))
        val id = export(vol, t)
        val item = TransferManager.getTransfer(id)!!
        assertEquals(TransferState.FAILED, item.state)
        assertEquals(LuksException.NO_SPACE, item.errorCode)
        assertEquals(0, t.opened)
        assertEquals(0, vol.reads)
        assertEquals("the empty document the picker created is removed", 1, t.discarded)
    }

    @Test
    fun `export with unknown free space proceeds`() {
        val t = Target(available = null)
        val id = export(SrcVolume(payload(10)), t)
        assertEquals(TransferState.COMPLETED, TransferManager.getTransfer(id)?.state)
    }

    @Test
    fun `pipelined export multi-chunk succeeds with verification`() {
        val data = payload(TransferController.CHUNK_SIZE * 2 + 500)
        val vol = SrcVolume(data)
        val t = Target()
        val id = export(vol, t)
        assertEquals(TransferState.COMPLETED, TransferManager.getTransfer(id)?.state)
        assertArrayEquals(data, t.out.toByteArray())
        assertEquals(1, t.committed)
        assertEquals(0, t.discarded)
    }

    @Test
    fun `pipelined export early EOF fails transfer and discards partial output`() {
        val vol = object : SrcVolume(payload(TransferController.CHUNK_SIZE * 3)) {
            override fun readAt(file: Long, offset: Long, len: Int): ByteArray {
                if (offset >= TransferController.CHUNK_SIZE) return ByteArray(0)
                return super.readAt(file, offset, len)
            }
        }
        val t = Target()
        val id = export(vol, t)
        assertEquals(TransferState.FAILED, TransferManager.getTransfer(id)?.state)
        assertEquals(1, t.discarded)
        assertEquals(0, t.committed)
    }

    @Test
    fun `R4 a NONE provider does not re-read or query size and completes fast`() {
        val data = payload(1000)
        val t = Target().also { it.verifyLevel = dev.luksandroid.transfer.VerifyLevel.NONE; it.reportedSize = null }
        val id = export(SrcVolume(data), t)
        val item = TransferManager.getTransfer(id)!!
        assertEquals(TransferState.COMPLETED, item.state)
        assertEquals(dev.luksandroid.transfer.VerifyLevel.NONE, item.verifyLevel)
        assertEquals("no re-read for a NONE provider", 0, t.reads)
    }

    @Test
    fun `R4 a non-local provider is verified by size only and the level is recorded`() {
        val data = payload(1000)
        val t = Target().also { it.verifyLevel = dev.luksandroid.transfer.VerifyLevel.SIZE; it.reportedSize = 1000L }
        val id = export(SrcVolume(data), t)
        val item = TransferManager.getTransfer(id)!!
        assertEquals(TransferState.COMPLETED, item.state)
        assertEquals(dev.luksandroid.transfer.VerifyLevel.SIZE, item.verifyLevel)
        assertEquals("no re-read for a non-local provider", 0, t.reads)
    }

    @Test
    fun `R4 a size-only provider that reports the wrong size fails and is deleted`() {
        val t = Target().also { it.verifyLevel = dev.luksandroid.transfer.VerifyLevel.SIZE; it.reportedSize = 999L }
        val id = export(SrcVolume(payload(1000)), t)
        assertEquals(TransferState.FAILED, TransferManager.getTransfer(id)?.state)
        assertEquals(1, t.discarded)
    }

    @Test
    fun `R4 a local provider is fully verified, recorded, and shows a Verifying phase`() {
        val t = Target()
        var verifyingSeen = false
        var idRef = 0L
        t.onOpenRead = { verifyingSeen = TransferManager.getTransfer(idRef)?.verifying == true }
        idRef = TransferManager.enqueueForTest("f.bin", TransferType.EXPORT)
        runBlocking { TransferManager.exportFileWithProgress(ctx, SrcVolume(payload(1000)), "/f.bin", uri!!, idRef, t) }
        val item = TransferManager.getTransfer(idRef)!!
        assertEquals(TransferState.COMPLETED, item.state)
        assertEquals(dev.luksandroid.transfer.VerifyLevel.FULL, item.verifyLevel)
        assertTrue(verifyingSeen)
        assertFalse("phase ends with the transfer", item.verifying)
    }

    @Test
    fun `R4 cancelling during verification stops it and deletes the destination`() {
        val data = payload(TransferController.CHUNK_SIZE * 3)
        val t = Target()
        val idRef = TransferManager.enqueueForTest("f.bin", TransferType.EXPORT)
        t.readBack = {
            // a stream that cancels the transfer as soon as the verifier starts reading it
            TransferManager.cancelTransfer(idRef)
            data
        }
        try {
            runBlocking { TransferManager.exportFileWithProgress(ctx, SrcVolume(data), "/f.bin", uri!!, idRef, t) }
        } catch (_: kotlinx.coroutines.CancellationException) {
        }
        assertEquals(TransferState.CANCELLED, TransferManager.getTransfer(idRef)?.state)
        assertEquals(1, t.discarded)
        assertEquals(0, t.committed)
    }

    // ------------------------------------------------------------------ import fakes

    private class Src(
        val name: String = "in.bin",
        val declared: Long,
        val bytes: ByteArray,
    ) : ImportSource {
        var opened = 0
        var channelClosed = false
        override fun describe() = ImportSourceInfo(name, declared)
        override fun open(): ReadableByteChannel {
            opened++
            val inner = Channels.newChannel(ByteArrayInputStream(bytes))
            return object : ReadableByteChannel {
                override fun read(dst: ByteBuffer) = inner.read(dst)
                override fun isOpen() = inner.isOpen
                override fun close() { channelClosed = true; inner.close() }
            }
        }
    }

    private open class DstVolume(
        private val available: Long = Long.MAX_VALUE,
        private val statFsThrows: Boolean = false,
    ) : LuksVolume(0L) {
        override val info = VolumeInfo("Test", "uuid-0000", 4096, 1 shl 30, "ext4", emptyList())
        val written = ByteArrayOutputStream()
        var begun = 0
        var finished = 0
        val closes = AtomicInteger(0)
        val commits = AtomicInteger(0)
        var failFinish = false
        var tokenSeenByWrite = -1L

        override fun statFs(): StatFsInfo {
            if (statFsThrows) throw IllegalStateException("volume is closed")
            return StatFsInfo(1 shl 30, available, available, 100, 100, 4096)
        }
        override fun commitActiveBatch() { commits.incrementAndGet() }

        override fun beginFile(sizeBytes: Long): FileWriter { begun++; return W() }

        inner class W : FileWriter(0L) {
            override fun write(data: ByteBuffer, len: Int) {
                tokenSeenByWrite = dev.luksandroid.currentNativeCancelToken()
                val tmp = ByteArray(len)
                data.get(tmp)
                written.write(tmp)
            }
            override fun finish(parentPath: String, name: String): Long {
                if (failFinish) throw LuksException("finish failed", LuksException.IO)
                finished++
                return 9L
            }
            override fun close() { closes.incrementAndGet(); super.close() }
        }
    }

    private fun import(vol: LuksVolume, src: Src): Long = runBlocking {
        TransferManager.importFileWithProgress(ctx, vol, "/", uri!!, source = src)
    }

    @Test
    fun `import success finishes once, closes the writer once and commits`() {
        val data = payload(TransferController.CHUNK_SIZE + 17)
        val vol = DstVolume()
        val id = import(vol, Src(declared = data.size.toLong(), bytes = data))
        assertEquals(TransferState.COMPLETED, TransferManager.getTransfer(id)?.state)
        assertArrayEquals(data, vol.written.toByteArray())
        assertEquals(1, vol.finished)
        assertEquals(1, vol.closes.get())
        assertEquals(1, vol.commits.get())
    }

    @Test
    fun `V2 import of a source longer than it declared fails clean and publishes nothing`() {
        val data = payload(5000)
        val vol = DstVolume()
        val src = Src(declared = 4000, bytes = data)
        val id = import(vol, src)
        val item = TransferManager.getTransfer(id)!!
        assertEquals(TransferState.FAILED, item.state)
        assertEquals("nothing published", 0, vol.finished)
        assertEquals("writer closed exactly once", 1, vol.closes.get())
        assertTrue(src.channelClosed)
    }

    @Test
    fun `V2 import of a source shorter than it declared fails clean and publishes nothing`() {
        val vol = DstVolume()
        val id = import(vol, Src(declared = 5000, bytes = payload(4000)))
        assertEquals(TransferState.FAILED, TransferManager.getTransfer(id)?.state)
        assertEquals(0, vol.finished)
        assertEquals(1, vol.closes.get())
    }

    @Test
    fun `V3 import prechecks free space with the volume's statfs before beginFile`() {
        val vol = DstVolume(available = 1000L)
        val id = import(vol, Src(declared = 5000, bytes = payload(5000)))
        val item = TransferManager.getTransfer(id)!!
        assertEquals(TransferState.FAILED, item.state)
        assertEquals(LuksException.NO_SPACE, item.errorCode)
        assertEquals(0, vol.begun)
    }

    @Test
    fun `V3 an unreadable statfs is unknown, not full`() {
        val vol = DstVolume(statFsThrows = true)
        val id = import(vol, Src(declared = 10, bytes = payload(10)))
        assertEquals(TransferState.COMPLETED, TransferManager.getTransfer(id)?.state)
    }

    @Test
    fun `W5 a failing finish still ends with exactly one writer close and the volume not committed`() {
        val vol = DstVolume().also { it.failFinish = true }
        val id = import(vol, Src(declared = 10, bytes = payload(10)))
        assertEquals(TransferState.FAILED, TransferManager.getTransfer(id)?.state)
        assertEquals(1, vol.closes.get())
    }

    @Test
    fun `import with no declared size is refused before anything is begun`() {
        val vol = DstVolume()
        val id = import(vol, Src(declared = -1, bytes = payload(10)))
        assertEquals(TransferState.FAILED, TransferManager.getTransfer(id)?.state)
        assertEquals(0, vol.begun)
    }

    @Test
    fun `import cancelled mid-flight closes the writer and publishes nothing`() {
        val data = payload(TransferController.CHUNK_SIZE * 3)
        val vol = DstVolume()
        val id = TransferManager.enqueueForTest("in.bin", TransferType.IMPORT)
        val src = object : ImportSource {
            override fun describe() = ImportSourceInfo("in.bin", data.size.toLong())
            override fun open(): ReadableByteChannel {
                val inner = Channels.newChannel(ByteArrayInputStream(data))
                var reads = 0
                return object : ReadableByteChannel {
                    override fun read(dst: ByteBuffer): Int {
                        if (++reads == 3) TransferManager.cancelTransfer(id)
                        return inner.read(dst)
                    }
                    override fun isOpen() = inner.isOpen
                    override fun close() = inner.close()
                }
            }
        }
        try {
            runBlocking { TransferManager.importFileWithProgress(ctx, vol, "/", uri!!, id, src) }
        } catch (_: kotlinx.coroutines.CancellationException) {
        }
        assertEquals(TransferState.CANCELLED, TransferManager.getTransfer(id)?.state)
        assertEquals(0, vol.finished)
        assertEquals(1, vol.closes.get())
    }

    @Test
    fun `L3 a cancel that lands after the file was published still ends COMPLETED`() {
        val id = TransferManager.enqueueForTest("in.bin", TransferType.IMPORT)
        val vol = object : DstVolume() {
            override fun beginFile(sizeBytes: Long): FileWriter = object : FileWriter(0L) {
                override fun write(data: ByteBuffer, len: Int) { data.position(data.limit()) }
                override fun finish(parentPath: String, name: String): Long {
                    TransferManager.cancelTransfer(id) // user pressed cancel while finish was in flight
                    return 9L
                }
            }
        }
        try {
            runBlocking { TransferManager.importFileWithProgress(ctx, vol, "/", uri!!, id, Src(declared = 10, bytes = payload(10))) }
        } catch (_: kotlinx.coroutines.CancellationException) {
        }
        assertEquals("the file exists, so the item must not say cancelled", TransferState.COMPLETED, TransferManager.getTransfer(id)?.state)
    }

    @Test
    fun `C3 import writes run with the transfer's cancel token`() {
        val c = TransferController()
        c.cancelTokens = object : CancelTokens {
            override fun create() = 55L
            override fun signal(token: Long) {}
            override fun close(token: Long) {}
        }
        val vol = DstVolume()
        runBlocking { c.importFileWithProgress(ctx, vol, "/", uri!!, source = Src(declared = 10, bytes = payload(10))) }
        assertEquals(55L, vol.tokenSeenByWrite)
    }

    @Test
    fun `export reads via direct buffer pipeline and produces identical output`() {
        val data = payload(TransferController.CHUNK_SIZE * 2 + 1024)
        val vol = SrcVolume(data)
        val t = Target()
        val id = export(vol, t)
        val item = TransferManager.getTransfer(id)!!
        assertEquals(TransferState.COMPLETED, item.state)
        assertArrayEquals(data, t.out.toByteArray())
    }
}
