package dev.luksandroid.session

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbManager
import android.os.Build
import dev.luksandroid.Trace

/**
 * BroadcastReceiver listening for system USB disconnect events.
 *
 * When the drive is detached, immediately transitions [LuksSession] to
 * [SessionState.Detached], tearing down volume handles and master key in memory.
 */
class UsbDetachReceiver(
    private val session: LuksSession = LuksSession,
) : BroadcastReceiver() {

    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action == UsbManager.ACTION_USB_DEVICE_DETACHED) {
            val detachedDev = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                intent.getParcelableExtra(UsbManager.EXTRA_DEVICE, UsbDevice::class.java)
            } else {
                @Suppress("DEPRECATION")
                intent.getParcelableExtra(UsbManager.EXTRA_DEVICE)
            }
            if (!isOurDetach(session.activeUsbDevice?.identity(), detachedDev?.identity())) {
                Trace.i("UsbDetachReceiver", "Ignoring detach: not the active session's device")
                return
            }
            Trace.i("UsbDetachReceiver", "Active USB device detached")
            session.notifyDeviceDetached()
        }
    }

    companion object {
        /** Identity of a USB device as far as detach matching is concerned. */
        data class UsbIdentity(val deviceId: Int, val vendorId: Int, val productId: Int)

        fun UsbDevice.identity() = UsbIdentity(deviceId, vendorId, productId)

        /**
         * Whether a detach broadcast is for the device the session has open (C7).
         *
         * Strict on purpose: no open device means nothing to tear down, a broadcast without
         * its device extra cannot be attributed, and a matching vendor+product pair is not
         * enough because two identical sticks share it. The usbfs-derived device id has to
         * agree as well; the vendor/product check additionally rejects a recycled id.
         */
        fun isOurDetach(active: UsbIdentity?, detached: UsbIdentity?): Boolean =
            active != null && detached != null &&
                active.deviceId == detached.deviceId &&
                active.vendorId == detached.vendorId &&
                active.productId == detached.productId

        /**
         * The export flag for registration, or null where the API has none (before 33).
         * Not exported: the detach broadcast comes from the system, which is still delivered, and
         * other apps cannot send this receiver a spoofed detach. Never RECEIVER_EXPORTED.
         */
        fun receiverExportFlag(sdkInt: Int): Int? =
            if (sdkInt >= Build.VERSION_CODES.TIRAMISU) Context.RECEIVER_NOT_EXPORTED else null

        @Volatile
        private var registeredReceiver: UsbDetachReceiver? = null

        fun register(context: Context, session: LuksSession = LuksSession): UsbDetachReceiver {
            val receiver = UsbDetachReceiver(session)
            val filter = IntentFilter(UsbManager.ACTION_USB_DEVICE_DETACHED)
            val flag = receiverExportFlag(Build.VERSION.SDK_INT)
            if (flag != null) {
                context.registerReceiver(receiver, filter, flag)
            } else {
                @Suppress("UnspecifiedRegisterReceiverFlag")
                context.registerReceiver(receiver, filter)
            }
            registeredReceiver = receiver
            return receiver
        }

        fun unregister(context: Context) {
            registeredReceiver?.let {
                runCatching { context.unregisterReceiver(it) }
                registeredReceiver = null
            }
        }
    }
}
