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
import java.io.File

/** F9 C4: parsing what Rust hands over, and surviving everything it might hand over wrongly. */
class NativeDrainTest {

    @get:Rule val tmp = TemporaryFolder()
    private lateinit var log: EventLogger
    private var now = 1_000L

    private class FakeSource(var drain: () -> String, var ring: () -> String = { "{\"dropped\":0,\"events\":[]}" }) : NativeEventSource {
        var drainCalls = 0
        override fun drain(): String { drainCalls++; return drain.invoke() }
        override fun ringJson(): String = ring.invoke()
    }

    @Before
    fun setUp() {
        log = EventLogger(File(tmp.root, "logs"), clock = { now++ }, startThread = false)
        EventLog.installForTest(log)
    }

    @After
    fun tearDown() = EventLog.installForTest(null)

    private fun lines(): List<JSONObject> {
        log.pump()
        val f = File(tmp.root, "logs/events.0.jsonl")
        return if (f.exists()) f.readLines().map { JSONObject(it) } else emptyList()
    }

    private fun drainer(src: NativeEventSource) = NativeDrain(src, executor = { it.run() })

    @Test
    fun drainedEventsBecomeNativeLinesWithFlatFieldsCopied() {
        val src = FakeSource({
            """{"dropped":0,"events":[
              {"nseq":41,"t":1699999999000,"ev":"usb_timeout","ep":129,"ok":false,"stage":"bulk_in","lat_us":12.5},
              {"nseq":42,"t":1699999999001,"ev":"btrfs_commit","gen":77}
            ]}"""
        })
        drainer(src).drainNow()
        val l = lines()
        assertEquals(2, l.size)
        val a = l[0]
        assertEquals("native", a.getString("ev"))
        assertEquals("rs", a.getString("src"))
        assertEquals("usb_timeout", a.getString("kind"))
        assertEquals(41L, a.getLong("nseq"))
        assertEquals(1699999999000L, a.getLong("nt"))
        assertEquals(129L, a.getLong("ep"))
        assertEquals(false, a.getBoolean("ok"))
        assertEquals("bulk_in", a.getString("stage"))
        assertEquals(12.5, a.getDouble("lat_us"), 0.0)
        assertFalse(a.has("xfer"))
        assertEquals("btrfs_commit", l[1].getString("kind"))
        assertEquals(77L, l[1].getLong("gen"))
    }

    @Test
    fun droppedCountBecomesNativeDroppedLine() {
        drainer(FakeSource({ """{"dropped":9,"events":[]}""" })).drainNow()
        val l = lines()
        assertEquals(1, l.size)
        assertEquals("native_dropped", l[0].getString("ev"))
        assertEquals("rs", l[0].getString("src"))
        assertEquals(9L, l[0].getLong("n"))
    }

    @Test
    fun noDroppedLineWhenZero() {
        drainer(FakeSource({ """{"dropped":0,"events":[]}""" })).drainNow()
        assertEquals(0, lines().size)
    }

    @Test
    fun disallowedValuesAreDroppedAndCounted() {
        val src = FakeSource({
            """{"dropped":0,"events":[{"nseq":1,"ev":"btrfs_error","stage":"/Volumes/secret/tax-return.pdf",
              "name":"tax-return.pdf","path":"/x/y","nested":{"a":1},"arr":[1,2],"nul":null,"Bad-Key":1,"code":"seven",
              "errno":5,"ok":true,"good":"alloc_chunk"}]}"""
        })
        drainer(src).drainNow()
        val l = lines()
        val n = l.first { it.getString("ev") == "native" }
        val text = n.toString()
        assertFalse(text, text.contains("secret"))
        assertFalse(text, text.contains("tax-return"))
        for (dropped in listOf("stage", "name", "path", "nested", "arr", "nul", "Bad-Key", "code")) {
            assertFalse("kept $dropped", n.has(dropped))
        }
        assertEquals(5L, n.getLong("errno"))
        assertEquals(true, n.getBoolean("ok"))
        assertEquals("alloc_chunk", n.getString("good"))
        assertEquals("btrfs_error", n.getString("kind"))
        val counted = l.first { it.optString("msg") == "invalid_field_values" }
        // stage(path), name (name-like key even though "tax-return.pdf" fits the grammar), path, nested, arr,
        // nul, Bad-Key, code ("seven" in an Int field) = 8.
        assertEquals(8L, counted.getLong("n"))
    }

    @Test
    fun reservedNamesFromNativeCannotOverwriteTheLineEnvelope() {
        val src = FakeSource({ """{"dropped":0,"events":[{"nseq":1,"ev":"x","seq":999,"sid":"evil","src":"kt","msg":"m","lvl":"E"}]}""" })
        drainer(src).drainNow()
        val n = lines().first { it.getString("ev") == "native" }
        assertEquals("rs", n.getString("src"))
        assertEquals("-", n.getString("sid"))
        assertTrue(n.getLong("seq") != 999L)
        assertFalse(n.has("msg"))
        assertFalse(n.has("lvl"))
    }

