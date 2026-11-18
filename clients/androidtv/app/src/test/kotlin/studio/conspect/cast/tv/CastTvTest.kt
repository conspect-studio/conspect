package studio.conspect.cast.tv

import org.json.JSONObject
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test


class PassphraseTest {
    @Test fun vector() {
        assertEquals(EXPECTED_PASSPHRASE, CastSrt.passphrase(TOKEN, SID))
    }

    @Test fun lengthIs43() {
        assertEquals(43, CastSrt.passphrase(TOKEN, SID)!!.length)
    }

    @Test fun badTokenIsNull() {
        assertNull(CastSrt.passphrase("short", SID))
        assertNull(CastSrt.passphrase("!".repeat(43), SID))
    }

    @Test fun streamIdFormat() {
        assertEquals("#!::r=cast/$SID,m=request", CastSrt.streamId(SID))
    }

    @Test fun base64urlRoundTrip() {
        val data = ByteArray(32) { it.toByte() }
        assertEquals(TOKEN, B64Url.encode(data))
        assertEquals(data.toList(), B64Url.decode(TOKEN)!!.toList())
    }
}

class LoadUrlTest {
    private val good = "https://cast.example.com/cast/$TOKEN/master.m3u8"

    @Test fun good() {
        val p = LoadUrl.parse(good)
        assertNotNull(p)
        assertEquals("https://cast.example.com", p!!.origin)
        assertEquals(good, p.masterUrl)
    }

    @Test fun goodWithPortAndIpv6() {
        assertEquals("https://host:8443", LoadUrl.parse("https://host:8443/cast/$TOKEN/master.m3u8")!!.origin)
        assertEquals("https://[::1]:8443", LoadUrl.parse("https://[::1]:8443/cast/$TOKEN/master.m3u8")!!.origin)
    }

    @Test fun rejects() {
        val bad = listOf(
            "http://cast.example.com/cast/$TOKEN/master.m3u8",
            "$good?x=1",
            "$good#f",
            "$good?",
            "https://user:pw@cast.example.com/cast/$TOKEN/master.m3u8",
            "https://user@cast.example.com/cast/$TOKEN/master.m3u8",
            "https://cast.example.com/cast/$TOKEN/other.m3u8",
            "https://cast.example.com/x/cast/$TOKEN/master.m3u8",
            "https://cast.example.com/cast/${TOKEN.take(42)}/master.m3u8",
            "https://cast.example.com/cast/${TOKEN}A/master.m3u8",
            "https://cast.example.com/cast/$TOKEN/master.m3u8/",
            "",
            "not a url",
        )
        for (u in bad) assertNull(u, LoadUrl.parse(u))
        assertNull(LoadUrl.parse(null))
    }

    @Test fun toStringHidesToken() {
        assertFalse(LoadUrl.parse(good).toString().contains(TOKEN))
    }
}

class LadderTest {
    private val rx = ReceiverInfo("0.1.0", 34)

    @Test fun requestShape() {
        val o = JSONObject(LadderRequest.body(rx, "1.5.7", 0, null))
        assertEquals(1, o.getInt("v"))
        assertEquals("android", o.getJSONObject("receiver").getString("kind"))
        assertEquals("0.1.0", o.getJSONObject("receiver").getString("build"))
        assertEquals("Conspect-AndroidTV/0.1.0 Android/34", o.getJSONObject("receiver").getString("ua"))
        assertEquals("1.5.7", o.getJSONObject("caps").getString("srt"))
        assertTrue(o.getJSONObject("caps").isNull("frames"))
        assertEquals(0, o.getJSONObject("skip").length())
        assertTrue(o.isNull("failed"))
    }

    @Test fun requestSkipAndFailed() {
        val o = JSONObject(LadderRequest.body(rx, null, 540, LadderFailure("connect", 1500)))
        assertTrue(o.getJSONObject("caps").isNull("srt"))
        assertEquals(540, o.getJSONObject("skip").getInt("srt"))
        val f = o.getJSONObject("failed")
        assertEquals("srt", f.getString("rung"))
        assertEquals("connect", f.getString("stage"))
        assertEquals(1500, f.getInt("ms"))
    }

