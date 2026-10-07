package dev.luksandroid.transfer

import android.system.ErrnoException
import android.system.OsConstants
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.ByteArrayOutputStream
import java.io.InputStream
import java.io.OutputStream

class PendingNameTargetTest {

    /** Documents are strings: "<id>". The provider keeps names by id; renames may change the id. */
    private class Ops : DocumentOps<String> {
        val names = mutableMapOf("doc1" to "out.bin")
        val deleted = mutableListOf<String>()
        var supportsRename = true
        var renameToPendingReturnsNull = false
        var renameBackThrows = false
        var renameBackReturnsNull = false
        var renameBackAdjustsName: String? = null

        override fun displayName(doc: String) = names[doc]
        override fun supportsRename(doc: String) = supportsRename
        override fun rename(doc: String, newName: String): String? {
            val isBack = !newName.endsWith(PendingNameTarget.PENDING_SUFFIX)
            if (isBack && renameBackThrows) throw java.io.IOException("refused")
            if (!isBack && renameToPendingReturnsNull) { names[doc] = newName; return null }
            if (isBack && renameBackReturnsNull) { names[doc] = newName; return null }
            val newId = doc + "'"
            names.remove(doc)
            names[newId] = if (isBack && renameBackAdjustsName != null) renameBackAdjustsName!! else newName
            return newId
        }
        override fun delete(doc: String): Boolean = if (names.remove(doc) != null) { deleted += doc; true } else false
        override fun openOutput(doc: String): OutputStream? = if (doc in names) ByteArrayOutputStream() else null
        override fun openInput(doc: String): InputStream? = null
        override fun availableBytes(doc: String): Long? = null
        override fun sizeBytes(doc: String): Long? = null
    }

    @Test
    fun `happy path writes under the pending name and ends under the final one`() {
        val ops = Ops()
        val t = PendingNameTarget(ops, "doc1")
        t.openOutput()
        assertEquals(setOf("out.bin.luks-partial"), ops.names.values.toSet())
        t.commit()
        assertEquals(setOf("out.bin"), ops.names.values.toSet())
        t.discard() // after a commit it must not delete the finished file
        assertTrue(ops.deleted.isEmpty())
    }

    @Test
    fun `a failed rename-back throws and discard deletes the pending document`() {
        val ops = Ops().also { it.renameBackThrows = true }
        val t = PendingNameTarget(ops, "doc1")
        t.openOutput()
        assertTrue(runCatching { t.commit() }.isFailure)
        t.discard()
        assertTrue("nothing complete-looking is left", ops.names.isEmpty())
    }

    @Test
    fun `a rename-back that returns nothing throws and discard still finds the document`() {
        val ops = Ops().also { it.renameBackReturnsNull = true }
        val t = PendingNameTarget(ops, "doc1")
        t.openOutput()
        assertTrue(runCatching { t.commit() }.isFailure)
        t.discard()
        // the provider renamed in place without telling us: the pending handle no longer exists,
        // the original one is what the document is called now
        assertTrue(ops.names.isEmpty())
    }

    @Test
    fun `a rename-back that returns a stale or different name throws and discard removes it`() {
        val ops = Ops().also { it.renameBackAdjustsName = "out (1).bin" }
        val t = PendingNameTarget(ops, "doc1")
        t.openOutput()
        assertTrue(runCatching { t.commit() }.isFailure)
        t.discard()
        assertTrue(ops.names.isEmpty())
    }

    @Test
    fun `a provider that cannot rename is written under the final name`() {
        val ops = Ops().also { it.supportsRename = false }
        val t = PendingNameTarget(ops, "doc1")
        t.openOutput()
        assertEquals(setOf("out.bin"), ops.names.values.toSet())
        t.commit()
        assertFalse(ops.names.isEmpty())
    }

    @Test
    fun `all authorities default to no voyeur verification to preserve maximum export throughput`() {
        assertEquals(VerifyLevel.NONE, verifyLevelForAuthority("com.android.externalstorage.documents"))
        assertEquals(VerifyLevel.NONE, verifyLevelForAuthority("com.android.providers.downloads.documents"))
        assertEquals(VerifyLevel.NONE, verifyLevelForAuthority("com.google.android.apps.docs.storage"))
        assertEquals(VerifyLevel.NONE, verifyLevelForAuthority(null))
    }

    @Test
    fun `syncOutputStream on ByteArrayOutputStream is a safe no-op`() {
        val baos = ByteArrayOutputStream()
        baos.write(byteArrayOf(1, 2, 3))
        syncOutputStream(baos)
        assertEquals(3, baos.size())
    }

    @Test
    fun `syncOutputStream on FileOutputStream forces dirty pages to disk`() {
        val tmp = java.io.File.createTempFile("fsync_test", ".tmp")
        try {
            val fos = java.io.FileOutputStream(tmp)
            fos.write(byteArrayOf(10, 20, 30, 40))
            syncOutputStream(fos)
            fos.close()
            assertEquals(4L, tmp.length())
        } finally {
            tmp.delete()
        }
    }

    @Test
    fun `syncOutputStream on FilterOutputStream unwraps and forces dirty pages to disk`() {
        val tmp = java.io.File.createTempFile("fsync_test_filter", ".tmp")
        try {
            val fos = java.io.FileOutputStream(tmp)
            val filtered = java.io.BufferedOutputStream(fos)
            filtered.write(byteArrayOf(50, 60, 70, 80))
            syncOutputStream(filtered)
            filtered.close()
            assertEquals(4L, tmp.length())
        } finally {
            tmp.delete()
        }
    }

    @Test
    fun `syncOutputStream classifies EIO and ENOSPC as fatal while ignoring non-syncable errnos`() {
        // Linux/Bionic standard errno codes
        val eio = 5
        val enospc = 28
        val edquot = 122
        val einval = 22
        val enotty = 25
        val ebadf = 9
        val erofs = 30
        val espipe = 29

        assertFalse("EIO must not be ignored", isNonSyncableErrno(eio))
        assertFalse("ENOSPC must not be ignored", isNonSyncableErrno(enospc))
        assertFalse("EDQUOT must not be ignored", isNonSyncableErrno(edquot))
        assertTrue("EINVAL must be ignored", isNonSyncableErrno(einval))
        assertTrue("ENOTTY must be ignored", isNonSyncableErrno(enotty))
        assertTrue("EBADF must be ignored", isNonSyncableErrno(ebadf))
        assertTrue("EROFS must be ignored", isNonSyncableErrno(erofs))
        assertTrue("ESPIPE must be ignored", isNonSyncableErrno(espipe))
    }
}