    @Test
    fun malformedJsonIsLoggedAsNativeErrNotThrown() {
        val d = drainer(FakeSource({ "{not json" }))
        d.drainNow()
        val e = lines().single()
        assertEquals("native_err", e.getString("ev"))
        assertEquals("kt", e.getString("src"))
        assertEquals("drain", e.getString("op"))
        assertEquals("JSONException", e.getString("exc"))
        assertEquals("parse", e.getString("stage"))
    }

    @Test
    fun wrongShapeIsAlsoLoggedNotThrown() {
        drainer(FakeSource({ """{"dropped":0,"events":[1,2]}""" })).drainNow()
        val e = lines().single()
        assertEquals("native_err", e.getString("ev"))
        assertEquals("JSONException", e.getString("exc"))
    }

    @Test
    fun unsatisfiedLinkErrorIsLoggedOnceAndDrainsAreDisabled() {
        val src = FakeSource({ throw UnsatisfiedLinkError("no nativeDrainEvents in /data/app/secret-folder/lib.so") })
        val d = drainer(src)
        repeat(5) { d.drainNow() }
        d.dumpRing(xfer = 3)
        d.onTransferFailure(3)
        val l = lines()
        assertEquals(1, l.size)
        assertEquals("native_err", l[0].getString("ev"))
        assertEquals("UnsatisfiedLinkError", l[0].getString("exc"))
        assertFalse(l[0].toString().contains("secret"))
        assertEquals(1, src.drainCalls)
    }

    @Test
    fun repeatedParseFailuresEventuallyDisableTheDrain() {
        val src = FakeSource({ "garbage" })
        val d = drainer(src)
        repeat(30) { d.drainNow() }
        val errs = lines().filter { it.getString("ev") == "native_err" }
        assertTrue(errs.size in 2..11)
        assertEquals("disabled", errs.last().getString("stage"))
        assertTrue(src.drainCalls <= 11)
    }

    @Test
    fun transferFailureDrainsThenDumpsRingTaggedWithTheTransfer() {
        val src = FakeSource(
            drain = { """{"dropped":0,"events":[{"nseq":10,"ev":"usb_reset"}]}""" },
            ring = { """{"dropped":0,"events":[{"nseq":8,"ev":"usb_submit","ep":2},{"nseq":9,"ev":"scsi_command","op":"write10"}]}""" },
        )
        drainer(src).onTransferFailure(xfer = 12)
        val l = lines()
        assertEquals(listOf("native", "native_ring", "native_ring"), l.map { it.getString("ev") })
        assertFalse(l[0].has("xfer") && l[0].getLong("xfer") == 99L)
        assertEquals(12L, l[1].getLong("xfer"))
        assertEquals(12L, l[2].getLong("xfer"))
        assertEquals("usb_submit", l[1].getString("kind"))
        assertEquals("write10", l[2].getString("op"))
        assertEquals(listOf(8L, 9L), listOf(l[1].getLong("nseq"), l[2].getLong("nseq")))
    }

    @Test
    fun sidCapturedAtRequestTimeSurvivesLaterSessionEnd() {
        val sid = EventLog.newSession()
        val d = drainer(FakeSource({ """{"dropped":0,"events":[{"nseq":1,"ev":"usb_reset"}]}""" }))
        EventLog.endSession()
        d.drainNow(sid = sid)
        assertEquals(sid, lines().single().getString("sid"))
    }

    @Test
    fun theRealJniBindingMissingFromAnOldSoIsLoggedOnceNotACrash() {
        // On the JVM the library is not loaded at all, so the real `external fun`s throw UnsatisfiedLinkError exactly as an .so without F9 would.
        val d = NativeDrain(LuksNativeEventSource, executor = { it.run() })
        d.requestDrain()
        d.requestDrain()
        d.onTransferFailure(4)
        val l = lines()
        assertEquals(1, l.size)
        assertEquals("native_err", l[0].getString("ev"))
        assertEquals("UnsatisfiedLinkError", l[0].getString("exc"))
        assertEquals("no_native_symbol", l[0].getString("stage"))
        assertTrue(d.disabled)
    }

    @Test
    fun requestDrainRunsThroughTheExecutorNotTheCallerThread() {
        val queued = mutableListOf<Runnable>()
        val src = FakeSource({ """{"dropped":0,"events":[]}""" })
        val d = NativeDrain(src, executor = { queued += it })
        d.requestDrain()
        assertEquals(0, src.drainCalls)
        queued.single().run()
        assertEquals(1, src.drainCalls)
    }
}
