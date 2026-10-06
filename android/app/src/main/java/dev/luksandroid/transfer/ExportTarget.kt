package dev.luksandroid.transfer

import android.content.Context
import android.net.Uri
import android.provider.DocumentsContract
import dev.luksandroid.Trace
import java.io.IOException
import java.io.InputStream
import java.io.OutputStream
import java.security.MessageDigest
import kotlinx.coroutines.CancellationException

/** How far an export was verified before it was reported done. Recorded on the transfer item. */
enum class VerifyLevel {
    /** Size and SHA-256 re-read from the destination. Only for local providers, where a re-read is cheap. */
    FULL,

    /** Size reported by the provider only: re-reading a cloud-backed document would download it again. */
    SIZE,

    /** Direct high-performance export with no post-hoc read-back or size querying. */
    NONE,
}

/**
 * The destination of a single-file export, as the engine needs it.
 *
 * Exists so the failure policy (R5r) is testable without `Uri`/`DocumentsContract`:
 * the engine writes through [openOutput], verifies, publishes with [commit], and on ANY
 * failure, cancel or FILE_CHANGED calls [discard], so a partial file is never left behind
 * under the final name.
 */
interface ExportTarget {
    /** Free bytes at the destination, or null when the provider will not say (unknown, not "full"). */
    fun availableBytes(): Long?

    /** Opens the destination for writing, under a pending name when the provider supports rename. */
    fun openOutput(): OutputStream

    /** How this destination is verified. */
    val verifyLevel: VerifyLevel get() = VerifyLevel.NONE

    /** Opens what was written for reading back ([VerifyLevel.FULL]), or null when the provider cannot. */
    fun openRead(): InputStream?

    /** The size the provider reports for what was written ([VerifyLevel.SIZE]), or null if it will not say. */
    fun sizeBytes(): Long? = null

    /** Gives the destination its final name. Throws if the provider refuses or cannot confirm it. */
    fun commit()

    /** Deletes the destination document under every name it may hold. Best effort, idempotent, never throws. */
    fun discard()
}

/** Result of re-reading an export's destination. */
sealed class ReadBack {
    object Verified : ReadBack()
    class SizeMismatch(val expected: Long, val actual: Long) : ReadBack()
    object HashMismatch : ReadBack()
    object Unreadable : ReadBack()
}

/**
 * Re-reads the destination through [open] and compares size and SHA-256 with what was read
 * from the source. [expectedSha256] is the digest of the bytes the engine handed to the
 * destination, taken as they left the source handle.
 *
 * [isCancelled] is checked per chunk and throws [CancellationException], so a cancel does not
 * wait out the rest of the re-read; [onProgress] gets the bytes verified so far.
 */
fun verifyReadBack(
    open: () -> InputStream?,
    expectedBytes: Long,
    expectedSha256: ByteArray,
    isCancelled: () -> Boolean = { false },
    onProgress: (Long) -> Unit = {},
): ReadBack {
    val input = try {
        open()
    } catch (_: Throwable) {
        null
    } ?: return ReadBack.Unreadable
    val digest = MessageDigest.getInstance("SHA-256")
    var total = 0L
    val buf = ByteArray(1 shl 20)
    try {
        input.use {
            while (true) {
                if (isCancelled()) throw CancellationException("Transfer cancelled")
                val n = it.read(buf)
                if (n < 0) break
                digest.update(buf, 0, n)
                total += n
                onProgress(total)
            }
        }
    } catch (_: IOException) {
        return ReadBack.Unreadable
    } finally {
        java.util.Arrays.fill(buf, 0.toByte())
    }
    if (total != expectedBytes) return ReadBack.SizeMismatch(expectedBytes, total)
    return if (MessageDigest.isEqual(digest.digest(), expectedSha256)) ReadBack.Verified else ReadBack.HashMismatch
}

/** Providers whose documents live on this device. */
val LOCAL_DOCUMENT_AUTHORITIES = setOf(
    "com.android.externalstorage.documents",
    "com.android.providers.downloads.documents",
)

/**
 * Verification level for exported documents.
 * Defaults to [VerifyLevel.NONE] to avoid reading back or querying the provider over
 * Android's slow SAF layer, preserving maximum USB 3.0 / SSD export throughput (~100+ MiB/s).
 */
fun verifyLevelForAuthority(authority: String?): VerifyLevel = VerifyLevel.NONE

/** The document operations [PendingNameTarget] needs, over an opaque document handle [D]. */
interface DocumentOps<D : Any> {
    fun displayName(doc: D): String?
    fun supportsRename(doc: D): Boolean

    /** Renames [doc]; returns the (possibly different) handle, or null if the provider did not say. */
    fun rename(doc: D, newName: String): D?
    fun delete(doc: D): Boolean
    fun openOutput(doc: D): OutputStream?
    fun openInput(doc: D): InputStream?
    fun availableBytes(doc: D): Long?
    fun sizeBytes(doc: D): Long?
}

/**
 * A document the user created with `CreateDocument`, written under a pending name.
 *
 * When the provider supports rename, the document is renamed to `<name>.luks-partial` before the
 * first byte and renamed back in [commit], so an app kill mid-export leaves an obviously
 * unfinished name instead of a truncated file under the final one. Every step is best effort
 * until bytes are written; if the rename is refused the export simply uses the final name.
 *
 * Both handles are tracked: [pendingDoc] (what the document is called while it is written) and
 * [finalDoc] (the handle the user picked). [commit] only succeeds if the provider hands back a
 * handle whose name is the final name. If the rename back fails, returns nothing, or returns a
 * stale name, [commit] throws and [discard] deletes by EVERY handle known, so no complete-looking
 * file is left behind under either name.
 */
