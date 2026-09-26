package dev.luksandroid.ui.browser.viewer

import android.net.Uri
import androidx.media3.common.C
import androidx.media3.datasource.BaseDataSource
import androidx.media3.datasource.DataSource
import androidx.media3.datasource.DataSpec
import dev.luksandroid.session.LuksSession
import kotlinx.coroutines.runBlocking
import java.io.IOException

/**
 * Reads a file inside the unlocked volume for ExoPlayer, one chunk per
 * [read] call via [LuksSession.withLease] — the same session-lifecycle guard
 * every other read/write in the app goes through, so a USB detach or lock
 * mid-playback surfaces as a normal player error instead of touching a
 * stale native handle.
 */
class LuksDataSource : BaseDataSource(/* isNetwork= */ false) {

    private var uri: Uri? = null
    private var path: String = ""
    private var position: Long = 0L
    private var bytesRemaining: Long = 0L

    override fun open(dataSpec: DataSpec): Long {
        uri = dataSpec.uri
        path = dataSpec.uri.path ?: throw IOException("no path in $dataSpec")
        transferInitializing(dataSpec)

        val size = try {
            runBlocking { LuksSession.withLease { v -> v.fileInfo(path).size } }
        } catch (e: Exception) {
            throw IOException("could not stat $path", e)
        }

        position = dataSpec.position
        bytesRemaining = if (dataSpec.length != C.LENGTH_UNSET.toLong()) {
            dataSpec.length
        } else {
            size - position
        }
        if (bytesRemaining < 0) {
            throw IOException("requested range starts past end of $path (size=$size)")
        }

        transferStarted(dataSpec)
        return bytesRemaining
    }

    override fun read(buffer: ByteArray, offset: Int, length: Int): Int {
        if (length == 0) return 0
        if (bytesRemaining == 0L) return C.RESULT_END_OF_INPUT

        val want = minOf(length.toLong(), bytesRemaining).toInt()
        val got = try {
            runBlocking { LuksSession.withLease { v -> v.readChunk(path, position, want) } }
        } catch (e: Exception) {
            throw IOException("read failed at $path:$position", e)
        }
        if (got.isEmpty()) return C.RESULT_END_OF_INPUT

        System.arraycopy(got, 0, buffer, offset, got.size)
        position += got.size
        bytesRemaining -= got.size
        bytesTransferred(got.size)
        return got.size
    }

    override fun getUri(): Uri? = uri

    override fun close() {
        uri = null
        transferEnded()
    }

    class Factory : DataSource.Factory {
        override fun createDataSource(): DataSource = LuksDataSource()
    }

    companion object {
        /** Builds the [Uri] ExoPlayer should be given for a file at [fullPath] inside the volume. */
        fun uriFor(fullPath: String): Uri = Uri.parse("luksvolume://volume$fullPath")
    }
}
