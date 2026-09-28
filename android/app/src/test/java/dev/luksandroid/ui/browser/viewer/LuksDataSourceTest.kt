package dev.luksandroid.ui.browser.viewer

import androidx.media3.common.C
import androidx.media3.common.PlaybackException
import androidx.media3.datasource.DataSourceException
import androidx.media3.datasource.DataSpec
import androidx.media3.datasource.TransferListener
import dev.luksandroid.FileInfo
import dev.luksandroid.LuksVolume
import dev.luksandroid.VolumeInfo
import dev.luksandroid.session.LuksSession
import dev.luksandroid.session.SessionController
import dev.luksandroid.session.SessionState
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
import org.junit.Assert.fail
import org.junit.Before
import org.junit.Test

class LuksDataSourceTest {

    private lateinit var testScope: CoroutineScope
    private lateinit var session: SessionController
    private lateinit var testVolume: TestLuksVolume

    class TestLuksVolume(
        override val info: VolumeInfo = VolumeInfo(
            label = "TestVol",
            uuid = "test-uuid-1",
            blockSize = 4096,
            sizeBytes = 10L * 1024 * 1024,
            fsType = "ext4",
            subvolumes = emptyList(),
        ),
        val fileDataMap: MutableMap<String, ByteArray> = mutableMapOf(),
    ) : LuksVolume(0L) {
        var chunkReadCount = 0

        override fun fileInfo(path: String): FileInfo {
            val data = fileDataMap[path] ?: throw IllegalArgumentException("File not found: $path")
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

        val returnedChunks = mutableListOf<ByteArray>()

        override fun readChunk(path: String, offset: Long, len: Int): ByteArray {
            chunkReadCount++
            val data = fileDataMap[path] ?: return ByteArray(0)
            if (offset >= data.size) return ByteArray(0)
            val end = minOf(data.size.toLong(), offset + len).toInt()
            val chunk = data.copyOfRange(offset.toInt(), end)
            returnedChunks.add(chunk)
            return chunk
        }
    }

    @Before
    fun setUp() {
        runBlocking {
            testScope = CoroutineScope(Dispatchers.Unconfined + SupervisorJob())
            session = SessionController(scope = testScope)
            testVolume = TestLuksVolume()
            session.startUnlockedForTest(volume = testVolume)
        }
    }

    @After
    fun tearDown() {
        runBlocking {
            session.lock()
            testScope.cancel()
        }
    }

    @Test
    fun `sequential small reads are coalesced by block cache`() {
        // Create 100 KB test payload
        val testData = ByteArray(100 * 1024) { (it % 127).toByte() }
        testVolume.fileDataMap["/movie.mp4"] = testData

        val dataSource = LuksDataSource(
            session = session,
            blockSize = 32 * 1024, // 32 KiB blocks for testing
            maxBlocks = 4,
        )

        val uri = LuksDataSource.uriFor("/movie.mp4")
        val dataSpec = DataSpec(uri)
        val length = dataSource.open(dataSpec)
        assertEquals(testData.size.toLong(), length)

        // Read 1000 times in 8-byte chunks (simulating MP4 box header parsing)
        val buffer = ByteArray(8)
        var totalRead = 0
        for (i in 0 until 1000) {
            val read = dataSource.read(buffer, 0, 8)
            assertEquals(8, read)
            for (j in 0 until 8) {
                assertEquals(testData[totalRead + j], buffer[j])
            }
            totalRead += read
        }
        assertEquals(8000, totalRead)

        // 8000 bytes within a 32 KiB block should only trigger 1 chunk read on storage!
        assertEquals(1, testVolume.chunkReadCount)

        dataSource.close()
    }

    @Test
    fun `seeking and jump to EOF retains start block in LRU cache`() {
        // 256 KiB file: Block 0 is 0..64KB, Block 3 is 192KB..256KB
        val testData = ByteArray(256 * 1024) { (it % 251).toByte() }
        testVolume.fileDataMap["/video.mp4"] = testData

        val dataSource = LuksDataSource(
            session = session,
            blockSize = 64 * 1024, // 64 KiB blocks
            maxBlocks = 4,
        )

        // 1. Read start of file (Block 0)
        dataSource.open(DataSpec(LuksDataSource.uriFor("/video.mp4")))
        val startBuf = ByteArray(16)
        dataSource.read(startBuf, 0, 16)
        assertEquals(1, testVolume.chunkReadCount)

        // 2. Jump to EOF to read metadata box (Block 3: offset 200,000)
        dataSource.close()
        dataSource.open(DataSpec(LuksDataSource.uriFor("/video.mp4"), 200_000L, 100L))
        val eofBuf = ByteArray(100)
        val eofRead = dataSource.read(eofBuf, 0, 100)
        assertEquals(100, eofRead)
        assertEquals(2, testVolume.chunkReadCount)

        // 3. Jump back to start of mdat (offset 32 in Block 0)
        dataSource.close()
        dataSource.open(DataSpec(LuksDataSource.uriFor("/video.mp4"), 32L, 50L))
        val mdatBuf = ByteArray(50)
        val mdatRead = dataSource.read(mdatBuf, 0, 50)
        assertEquals(50, mdatRead)

        // Since Block 0 was retained in the 4-block LRU cache, chunkReadCount should STILL be 2!
        assertEquals(2, testVolume.chunkReadCount)

        dataSource.close()
    }

    @Test
    fun `open position out of range throws DataSourceException`() {
        val testData = ByteArray(1024) { 1 }
        testVolume.fileDataMap["/short.mp4"] = testData

        val dataSource = LuksDataSource(session = session)

        try {
            dataSource.open(DataSpec(LuksDataSource.uriFor("/short.mp4"), 2000L, 100L))
            fail("Expected DataSourceException for out of range position")
        } catch (e: DataSourceException) {
            assertEquals(PlaybackException.ERROR_CODE_IO_READ_POSITION_OUT_OF_RANGE, e.reason)
        }
    }

    @Test
    fun `file not found throws DataSourceException`() {
        val dataSource = LuksDataSource(session = session)
        try {
            dataSource.open(DataSpec(LuksDataSource.uriFor("/does_not_exist.mp4")))
            fail("Expected DataSourceException for nonexistent file")
        } catch (e: DataSourceException) {
            assertEquals(PlaybackException.ERROR_CODE_IO_FILE_NOT_FOUND, e.reason)
        }
    }

    @Test
    fun `reading to EOF returns RESULT_END_OF_INPUT`() {
        val testData = ByteArray(20) { it.toByte() }
        testVolume.fileDataMap["/tiny.mp3"] = testData

        val dataSource = LuksDataSource(session = session, blockSize = 64)
        dataSource.open(DataSpec(LuksDataSource.uriFor("/tiny.mp3")))

        val buf = ByteArray(15)
        val read1 = dataSource.read(buf, 0, 15)
        assertEquals(15, read1)

        val read2 = dataSource.read(buf, 0, 15)
        assertEquals(5, read2)

        val readEof = dataSource.read(buf, 0, 15)
        assertEquals(C.RESULT_END_OF_INPUT, readEof)

        dataSource.close()
    }

    @Test
    fun `URI with spaces and special characters is decoded properly`() {
        val testData = ByteArray(128) { 42 }
        testVolume.fileDataMap["/My Videos/Track #1 [Final].mp4"] = testData

        val uri = LuksDataSource.uriFor("/My Videos/Track #1 [Final].mp4")
        val dataSource = LuksDataSource(session = session)

        val len = dataSource.open(DataSpec(uri))
        assertEquals(128L, len)

        val buf = ByteArray(128)
        val read = dataSource.read(buf, 0, 128)
        assertEquals(128, read)
        assertArrayEquals(testData, buf)

        dataSource.close()
    }

    @Test
    fun `transfer listener events match contract lifecycle`() {
        val testData = ByteArray(1024) { 7 }
        testVolume.fileDataMap["/audio.wav"] = testData

        var initCount = 0
        var startCount = 0
        var transferredBytes = 0
        var endCount = 0

        val listener = object : TransferListener {
            override fun onTransferInitializing(source: androidx.media3.datasource.DataSource, dataSpec: DataSpec, isNetwork: Boolean) {
                initCount++
            }
            override fun onTransferStart(source: androidx.media3.datasource.DataSource, dataSpec: DataSpec, isNetwork: Boolean) {
                startCount++
            }
            override fun onBytesTransferred(source: androidx.media3.datasource.DataSource, dataSpec: DataSpec, isNetwork: Boolean, bytesTransferred: Int) {
                transferredBytes += bytesTransferred
            }
            override fun onTransferEnd(source: androidx.media3.datasource.DataSource, dataSpec: DataSpec, isNetwork: Boolean) {
                endCount++
            }
        }

        val dataSource = LuksDataSource(session = session)
        dataSource.addTransferListener(listener)

        dataSource.open(DataSpec(LuksDataSource.uriFor("/audio.wav")))
        assertEquals(1, initCount)
        assertEquals(1, startCount)

        val buf = ByteArray(500)
        dataSource.read(buf, 0, 500)
        assertEquals(500, transferredBytes)

        dataSource.close()
        assertEquals(1, endCount)

        // Multiple close calls must be idempotent (no duplicate transferEnd)
        dataSource.close()
        assertEquals(1, endCount)
    }

    @Test
    fun `cached blocks are zeroized on release`() {
        val testData = ByteArray(1024) { (it + 1).toByte() }
        testVolume.fileDataMap["/secret.bin"] = testData

        val dataSource = LuksDataSource(session = session, blockSize = 512, maxBlocks = 2)
        dataSource.open(DataSpec(LuksDataSource.uriFor("/secret.bin")))

        val buf = ByteArray(512)
        dataSource.read(buf, 0, 512)
        assertEquals(1, testVolume.returnedChunks.size)
        val cachedChunk = testVolume.returnedChunks[0]
        // Chunk should not be zeroed yet
        assertTrue(cachedChunk.any { it != 0.toByte() })

        dataSource.release()

        // After release, the cached block ByteArray must have been wiped with zeroes
        assertTrue(cachedChunk.all { it == 0.toByte() })
    }

    @Test
    fun `cached blocks are zeroized on session lock`() {
        val testData = ByteArray(1024) { (it + 1).toByte() }
        testVolume.fileDataMap["/secret2.bin"] = testData

        val dataSource = LuksDataSource(session = session, blockSize = 512, maxBlocks = 2)
        dataSource.open(DataSpec(LuksDataSource.uriFor("/secret2.bin")))

        val buf = ByteArray(512)
        dataSource.read(buf, 0, 512)
        assertEquals(1, testVolume.returnedChunks.size)
        val cachedChunk = testVolume.returnedChunks[0]
        assertTrue(cachedChunk.any { it != 0.toByte() })

        runBlocking { session.lock() }

        // After session lock, the cached block ByteArray must have been wiped with zeroes
        assertTrue(cachedChunk.all { it == 0.toByte() })
    }

    @Test
    fun `cached blocks are zeroized on LRU eviction and file switch`() {
        val testData1 = ByteArray(1024) { (it + 1).toByte() }
        val testData2 = ByteArray(1024) { (it + 50).toByte() }
        testVolume.fileDataMap["/file1.bin"] = testData1
        testVolume.fileDataMap["/file2.bin"] = testData2

        // Cache capacity = 1 block (512 bytes)
        val dataSource = LuksDataSource(session = session, blockSize = 512, maxBlocks = 1)
        dataSource.open(DataSpec(LuksDataSource.uriFor("/file1.bin")))

        val buf = ByteArray(512)
        // Read block 0 of file1
        dataSource.read(buf, 0, 512)
        assertEquals(1, testVolume.returnedChunks.size)
        val block0 = testVolume.returnedChunks[0]
        assertTrue(block0.any { it != 0.toByte() })

        // Read block 1 of file1 -> triggers LRU eviction of block 0
        dataSource.read(buf, 0, 512)
        assertEquals(2, testVolume.returnedChunks.size)
        val block1 = testVolume.returnedChunks[1]
        assertTrue(block0.all { it == 0.toByte() }) // evicted block 0 wiped
        assertTrue(block1.any { it != 0.toByte() })

        // Switch to file2 -> triggers clearAndZeroBlockCache
        dataSource.open(DataSpec(LuksDataSource.uriFor("/file2.bin")))
        assertTrue(block1.all { it == 0.toByte() }) // block 1 wiped on file switch

        dataSource.release()
    }
}
