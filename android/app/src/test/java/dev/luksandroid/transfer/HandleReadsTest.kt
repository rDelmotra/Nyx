package dev.luksandroid.transfer

import dev.luksandroid.LuksException
import dev.luksandroid.LuksVolume
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

/**
 * F4 B5: the export source reads through an open handle (resolved once, read by identity),
 * never by path per chunk.
 */
class HandleReadsTest {

    /** A volume that only serves handles: any path-based read API would be a compile error. */
    private class HandleVolume(val files: MutableMap<String, ByteArray>) : LuksVolume(0L) {
        val opened = mutableListOf<String>()
        val reads = mutableListOf<Triple<Long, Long, Int>>()
        var closed = 0
        var failReadsWith: LuksException? = null
        private val byId = mutableMapOf<Long, String>()
        private var next = 1L

        override fun openFile(path: String): Long {
            if (path !in files) throw LuksException("not found", LuksException.NOT_FOUND)
            opened += path
            return next++.also { byId[it] = path }
        }

        override fun readAt(file: Long, offset: Long, len: Int): ByteArray {
            failReadsWith?.let { throw it }
            reads += Triple(file, offset, len)
            val data = files.getValue(byId.getValue(file))
            if (offset >= data.size) return ByteArray(0)
            return data.copyOfRange(offset.toInt(), minOf(data.size.toLong(), offset + len).toInt())
        }

        override fun closeFile(file: Long) {
            closed++
            byId.remove(file)
        }

        val openCount get() = byId.size
    }

    @Test
    fun `export stream opens one handle, reads every chunk through it, and closes it`() {
        val data = ByteArray(10_000) { (it * 7 % 251).toByte() }
        val vol = HandleVolume(mutableMapOf("/video.bin" to data))

        VolumeSourceBytes(vol, chunkBytes = 4096).open("/video.bin").use { stream ->
            assertArrayEquals(data, stream.readBytes())
        }

        assertEquals("the path is resolved once", listOf("/video.bin"), vol.opened)
        assertTrue("a 10000-byte file in 4096-byte chunks needs several reads", vol.reads.size >= 3)
        assertEquals("every read names the same handle", 1, vol.reads.map { it.first }.toSet().size)
        assertEquals("offsets advance sequentially", listOf(0L, 4096L, 8192L), vol.reads.take(3).map { it.second })
        assertEquals("closing the stream closes the handle", 1, vol.closed)
        assertEquals(0, vol.openCount)
    }

    @Test
    fun `file changed mid-export fails the stream and still closes the handle`() {
        val vol = HandleVolume(mutableMapOf("/a" to ByteArray(10_000) { 1 }))
        val stream = VolumeSourceBytes(vol, chunkBytes = 4096).open("/a")
        val first = ByteArray(4096)
        assertEquals(4096, stream.read(first))

        vol.failReadsWith = LuksException("file changed", LuksException.FILE_CHANGED)
        val rest = ByteArray(4096)
        try {
            stream.read(rest, 0, rest.size)
            // The first buffered chunk is spent, so this read has to hit the volume.
            stream.read(rest, 0, rest.size)
            fail("a changed file must fail the stream, not return bytes")
        } catch (e: LuksException) {
            assertTrue(e.isFileChanged)
        } finally {
            stream.close()
        }
        assertTrue("no bytes may have arrived after the change", rest.all { it == 0.toByte() })
        assertEquals(1, vol.closed)
        assertEquals(0, vol.openCount)
    }

    @Test
    fun `an unreadable path fails at open of the first read without leaking a handle`() {
        val vol = HandleVolume(mutableMapOf())
        val stream = VolumeSourceBytes(vol).open("/missing")
        try {
            stream.read()
            fail("expected NOT_FOUND")
        } catch (e: LuksException) {
            assertEquals(LuksException.NOT_FOUND, e.code)
        }
        stream.close()
        assertEquals(0, vol.closed)
        assertEquals(0, vol.openCount)
    }

    @Test
    fun `direct unbuffered reads when len is at least chunkBytes`() {
        val data = ByteArray(8192) { (it % 127).toByte() }
        val vol = HandleVolume(mutableMapOf("/direct.bin" to data))
        val stream = VolumeSourceBytes(vol, chunkBytes = 4096).open("/direct.bin")

        val target = ByteArray(4096)
        val n1 = stream.read(target, 0, target.size)
        assertEquals(4096, n1)
        assertArrayEquals(data.copyOfRange(0, 4096), target)
        assertEquals(0, stream.available())

        val n2 = stream.read(target, 0, target.size)
        assertEquals(4096, n2)
        assertArrayEquals(data.copyOfRange(4096, 8192), target)
        assertEquals(0, stream.available())

        val n3 = stream.read(target, 0, target.size)
        assertEquals(-1, n3)
        stream.close()
    }

    @Test
    fun `small reads buffer a chunk and mixed reads drain buffer before direct reading`() {
        val data = ByteArray(5000) { (it % 127).toByte() }
        val vol = HandleVolume(mutableMapOf("/mixed.bin" to data))
        val stream = VolumeSourceBytes(vol, chunkBytes = 4096).open("/mixed.bin")

        // 1-byte read triggers fill()
        val b0 = stream.read()
        assertEquals(data[0].toInt() and 0xFF, b0)
        assertEquals(4095, stream.available())

        // Small block read drains partial buffer
        val small = ByteArray(100)
        val readSmall = stream.read(small, 0, 100)
        assertEquals(100, readSmall)
        assertArrayEquals(data.copyOfRange(1, 101), small)
        assertEquals(3995, stream.available())

        // Block read larger than remaining buffered bytes drains remainder
        val rem = ByteArray(4000)
        val readRem = stream.read(rem, 0, 4000)
        assertEquals(3995, readRem)
        assertArrayEquals(data.copyOfRange(101, 4096), rem.copyOfRange(0, 3995))
        assertEquals(0, stream.available())

        // Buffer is spent: next large read does direct read of remaining 904 bytes
        val next = ByteArray(4096)
        val readNext = stream.read(next, 0, 4096)
        assertEquals(904, readNext)
        assertArrayEquals(data.copyOfRange(4096, 5000), next.copyOfRange(0, 904))
        assertEquals(0, stream.available())

        // EOF
        assertEquals(-1, stream.read(next, 0, 4096))
        stream.close()
    }

    @Test
    fun `read bounds checking throws IndexOutOfBoundsException`() {
        val data = ByteArray(100) { 1 }
        val vol = HandleVolume(mutableMapOf("/bounds.bin" to data))
        val stream = VolumeSourceBytes(vol, chunkBytes = 4096).open("/bounds.bin")
        val buf = ByteArray(10)

        try {
            stream.read(buf, -1, 5)
            fail("expected IndexOutOfBoundsException")
        } catch (_: IndexOutOfBoundsException) {}

        try {
            stream.read(buf, 0, -1)
            fail("expected IndexOutOfBoundsException")
        } catch (_: IndexOutOfBoundsException) {}

        try {
            stream.read(buf, 5, 10)
            fail("expected IndexOutOfBoundsException")
        } catch (_: IndexOutOfBoundsException) {}

        assertEquals(0, stream.read(buf, 0, 0))
        stream.close()
    }
}
