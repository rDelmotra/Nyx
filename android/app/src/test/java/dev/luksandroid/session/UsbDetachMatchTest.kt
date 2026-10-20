package dev.luksandroid.session

import dev.luksandroid.session.UsbDetachReceiver.Companion.UsbIdentity
import dev.luksandroid.session.UsbDetachReceiver.Companion.isOurDetach
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/** C7: a detach is ours only on a real device match. */
class UsbDetachMatchTest {
    private val ours = UsbIdentity(deviceId = 1002, vendorId = 0x0781, productId = 0x5581)

    @Test fun sameDevice_matches() = assertTrue(isOurDetach(ours, ours.copy()))

    @Test fun noActiveDevice_isNotOurs() = assertFalse(isOurDetach(null, ours))

    @Test fun nullExtra_isNotOurs() = assertFalse(isOurDetach(ours, null))

    @Test fun bothNull_isNotOurs() = assertFalse(isOurDetach(null, null))

    @Test fun sameVidPid_differentDeviceId_isNotOurs() =
        assertFalse(isOurDetach(ours, ours.copy(deviceId = 1003)))

    @Test fun sameDeviceId_differentVidPid_isNotOurs() =
        assertFalse(isOurDetach(ours, ours.copy(vendorId = 0x1234)))

    @Test fun unrelatedDevice_isNotOurs() =
        assertFalse(isOurDetach(ours, UsbIdentity(2001, 0x046d, 0xc077)))

    // ---- item 4: registration must not be exported

    @Test fun receiverFlag_isNotExported_onApi33Plus() {
        val flag = UsbDetachReceiver.receiverExportFlag(33)
        assertTrue(flag == android.content.Context.RECEIVER_NOT_EXPORTED)
        assertFalse(flag == android.content.Context.RECEIVER_EXPORTED)
        assertTrue(UsbDetachReceiver.receiverExportFlag(36) == android.content.Context.RECEIVER_NOT_EXPORTED)
    }

    @Test fun receiverFlag_isNull_beforeApi33_whereNoFlagExists() =
        assertTrue(UsbDetachReceiver.receiverExportFlag(32) == null)

    @Test fun register_neverUsesTheExportedFlag() {
        val src = java.io.File("src/main/java/dev/luksandroid/session/UsbDetachReceiver.kt").readText()
        assertTrue("register must go through receiverExportFlag", src.contains("receiverExportFlag(Build.VERSION.SDK_INT)"))
        assertFalse(
            "RECEIVER_EXPORTED must not appear in code",
            src.lines().filterNot { it.trim().startsWith("*") || it.trim().startsWith("//") }
                .any { it.contains("RECEIVER_EXPORTED") },
        )
    }
}
