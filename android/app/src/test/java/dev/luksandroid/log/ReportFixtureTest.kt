package dev.luksandroid.log

import dev.luksandroid.LuksException
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

/**
 * The fixture for tools/log-report.py is real [EventLogger] output, not hand-written, so the report
 * tool is tested against the format Kotlin actually writes.
 *
 * Normal runs only COMPARE: the checked-in files must equal what [generate] produces now, so a format
 * change that is not regenerated fails here. Regenerate with
 * `./gradlew :app:testDebugUnitTest --tests '*ReportFixtureTest*' -PregenFixture=1`.
 *
 * Two defects are injected on purpose, after generation, for the tool's resilience paths: one line
 * removed from the middle of a process (a `seq` gap) and a malformed line appended to the newest file.
 */
class ReportFixtureTest {
    @get:Rule val tmp = TemporaryFolder()

    @After fun tearDown() = EventLog.installForTest(null)

    private fun fixtureDir(): File {
        var dir = File(System.getProperty("user.dir")!!).absoluteFile
        repeat(6) {
            val c = File(dir, "tools/testdata/logs")
            if (File(dir, "tools").isDirectory && File(dir, "android").isDirectory) return c
            dir = dir.parentFile ?: dir
        }
        error("repo root not found")
    }

    private fun clockFrom(start: Long): () -> Long { var t = start; return { t += 7; t } }

    private fun native(ev: Ev, json: String, xfer: Long? = null) = EventLog.nativeEvent(ev, JSONObject(json), xfer)

    private fun header(sid: String) = EventLog.ev(Ev.SESSION_HEADER) {
        app("0.4.0"); git("d60ee82"); model("pixel_7"); sdk(34); vid(1921); pid(21889)
        devBytes(500107862016L); sector(512); flush(true); maxXfer(1048576); fs("btrfs")
        cipher("aes-xts-plain64"); keyBits(512); write(true)
    }

    /** Process 1 (oldest): a wrong passphrase, then an unlock, a clean scan and one good import. */
    private fun processOne(dir: File): String {
        val log = EventLogger(dir, clock = clockFrom(1_790_000_060_000L), startThread = false)
        EventLog.installForTest(log)
        EventLog.ev(Ev.PROCESS_START) { n(4222) }
        EventLog.setSidForTest("11111111")
        EventLog.ev(Ev.UNLOCK_FAIL) { stage("unlock"); exc(LuksException("x", LuksException.WRONG_PASSWORD)) }
        EventLog.endSession()
        EventLog.setSidForTest("e5f60718"); header("e5f60718")
        EventLog.ev(Ev.UNLOCK) { ms(3900) }
        EventLog.ev(Ev.HEALTH) { verdict("leaks"); reason("write_safe"); ms(120); n(1) }
        EventLog.ev(Ev.HEALTH_FINDING) { kind("leaked_extent"); n(3) }
        EventLog.ev(Ev.XFER_QUEUED) { xfer(1); op("import"); n(1); bytes(52_428_800) }
        EventLog.ev(Ev.XFER_START) { xfer(1) }
        EventLog.ev(Ev.XFER_PHASE) { xfer(1); stage("begin"); ms(4) }
        EventLog.ev(Ev.XFER_PHASE) { xfer(1); stage("write"); ms(4_100); bytes(52_428_800) }
        EventLog.ev(Ev.XFER_PHASE) { xfer(1); stage("finish"); state("enter") }
        EventLog.ev(Ev.XFER_PHASE) { xfer(1); stage("finish"); ms(31); bytes(52_428_800); ino(257) }
        EventLog.ev(Ev.XFER_PHASE) { xfer(1); stage("commit"); state("enter") }
        EventLog.ev(Ev.XFER_PHASE) { xfer(1); stage("commit"); ms(88) }
        native(Ev.NATIVE, """{"nseq":10,"t":1790000064000,"ev":"ext4_finish_file","ino":257,"bytes":52428800}""")
        EventLog.ev(Ev.XFER_END) { xfer(1); state("completed"); ms(4_230); bytes(52_428_800); n(1) }
        EventLog.ev(Ev.LOCK) { reason("user") }
        EventLog.endSession()
        log.pump()
        return File(dir, "events.0.jsonl").readText()
    }

