package dev.luksandroid.log

import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.BufferedOutputStream
import java.io.File
import java.io.IOException
import java.io.OutputStream

/** F9 C1/C5: the on-disk format, the writer's accounting, rotation and the typed-API privacy guarantees. */
class EventLogTest {

    @get:Rule val tmp = TemporaryFolder()

    private var now = 1_700_000_000_000L
    private lateinit var dir: File
    private lateinit var log: EventLogger

    @Before
    fun setUp() {
        dir = File(tmp.root, "logs")
        log = EventLogger(dir, clock = { now++ }, startThread = false)
        EventLog.installForTest(log)
    }

    @After
    fun tearDown() {
        EventLog.installForTest(null)
    }

    private fun lines(): List<String> =
        (EventLogger.MAX_FILES - 1 downTo 0)
            .map { File(dir, "events.$it.jsonl") }
            .filter { it.exists() }
            .flatMap { it.readLines() }

    private fun parsed(): List<JSONObject> = lines().map { JSONObject(it) }

    @Test
    fun everyLineParsesWithRequiredFieldsAndFakeClock() {
        EventLog.ev(Ev.PROCESS_START) { n(1234) }
        EventLog.ev(Ev.XFER_START) { xfer(7) }
        log.pump()
        val all = parsed()
        assertEquals(2, all.size)
        for (o in all) {
            assertTrue(o.get("t") is Number)
            assertTrue(o.get("seq") is Number)
            assertTrue(o.get("sid") is String)
            assertEquals("kt", o.getString("src"))
            assertTrue(o.getString("ev").matches(Regex("[a-z_]+")))
        }
        assertEquals("process_start", all[0].getString("ev"))
        assertEquals(1234L, all[0].getLong("n"))
        assertEquals(7L, all[1].getLong("xfer"))
        assertEquals(1_700_000_000_000L, all[0].getLong("t"))
        assertEquals("-", all[0].getString("sid"))
    }

    @Test
    fun allowedFieldSetIsPinned() {
        assertEquals(
            setOf(
                "t", "seq", "sid", "src", "ev",
                "xfer", "ino", "parent", "bytes", "ms", "lba", "blocks", "n", "code", "errno",
                "stage", "exc", "state", "verdict", "reason", "kind", "op", "nseq", "lvl", "msg", "frames",
                "app", "git", "model", "sdk", "vid", "pid", "dev_bytes", "sector", "flush", "max_xfer",
                "fs", "cipher", "key_bits", "write",
            ),
            EventLog.ALLOWED_FIELDS,
        )
        EventLog.ev(Ev.SESSION_HEADER) {
            xfer(1); ino(2); parent(3); bytes(4); ms(5); lba(6); blocks(7); n(8); code(9); errno(10)
            stage("begin"); exc(IllegalStateException("/secret/path")); state("failed"); verdict("clean")
            reason("usb_detach"); kind("orphan_inode"); op("import"); nseq(11); lvl(error = false)
            app("0.4.0"); git("abc1234"); model("Pixel 8 Pro"); sdk(35); vid(0x1234); pid(0x5678)
            devBytes(1L shl 30); sector(512); flush(true); maxXfer(1 shl 20); fs("btrfs")
            cipher("aes-xts-plain64"); keyBits(512); write(true); frames(RuntimeException("x"))
        }
        EventLog.trace(isError = true, msg = "hello")
        log.pump()
        for (o in parsed()) {
            for (k in o.keys()) assertTrue("field $k not allowed", k in EventLog.ALLOWED_FIELDS)
        }
        val header = parsed().first()
        assertEquals("pixel_8_pro", header.getString("model"))
        assertEquals(0L, EventLog.invalidCount())
    }

    @Test
    fun invalidStringValuesBecomeInvalidAndAreCountedAndSurfaced() {
        EventLog.ev(Ev.XFER_END) {
            stage("/Volumes/secret/tax-return.pdf")
            state("UPPER")
            reason("has space")
            op("x".repeat(49))
            kind("")
        }
        log.pump()
        val o = parsed().first { it.getString("ev") == "xfer_end" }
        for (k in listOf("stage", "state", "reason", "op", "kind")) assertEquals("invalid", o.getString(k))
        assertFalse(lines().joinToString("\n").contains("secret"))
        assertFalse(lines().joinToString("\n").contains("tax-return"))
        // The count is not silent: a later line reports it.
        val surfaced = parsed().first { it.getString("ev") == "trace" && it.getString("msg") == "invalid_field_values" }
        assertEquals(5L, surfaced.getLong("n"))
        assertEquals("E", surfaced.getString("lvl"))
    }

