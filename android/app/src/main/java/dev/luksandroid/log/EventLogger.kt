package dev.luksandroid.log

import java.io.File
import java.io.IOException
import java.io.OutputStream
import java.util.concurrent.ArrayBlockingQueue
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong

/** One typed field of an event. [value] is a Long, Int, Boolean, String or `List<String>` and nothing else. */
class Field(val name: String, val value: Any)

/** A queued, not yet serialized line. [t] and [sid] are captured on the caller's thread. */
internal class Entry(
    val t: Long,
    val sid: String,
    val src: String,
    val ev: String,
    val fields: List<Field>,
    val mirror: Boolean,
    val urgent: Boolean,
)

private class Control(val action: () -> Unit, val done: CountDownLatch)

/**
 * The F9 writer (C5): JSON Lines under [dir], rotated `events.0.jsonl` .. `events.{maxFiles-1}.jsonl`.
 *
 * * Callers only ever [offer] to a bounded queue; they never touch the disk and never wait.
 *   A full queue drops the new line and counts it; the writer reports the exact count in a
 *   `dropped{n}` line before the next batch.
 * * One writer thread (or, with `startThread = false`, the test's own [pump] calls) assigns
 *   `seq` at write time, so a gap in `seq` means a line that was assigned a number and lost.
 * * Flush: after every drained batch, and immediately after an urgent (error) line, so the
 *   events around a crash are in the file before the process dies.
 * * A disk failure never throws into the app and never vanishes: it is counted, the line is lost
 *   (leaving a `seq` gap), and the next write that succeeds is preceded by a
 *   `trace{lvl=E, msg=log_io_failed, n, exc}` line. That is the one place log I/O errors are
 *   swallowed, and it is accounted for.
 */