    @Test fun requestCarriesNoToken() {
        assertFalse(LadderRequest.body(rx, "1.5.7", 0, null).contains(TOKEN))
    }

    @Test fun answerSrt() {
        val a = LadderParser.parse(
            200,
            """{"v":1,"attach":{"rung":"srt","host":"h.example","port":9010,"latency_ms":250,"sid":"$SID","expires_in_ms":30000}}""",
        ) as LadderAnswer.Srt
        assertEquals("h.example", a.host)
        assertEquals(9010, a.port)
        assertEquals(250, a.latencyMs)
        assertEquals(SID, a.sid)
    }

    @Test fun answerSrtDefaultsLatency() {
        val a = LadderParser.parse(
            200, """{"v":1,"attach":{"rung":"srt","host":"h","port":9010,"sid":"$SID"}}""",
        ) as LadderAnswer.Srt
        assertEquals(250, a.latencyMs)
    }

    @Test fun answerHlsAndOthersAreHls() {
        assertEquals(LadderAnswer.Hls, LadderParser.parse(200, """{"v":1,"attach":{"rung":"hls"}}"""))
        assertEquals(LadderAnswer.Hls, LadderParser.parse(200, """{"v":1,"attach":{"rung":"moq","url":"x"}}"""))
        assertEquals(LadderAnswer.Hls, LadderParser.parse(200, """{"v":1,"attach":{"rung":"webrtc","whep":"whep"}}"""))
    }

    @Test fun answer404() {
        assertEquals(LadderAnswer.NotFound, LadderParser.parse(404, null))
    }

    @Test fun answer429() {
        assertEquals(LadderAnswer.RateLimited(3), LadderParser.parse(429, null, "3"))
        assertEquals(LadderAnswer.RateLimited(1), LadderParser.parse(429, null, null))
        assertEquals(LadderAnswer.RateLimited(5), LadderParser.parse(429, null, "9999"))
    }

    @Test fun malformedIsUnusable() {
        assertEquals(LadderAnswer.Unusable, LadderParser.parse(200, "{nope"))
        assertEquals(LadderAnswer.Unusable, LadderParser.parse(200, """{"v":2,"attach":{"rung":"hls"}}"""))
        assertEquals(LadderAnswer.Unusable, LadderParser.parse(200, """{"v":1}"""))
        assertEquals(LadderAnswer.Unusable, LadderParser.parse(500, "{}"))
        assertEquals(
            LadderAnswer.Unusable,
            LadderParser.parse(200, """{"v":1,"attach":{"rung":"srt","host":"h","port":9010,"sid":"short"}}"""),
        )
        assertEquals(
            LadderAnswer.Unusable,
            LadderParser.parse(200, """{"v":1,"attach":{"rung":"srt","host":"h","port":0,"sid":"$SID"}}"""),
        )
    }
}

class FailureMemoryTest {
    private class MemStore : FailureStore {
        val m = HashMap<String, Long>()
        override fun all() = HashMap(m)
        override fun put(origin: String, expiryMs: Long) { m[origin] = expiryMs }
        override fun remove(origin: String) { m.remove(origin) }
    }

    @Test fun expiresAfterFifteenMinutes() {
        var now = 1_000_000L
        val store = MemStore()
        val mem = FailureMemory(store) { now }
        assertFalse(mem.isActive("https://a"))
        mem.remember("https://a")
        assertTrue(mem.isActive("https://a"))
        assertFalse(mem.isActive("https://b"))
        assertEquals(900, mem.secondsLeft("https://a"))
        now += 899_000
        assertEquals(1, mem.secondsLeft("https://a"))
        assertTrue(mem.anyActive())
        now += 1_000
        assertEquals(0, mem.secondsLeft("https://a"))
        assertFalse(mem.anyActive())
        assertTrue(store.m.isEmpty())
    }