    @Test
    fun seqIsStrictlyPlusOnePerWrittenLine() {
        repeat(300) { EventLog.ev(Ev.XFER_START) { xfer(it.toLong()) } }
        log.pump()
        repeat(10) { EventLog.ev(Ev.XFER_START) { xfer(it.toLong()) } }
        log.pump()
        val seqs = parsed().map { it.getLong("seq") }
        assertEquals(310, seqs.size)
        assertEquals((1L..310L).toList(), seqs)
    }

    @Test
    fun queueOverflowYieldsDroppedLineWithExactCount() {
        val extra = 37
        repeat(EventLogger.QUEUE_CAPACITY + extra) { EventLog.ev(Ev.XFER_START) { xfer(it.toLong()) } }
        log.pump()
        val all = parsed()
        assertEquals(1 + EventLogger.QUEUE_CAPACITY, all.size)
        assertEquals("dropped", all[0].getString("ev"))
        assertEquals(extra.toLong(), all[0].getLong("n"))
        // The kept lines are the first 4096, in order.
        assertEquals(0L, all[1].getLong("xfer"))
        assertEquals((EventLogger.QUEUE_CAPACITY - 1).toLong(), all.last().getLong("xfer"))
        assertEquals((1L..all.size.toLong()).toList(), all.map { it.getLong("seq") })
        // A second drain reports nothing stale.
        repeat(3) { EventLog.ev(Ev.XFER_START) { xfer(1) } }
        log.pump()
        assertEquals(1, parsed().count { it.getString("ev") == "dropped" })
    }

    @Test
    fun rotationKeepsFiveFilesEachBoundedByFourMiBPlusOneLine() {
        val big = EventLogger(File(tmp.root, "big"), clock = { now++ }, startThread = false)
        EventLog.installForTest(big)
        val payload = "x".repeat(300)
        var written = 0L
        var maxLine = 0
        // ~26 MiB, more than 5 x 4 MiB, so the oldest must be shed.
        while (written < 26L * 1024 * 1024) {
            repeat(2000) { EventLog.trace(isError = false, msg = payload) }
            big.pump()
            written += 2000L * 350
        }
        val files = File(tmp.root, "big").listFiles()!!.filter { it.name.matches(Regex("events\\.\\d\\.jsonl")) }
        assertEquals(5, files.size)
        assertEquals(setOf("events.0.jsonl", "events.1.jsonl", "events.2.jsonl", "events.3.jsonl", "events.4.jsonl"), files.map { it.name }.toSet())
        for (f in files) {
            f.forEachLine { maxLine = maxOf(maxLine, it.length + 1) }
            assertTrue("${f.name} is ${f.length()}", f.length() <= 4L * 1024 * 1024 + maxLine)
        }
        assertTrue("older files are full", File(tmp.root, "big/events.1.jsonl").length() >= 4L * 1024 * 1024)
        assertEquals(0, File(tmp.root, "big").listFiles()!!.count { !it.name.startsWith("events.") })
    }

    @Test
    fun rotationShiftsFilesOldestDropped() {
        val small = EventLogger(File(tmp.root, "small"), clock = { now++ }, maxFileBytes = 500, startThread = false)
        EventLog.installForTest(small)
        repeat(200) { EventLog.ev(Ev.XFER_START) { xfer(it.toLong()) } }
        small.pump()
        val d = File(tmp.root, "small")
        assertEquals(5, d.listFiles()!!.size)
        val newestFirstSeq = (0..4).map { i -> File(d, "events.$i.jsonl").readLines().map { JSONObject(it).getLong("seq") } }
        // events.0 holds the newest lines, events.4 the oldest kept; sequences are contiguous across files.
        for (i in 0..3) assertEquals(newestFirstSeq[i + 1].last() + 1, newestFirstSeq[i].first())
        assertEquals(200L, newestFirstSeq[0].last())
    }