    /** Process 2: a restart, a failed export whose cable dies, the ring dump, a detach, a dropped line. */
    private fun processTwo(dir: File): List<String> {
        val log = EventLogger(dir, clock = clockFrom(1_790_003_660_000L), startThread = false)
        EventLog.installForTest(log)
        EventLog.ev(Ev.PROCESS_START) { n(4301) }
        EventLog.setSidForTest("0a1b2c3d"); header("0a1b2c3d")
        EventLog.ev(Ev.UNLOCK) { ms(4012) }
        EventLog.ev(Ev.HEALTH) { verdict("clean"); ms(95); n(0) }
        EventLog.ev(Ev.XFER_QUEUED) { xfer(2); op("export"); n(1); bytes(734_003_200) }
        EventLog.ev(Ev.XFER_START) { xfer(2) }
        EventLog.ev(Ev.XFER_PHASE) { xfer(2); stage("begin"); ms(2) }
        native(Ev.NATIVE, """{"nseq":101,"t":1790003665000,"ev":"usb_timeout","active_urbs":1,"elapsed_ms":5000,"ep":129,"gen":4}""")
        native(Ev.NATIVE, """{"nseq":102,"t":1790003665001,"ev":"scsi_result","opcode":40,"sense_key":3,"status":"failed","transferred":0}""")
        native(Ev.NATIVE, """{"nseq":103,"t":1790003665002,"ev":"usb_reset","reset_kind":"clear_halt_in","status":-19}""")
        native(Ev.NATIVE, """{"nseq":104,"t":1790003665003,"ev":"usb_state_transition","from":"healthy","to":"dead"}""")
        EventLog.nativeErr("export", LuksException("x", LuksException.TRANSPORT), xfer = 2, stage = "read")
        EventLog.ev(Ev.XFER_END) { xfer(2); state("failed"); code(LuksException.TRANSPORT); stage("read"); exc("LuksException"); ms(5_210); bytes(104_857_600) }
        native(Ev.NATIVE_RING, """{"nseq":99,"t":1790003664990,"ev":"usb_submit","ep":129,"gen":4,"bytes":1048576,"count":1}""", xfer = 2)
        native(Ev.NATIVE_RING, """{"nseq":100,"t":1790003664995,"ev":"scsi_command","opcode":40,"direction":"in","lba":204800,"blocks":2048,"data_len":1048576,"tag":9}""", xfer = 2)
        native(Ev.NATIVE_RING, """{"nseq":101,"t":1790003665000,"ev":"usb_timeout","active_urbs":1,"elapsed_ms":5000,"ep":129,"gen":4}""", xfer = 2)
        EventLog.ev(Ev.DETACH) { reason("usb_detach") }
        log.pump()
        // Lines lost to a full queue: the writer reports the exact count ahead of the next line.
        EventLog.noteDropped(5)
        EventLog.trace(isError = true, msg = "luks_err: [tid=12] code=7 op=export")
        EventLog.endSession()
        log.pump()
        return File(dir, "events.0.jsonl").readLines()
    }

    /** events.2 (oldest), events.1, events.0, with the two injected defects. */
    fun generate(): Map<String, String> {
        val one = processOne(File(tmp.root, "p1"))
        val two = processTwo(File(tmp.root, "p2")).toMutableList()
        val split = two.indexOfFirst { it.contains("\"ev\":\"native_ring\"") }
        // Injected defect 1: a seq gap (the scsi_result native line "was lost").
        val gap = two.indexOfFirst { it.contains("\"kind\":\"scsi_result\"") }
        two.removeAt(gap)
        val cut = split - 1
        val newest = two.drop(cut)
        // Injected defect 2: a malformed line at the end of the newest file.
        val newestText = newest.joinToString("\n", postfix = "\n") + "{\"t\":1790003670000,\"seq\":99,\"sid\":\"-\",\"src\":\"kt\",\"ev\":\"xfer_end\",\"xfe\n"
        return mapOf(
            "events.2.jsonl" to one,
            "events.1.jsonl" to two.take(cut).joinToString("\n", postfix = "\n"),
            "events.0.jsonl" to newestText,
        )
    }

    @Test
    fun theCheckedInReportFixtureIsWhatTheGeneratorProduces() {
        val gen = generate()
        val dir = fixtureDir()
        if (System.getProperty("regenFixture") == "1") {
            dir.mkdirs()
            for ((name, text) in gen) File(dir, name).writeText(text)
        }
        for ((name, text) in gen) {
            val f = File(dir, name)
            assertTrue("${f.path} is missing: regenerate with -PregenFixture=1", f.exists())
            assertEquals("$name is stale: regenerate with -PregenFixture=1", text, f.readText())
        }
    }

    @Test
    fun theFixtureCoversWhatTheReportToolMustHandle() {
        val all = generate().values.joinToString("\n")
        val lines = all.lines().filter { it.isNotBlank() }
        val parsed = lines.mapNotNull { runCatching { JSONObject(it) }.getOrNull() }
        assertEquals("exactly one malformed line", 1, lines.size - parsed.size)
        assertEquals("two process starts", 2, parsed.count { it.getString("ev") == "process_start" })
        assertTrue(parsed.any { it.getString("ev") == "xfer_end" && it.getString("state") == "completed" })
        assertTrue(parsed.any { it.getString("ev") == "xfer_end" && it.getString("state") == "failed" })
        assertTrue(parsed.any { it.getString("ev") == "native_ring" && it.has("xfer") })
        assertTrue(parsed.any { it.optString("kind") == "usb_timeout" })
        assertTrue(parsed.any { it.optString("kind") == "usb_state_transition" && it.optString("to") == "dead" })
        assertTrue(parsed.any { it.getString("ev") == "dropped" && it.getLong("n") == 5L })
        // A seq gap inside the second process.
        val p2 = parsed.dropWhile { it.getString("ev") != "process_start" || it.getLong("n") != 4301L }
        val seqs = p2.map { it.getLong("seq") }
        assertTrue("a gap in $seqs", seqs.zipWithNext().any { (a, b) -> b - a > 1 })
    }
}
