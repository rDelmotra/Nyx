package dev.luksandroid.session

import android.content.ContentResolver
import android.content.Context
import android.content.ContextWrapper
import android.net.Uri
import dev.luksandroid.LuksException
import dev.luksandroid.LuksVolume
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.ByteArrayOutputStream
import java.io.OutputStream

/**
 * F4 B5: `TransferManager.exportFileWithProgress` resolves the source once (`openFile`),
 * reads every chunk by handle (`readAt`), and always closes the handle. A source that
 * changes mid-export fails the transfer with nothing foreign in the output.
 */
class ExportHandleTest {

    private class ExportVolume(private val data: ByteArray) : LuksVolume(0L) {
        val opened = mutableListOf<String>()
        val readHandles = mutableListOf<Long>()
        var closed = 0
        /** Fail the Nth `readAt` (1-based) with FILE_CHANGED. 0 = never. */
        var changeAtRead = 0
        private var live = 0

        override fun fileSize(path: String): Long = data.size.toLong()
        override fun fileSizeOf(file: Long): Long = data.size.toLong()

        override fun openFile(path: String): Long {
            opened += path
            live++
            return 42L
        }

        override fun readAt(file: Long, offset: Long, len: Int): ByteArray {
            readHandles += file
            if (changeAtRead != 0 && readHandles.size == changeAtRead) {
                throw LuksException("file changed", LuksException.FILE_CHANGED)
            }
            val end = minOf(data.size.toLong(), offset + len).toInt()
            return data.copyOfRange(offset.toInt(), end)
        }

        override fun closeFile(file: Long) {
            closed++
            live--
        }

        val liveHandles get() = live
    }

    private class Sink : ContentResolver(null) {
        val out = ByteArrayOutputStream()
        override fun openOutputStream(uri: Uri): OutputStream = out
        override fun openInputStream(uri: Uri): java.io.InputStream = java.io.ByteArrayInputStream(out.toByteArray())
    }

    /** Writes into [sink]; local-provider semantics (full read-back). */
    private fun targetOf(sink: Sink) = object : dev.luksandroid.transfer.ExportTarget {
        override fun availableBytes(): Long? = null
        override fun openOutput(): OutputStream = sink.out
        override fun openRead(): java.io.InputStream = java.io.ByteArrayInputStream(sink.out.toByteArray())
        override fun commit() {}
        override fun discard() {}
    }

    private fun ctx(sink: Sink): Context = object : ContextWrapper(null) {
        override fun getContentResolver(): ContentResolver = sink
    }

    @Test
    fun `export resolves the path once, reads by handle and closes it`() = runBlocking {
        val data = ByteArray(TransferController.CHUNK_SIZE * 2 + 123) { (it * 13 % 253).toByte() }
        val vol = ExportVolume(data)
        val sink = Sink()

        val id = TransferManager.exportFileWithProgress(ctx(sink), vol, "/big.bin", Uri.parse("content://t/out"), target = targetOf(sink))

        assertEquals(TransferState.COMPLETED, TransferManager.getTransfer(id)?.state)
        assertArrayEquals(data, sink.out.toByteArray())
        assertEquals(listOf("/big.bin"), vol.opened)
        assertEquals("three chunks, all through the one handle", 3, vol.readHandles.size)
        assertEquals(setOf(42L), vol.readHandles.toSet())
        assertEquals(1, vol.closed)
        assertEquals(0, vol.liveHandles)
    }

    @Test
    fun `a source that changes mid-export fails the transfer and closes the handle`() = runBlocking {
        val data = ByteArray(TransferController.CHUNK_SIZE * 3) { 5 }
        val vol = ExportVolume(data).also { it.changeAtRead = 2 }
        val sink = Sink()

        val id = TransferManager.exportFileWithProgress(ctx(sink), vol, "/big.bin", Uri.parse("content://t/out2"), target = targetOf(sink))

        assertEquals(TransferState.FAILED, TransferManager.getTransfer(id)?.state)
        assertEquals("only the chunk read before the change reached the output", TransferController.CHUNK_SIZE, sink.out.size())
        assertTrue(vol.closed == 1 && vol.liveHandles == 0)
    }
}