    @Test
    fun errorEventsAreFlushedImmediatelyAndBatchesAreFlushed() {
        val events = mutableListOf<String>()
        val flushLog = EventLogger(
            File(tmp.root, "flush"),
            clock = { now++ },
            startThread = false,
            openStream = { f ->
                object : BufferedOutputStream(java.io.FileOutputStream(f, true)) {
                    override fun write(b: ByteArray, off: Int, len: Int) {
                        events += "write"; super.write(b, off, len)
                    }
                    override fun flush() {
                        events += "flush"; super.flush()
                    }
                }
            },
        )
        EventLog.installForTest(flushLog)
        EventLog.ev(Ev.XFER_START) { xfer(1) }
        EventLog.ev(Ev.NATIVE_ERR) { op("import"); code(7) }
        EventLog.ev(Ev.XFER_START) { xfer(2) }
        flushLog.pump()
        // write, write, FLUSH (after the error line), write, FLUSH (end of batch)
        assertEquals(listOf("write", "write", "flush", "write", "flush"), events)
    }

    @Test
    fun ioFailureIsCountedAndSurfacedAsALaterLineWithASeqGap() {
        var failNext = true
        val f = EventLogger(
            File(tmp.root, "io"),
            clock = { now++ },
            startThread = false,
            openStream = { file ->
                if (failNext) {
                    failNext = false
                    throw IOException("disk full /secret/path")
                }
                BufferedOutputStream(java.io.FileOutputStream(file, true))
            },
        )
        EventLog.installForTest(f)
        EventLog.ev(Ev.XFER_START) { xfer(1) }
        f.pump()
        EventLog.ev(Ev.XFER_START) { xfer(2) }
        f.pump()
        val text = File(tmp.root, "io/events.0.jsonl").readLines().map { JSONObject(it) }
        val fail = text.first { it.optString("msg") == "log_io_failed" }
        assertEquals(1L, fail.getLong("n"))
        assertEquals("IOException", fail.getString("exc"))
        assertFalse(File(tmp.root, "io/events.0.jsonl").readText().contains("secret"))
        // The lost line consumed seq 1, so the first surviving line is not 1.
        assertTrue(text.first().getLong("seq") > 1)
    }

    @Test
    fun writerThreadDeliversInOrderWithoutBlockingCallers() {
        val threaded = EventLogger(File(tmp.root, "thr"), clock = { now++ }, startThread = true)
        EventLog.installForTest(threaded)
        repeat(500) { EventLog.ev(Ev.XFER_START) { xfer(it.toLong()) } }
        assertTrue(threaded.flushBlocking(5_000))
        val got = File(tmp.root, "thr/events.0.jsonl").readLines().map { JSONObject(it) }
        assertEquals(500, got.size)
        assertEquals((0L until 500L).toList(), got.map { it.getLong("xfer") })
        threaded.shutdown()
    }

    @Test
    fun disabledWhenNoLoggerInstalled() {
        EventLog.installForTest(null)
        EventLog.ev(Ev.XFER_START) { xfer(1) }
        EventLog.trace(isError = false, msg = "x")
        log.pump()
        assertEquals(0, lines().size)
    }

    @Test
    fun traceMessagesAreEscapedAndBounded() {
        EventLog.trace(isError = false, msg = "a\"b\\c\nd\u0001" + "z".repeat(2000))
        log.pump()
        val o = parsed().single()
        assertEquals("trace", o.getString("ev"))
        assertEquals("I", o.getString("lvl"))
        assertTrue(o.getString("msg").startsWith("a\"b\\c\nd\u0001"))
        assertTrue(o.getString("msg").length <= 512)
        assertEquals(1, lines().size)
    }

    @Test
    fun clearRemovesAllFilesAndWritingResumes() {
        EventLog.ev(Ev.XFER_START) { xfer(1) }
        log.pump()
        assertTrue(log.clearBlocking(1_000))
        assertEquals(0, lines().size)
        EventLog.ev(Ev.XFER_START) { xfer(2) }
        log.pump()
        assertEquals(1, lines().size)
    }

    @Test
    fun aKeySetTwiceIsWrittenOnceSoTheLineStaysValidJson() {
        EventLog.ev(Ev.XFER_END) { code(1); exc(dev.luksandroid.LuksException("m", 7)); code(9) }
        log.pump()
        val o = parsed().single()
        assertEquals(9, o.getInt("code"))
    }

