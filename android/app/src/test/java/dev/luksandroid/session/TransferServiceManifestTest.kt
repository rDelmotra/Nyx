package dev.luksandroid.session

import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * API 34+ throws if the type passed to startForeground() is not covered by the
 * manifest's foregroundServiceType, or the matching permission is missing. A JVM test
 * cannot start a service, so pin the three places that must agree.
 */
class TransferServiceManifestTest {
    private fun file(rel: String): File {
        val f = File(rel)
        assertTrue("fixture missing (cwd ${File(".").absolutePath}): $rel", f.isFile)
        return f
    }

    private fun serviceTag(manifest: String, name: String): String {
        val re = Regex("<service\\b[^>]*android:name=\"\\.$name\"[^>]*>", RegexOption.DOT_MATCHES_ALL)
        val m = re.find(manifest)
        assertTrue("manifest declares no <service .$name>", m != null)
        return m!!.value
    }

    @Test
    fun transferService_isDeclared_asDataSync_notExported() {
        val manifest = file("src/main/AndroidManifest.xml").readText()
        val tag = serviceTag(manifest, "TransferService")
        assertTrue(tag, tag.contains("android:foregroundServiceType=\"dataSync\""))
        assertTrue(tag, tag.contains("android:exported=\"false\""))
        assertTrue(manifest.contains("android.permission.FOREGROUND_SERVICE_DATA_SYNC"))
        assertTrue(manifest.contains("android.permission.FOREGROUND_SERVICE\""))
        assertTrue(manifest.contains("android.permission.POST_NOTIFICATIONS"))
    }

    @Test
    fun transferService_startsForegroundWithTheDataSyncType() {
        val src = file("src/main/java/dev/luksandroid/TransferService.kt").readText()
        assertTrue("startForeground must pass FOREGROUND_SERVICE_TYPE_DATA_SYNC",
            Regex("startForeground\\([^;]*FOREGROUND_SERVICE_TYPE_DATA_SYNC").containsMatchIn(src))
        assertTrue("API 35 dataSync has a timeout: onTimeout must stop the service",
            src.contains("onTimeout"))
    }

    @Test
    fun transferService_guardsStartForeground_andStopsItselfWhenIdle() {
        val src = file("src/main/java/dev/luksandroid/TransferService.kt").readText()
        val i = src.indexOf("startForeground(")
        assertTrue(i > 0)
        val before = src.substring(0, i)
        assertTrue("startForeground must sit inside a try", before.trimEnd().substringAfterLast("onStartCommand").contains("try {"))
        assertTrue("a refused start must stopSelf and not crash",
            Regex("catch \\(t: Throwable\\)[\\s\\S]*?stopSelf\\(\\)").containsMatchIn(src))
        assertTrue("must stop itself when the engine is already idle", src.contains("if (!TransferManager.busy.value) stopSelf()"))
    }

    @Test
    fun unlockService_control_stillMatches() {
        // Vacuity control for the scan above: the same checks on the existing service.
        val manifest = file("src/main/AndroidManifest.xml").readText()
        assertEquals(true, serviceTag(manifest, "UnlockService").contains("dataSync"))
    }
}
