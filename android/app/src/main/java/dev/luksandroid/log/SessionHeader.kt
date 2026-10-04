package dev.luksandroid.log

import dev.luksandroid.BuildConfig
import dev.luksandroid.DeviceInfo
import dev.luksandroid.VolumeInfo

/**
 * The `session_header` line (C3): what a reader needs to know about the drive and the build
 * before reading anything else in the session. Facts only: no serial number, label, UUID or
 * key material, and fields the native side did not report are left out rather than guessed.
 */
object SessionHeader {
    fun emit(device: DeviceInfo, volume: VolumeInfo, canWrite: Boolean, vendorId: Int?, productId: Int?) {
        EventLog.ev(Ev.SESSION_HEADER) {
            app(BuildConfig.VERSION_NAME)
            git(BuildConfig.GIT_COMMIT)
            model(android.os.Build.MODEL ?: "unknown")
            sdk(android.os.Build.VERSION.SDK_INT)
            if (vendorId != null) vid(vendorId)
            if (productId != null) pid(productId)
            devBytes(device.sizeBytes)
            sector(device.blockSize)
            volume.flushSupported?.let { flush(it) }
            device.maxTransfer?.let { maxXfer(it) }
            fs(volume.fsType.ifEmpty { "unknown" })
            volume.cipher?.let { cipher(it) }
            volume.keyBits?.let { keyBits(it) }
            write(canWrite)
        }
    }
}