    @Test
    fun manyThreadsNeverBlockLoseSilentlyOrBreakSeq() {
        val threads = (1..8).map { t ->
            Thread { repeat(1000) { i -> EventLog.ev(Ev.XFER_START) { xfer((t * 10_000 + i).toLong()) } } }
        }
        threads.forEach { it.start() }
        threads.forEach { it.join() }
        log.pump()
        val all = parsed()
        val kept = all.count { it.getString("ev") == "xfer_start" }
        val dropped = all.filter { it.getString("ev") == "dropped" }.sumOf { it.getLong("n") }
        assertEquals("every call is either written or counted", 8000L, kept + dropped)
        assertEquals((1L..all.size.toLong()).toList(), all.map { it.getLong("seq") })
    }

    @Test
    fun traceCallsBecomeTraceLinesAndTheSinkStillSeesThem() {
        val sink = mutableListOf<String>()
        dev.luksandroid.Trace.testSink = { sink += it }
        try {
            dev.luksandroid.Trace.i("hello info")
            dev.luksandroid.Trace.e("TagX", "hello error")
            dev.luksandroid.Trace.err(7, "import", dev.luksandroid.Trace.ErrDetail.Bytes(5))
        } finally {
            dev.luksandroid.Trace.testSink = null
        }
        log.pump()
        val t = parsed().filter { it.getString("ev") == "trace" }
        assertEquals(3, t.size)
        assertEquals("I", t[0].getString("lvl"))
        assertTrue(t[0].getString("msg").endsWith("hello info"))
        assertEquals("E", t[1].getString("lvl"))
        assertTrue(t[1].getString("msg").startsWith("TagX: "))
        assertTrue(t[2].getString("msg").contains("code=7 op=import bytes=5"))
        assertEquals(3, sink.size)
        assertTrue(sink[0].endsWith("hello info"))
    }

    @Test
    fun sessionHeaderCarriesUsbIdsAndOptionalNativeFactsOnlyWhenKnown() {
        val device = dev.luksandroid.DeviceInfo("v", "p", 4096, 1L shl 40, "GPT", emptyList(), null, maxTransfer = 1 shl 20)
        val vol = dev.luksandroid.VolumeInfo("LABEL-SECRET", "UUID-SECRET", 4096, 1L shl 39, "btrfs", emptyList(), flushSupported = true, cipher = "aes-xts-plain64", keyBits = 512)
        SessionHeader.emit(device, vol, canWrite = true, vendorId = 0x0781, productId = 0x5583)
        SessionHeader.emit(dev.luksandroid.DeviceInfo("v", "p", 512, 10L, "GPT", emptyList(), null), dev.luksandroid.VolumeInfo("", "", 4096, 1L, "ext4", emptyList()), false, null, null)
        log.pump()
        val (full, bare) = parsed()
        assertEquals(0x0781, full.getInt("vid"))
        assertEquals(0x5583, full.getInt("pid"))
        assertEquals(4096, full.getInt("sector"))
        assertEquals(true, full.getBoolean("flush"))
        assertEquals(1 shl 20, full.getInt("max_xfer"))
        assertEquals("aes-xts-plain64", full.getString("cipher"))
        assertEquals(512, full.getInt("key_bits"))
        assertEquals("btrfs", full.getString("fs"))
        for (k in listOf("vid", "pid", "flush", "max_xfer", "cipher", "key_bits")) assertFalse("$k present when unknown", bare.has(k))
        assertFalse(full.toString().contains("SECRET"))
    }

    @Test
    fun optionalNativeFactsParseWhenPresentAndStayNullWhenAbsent() {
        val v = dev.luksandroid.parseVolumeInfo("""{"label":null,"uuid":"u","blockSize":4096,"sizeBytes":1,"fsType":"ext4","cipher":"aes-xts-plain64","keyBits":512}""")
        assertEquals("aes-xts-plain64", v.cipher)
        assertEquals(512, v.keyBits)
        val v2 = dev.luksandroid.parseVolumeInfo("""{"label":null,"uuid":"u","blockSize":4096,"sizeBytes":1,"fsType":"ext4"}""")
        assertEquals(null, v2.cipher)
        val d = dev.luksandroid.parseDeviceInfo("""{"vendor":"a","product":"b","blockSize":512,"sizeBytes":1,"tableKind":"GPT","partitions":[],"maxTransfer":131072}""")
        assertEquals(131072, d.maxTransfer)
    }