    @Test fun storeHoldsOriginAndExpiryOnly() {
        val store = MemStore()
        FailureMemory(store) { 5L }.remember("https://a")
        assertEquals(mapOf("https://a" to 5L + 900_000L), store.m)
    }
}

class LibsrtVersionTest {
    @Test fun floor() {
        assertTrue(LibsrtVersion.isAtLeastFloor(0x010507))
        assertTrue(LibsrtVersion.isAtLeastFloor(0x010508))
        assertTrue(LibsrtVersion.isAtLeastFloor(0x010600))
        assertTrue(LibsrtVersion.isAtLeastFloor(0x020000))
        assertFalse(LibsrtVersion.isAtLeastFloor(0x010506))
        assertFalse(LibsrtVersion.isAtLeastFloor(0x010504))
        assertFalse(LibsrtVersion.isAtLeastFloor(0))
    }

    @Test fun format() {
        assertEquals("1.5.7", LibsrtVersion.format(0x010507))
    }
}

class TsContinuityTest {
    private val vpid = 0x100

    private fun pkt(pid: Int, cc: Int, pesStart: Boolean): ByteArray {
        val p = ByteArray(188) { 0xff.toByte() }
        p[0] = 0x47
        p[1] = (((if (pesStart) 0x40 else 0) or (pid shr 8))).toByte()
        p[2] = (pid and 0xff).toByte()
        p[3] = (0x10 or (cc and 0xf)).toByte()
        if (pesStart) {
            p[4] = 0; p[5] = 0; p[6] = 1; p[7] = 0xe0.toByte()
        }
        return p
    }

    private fun feed(d: TsContinuityDetector, vararg p: ByteArray): Boolean {
        val all = p.fold(ByteArray(0)) { a, b -> a + b }
        return d.feed(all, 0, all.size)
    }

    @Test fun noGapAcrossWrap() {
        val d = TsContinuityDetector()
        val pk = (0..40).map { pkt(vpid, it, it == 0) }
        assertFalse(feed(d, *pk.toTypedArray()))
        assertEquals(0, d.gaps)
    }

    @Test fun gapDetected() {
        val d = TsContinuityDetector()
        assertFalse(feed(d, pkt(vpid, 0, true), pkt(vpid, 1, false)))
        assertTrue(feed(d, pkt(vpid, 3, false)))
        assertEquals(1, d.gaps)
    }

    @Test fun duplicateIsNotAGap() {
        val d = TsContinuityDetector()
        assertFalse(feed(d, pkt(vpid, 0, true), pkt(vpid, 1, false), pkt(vpid, 1, false)))
    }

    @Test fun otherPidsIgnored() {
        val d = TsContinuityDetector()
        assertFalse(feed(d, pkt(vpid, 0, true), pkt(0x101, 7, false), pkt(vpid, 1, false), pkt(0x101, 2, false)))
    }
}

class ReportTest {
    private val rx = ReceiverInfo("0.1.0", 34)

    @Test fun srtLatencyUnavailable() {
        assertEquals("unavailable", Report.latencyMethod("srt"))
        val o = JSONObject(Report.build(rx, "playing", "srt", 640, 10, null, 0, 0, null, 1))
        val t = o.getJSONObject("transport")
        assertEquals("unavailable", t.getString("latency_method"))
        assertTrue(t.isNull("latency_ms"))
        assertEquals(640, t.getLong("first_frame_ms"))
        assertEquals("srt", t.getString("rung"))
        assertEquals("android-tv", o.getJSONObject("receiver").getString("kind"))
    }

    @Test fun flushBudget() {
        val b = FlushBudget()
        assertFalse(b.record(0)); assertFalse(b.record(1000)); assertFalse(b.record(2000))
        assertTrue(b.record(3000))
        val c = FlushBudget()
        assertFalse(c.record(0)); assertFalse(c.record(30_000)); assertFalse(c.record(59_000))
        assertFalse(c.record(61_000)) // the first fell out of the window
    }
}