class EventLogger internal constructor(
    private val dir: File,
    private val clock: () -> Long = System::currentTimeMillis,
    private val maxFileBytes: Long = MAX_FILE_BYTES,
    private val maxFiles: Int = MAX_FILES,
    startThread: Boolean = true,
    private val mirror: ((isError: Boolean, line: String) -> Unit)? = null,
    private val sidProvider: () -> String = { "-" },
    private val openStream: (File) -> OutputStream = { f -> java.io.BufferedOutputStream(java.io.FileOutputStream(f, true), 64 * 1024) },
    queueCapacity: Int = QUEUE_CAPACITY,
    /** Seam for rotation failures; replaces the destination if it exists. */
    private val move: (File, File) -> Unit = { from, to ->
        java.nio.file.Files.move(from.toPath(), to.toPath(), java.nio.file.StandardCopyOption.REPLACE_EXISTING)
    },
) {
    companion object {
        const val QUEUE_CAPACITY = 4096
        const val MAX_FILES = 5
        const val MAX_FILE_BYTES = 4L * 1024 * 1024
        const val MAX_MSG_CHARS = 400
        private const val BATCH = 256
    }

    private val queue = ArrayBlockingQueue<Any>(queueCapacity)
    private val dropped = AtomicLong(0)
    internal val invalidPending = AtomicLong(0)
    internal val invalidTotal = AtomicLong(0)
    private val ioFailures = AtomicLong(0)
    @Volatile private var lastIoExc = "IOException"

    // Touched only by the writer (the thread, or whoever is inside pump()).
    @Volatile private var seq = 0L
    private var stream: OutputStream? = null
    private var curBytes = 0L
    private val pumpLock = Any()

    @Volatile private var running = true
    private val thread: Thread? = if (startThread) {
        Thread({ loop() }, "luks-eventlog").apply { isDaemon = true; priority = Thread.MIN_PRIORITY; start() }
    } else {
        null
    }

    init {
        // Directory creation is plain I/O but happens once, at init, never on a caller's hot path.
        dir.mkdirs()
    }

    internal fun now(): Long = clock()

    internal fun offer(e: Entry) {
        if (!queue.offer(e)) dropped.incrementAndGet()
    }

    /**
     * Backpressure for the native drain thread only (never a transfer or JNI thread): waits up to
     * [timeoutMs] for room. Returns false on timeout; the line is then counted as dropped.
     */
    internal fun offerBlocking(e: Entry, timeoutMs: Long): Boolean {
        val ok = try {
            queue.offer(e, timeoutMs, TimeUnit.MILLISECONDS)
        } catch (_: InterruptedException) {
            Thread.currentThread().interrupt()
            false
        }
        if (!ok) dropped.incrementAndGet()
        return ok
    }

    /** Lines the caller gave up on before offering them (counted in the next `dropped{n}`). */
    internal fun noteDropped(n: Long) {
        if (n > 0) dropped.addAndGet(n)
    }

    private fun loop() {
        while (running) {
            try {
                val first = queue.poll(500, TimeUnit.MILLISECONDS) ?: continue
                val batch = ArrayList<Any>(BATCH)
                batch.add(first)
                queue.drainTo(batch, BATCH - 1)
                synchronized(pumpLock) { process(batch) }
            } catch (_: InterruptedException) {
                return
            } catch (t: Throwable) {
                // A bug in the writer must not kill the thread; it is counted and reported like any I/O failure.
                lastIoExc = t.javaClass.simpleName.ifEmpty { "Throwable" }
                ioFailures.incrementAndGet()
            }
        }
    }

    /** Synchronously writes everything queued. For the test seam, and the only writer when no thread runs. */
    fun pump(): Int = synchronized(pumpLock) {
        var n = 0
        while (true) {
            val batch = ArrayList<Any>(BATCH)
            queue.drainTo(batch, BATCH)
            if (batch.isEmpty()) break
            n += process(batch)
        }
        n
    }

    private fun process(items: List<Any>): Int {
        var written = 0
        writeSynthetic()
        for (item in items) {
            when (item) {
                is Entry -> {
                    // A dropped count that accrued mid-batch is still reported before later lines.
                    writeLine(item)
                    written++
                }
                is Control -> {
                    flushStream()
                    try {
                        item.action()
                    } finally {
                        item.done.countDown()
                    }
                }
            }
        }
        flushStream()
        return written
    }

    /** `dropped`, invalid-value and log-I/O accounting lines, written ahead of the next real line. */
    private fun writeSynthetic() {
        val d = dropped.getAndSet(0)
        if (d > 0) writeLine(synthetic("dropped", listOf(Field("n", d)), error = false))
        val inv = invalidPending.getAndSet(0)
        if (inv > 0) {
            writeLine(synthetic("trace", listOf(Field("lvl", "E"), Field("msg", "invalid_field_values"), Field("n", inv)), error = true))
        }
        val io = ioFailures.getAndSet(0)
        if (io > 0) {
            writeLine(
                synthetic("trace", listOf(Field("lvl", "E"), Field("msg", "log_io_failed"), Field("n", io), Field("exc", lastIoExc)), error = true),
            )
        }
    }

    private fun synthetic(ev: String, fields: List<Field>, error: Boolean) =
        Entry(clock(), sidProvider(), "kt", ev, fields, mirror = true, urgent = error)

    private fun writeLine(e: Entry) {
        val n = ++seq
        val line = EventJson.render(n, e)
        val bytes = (line + "\n").toByteArray(Charsets.UTF_8)
        try {
            var s = stream ?: open()
            if (curBytes >= maxFileBytes) {
                rotate()
                s = stream ?: open()
            }
            s.write(bytes, 0, bytes.size)
            curBytes += bytes.size
            if (e.urgent) s.flush()
        } catch (t: Throwable) {
            ioFailures.incrementAndGet()
            lastIoExc = t.javaClass.simpleName.ifEmpty { "Throwable" }
            closeQuietly()
            return
        }
        if (e.mirror) mirror?.invoke(e.urgent, line)
    }

    private fun file(i: Int) = File(dir, "events.$i.jsonl")

    private fun open(): OutputStream {
        dir.mkdirs()
        val f = file(0)
        curBytes = if (f.exists()) f.length() else 0L
        return openStream(f).also { stream = it }
    }

    /**
     * Shifts `.N` to `.N+1` oldest first, each move replacing its destination, so no file is deleted
     * ahead of the move that supersedes it. A failure partway leaves every file still present (the
     * unmoved ones keep their names), is counted by the caller, and the next write retries.
     */
    private fun rotate() {
        closeStream()
        for (i in maxFiles - 2 downTo 0) {
            val from = file(i)
            if (from.exists()) move(from, file(i + 1))
        }
        curBytes = 0L
    }

    private fun closeStream() {
        val s = stream ?: return
        stream = null
        s.close()
    }

    /** After a failed write: drop the stream so the next line reopens, without letting close() mask the failure. */
    private fun closeQuietly() {
        try {
            closeStream()
        } catch (_: IOException) {
            // Already counted as the I/O failure that brought us here.
        }
    }

    private fun flushStream() {
        try {
            stream?.flush()
        } catch (t: Throwable) {
            ioFailures.incrementAndGet()
            lastIoExc = t.javaClass.simpleName.ifEmpty { "Throwable" }
            closeQuietly()
        }
    }

    private fun runControl(timeoutMs: Long, action: () -> Unit): Boolean {
        val c = Control(action, CountDownLatch(1))
        if (thread == null) {
            synchronized(pumpLock) {
                pump()
                flushStream()
                action()
            }
            return true
        }
        if (!queue.offer(c, timeoutMs, TimeUnit.MILLISECONDS)) return false
        return c.done.await(timeoutMs, TimeUnit.MILLISECONDS)
    }

    /** Waits until everything queued before this call is on disk. For the crash handler and file readers. */
    fun flushBlocking(timeoutMs: Long): Boolean = runControl(timeoutMs) { }

    /** Deletes every log file (writer-side, so it cannot race a rotation) and carries on writing. */
    fun clearBlocking(timeoutMs: Long): Boolean = runControl(timeoutMs) {
        closeQuietly()
        for (i in 0 until maxFiles) file(i).delete()
        curBytes = 0L
    }

    /**
     * The newest [maxBytes] of log text, oldest line first, cut at a line boundary. Reads only the tail of
     * each file (seek, not whole-file), and holds the writer lock so a rotation cannot move files underneath it.
     */
    fun readTail(maxBytes: Int): String {
        flushBlocking(2_000)
        val chunks = ArrayList<ByteArray>()
        var total = 0
        synchronized(pumpLock) {
            for (i in 0 until maxFiles) {
                val f = file(i)
                if (!f.exists()) continue
                java.io.RandomAccessFile(f, "r").use { raf ->
                    val take = minOf(raf.length(), (maxBytes - total).toLong()).toInt()
                    val buf = ByteArray(take)
                    raf.seek(raf.length() - take)
                    raf.readFully(buf)
                    chunks.add(0, buf)
                    total += take
                }
                if (total >= maxBytes) break
            }
        }
        val all = ByteArray(total)
        var p = 0
        for (c in chunks) { System.arraycopy(c, 0, all, p, c.size); p += c.size }
        val text = String(all, Charsets.UTF_8)
        // The oldest slice may start mid-line.
        return if (total >= maxBytes) text.substringAfter('\n', "") else text
    }

    fun files(): List<File> = (maxFiles - 1 downTo 0).map { file(it) }.filter { it.exists() }

    fun shutdown() {
        running = false
        thread?.interrupt()
        thread?.join(2_000)
        synchronized(pumpLock) {
            pump()
            closeQuietly()
        }
    }
}

