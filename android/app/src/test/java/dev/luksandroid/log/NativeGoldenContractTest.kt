package dev.luksandroid.log

import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Before
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import java.io.File

/**
 * F9 C7, Kotlin half of the cross-language contract. Rust renders one record of every native variant
 * into `src/test/resources/native-events-golden.json` (and checks it is current); this pushes that exact
 * file through the real [NativeDrain] and requires that nothing is dropped or rewritten except the two
 * documented renames. A missing file FAILS: a contract that is silently skipped is no contract.
 */
class NativeGoldenContractTest {
    @get:Rule val tmp = TemporaryFolder()
    private lateinit var log: EventLogger

    @Before fun setUp() {
        log = EventLogger(File(tmp.root, "logs"), startThread = false)
        EventLog.installForTest(log)
    }

    @After fun tearDown() = EventLog.installForTest(null)

    private fun golden(): String {
        val url = javaClass.getResource("/native-events-golden.json")
        assertNotNull(
            "native-events-golden.json is missing from src/test/resources: the Rust test " +
                "(UPDATE_GOLDEN=1) writes it; this test cannot pass without it",
            url,
        )
        return url!!.readText()
    }

    @Test
    fun everyGoldenNativeEventSurvivesTheKotlinSideIntact() {
        val text = golden()
        val root = JSONObject(text)
        val events = root.getJSONArray("events")
        assertTrue("golden has no events", events.length() > 0)

        val src = object : NativeEventSource {
            override fun drain() = text
            override fun ringJson() = text
        }
        val d = NativeDrain(src, executor = { it.run() })
        d.drainNow()
        d.dumpRing(xfer = 77)
        log.pump()
        val lines = File(tmp.root, "logs/events.0.jsonl").readLines().map { JSONObject(it) }

        assertEquals("fields were dropped or invalid", 0L, EventLog.invalidCount())
        assertTrue("a drain error was logged: ${lines.filter { it.getString("ev") == "native_err" }}", lines.none { it.getString("ev") == "native_err" })
        assertTrue("an invalid-field line was written", lines.none { it.optString("msg") == "invalid_field_values" })
        assertTrue("the drain gave up", !d.disabled)

        val native = lines.filter { it.getString("ev") == "native" }
        val ring = lines.filter { it.getString("ev") == "native_ring" }
        assertEquals("every event kept (drain)", events.length(), native.size)
        assertEquals("every event kept (ring)", events.length(), ring.size)

        for (i in 0 until events.length()) {
            val ev = events.getJSONObject(i)
            for (line in listOf(native[i], ring[i])) {
                for (k in ev.keys()) {
                    val key = when (k) { "ev" -> "kind"; "t" -> "nt"; else -> k }
                    assertTrue("$k of ${ev.optString("ev")} missing from $line", line.has(key))
                    val want = ev.get(k)
                    val got = line.get(key)
                    if (want is Number && got is Number) assertEquals("$k of ${ev.optString("ev")}", want.toDouble(), got.toDouble(), 0.0)
                    else assertEquals("$k of ${ev.optString("ev")}", want, got)
                }
                assertEquals("rs", line.getString("src"))
            }
            assertEquals(77L, ring[i].getLong("xfer"))
        }
    }
}
