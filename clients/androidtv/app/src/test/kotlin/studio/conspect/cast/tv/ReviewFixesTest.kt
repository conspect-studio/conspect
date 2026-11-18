package studio.conspect.cast.tv

import androidx.media3.common.ParserException
import androidx.media3.common.util.Log
import androidx.media3.common.util.UnstableApi
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.IOException


@UnstableApi
class RedactionTest {
    private val url = "https://cast.example.com/cast/$TOKEN/master.m3u8"

    @Test fun textRemovesTokenPathAndBareToken() {
        val r = Redact.text("failed $url and again $TOKEN")
        assertFalse(r.contains(TOKEN))
        assertTrue(r.contains("cast.example.com"))
    }

    @Test fun malformedPlaylistWithTokenUrlLeavesNoToken() {
        val captured = StringBuilder()
        Log.setLogger(object : Log.Logger {
            override fun d(tag: String, message: String, throwable: Throwable?) { captured.append(message) }
            override fun i(tag: String, message: String, throwable: Throwable?) { captured.append(message) }
            override fun w(tag: String, message: String, throwable: Throwable?) { captured.append(message) }
            override fun e(tag: String, message: String, throwable: Throwable?) { captured.append(message) }
        })
        try {
            Log.setLogLevel(Log.LOG_LEVEL_OFF)
            val ex = try {
                Redact.guardParse<Unit> { throw ParserException.createForMalformedManifest("bad playlist at $url", IOException(url)) }
                throw AssertionError("expected a throw")
            } catch (e: ParserException) { e }
            assertFalse(ex.message!!.contains(TOKEN))
            assertNull(ex.cause)
            Log.e("T", "player error for $url", ex)
            assertFalse(captured.toString().contains(TOKEN)) // level OFF: nothing reaches the logger
            assertEquals("", captured.toString())
        } finally {
            Log.setLogger(Log.Logger.DEFAULT)
        }
    }

    @Test fun ioFailureIsRedactedWithoutCause() {
        val e = Redact.io(IOException("Unable to connect to $url", RuntimeException(url)))
        assertFalse(e.message!!.contains(TOKEN))
        assertNull(e.cause)
    }

    @Test fun tokenGoneStatuses() {
        assertTrue(isTokenGone(404)); assertTrue(isTokenGone(403)); assertTrue(isTokenGone(410))
        assertFalse(isTokenGone(200)); assertFalse(isTokenGone(500)); assertFalse(isTokenGone(0))
    }
}

class GenerationGateTest {
    @Test fun staleCallbackIsRejectedAfterReplacementLoad() {
        val gate = GenerationGate()
        val old = gate.next()
        var failures = 0
        val oldFailure = { if (gate.isCurrent(old)) failures += 1 }
        val replacement = gate.next() // a new LOAD
        oldFailure() // the old attempt's connect failure arrives late
        assertEquals(0, failures)
        assertTrue(gate.isCurrent(replacement))
    }

    @Test fun invalidateRejectsEverything() {
        val gate = GenerationGate()
        val g = gate.next()
        gate.invalidate()
        assertFalse(gate.isCurrent(g))
    }
}

class ReportTickerTest {
    private class FakeScheduler {
        var now = 0L
        val queue = ArrayList<Pair<Long, Runnable>>()
        fun post(d: Long, r: Runnable) { queue.add(now + d to r) }
        fun cancel(r: Runnable) { queue.removeAll { it.second === r } }
        fun advance(ms: Long) {
            val end = now + ms
            while (true) {
                val next = queue.filter { it.first <= end }.minByOrNull { it.first } ?: break
                queue.remove(next); now = next.first; next.second.run()
            }
            now = end
        }
    }

    @Test fun oneReportEveryFiveSecondsAcrossAFallback() {
        val sch = FakeScheduler()
        var sent = 0
        val t = ReportTicker(sch::post, sch::cancel, 5000) { sent += 1 }
        t.start() // SRT rung
        sch.advance(5000); assertEquals(1, sent)
        t.start() // fallback to HLS restarts the loop: still one loop
        sch.advance(5000); assertEquals(2, sent)
        sch.advance(10_000); assertEquals(4, sent)
    }

    @Test fun cancelStopsReports() {
        val sch = FakeScheduler()
        var sent = 0
        val t = ReportTicker(sch::post, sch::cancel, 5000) { sent += 1 }
        t.start(); sch.advance(5000); t.cancel(); sch.advance(60_000)
        assertEquals(1, sent)
    }
}

class LossRecoveryTest {
    @Test fun onePacketGapContinuesInsideTheConnection() {
        val d = TsContinuityDetector()
        fun pkt(cc: Int, start: Boolean): ByteArray {
            val p = ByteArray(188) { 0xff.toByte() }
            p[0] = 0x47; p[1] = ((if (start) 0x40 else 0) or 1).toByte(); p[2] = 0; p[3] = (0x10 or cc).toByte()
            if (start) { p[4] = 0; p[5] = 0; p[6] = 1; p[7] = 0xe0.toByte() }
            return p
        }
        val a = pkt(0, true) + pkt(1, false) + pkt(3, false) // one packet (cc 2) lost
        assertTrue(d.feed(a, 0, a.size))
        val policy = LossPolicy()
        assertEquals(LossAction.CONTINUE_IN_CONNECTION, policy.onLoss(1000)) // no reopen, no step-down
    }

    @Test fun morethanThreeLossesInAMinuteStepDown() {
        val p = LossPolicy()
        assertEquals(LossAction.CONTINUE_IN_CONNECTION, p.onLoss(0))
        assertEquals(LossAction.CONTINUE_IN_CONNECTION, p.onLoss(1))
        assertEquals(LossAction.CONTINUE_IN_CONNECTION, p.onLoss(2))
        assertEquals(LossAction.STEP_DOWN, p.onLoss(3))
    }

    @Test fun liveEdgeSeekOnlyInsideTheBuffer() {
        assertNull(LiveEdge.seekTargetMs(10_000, 10_500, 800, 100)) // offset ok
        assertEquals(11_700L, LiveEdge.seekTargetMs(10_000, 12_000, 1_800, 100))
        assertNull(LiveEdge.seekTargetMs(10_000, 10_200, 1_800, 100)) // not buffered: no seek, no reopen
    }
}