/** Serialization kept apart so the format has one definition. */
internal object EventJson {
    fun render(seq: Long, e: Entry): String {
        val sb = StringBuilder(192)
        sb.append("{\"t\":").append(e.t)
        sb.append(",\"seq\":").append(seq)
        sb.append(",\"sid\":"); str(sb, e.sid)
        sb.append(",\"src\":"); str(sb, e.src)
        sb.append(",\"ev\":"); str(sb, e.ev)
        for (f in e.fields) {
            sb.append(',')
            str(sb, f.name)
            sb.append(':')
            when (val v = f.value) {
                is String -> str(sb, v)
                is List<*> -> {
                    sb.append('[')
                    v.forEachIndexed { i, s -> if (i > 0) sb.append(','); str(sb, s as String) }
                    sb.append(']')
                }
                else -> sb.append(v.toString())
            }
        }
        sb.append('}')
        return sb.toString()
    }

    private fun str(sb: StringBuilder, s: String) {
        sb.append('"')
        for (c in s) {
            when {
                c == '"' -> sb.append("\\\"")
                c == '\\' -> sb.append("\\\\")
                c == '\n' -> sb.append("\\n")
                c == '\r' -> sb.append("\\r")
                c == '\t' -> sb.append("\\t")
                c < ' ' -> sb.append("\\u").append(String.format("%04x", c.code))
                else -> sb.append(c)
            }
        }
        sb.append('"')
    }
}