class PendingNameTarget<D : Any>(
    private val ops: DocumentOps<D>,
    private val finalDoc: D,
    override val verifyLevel: VerifyLevel = VerifyLevel.NONE,
) : ExportTarget {

    private var current: D = finalDoc
    private var pendingDoc: D? = null
    private var finalName: String? = null
    private var renamedBack: D? = null
    private var committed = false

    override fun availableBytes(): Long? = try {
        ops.availableBytes(current)
    } catch (_: Throwable) {
        null
    }

    override fun openOutput(): OutputStream {
        tryMoveToPendingName()
        return ops.openOutput(current) ?: throw IllegalStateException("Could not open destination URI for writing")
    }

    override fun openRead(): InputStream? = try {
        ops.openInput(current)
    } catch (_: Throwable) {
        null
    }

    override fun sizeBytes(): Long? = try {
        ops.sizeBytes(current)
    } catch (_: Throwable) {
        null
    }

    override fun commit() {
        val name = finalName
        val pending = pendingDoc
        if (pending == null || name == null) {
            committed = true
            return
        }
        val renamed = try {
            ops.rename(pending, name)
        } catch (t: Throwable) {
            throw IOException("the destination refused to give the export its final name")
        } ?: throw IOException("the destination did not confirm the export's final name")
        renamedBack = renamed
        // A stale handle or a provider-adjusted name is not the final name: refuse it.
        val actual = try {
            ops.displayName(renamed)
        } catch (_: Throwable) {
            null
        }
        if (actual != name) throw IOException("the destination gave the export a different name")
        current = renamed
        pendingDoc = null
        committed = true
    }

    override fun discard() {
        if (committed) return
        // Pending handle first (where the bytes live while writing), then whatever we last held,
        // then the handle the user picked: a provider that renamed and then failed to say so
        // leaves the document under any of them.
        val seen = ArrayList<D>(3)
        for (d in listOfNotNull(pendingDoc, renamedBack, current, finalDoc)) {
            if (seen.any { it == d }) continue
            seen += d
            val ok = try {
                ops.delete(d)
            } catch (_: Throwable) {
                false
            }
            // Only the handle that really held the document deletes; the others are expected to fail.
            if (ok) return
        }
        Trace.e("PendingNameTarget: could not delete a failed export under any known name")
    }

    private fun tryMoveToPendingName() {
        try {
            val display = ops.displayName(current) ?: return
            if (!ops.supportsRename(current)) return
            val renamed = ops.rename(current, "$display$PENDING_SUFFIX") ?: return
            pendingDoc = renamed
            current = renamed
            finalName = display
        } catch (_: Throwable) {
            // Fall back to writing under the final name.
        }
    }

    companion object {
        const val PENDING_SUFFIX = ".luks-partial"
    }
}

/** [DocumentOps] over SAF `Uri`s. Device-only: `DocumentsContract` is inert under JVM unit tests. */
class SafDocumentOps(private val context: Context) : DocumentOps<Uri> {
    private val resolver get() = context.contentResolver

    private fun column(doc: Uri, name: String): Pair<String?, Long?> {
        resolver.query(doc, arrayOf(name), null, null, null)?.use { c ->
            if (c.moveToFirst() && !c.isNull(0)) {
                return if (name == DocumentsContract.Document.COLUMN_DISPLAY_NAME) c.getString(0) to null else null to c.getLong(0)
            }
        }
        return null to null
    }

    override fun displayName(doc: Uri): String? = column(doc, DocumentsContract.Document.COLUMN_DISPLAY_NAME).first

    override fun supportsRename(doc: Uri): Boolean {
        val flags = column(doc, DocumentsContract.Document.COLUMN_FLAGS).second ?: return false
        return flags.toInt() and DocumentsContract.Document.FLAG_SUPPORTS_RENAME != 0
    }

    override fun rename(doc: Uri, newName: String): Uri? = DocumentsContract.renameDocument(resolver, doc, newName)

    override fun delete(doc: Uri): Boolean = DocumentsContract.deleteDocument(resolver, doc)

    override fun openOutput(doc: Uri): OutputStream? = resolver.openOutputStream(doc)

    override fun openInput(doc: Uri): InputStream? = resolver.openInputStream(doc)

    override fun availableBytes(doc: Uri): Long? = resolver.openFileDescriptor(doc, "r")?.use { pfd ->
        val st = android.system.Os.fstatvfs(pfd.fileDescriptor)
        // A provider that fills in zeros has told us nothing; unknown is not full.
        availableFromStatVfs(st.f_bavail, st.f_frsize).takeIf { it > 0L }
    }

    override fun sizeBytes(doc: Uri): Long? = column(doc, DocumentsContract.Document.COLUMN_SIZE).second
}

/** A `CreateDocument` result as an [ExportTarget]; the verification level follows the provider's authority. */
fun SafExportTarget(context: Context, uri: Uri): ExportTarget =
    PendingNameTarget(SafDocumentOps(context), uri, verifyLevelForAuthority(uri.authority))

/** Free bytes from a `statvfs` result, saturating rather than overflowing. */
fun availableFromStatVfs(blocksAvailable: Long, fragmentSize: Long): Long {
    if (blocksAvailable <= 0L || fragmentSize <= 0L) return 0L
    return if (blocksAvailable > Long.MAX_VALUE / fragmentSize) Long.MAX_VALUE else blocksAvailable * fragmentSize
}
