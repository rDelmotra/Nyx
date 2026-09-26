package dev.luksandroid.streaming

import dev.luksandroid.FileInfo
import dev.luksandroid.LuksVolume
import dev.luksandroid.VolumeInfo
import dev.luksandroid.session.LuksSession
import dev.luksandroid.session.SessionController
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Test
import java.io.ByteArrayOutputStream
import java.io.InputStream
import java.net.HttpURLConnection
import java.net.URL

class LocalHttpStreamServerTest {

    private lateinit var testScope: CoroutineScope
    private lateinit var session: SessionController
    private lateinit var testVolume: TestLuksVolume

    class TestLuksVolume(
        override val info: VolumeInfo = VolumeInfo(
            label = "StreamTestVol",
            uuid = "stream-uuid-1",
            blockSize = 4096,
            sizeBytes = 10L * 1024 * 1024,
            fsType = "ext4",
            subvolumes = emptyList(),
        ),
        val fileDataMap: MutableMap<String, ByteArray> = mutableMapOf(),
    ) : LuksVolume(0L) {
        override fun fileInfo(path: String): FileInfo {
            val data = fileDataMap[path] ?: throw IllegalArgumentException("Not found: $path")
            return FileInfo(
                path = path,
                size = data.size.toLong(),
                mode = 0,
                uid = 0,
                gid = 0,
                links = 1,
                type = "file",
                atime = 1000L,
                mtime = 2000L,
                ctime = 3000L,
            )
        }

        override fun readChunk(path: String, offset: Long, len: Int): ByteArray {
            val data = fileDataMap[path] ?: return ByteArray(0)
            if (offset >= data.size) return ByteArray(0)
            val end = minOf(data.size.toLong(), offset + len).toInt()
            return data.copyOfRange(offset.toInt(), end)
        }
    }

    @Before
    fun setUp() {
        runBlocking {
            testScope = CoroutineScope(Dispatchers.Unconfined + SupervisorJob())
            session = SessionController(scope = testScope)
            testVolume = TestLuksVolume()
            session.startUnlockedForTest(volume = testVolume)
            LocalHttpStreamServer.session = session
        }
    }

    @After
    fun tearDown() {
        LocalHttpStreamServer.stop()
        LocalHttpStreamServer.session = LuksSession
        runBlocking {
            session.lock()
            testScope.cancel()
        }
    }

    private fun readAll(input: InputStream): ByteArray {
        val out = ByteArrayOutputStream()
        val buf = ByteArray(4096)
        var n: Int
        while (input.read(buf).also { n = it } != -1) {
            out.write(buf, 0, n)
        }
        return out.toByteArray()
    }

    @Test
    fun `server serves full file with 200 OK when no range requested`() {
        val testData = ByteArray(10 * 1024) { (it % 100).toByte() }
        testVolume.fileDataMap["/stream_test.mp4"] = testData

        val streamUrl = LocalHttpStreamServer.registerStream(
            path = "/stream_test.mp4",
            mimeType = "video/mp4",
            totalSize = testData.size.toLong(),
        )

        assertTrue(LocalHttpStreamServer.isRunning)
        assertTrue(LocalHttpStreamServer.port > 0)

        val url = URL(streamUrl)
        val conn = url.openConnection() as HttpURLConnection
        conn.requestMethod = "GET"
        conn.connectTimeout = 5000
        conn.readTimeout = 5000

        assertEquals(200, conn.responseCode)
        assertEquals("video/mp4", conn.contentType)
        assertEquals(testData.size.toLong(), conn.contentLengthLong)

        val received = readAll(conn.inputStream)
        assertArrayEquals(testData, received)
        conn.disconnect()
    }

    @Test
    fun `server serves partial content 206 for byte range requests`() {
        val testData = ByteArray(10 * 1024) { (it % 250).toByte() }
        testVolume.fileDataMap["/range_test.mkv"] = testData

        val streamUrl = LocalHttpStreamServer.registerStream(
            path = "/range_test.mkv",
            mimeType = "video/x-matroska",
            totalSize = testData.size.toLong(),
        )

        val url = URL(streamUrl)
        val conn = url.openConnection() as HttpURLConnection
        conn.setRequestProperty("Range", "bytes=100-199")
        conn.connectTimeout = 5000
        conn.readTimeout = 5000

        assertEquals(206, conn.responseCode)
        assertEquals("video/x-matroska", conn.contentType)
        assertEquals("bytes 100-199/${testData.size}", conn.getHeaderField("Content-Range"))
        assertEquals(100L, conn.contentLengthLong)

        val received = readAll(conn.inputStream)
        assertEquals(100, received.size)
        val expectedSlice = testData.copyOfRange(100, 200)
        assertArrayEquals(expectedSlice, received)
        conn.disconnect()
    }

    @Test
    fun `unregistered token returns 404 Not Found`() {
        LocalHttpStreamServer.start()
        val port = LocalHttpStreamServer.port

        val url = URL("http://127.0.0.1:$port/media/fake-token-1234/test.mp4")
        val conn = url.openConnection() as HttpURLConnection
        conn.connectTimeout = 5000
        conn.readTimeout = 5000

        assertEquals(404, conn.responseCode)
        conn.disconnect()
    }

    @Test
    fun `server stop cleanly closes sockets`() {
        LocalHttpStreamServer.start()
        assertTrue(LocalHttpStreamServer.isRunning)

        LocalHttpStreamServer.stop()
        assertFalse(LocalHttpStreamServer.isRunning)
    }
}
