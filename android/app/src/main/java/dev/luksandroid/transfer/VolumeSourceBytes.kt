package dev.luksandroid.transfer

import dev.luksandroid.LuksVolume
import dev.luksandroid.zeroEntireCapacity
import java.io.InputStream

/**
 * [SourceBytes] over the drive, for export. The mirror of the SAF-backed
 * source [TreeImporter] reads from: `sourceId` here is the absolute volume
 * path, exactly what [VolumeChildSource] puts in [RawChild.id].
 *
 * Reads through an [LuksVolume.openFile] handle and [LuksVolume.readAt] rather than
 * [LuksVolume.readFile], which caps at a few MiB by default and would quietly truncate anything
 * larger. Streaming also keeps peak memory at one chunk regardless of file
 * size, which matters on a phone exporting a video.
 *
 * The path is resolved once, when the stream first reads. Every chunk then reads by
 * identity, so a file deleted or replaced mid-export fails the stream with
 * [dev.luksandroid.LuksException.FILE_CHANGED] instead of splicing another file's bytes into
 * the copy.
 */
class VolumeSourceBytes(
    private val volume: LuksVolume,
    private val chunkBytes: Int = TreeExporter.CHUNK_SIZE,
) : SourceBytes {

    override fun open(sourceId: String): InputStream = VolumeInputStream(volume, sourceId, chunkBytes)
}

/**
 * Pulls a file off the volume one chunk at a time.
 *
 * Backed by a single reusable direct [ByteBuffer] wiped on [close], avoiding heap allocations
 * and lingering plaintext tails.
 */
private class VolumeInputStream(
    private val volume: LuksVolume,
    private val path: String,
    private val chunkBytes: Int,
) : InputStream() {

    private var offset = 0L
    private var file = 0L
    private var closed = false
    private val directBuf: java.nio.ByteBuffer = java.nio.ByteBuffer.allocateDirect(chunkBytes)
    private var bufferPos = 0
    private var bufferLimit = 0

    /**
     * Fills [directBuf] if it is spent. Returns false at end of file.
     *
     * A short read is treated as end of file rather than retried: the native
     * side returns what the extent actually holds, so a chunk shorter than
     * requested means the file ended, and looping on it would spin forever on
     * a zero-length return.
     */
    private fun fill(): Boolean {
        if (bufferPos < bufferLimit) return true
        if (closed) throw java.io.IOException("stream is closed")
        if (file == 0L) file = volume.openFile(path)
        directBuf.clear()
        val got = volume.readAt(file, offset, directBuf, chunkBytes)
        if (got <= 0) {
            bufferPos = 0
            bufferLimit = 0
            return false
        }
        bufferPos = 0
        bufferLimit = got
        offset += got
        return true
    }

    override fun read(): Int {
        if (!fill()) return -1
        return directBuf.get(bufferPos++).toInt() and 0xFF
    }

    override fun read(b: ByteArray, off: Int, len: Int): Int {
        if (off < 0 || len < 0 || len > b.size - off) {
            throw IndexOutOfBoundsException()
        }
        if (len == 0) return 0

        if (bufferPos < bufferLimit) {
            val n = minOf(len, bufferLimit - bufferPos)
            directBuf.position(bufferPos)
            directBuf.get(b, off, n)
            bufferPos += n
            return n
        }

        if (closed) throw java.io.IOException("stream is closed")
        if (file == 0L) file = volume.openFile(path)

        if (len >= chunkBytes) {
            val toRead = minOf(len, chunkBytes)
            directBuf.clear()
            val got = volume.readAt(file, offset, directBuf, toRead)
            if (got <= 0) return -1
            offset += got
            directBuf.position(0)
            directBuf.get(b, off, got)
            return got
        }

        if (!fill()) return -1
        val n = minOf(len, bufferLimit - bufferPos)
        directBuf.position(bufferPos)
        directBuf.get(b, off, n)
        bufferPos += n
        return n
    }

    override fun available(): Int = bufferLimit - bufferPos

    override fun close() {
        if (closed) return
        closed = true
        bufferPos = 0
        bufferLimit = 0
        directBuf.zeroEntireCapacity()
        val f = file
        file = 0L
        if (f != 0L) volume.closeFile(f)
    }
}
