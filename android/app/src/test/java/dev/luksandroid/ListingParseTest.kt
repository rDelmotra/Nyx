package dev.luksandroid

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class ListingParseTest {

    @Test
    fun `listing carries the unreadable count`() {
        val l = parseListing(
            """{"path":"/","entries":[{"name":"a","inode":12,"type":"file","isSubvolume":false,"size":3,"mtime":4}],"unreadable":2}""",
        )
        assertEquals(1, l.entries.size)
        assertEquals("a", l.entries[0].name)
        assertEquals(2, l.unreadable)
    }

    @Test
    fun `a listing without the field, or with null, reports zero`() {
        assertEquals(0, parseListing("""{"path":"/","entries":[]}""").unreadable)
        assertEquals(0, parseListing("""{"path":"/","entries":[],"unreadable":null}""").unreadable)
    }

    @Test
    fun `paged listing parses the same way`() {
        val l = parseListing("""{"path":"/","entries":[],"next_offset":0,"has_more":false,"total_count":0,"unreadable":1}""")
        assertEquals(1, l.unreadable)
    }

    @Test
    fun `volume info carries flushSupported as a tri-state`() {
        assertEquals(false, parseVolumeInfo("""{"flushSupported":false}""").flushSupported)
        assertEquals(true, parseVolumeInfo("""{"flushSupported":true}""").flushSupported)
        assertNull(parseVolumeInfo("""{"flushSupported":null}""").flushSupported)
        assertNull(parseVolumeInfo("""{"label":"x"}""").flushSupported)
    }
}
