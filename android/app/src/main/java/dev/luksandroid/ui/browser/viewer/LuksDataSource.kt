package dev.luksandroid.ui.browser.viewer

import android.net.Uri
import androidx.media3.common.C
import androidx.media3.common.PlaybackException
import androidx.media3.datasource.BaseDataSource
import androidx.media3.datasource.DataSource
import androidx.media3.datasource.DataSourceException
import androidx.media3.datasource.DataSpec
import dev.luksandroid.session.LuksSession
import dev.luksandroid.session.SessionController
import dev.luksandroid.session.SessionState
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import java.io.EOFException

/**
 * Read-only [DataSource] for Media3, reading directly from an unlocked LUKS
 * volume via [SessionController.withLease].
 *
 * Never calls into the write/commit path. Caches the last [maxBlocks] x
 * [blockSize] blocks in RAM so small, repeated reads in the same region
 * (container-header parsing, seeking within a buffered range) don't each
 * cost a separate native call; nothing is measured beyond that.
 */
class LuksDataSource(
    private val session: SessionController = LuksSession,
    private val blockSize: Int = DEFAULT_BLOCK_SIZE,
    private val maxBlocks: Int = DEFAULT_MAX_BLOCKS,
) : BaseDataSource(/* isNetwork= */ false) {

    private var uri: Uri? = null
    private var path: String = ""
    private var totalFileSize: Long = 0L
    private var readPosition: Long = 0L
    private var bytesRemaining: Long = 0L
    private var opened: Boolean = false
    private var sessionJob: Job? = null

    /**
     * Read handle for [path] (0 = none), so every block is read by identity: a file replaced
     * under the player fails with FILE_CHANGED instead of mixing in another file's bytes. Its
     * lifetime matches the block cache's: closed when the path changes, when it went stale,
     * and on [release].
     */
    private var file: Long = 0L
    /** The volume [file] was opened on, kept so it can be closed without a session lease. */
    private var fileVolume: dev.luksandroid.LuksVolume? = null
    private var handleStale: Boolean = false

    // LRU Block Cache: Block Index (position / blockSize) -> ByteArray
    private val blockCache = object : LinkedHashMap<Long, ByteArray>(maxBlocks, 0.75f, true) {
        override fun removeEldestEntry(eldest: MutableMap.MutableEntry<Long, ByteArray>?): Boolean {
            if (size > maxBlocks) {
                eldest?.value?.let { java.util.Arrays.fill(it, 0.toByte()) }
                return true
            }
            return false
        }
    }

    init {
        sessionJob = session.scope.launch {
            session.state.collect { state ->
                if (state is SessionState.Locked) {
                    clearAndZeroBlockCache()
                }
            }
        }
    }

    private fun clearAndZeroBlockCache() {
        synchronized(blockCache) {
            for (block in blockCache.values) {
                java.util.Arrays.fill(block, 0.toByte())
            }
            blockCache.clear()
        }
    }

    override fun open(dataSpec: DataSpec): Long {
        uri = dataSpec.uri
        val rawPath = dataSpec.uri.path ?: throw DataSourceException(
            "No path in $dataSpec",
            PlaybackException.ERROR_CODE_IO_FILE_NOT_FOUND
        )

        // Decode URL-encoded characters (spaces, %, #, etc.) and normalize leading path
        val decodedPath = Uri.decode(rawPath)
        val targetPath = if (decodedPath.startsWith("/volume/") && dataSpec.uri.authority != "volume") {
            decodedPath.removePrefix("/volume")
        } else {
            decodedPath
        }
        val cleanPath = if (targetPath.isEmpty()) "/" else targetPath

        // Invalidate cache only if switching to a different file, or if the handle's file
        // changed (its cached blocks may then belong to the old file).
        if (cleanPath != path || handleStale) {
            closeHandle()
            clearAndZeroBlockCache()
            path = cleanPath
        }

        transferInitializing(dataSpec)

        // Open first, then take the size FROM THE HANDLE: a second path-based stat could name
        // a different file than the one the reads will be pinned to.
        if (file == 0L) {
            try {
                runBlocking {
                    session.withLease { v ->
                        file = v.openFile(path)
                        fileVolume = v
                    }
                }
            } catch (t: Throwable) {
                throw DataSourceException(
                    "Could not open LUKS file $path",
                    t,
                    PlaybackException.ERROR_CODE_IO_FILE_NOT_FOUND
                )
            }
            handleStale = false
        }

        totalFileSize = try {
            runBlocking { session.withLease { v -> v.fileSizeOf(file) } }
        } catch (t: Throwable) {
            if ((t as? dev.luksandroid.LuksException)?.isFileChanged == true) handleStale = true
            throw DataSourceException(
                "Could not stat LUKS file $path",
                t,
                PlaybackException.ERROR_CODE_IO_FILE_NOT_FOUND
            )
        }

        if (dataSpec.position > totalFileSize) {
            throw DataSourceException(PlaybackException.ERROR_CODE_IO_READ_POSITION_OUT_OF_RANGE)
        }

        readPosition = dataSpec.position
        bytesRemaining = if (dataSpec.length != C.LENGTH_UNSET.toLong()) {
            dataSpec.length
        } else {
            totalFileSize - readPosition
        }

        if (bytesRemaining < 0L) {
            throw DataSourceException(PlaybackException.ERROR_CODE_IO_READ_POSITION_OUT_OF_RANGE)
        }

        opened = true
        transferStarted(dataSpec)
        return bytesRemaining
    }

    override fun read(buffer: ByteArray, offset: Int, length: Int): Int {
        if (length == 0) return 0
        if (bytesRemaining == 0L) return C.RESULT_END_OF_INPUT

        val bytesToRead = minOf(length.toLong(), bytesRemaining).toInt()
        val currentBlockIndex = readPosition / blockSize
        val blockOffset = (readPosition % blockSize).toInt()

        val blockData = getOrLoadBlock(currentBlockIndex)
        val availableInBlock = blockData.size - blockOffset
        if (availableInBlock <= 0) {
            if (bytesRemaining != C.LENGTH_UNSET.toLong() && bytesRemaining > 0) {
                throw DataSourceException(
                    "Unexpected EOF in LUKS file $path at position $readPosition",
                    EOFException(),
                    PlaybackException.ERROR_CODE_IO_READ_POSITION_OUT_OF_RANGE
                )
            }
            return C.RESULT_END_OF_INPUT
        }

        val toCopy = minOf(bytesToRead, availableInBlock)
        System.arraycopy(blockData, blockOffset, buffer, offset, toCopy)

        readPosition += toCopy
        bytesRemaining -= toCopy
        bytesTransferred(toCopy)
        return toCopy
    }

    private fun getOrLoadBlock(blockIndex: Long): ByteArray {
        synchronized(blockCache) {
            blockCache[blockIndex]?.let { return it }
        }

        val startOffset = blockIndex * blockSize
        val fetchSize = minOf(blockSize.toLong(), totalFileSize - startOffset).toInt()
        if (fetchSize <= 0) return EMPTY_BYTE_ARRAY

        val data = try {
            runBlocking { session.withLease { v -> v.readAt(file, startOffset, fetchSize) } }
        } catch (t: Throwable) {
            if ((t as? dev.luksandroid.LuksException)?.isFileChanged == true) handleStale = true
            throw DataSourceException(
                "Native readAt failed at $path:$startOffset",
                t,
                PlaybackException.ERROR_CODE_IO_UNSPECIFIED
            )
        }

        synchronized(blockCache) {
            blockCache[blockIndex] = data
        }
        return data
    }

    override fun getUri(): Uri? = uri

    override fun close() {
        uri = null
        if (opened) {
            opened = false
            transferEnded()
        }
    }

    /**
     * Closes the read handle. No session lease: closing only needs the native id, and this
     * runs from [release] (possibly the finalizer) and after the session may be gone.
     */
    private fun closeHandle() {
        val f = file
        val v = fileVolume
        file = 0L
        fileVolume = null
        if (f != 0L) dev.luksandroid.Trace.cleanup("data_source_close_file") { v?.closeFile(f) }
    }

    fun release() {
        sessionJob?.cancel()
        sessionJob = null
        closeHandle()
        clearAndZeroBlockCache()
    }

    protected fun finalize() {
        release()
    }

    class Factory(
        private val session: SessionController = LuksSession,
        private val blockSize: Int = DEFAULT_BLOCK_SIZE,
        private val maxBlocks: Int = DEFAULT_MAX_BLOCKS,
    ) : DataSource.Factory {
        private val instances = mutableListOf<LuksDataSource>()

        override fun createDataSource(): DataSource {
            val ds = LuksDataSource(session, blockSize, maxBlocks)
            synchronized(instances) { instances.add(ds) }
            return ds
        }

        fun release() {
            synchronized(instances) {
                instances.forEach { it.release() }
                instances.clear()
            }
        }
    }

    companion object {
        const val DEFAULT_BLOCK_SIZE = 256 * 1024 // 256 KiB aligned blocks
        const val DEFAULT_MAX_BLOCKS = 4         // 1 MiB total RAM cache
        private val EMPTY_BYTE_ARRAY = ByteArray(0)

        /** Builds the [Uri] ExoPlayer should be given for a file at [fullPath] inside the volume. */
        fun uriFor(fullPath: String): Uri {
            val cleanPath = if (fullPath.startsWith("/")) fullPath else "/$fullPath"
            return Uri.Builder()
                .scheme("luksvolume")
                .authority("volume")
                .encodedPath(Uri.encode(cleanPath, "/"))
                .build()
        }
    }
}
