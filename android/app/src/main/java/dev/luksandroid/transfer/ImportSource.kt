package dev.luksandroid.transfer

import android.content.Context
import android.net.Uri
import android.os.ParcelFileDescriptor
import android.provider.OpenableColumns
import java.io.FileInputStream
import java.nio.ByteBuffer
import java.nio.channels.ReadableByteChannel

/** What the phone side of a single-file import says about itself. [sizeBytes] is -1 when unknown. */
data class ImportSourceInfo(val name: String, val sizeBytes: Long)

/** The phone side of a single-file import. Exists so the size/cancel/cleanup policy is testable off-device. */
interface ImportSource {
    fun describe(): ImportSourceInfo

    /** Opens the bytes. The caller closes the channel. */
    fun open(): ReadableByteChannel
}

/** [ImportSource] over a content `Uri`. Device-only: `Uri`/`ContentResolver` are inert in JVM tests. */
class ContentResolverImportSource(private val context: Context, private val uri: Uri) : ImportSource {

    override fun describe(): ImportSourceInfo {
        val resolver = context.contentResolver
        var queryName: String? = null
        var querySize = -1L
        try {
            resolver.query(uri, null, null, null, null)?.use { cursor ->
                if (cursor.moveToFirst()) {
                    val nameIdx = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                    val sizeIdx = cursor.getColumnIndex(OpenableColumns.SIZE)
                    if (nameIdx != -1) queryName = cursor.getString(nameIdx)
                    if (sizeIdx != -1 && !cursor.isNull(sizeIdx)) querySize = cursor.getLong(sizeIdx)
                }
            }
        } catch (_: Throwable) {
        }
        if (querySize < 0L) {
            try {
                resolver.openFileDescriptor(uri, "r")?.use { pfd -> querySize = pfd.statSize }
            } catch (_: Throwable) {
            }
        }
        val name = queryName
            ?: uri.lastPathSegment?.substringAfterLast('/')
            ?: "imported_${System.currentTimeMillis()}"
        return ImportSourceInfo(name, querySize)
    }

    override fun open(): ReadableByteChannel {
        val pfd: ParcelFileDescriptor = context.contentResolver.openFileDescriptor(uri, "r")
            ?: throw IllegalStateException("Could not open input file descriptor")
        val channel = FileInputStream(pfd.fileDescriptor).channel
        return object : ReadableByteChannel {
            override fun read(dst: ByteBuffer): Int = channel.read(dst)
            override fun isOpen(): Boolean = channel.isOpen
            override fun close() {
                try {
                    channel.close()
                } finally {
                    pfd.close()
                }
            }
        }
    }
}

/** A refusal with a path-free message the UI may show verbatim (never carries a name from either side). */
class TransferFailure(val userMessage: String, val code: Int? = null) : Exception(userMessage)
