package studio.conspect.cast.tv

import androidx.media3.common.util.TimestampAdjuster
import androidx.media3.common.util.UnstableApi
import androidx.test.ext.junit.runners.AndroidJUnit4
import org.junit.Assert.assertEquals
import org.junit.Test
import org.junit.runner.RunWith

/**
 * Device-only check from ADR-0114 (decision 2, "TS contract"). CI does not run it: it needs an emulator or a TV.
 * It covers the adjuster half of the contract: with MODE_NO_OFFSET the first timestamp is not subtracted, and
 * the 33-bit wrap is unwrapped against the previous timestamp.
 * The full chain (a synthetic TS through TsExtractor to the renderer's presentation time, and the reader stall)
 * is a device procedure, listed in clients/androidtv/README.md; it is not written yet.
 */
@UnstableApi
@RunWith(AndroidJUnit4::class)
class TsExtractorInstrumentedTest {
    private fun us(pts90k: Long) = pts90k * 1_000_000L / 90_000L

    @Test
    fun noOffsetAdjusterKeepsFirstPts() {
        val a = TimestampAdjuster(TimestampAdjuster.MODE_NO_OFFSET)
        val first = 90_000L * 5400 // 90 minutes into an engine run
        assertEquals(us(first), a.adjustTsTimestamp(first))
        assertEquals(us(first + 3000), a.adjustTsTimestamp(first + 3000))
    }

    @Test
    fun noOffsetAdjusterUnwrapsThe33BitCycle() {
        val a = TimestampAdjuster(TimestampAdjuster.MODE_NO_OFFSET)
        val cycle = 1L shl 33
        val before = cycle - 3000
        assertEquals(us(before), a.adjustTsTimestamp(before))
        // After the wrap the wire value restarts near 0; the adjuster continues on the same timeline.
        assertEquals(us(cycle + 3000), a.adjustTsTimestamp(3000))
    }
}