    @Test
    fun aRotationThatFailsHalfwayLosesNoFileAndTheNextWriteRetries() {
        val failures = java.util.concurrent.atomic.AtomicInteger(1)
        val triggered = java.util.concurrent.atomic.AtomicBoolean(false)
        var reported = false
        val dirR = File(tmp.root, "rot")
        val r = EventLogger(
            dirR, clock = { now++ }, maxFileBytes = 300, startThread = false,
            move = { from, to ->
                // Fail the second move of the first rotation attempt only.
                if (from.name == "events.2.jsonl" && failures.getAndDecrement() > 0) { triggered.set(true); throw IOException("rename refused") }
                java.nio.file.Files.move(from.toPath(), to.toPath(), java.nio.file.StandardCopyOption.REPLACE_EXISTING)
            },
        )
        EventLog.installForTest(r)
        fun live() = dirR.listFiles()!!.map { it.name }.toSet()
        var maxSeen = 0
        repeat(400) {
            EventLog.ev(Ev.XFER_START) { xfer(it.toLong()) }
            r.pump()
            maxSeen = maxOf(maxSeen, live().size)
            if (triggered.get() && !reported) {
                EventLog.ev(Ev.XFER_START) { xfer(-1) }
                r.pump()
                reported = (4 downTo 0).map { File(dirR, "events.$it.jsonl") }.filter { it.exists() }
                    .flatMap { it.readLines() }.any { it.contains("log_io_failed") }
            }
        }
        assertTrue("the failed rotation was reported", reported)
        val all = (4 downTo 0).map { File(dirR, "events.$it.jsonl") }.filter { it.exists() }.flatMap { it.readLines() }.map { JSONObject(it) }
        assertEquals(5, live().size)
        // The failure is reported, and apart from the one lost line no seq is missing inside the kept window.
        val seqs = all.map { it.getLong("seq") }
        assertEquals(seqs.sorted(), seqs)
        assertTrue("newest lines survived", seqs.last() >= 400)
        // The reporting lines themselves are written too; what matters is that rotation resumed and nothing is torn.
        assertTrue("only the single failed line may be missing", (seqs.last() - seqs.first() + 1) - seqs.size <= 1)
    }

    @Test
    fun readTailDuringRotationNeverTearsALineOrThrows() {
        val dirT = File(tmp.root, "tail")
        val t = EventLogger(dirT, maxFileBytes = 4_000, startThread = true)
        EventLog.installForTest(t)
        val stop = java.util.concurrent.atomic.AtomicBoolean(false)
        val producer = Thread { var i = 0L; while (!stop.get()) { EventLog.ev(Ev.XFER_START) { xfer(i++) }; if (i % 50 == 0L) Thread.sleep(1) } }
        producer.start()
        try {
            repeat(60) {
                val text = t.readTail(6_000)
                for (line in text.lineSequence().filter { it.isNotEmpty() }) JSONObject(line)
            }
        } finally {
            stop.set(true); producer.join(); t.shutdown()
        }
    }

    @Test
    fun tailReadsOnlyTheTailOfAHugeFile() {
        val huge = File(tmp.root, "huge").apply { mkdirs() }
        File(huge, "events.0.jsonl").bufferedWriter().use { w -> repeat(200_000) { w.write("""{"t":1,"seq":$it,"sid":"-","src":"kt","ev":"xfer_start","xfer":$it}""" + "\n") } }
        val t = EventLogger(huge, startThread = false)
        val tail = t.readTail(1_000)
        assertTrue(tail.length <= 1_000)
        assertTrue(tail.trimEnd().endsWith("\"xfer\":199999}"))
        assertFalse(tail.startsWith("{\"t\":1,\"seq\":0,"))
        for (line in tail.lineSequence().filter { it.isNotEmpty() }) JSONObject(line)
    }

    @Suppress("unused")
    private fun unused(o: OutputStream) = o
}
