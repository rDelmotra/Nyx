package dev.luksandroid.session

import dev.luksandroid.Entry
import dev.luksandroid.LuksDevice
import dev.luksandroid.LuksVolume
import dev.luksandroid.DeviceInfo
import dev.luksandroid.PartitionInfo
import dev.luksandroid.VolumeInfo
import java.util.Collections
import java.util.concurrent.CountDownLatch

/** Records every native-ish call in one shared, ordered list. */
class EventLog {
    val events: MutableList<String> = Collections.synchronizedList(mutableListOf())
    fun add(e: String) { events.add(e) }
    fun snapshot(): List<String> = synchronized(events) { events.toList() }
}

/** A volume whose commit/close are observable, blockable and failable. */
open class RecordingVolume(
    val log: EventLog,
    val name: String = "VOLUME",
) : LuksVolume(0L) {
    @Volatile var commitFailure: Throwable? = null

    /** Native close commits once more (F4); a persistent commit failure therefore fails the close too. */
    @Volatile var closeRetriesCommit: Boolean = true
    @Volatile var closeFailure: Throwable? = null
    /** Runs inside every commit, e.g. to observe what the caller holds at that moment. */
    @Volatile var onCommit: (() -> Unit)? = null
    @Volatile var commitGate: CountDownLatch? = null
    @Volatile var commitEntered: CountDownLatch? = null
    @Volatile var listFailure: Throwable? = null

    override val info = VolumeInfo(
        label = "t", uuid = "u", blockSize = 4096, sizeBytes = 1L shl 30,
        fsType = "btrfs", subvolumes = emptyList(), encrypted = true,
    )
    override val canWrite: Boolean get() = true

    override fun listDir(path: String): List<Entry> {
        listFailure?.let { throw it }
        return emptyList()
    }

    override fun commitActiveBatch() {
        log.add("${name}_COMMIT_START")
        onCommit?.invoke()
        commitEntered?.countDown()
        commitGate?.await()
        commitFailure?.let { log.add("${name}_COMMIT_FAILED"); throw it }
        log.add("${name}_COMMIT")
    }

    override fun close() {
        log.add("${name}_CLOSE")
        closeFailure?.let { throw it }
        if (closeRetriesCommit) commitFailure?.let { throw it }
    }
}

class RecordingNotifier : SessionNotifier {
    val notices: MutableList<SessionNotice> = Collections.synchronizedList(mutableListOf())
    override fun notify(notice: SessionNotice) { notices.add(notice) }
}

fun testPartition(offset: Long = 1048576L) = PartitionInfo(
    index = 1, name = "cryptdata", offsetBytes = offset, sizeBytes = 1L shl 30, isLuks = true, luksVersion = 2,
)

/** A device whose `unlock`/`openPlain` return whatever the test supplies. */
class FakeDevice(
    private val partition: PartitionInfo,
    val log: EventLog? = null,
    private val produce: () -> LuksVolume,
) : LuksDevice(0L) {
    override val info = DeviceInfo(
        vendor = "USB", product = "Drive", blockSize = 512, sizeBytes = 2L shl 30,
        tableKind = "GPT", partitions = listOf(partition), writeProbe = null,
    )

    override fun unlock(
        partitionOffset: Long,
        password: dev.luksandroid.security.SecurePassphraseBuffer,
    ): LuksVolume = produce()

    override fun openPlain(partitionOffset: Long): LuksVolume = produce()

    override fun close() { log?.add("DEVICE_CLOSE") }
}

fun fakeContext() = object : android.content.ContextWrapper(null) {
    override fun startForegroundService(service: android.content.Intent?): android.content.ComponentName? = null
    override fun stopService(name: android.content.Intent?): Boolean = true
}

fun passphrase(): dev.luksandroid.security.SecurePassphraseBuffer {
    val direct = java.nio.ByteBuffer.allocateDirect(16)
    direct.put("test".toByteArray())
    return dev.luksandroid.security.SecurePassphraseBuffer.of(direct, 4)
}
