package dev.luksandroid

import dev.luksandroid.transfer.availableFromStatVfs
import org.junit.Assert.assertEquals
import org.junit.Test

class NativeCancelScopeTest {
    @Test
    fun `token is scoped to the block and restored, including nested`() {
        assertEquals(0L, currentNativeCancelToken())
        withNativeCancel(5L) {
            assertEquals(5L, currentNativeCancelToken())
            withNativeCancel(9L) { assertEquals(9L, currentNativeCancelToken()) }
            assertEquals(5L, currentNativeCancelToken())
        }
        assertEquals(0L, currentNativeCancelToken())
    }

    @Test
    fun `token is restored when the block throws`() {
        try {
            withNativeCancel(5L) { throw IllegalStateException("x") }
        } catch (_: IllegalStateException) {
        }
        assertEquals(0L, currentNativeCancelToken())
    }

    @Test
    fun `token does not leak to another thread`() {
        withNativeCancel(5L) {
            var other = -1L
            val t = Thread { other = currentNativeCancelToken() }
            t.start(); t.join()
            assertEquals(0L, other)
        }
    }

    @Test
    fun `a non-positive token means none`() {
        withNativeCancel(-3L) { assertEquals(0L, currentNativeCancelToken()) }
    }

    @Test
    fun `statvfs available bytes saturate instead of overflowing`() {
        assertEquals(4096L * 10, availableFromStatVfs(10, 4096))
        assertEquals(0L, availableFromStatVfs(-1, 4096))
        assertEquals(Long.MAX_VALUE, availableFromStatVfs(Long.MAX_VALUE, 4096))
    }
}
